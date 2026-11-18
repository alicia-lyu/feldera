#!/usr/bin/env python3
"""Prepare the RF1/RF2 refresh inputs of the Linux SF 10 campaign.

Inputs are dbgen's output: the base tables (`customer.tbl`, `orders.tbl`,
`lineitem.tbl`, from `dbgen -T c` and `dbgen -T o`) and `-U N` update sets
(`orders.tbl.uK`, `lineitem.tbl.uK`; run `dbgen -U N` without `-f`, from inside
the output directory with `DSS_PATH=.`, because dbgen formats the update path
into a 128-byte buffer). dbgen's own `delete.K` files are not used.

For each workload (`clustered`, `scattered`) the script writes, under
`<out>/<workload>/`:

- `rf1-orders-K.tbl`, `rf1-lines-K.tbl`: update set K with its Orders re-pointed
  at new Customers (below). The Lineitem rows are copied unchanged, so every
  Order keeps its complete group of Lineitems.
- `rf2-delete-K.tbl`: one existing base Order key per line, in dbgen's
  `delete.K` format.
- `refreshes.json`: RF1 1, RF2 1, RF1 2, RF2 2, ... in the format of
  `measure.py --refresh-list` and the driver's `--refresh-list`.

`<out>/prepare-manifest.json` records the seed, the pair count, the sizes of
every set, and the SHA-256 of every input and output file.

With `--warm-up`, each workload also gets `<out>/<workload>/warm-up/`: one more
pair, from update set `pairs + 1` (run `dbgen -U pairs+1`), chosen by the same
rules after the measured pairs, with its own `refreshes.json` for the refresh
program's `--warm-up`. The measured pairs are the same with or without it.

Let N(K) be the number of Orders in update set K. Pair K is RF1 set K plus an
RF2 set of N(K) distinct base Orders. No base Order appears in two RF2 sets of
one workload. The base Order keys must be strictly ascending (what the driver's
loader requires), and every RF1 Order key must be absent from the base Orders
and from the other RF1 sets; the script fails otherwise.

RF2 selection (inserts and deletes are chosen independently): both workloads
delete the oldest Orders, as an analytical database ages out its history. Let T
be the sum of the N(K). The T lowest base Order keys are the candidates, and
pair K takes the next N(K) of them in key order. Nothing is random, so the two
workloads share their RF2 sets and differ only in RF1.

Transactions. With `--transaction-rows K`, each RF1 and RF2 set is cut into
consecutive transactions of about K rows (decision of 2026-11-17): Orders are
taken in file order, an Order counts as 1 + its Lineitems, and a transaction
closes once it holds K rows or more, so every Order stays whole and only a set's
last transaction can be smaller. An RF2 Order's Lineitems are counted from the
head of `lineitem.tbl`, which dbgen sorts by Order key. Each refresh of
`refreshes.json` then carries `transaction_orders`, the Order count of each of
its transactions in file order; the driver applies them one by one. The
refreshes keep their order (all of RF1 1, then all of RF2 1, ...). Without the
option each set is one transaction and the field is absent.

RF1 remapping. Only o_custkey of each RF1 Order changes. As TPC-H specifies,
only Customers whose key is not a multiple of 3 receive Orders (the eligible
Customers, E, in key order). Neither rule looks at the RF2 sets. Orders stay in
ascending Order-key order whatever Customer they get, so the Customer keys of a
file are not sorted:

- clustered: TPC-C's skew in sessions. E is cut into consecutive blocks of 3,000
  (`NURAND_BLOCK`; the last may be shorter), which stand in for TPC-C's
  districts, since TPC-H has none. A Customer is drawn by choosing a block with
  probability proportional to its size, then position NURand(1023, 1, size)
  within it, where NURand(A, x, y) = (((random(0, A) | random(x, y)) + C) %
  (y - x + 1)) + x (TPC-C clause 2.1.6) and C, one constant in 0..A, comes from
  `random.Random(f"{seed}:clustered-nurand-c")`. Each transaction of n Orders
  draws ceil(n / m) Customers (m = `--orders-per-session`, default 4) and gives
  them its Orders m at a time, in file order. The draws of pair K come from
  `random.Random(f"{seed}:clustered-rf1:{K}")`.
- scattered: every Order gets an independent uniform draw from E, from
  `random.Random(f"{seed}:scattered-rf1")` consumed in pair and file order.

The manifest records the NURand constants, m, and K, and for every pair the
distinct RF1 Customers (`rf1_distinct_customers`), the number of RF1 Orders held
by the 1, 10 and 100 most popular Customers of that pair
(`rf1_top_customer_orders`), and the transactions' sizes.

Equal seeds and inputs give byte-identical outputs on one Python version.
"""

from __future__ import annotations

import argparse
from collections import Counter
from itertools import islice
from pathlib import Path
import json
import random
import sys
from typing import Any, Callable, Iterable, Iterator

from measure import sha256, write_json


WORKLOADS = ("clustered", "scattered")
# TPC-C's customer NURand (clause 2.1.6): NURand(1023, 1, 3000) over a district's 3,000
# Customers. TPC-H has no districts, so consecutive blocks of the eligible Customers stand in.
NURAND_A = 1023
NURAND_BLOCK = 3000
DEFAULT_ORDERS_PER_SESSION = 4
BASE_FILES = {"customer": "customer.tbl", "orders": "orders.tbl", "lineitem": "lineitem.tbl"}


class InputError(ValueError):
    """The dbgen inputs or the requested sizes cannot produce a valid campaign."""


def tbl_rows(path: Path) -> Iterator[bytes]:
    """Yield the rows of a `.tbl` file without their newline; blank rows are errors."""
    with path.open("rb") as source:
        for number, raw in enumerate(source, start=1):
            row = raw.rstrip(b"\n")
            if not row:
                raise InputError(f"{path}:{number}: blank row")
            yield row


def integer_field(row: bytes, index: int, path: Path) -> int:
    parts = row.split(b"|", index + 1)
    try:
        return int(parts[index])
    except (IndexError, ValueError):
        raise InputError(f"{path}: cannot read field {index} of row {row[:60]!r}") from None


def order_key(row: bytes, path: Path) -> int:
    return integer_field(row, 0, path)


def order_customer(row: bytes, path: Path) -> int:
    return integer_field(row, 1, path)


def with_customer(order_row: bytes, customer_key: int) -> bytes:
    key, _, rest = order_row.split(b"|", 2)
    return b"|".join((key, str(customer_key).encode(), rest))


def read_update_set(updates: Path, number: int) -> tuple[list[bytes], list[bytes]]:
    """Read update set `number` and check that its Order and Lineitem groups are complete.

    Orders must ascend strictly. Every Lineitem must belong to an Order of the
    set, and the Lineitems must follow the Orders' order, so that each Order's
    Lineitems are contiguous.
    """
    orders_path = updates / f"orders.tbl.u{number}"
    lines_path = updates / f"lineitem.tbl.u{number}"
    for path in (orders_path, lines_path):
        if not path.is_file():
            raise InputError(f"missing dbgen update file {path}")
    orders = list(tbl_rows(orders_path))
    lines = list(tbl_rows(lines_path))
    if not orders:
        raise InputError(f"{orders_path} has no Orders")
    position: dict[int, int] = {}
    previous = None
    for row in orders:
        key = order_key(row, orders_path)
        if previous is not None and key <= previous:
            raise InputError(f"{orders_path}: Order keys must ascend strictly (at {key})")
        position[key] = len(position)
        previous = key
    last_position = -1
    for row in lines:
        key = order_key(row, lines_path)
        if key not in position:
            raise InputError(f"{lines_path}: Lineitem of Order {key} has no Order in {orders_path}")
        if position[key] < last_position:
            raise InputError(f"{lines_path}: Lineitems of Order {key} are not grouped in Order order")
        last_position = position[key]
    return orders, lines


def read_customer_keys(path: Path) -> list[int]:
    keys = sorted(integer_field(row, 0, path) for row in tbl_rows(path))
    if not keys:
        raise InputError(f"{path} has no Customers")
    if any(a == b for a, b in zip(keys, keys[1:])):
        raise InputError(f"{path} repeats a Customer key")
    return keys


def scan_base_orders(path: Path, customers: list[int], rf1_keys: set[int]) -> int:
    """One pass over the base Orders: validate them and return their count."""
    known = set(customers)
    count = 0
    previous = None
    for row in tbl_rows(path):
        key = order_key(row, path)
        if previous is not None and key <= previous:
            raise InputError(f"{path}: base Order keys must ascend strictly (at {key})")
        previous = key
        if key in rf1_keys:
            raise InputError(f"RF1 Order key {key} already exists in {path}")
        customer = order_customer(row, path)
        if customer not in known:
            raise InputError(f"{path}: Order {key} references missing Customer {customer}")
        count += 1
    return count


def oldest_orders(path: Path, needed: int) -> list[tuple[int, int]]:
    """Return (o_custkey, o_orderkey) of the `needed` lowest Order keys.

    `scan_base_orders` has checked that the keys ascend, so these are the file's
    first `needed` rows.
    """
    return [(order_customer(row, path), order_key(row, path))
            for row in islice(tbl_rows(path), needed)]


def write_rows(path: Path, rows: Iterable[bytes]) -> None:
    with path.open("wb") as target:
        for row in rows:
            target.write(row + b"\n")


def file_record(path: Path) -> dict[str, Any]:
    return {"sha256": sha256(path), "bytes": path.stat().st_size}


def eligible_customers(customers: list[int]) -> list[int]:
    """The Customers that may place Orders: TPC-H gives none to a key that is a multiple of 3."""
    eligible = [key for key in customers if key % 3]
    if not eligible:
        raise InputError("no Customer key is outside the multiples of 3")
    return eligible


def nurand(generator: random.Random, a: int, x: int, y: int, c: int) -> int:
    """TPC-C's NURand(A, x, y) with run-time constant C (clause 2.1.6)."""
    return (((generator.randint(0, a) | generator.randint(x, y)) + c) % (y - x + 1)) + x


def draw_nurand_customer(eligible: list[int], generator: random.Random, c: int) -> int:
    """One Customer: a block of `NURAND_BLOCK` eligible Customers chosen in proportion to its
    size, then NURand(`NURAND_A`, 1, size) within the block."""
    start = generator.randrange(len(eligible)) // NURAND_BLOCK * NURAND_BLOCK
    size = min(NURAND_BLOCK, len(eligible) - start)
    return eligible[start + nurand(generator, NURAND_A, 1, size, c) - 1]


def cut_transactions(rows_per_order: list[int], target: int | None) -> list[int]:
    """The Order count of each consecutive transaction: a transaction closes once it holds
    `target` rows or more; with no target the whole set is one transaction."""
    if target is None:
        return [len(rows_per_order)]
    counts: list[int] = []
    orders = rows = 0
    for order_rows in rows_per_order:
        orders += 1
        rows += order_rows
        if rows >= target:
            counts.append(orders)
            orders = rows = 0
    if orders:
        counts.append(orders)
    return counts


def session_customers(transaction_orders: list[int], per_session: int,
                      draw: Callable[[], int]) -> list[int]:
    """One Customer per Order: each transaction of n Orders draws ceil(n / per_session)
    Customers and gives them its Orders `per_session` at a time."""
    customers: list[int] = []
    for count in transaction_orders:
        drawn = [draw() for _ in range(-(-count // per_session))]
        customers.extend(drawn[index // per_session] for index in range(count))
    return customers


def rf1_rows_per_order(orders: list[bytes], lines: list[bytes], path: Path) -> list[int]:
    """1 + the Lineitem count of each Order of an update set, in file order."""
    counts = Counter(order_key(row, path) for row in lines)
    return [1 + counts[order_key(row, path)] for row in orders]


def base_line_counts(path: Path, keys: list[int]) -> dict[int, int]:
    """The Lineitem count of each Order in `keys`, read from the head of the base
    `lineitem.tbl`, whose Order keys ascend (dbgen's order)."""
    wanted = set(keys)
    highest = max(keys)
    counts: Counter[int] = Counter()
    previous = None
    for row in tbl_rows(path):
        key = order_key(row, path)
        if previous is not None and key < previous:
            raise InputError(f"{path}: base Lineitem Order keys must ascend (at {key})")
        previous = key
        if key > highest:
            break
        if key in wanted:
            counts[key] += 1
    return counts


def top_customer_orders(customers: list[int], tops: tuple[int, ...] = (1, 10, 100)
                        ) -> dict[str, int]:
    """Orders held by the `n` most popular Customers of `customers`, for each n in `tops`."""
    counts = sorted(Counter(customers).values(), reverse=True)
    return {str(n): sum(counts[:n]) for n in tops}


def write_workload(directory: Path, pairs: list[dict[str, Any]]) -> list[dict[str, str]]:
    """Write one workload's files and refreshes.json; return the refresh list."""
    directory.mkdir(parents=True, exist_ok=False)
    refreshes: list[dict[str, str]] = []
    for pair in pairs:
        number = pair["number"]
        orders_file = f"rf1-orders-{number}.tbl"
        lines_file = f"rf1-lines-{number}.tbl"
        delete_file = f"rf2-delete-{number}.tbl"
        write_rows(directory / orders_file,
                   (with_customer(row, customer)
                    for row, customer in zip(pair["orders"], pair["rf1_customers"])))
        write_rows(directory / lines_file, pair["lines"])
        write_rows(directory / delete_file,
                   (str(key).encode() for key in sorted(pair["rf2_keys"])))
        rf1: dict[str, Any] = {"rf1_orders": orders_file, "rf1_lines": lines_file}
        rf2: dict[str, Any] = {"rf2": delete_file}
        if pair["cut"]:
            rf1["transaction_orders"] = pair["rf1_transactions"]
            rf2["transaction_orders"] = pair["rf2_transactions"]
        refreshes.append(rf1)
        refreshes.append(rf2)
    write_json(directory / "refreshes.json", refreshes)
    return refreshes


def transaction_sizes(orders: list[int], rows_per_order: list[int]) -> dict[str, Any]:
    """The count and the row sizes (minimum, mean, maximum) of a set's transactions."""
    sizes, cursor = [], 0
    for count in orders:
        sizes.append(sum(rows_per_order[cursor:cursor + count]))
        cursor += count
    return {"count": len(orders), "rows_min": min(sizes), "rows_max": max(sizes),
            "rows_mean": sum(sizes) / len(sizes),
            "orders_mean": sum(orders) / len(orders)}


def describe_pair(pair: dict[str, Any]) -> dict[str, Any]:
    per_transaction, cursor = [], 0
    for count in pair["rf1_transactions"]:
        per_transaction.append(len(set(pair["rf1_customers"][cursor:cursor + count])))
        cursor += count
    return {
        "rf1_transactions": transaction_sizes(pair["rf1_transactions"], pair["rf1_rows"]),
        "rf2_transactions": transaction_sizes(pair["rf2_transactions"], pair["rf2_rows"]),
        "rf1_customers_per_transaction_mean": sum(per_transaction) / len(per_transaction),
        "pair": pair["number"],
        "rf1_orders": len(pair["orders"]),
        "rf1_lines": len(pair["lines"]),
        "rf1_distinct_customers": len(set(pair["rf1_customers"])),
        "rf1_top_customer_orders": top_customer_orders(pair["rf1_customers"]),
        "rf2_orders": len(pair["rf2_keys"]),
        "rf2_customer_min": min(pair["rf2_customers"]),
        "rf2_customer_max": max(pair["rf2_customers"]),
        "rf2_distinct_customers": len(set(pair["rf2_customers"])),
    }


def prepare(base: Path, updates: Path, out: Path, seed: int, pair_count: int,
            warm_up: bool = False, transaction_rows: int | None = None,
            orders_per_session: int = DEFAULT_ORDERS_PER_SESSION) -> dict[str, Any]:
    if pair_count < 1:
        raise InputError("--pairs must be at least 1")
    if transaction_rows is not None and transaction_rows < 1:
        raise InputError(f"--transaction-rows must be positive, got {transaction_rows}")
    if orders_per_session < 1:
        raise InputError(f"--orders-per-session must be positive, got {orders_per_session}")
    base_paths = {name: base / file for name, file in BASE_FILES.items()}
    for path in base_paths.values():
        if not path.is_file():
            raise InputError(f"missing dbgen base file {path}")
    for workload in WORKLOADS:
        if (out / workload).exists():
            raise InputError(f"{out / workload} already exists; use a fresh --out")

    # The warm-up pair, if any, is one more pair after the measured ones, so the measured
    # pairs do not depend on it.
    set_count = pair_count + (1 if warm_up else 0)
    update_sets = [read_update_set(updates, number) for number in range(1, set_count + 1)]
    rf1_keys: set[int] = set()
    for number, (orders, _) in enumerate(update_sets, start=1):
        keys = {order_key(row, updates / f"orders.tbl.u{number}") for row in orders}
        if keys & rf1_keys:
            raise InputError(f"update set {number} repeats an Order key of an earlier set")
        rf1_keys |= keys
    sizes = [len(orders) for orders, _ in update_sets]
    needed = sum(sizes)

    customers = read_customer_keys(base_paths["customer"])
    order_count = scan_base_orders(base_paths["orders"], customers, rf1_keys)
    if order_count < needed:
        raise InputError(f"base Orders hold {order_count}, fewer than the {needed} "
                         f"RF2 deletions needed for {set_count} pairs")

    oldest = oldest_orders(base_paths["orders"], needed)
    deleted_lines = base_line_counts(base_paths["lineitem"], [key for _, key in oldest])

    eligible = eligible_customers(customers)
    nurand_c = random.Random(f"{seed}:clustered-nurand-c").randint(0, NURAND_A)
    scattered_rf1 = random.Random(f"{seed}:scattered-rf1")
    workloads: dict[str, list[dict[str, Any]]] = {name: [] for name in WORKLOADS}
    cursor = 0
    for index, (size, (orders, lines)) in enumerate(zip(sizes, update_sets)):
        number = index + 1
        block = oldest[cursor:cursor + size]
        cursor += size
        rf1_rows = rf1_rows_per_order(orders, lines, updates / f"lineitem.tbl.u{number}")
        rf2_keys = [key for _, key in block]
        rf2_rows = [1 + deleted_lines[key] for key in rf2_keys]
        shared = {
            "number": number, "orders": orders, "lines": lines, "cut": transaction_rows is not None,
            "rf1_rows": rf1_rows, "rf2_rows": rf2_rows,
            "rf1_transactions": cut_transactions(rf1_rows, transaction_rows),
            "rf2_transactions": cut_transactions(rf2_rows, transaction_rows),
            "rf2_keys": rf2_keys,
            "rf2_customers": [customer for customer, _ in block],
        }
        clustered_draws = random.Random(f"{seed}:clustered-rf1:{number}")
        workloads["clustered"].append({**shared, "rf1_customers": session_customers(
            shared["rf1_transactions"], orders_per_session,
            lambda: draw_nurand_customer(eligible, clustered_draws, nurand_c))})
        workloads["scattered"].append({**shared, "rf1_customers": [
            eligible[scattered_rf1.randrange(len(eligible))] for _ in orders]})

    manifest: dict[str, Any] = {
        "seed": seed, "pairs": pair_count, "warm_up": warm_up,
        "transaction_rows": transaction_rows, "orders_per_session": orders_per_session,
        "nurand": {"a": NURAND_A, "block": NURAND_BLOCK, "c": nurand_c},
        "base": {"customers": len(customers), "eligible_customers": len(eligible),
                 "orders": order_count,
                 "rf2_highest_order_key": oldest[-1][1]},
        "inputs": {}, "workloads": {},
        "unused_dbgen_files": [f"delete.{n}" for n in range(1, set_count + 1)],
    }
    for name, path in base_paths.items():
        manifest["inputs"][name] = {"path": str(path), **file_record(path)}
    for number in range(1, set_count + 1):
        for table in ("orders", "lineitem"):
            path = updates / f"{table}.tbl.u{number}"
            manifest["inputs"][f"{table}_u{number}"] = {"path": str(path), **file_record(path)}
    for name in WORKLOADS:
        directory = out / name
        measured, warming = workloads[name][:pair_count], workloads[name][pair_count:]
        write_workload(directory, measured)
        manifest["workloads"][name] = {
            "pairs": [describe_pair(pair) for pair in measured],
            "files": {path.name: file_record(path)
                      for path in sorted(directory.iterdir()) if path.is_file()},
        }
        if warming:
            write_workload(directory / "warm-up", warming)
            manifest["workloads"][name]["warm_up"] = {
                "pairs": [describe_pair(pair) for pair in warming],
                "files": {path.name: file_record(path)
                          for path in sorted((directory / "warm-up").iterdir())},
            }
    write_json(out / "prepare-manifest.json", manifest)
    return manifest


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--base", type=Path, required=True,
                        help="directory with dbgen customer.tbl, orders.tbl, lineitem.tbl")
    parser.add_argument("--updates", type=Path, required=True,
                        help="directory with dbgen -U output (orders.tbl.uK, lineitem.tbl.uK)")
    parser.add_argument("--out", type=Path, required=True,
                        help="new directory for clustered/, scattered/ and prepare-manifest.json")
    parser.add_argument("--seed", type=int, required=True,
                        help="seed of the scattered RF2 sample and of both RF1 remappings")
    parser.add_argument("--pairs", type=int, default=3, help="RF1/RF2 pairs per workload")
    parser.add_argument("--transaction-rows", type=int,
                        help="cut every RF1 and RF2 set into transactions of about this many "
                             "rows (whole Orders, an Order counting 1 + its Lineitems); by "
                             "default each set is one transaction")
    parser.add_argument("--orders-per-session", type=int, default=DEFAULT_ORDERS_PER_SESSION,
                        help="clustered: Orders each drawn Customer gets within a transaction "
                             f"(default {DEFAULT_ORDERS_PER_SESSION})")
    parser.add_argument("--warm-up", action="store_true",
                        help="also write one warm-up pair per workload, from update set "
                             "pairs + 1, into <workload>/warm-up/")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    try:
        manifest = prepare(args.base, args.updates, args.out, args.seed, args.pairs,
                           args.warm_up, args.transaction_rows, args.orders_per_session)
    except (InputError, OSError) as error:
        print(f"prepare_inputs: {error}", file=sys.stderr)
        return 1
    for name, workload in manifest["workloads"].items():
        sizes = [(pair["rf1_orders"], pair["rf1_lines"], pair["rf2_orders"])
                 for pair in workload["pairs"]]
        print(f"{name}: (RF1 orders, RF1 lines, RF2 orders) per pair = {sizes}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
