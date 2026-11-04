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

RF1 remapping. Only o_custkey of each RF1 Order changes. Let C be the sorted
list of all Customer keys in `customer.tbl`. Neither rule looks at the RF2
sets. Orders stay in ascending Order-key order whatever Customer they get, so
the Customer keys of a file are not sorted:

- clustered: popularity skew without key locality. Shuffle C once into the
  permutation P with `random.Random(f"{seed}:clustered-rf1-permutation")`; all
  pairs share P, so the same Customers stay popular across pairs. Each Order
  draws a rank r in 0..len(C)-1 with probability proportional to
  1 / (r + 1)^s (Zipf with exponent s, `--zipf-exponent`, default 1.0) and gets
  Customer P[r]. The draws of pair K come from
  `random.Random(f"{seed}:clustered-rf1:{K}")`, one per Order in file order,
  by binary search over the cumulative weights.
- scattered: every Order gets an independent uniform draw from all of C, from
  `random.Random(f"{seed}:scattered-rf1")` consumed in pair and file order.

The manifest records s, and for every pair the distinct RF1 Customers
(`rf1_distinct_customers`) and the number of RF1 Orders held by the 1, 10 and
100 most popular Customers of that pair (`rf1_top_customer_orders`).

Equal seeds and inputs give byte-identical outputs on one Python version.
"""

from __future__ import annotations

import argparse
from bisect import bisect_right
from collections import Counter
from itertools import accumulate, islice
import math
from pathlib import Path
import json
import random
import sys
from typing import Any, Iterable, Iterator

from measure import sha256, write_json


WORKLOADS = ("clustered", "scattered")
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


def zipf_cumulative_weights(count: int, exponent: float) -> list[float]:
    """Cumulative weights of rank r = 0..count-1 with weight 1 / (r + 1)^exponent."""
    return list(accumulate(1.0 / (rank + 1) ** exponent for rank in range(count)))


def draw_zipf_customers(permutation: list[int], cumulative: list[float],
                        generator: random.Random, count: int) -> list[int]:
    """Draw `count` Customers: rank by Zipf weight, then look the rank up in `permutation`."""
    total = cumulative[-1]
    last_rank = len(permutation) - 1
    # min() guards the float edge where u * total rounds up to the total.
    return [permutation[min(bisect_right(cumulative, generator.random() * total), last_rank)]
            for _ in range(count)]


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
        refreshes.append({"rf1_orders": orders_file, "rf1_lines": lines_file})
        refreshes.append({"rf2": delete_file})
    write_json(directory / "refreshes.json", refreshes)
    return refreshes


def describe_pair(pair: dict[str, Any]) -> dict[str, Any]:
    return {
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
            zipf_exponent: float = 1.0) -> dict[str, Any]:
    if pair_count < 1:
        raise InputError("--pairs must be at least 1")
    if not (math.isfinite(zipf_exponent) and zipf_exponent > 0):
        raise InputError(f"--zipf-exponent must be finite and positive, got {zipf_exponent}")
    base_paths = {name: base / file for name, file in BASE_FILES.items()}
    for path in base_paths.values():
        if not path.is_file():
            raise InputError(f"missing dbgen base file {path}")
    for workload in WORKLOADS:
        if (out / workload).exists():
            raise InputError(f"{out / workload} already exists; use a fresh --out")

    update_sets = [read_update_set(updates, number) for number in range(1, pair_count + 1)]
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
                         f"RF2 deletions needed for {pair_count} pairs")

    oldest = oldest_orders(base_paths["orders"], needed)

    scattered_rf1 = random.Random(f"{seed}:scattered-rf1")
    popularity = customers.copy()
    random.Random(f"{seed}:clustered-rf1-permutation").shuffle(popularity)
    cumulative = zipf_cumulative_weights(len(popularity), zipf_exponent)
    workloads: dict[str, list[dict[str, Any]]] = {name: [] for name in WORKLOADS}
    cursor = 0
    for index, (size, (orders, lines)) in enumerate(zip(sizes, update_sets)):
        number = index + 1
        block = oldest[cursor:cursor + size]
        cursor += size
        workloads["clustered"].append({
            "number": number, "orders": orders, "lines": lines,
            "rf2_keys": [key for _, key in block],
            "rf2_customers": [customer for customer, _ in block],
            "rf1_customers": draw_zipf_customers(
                popularity, cumulative,
                random.Random(f"{seed}:clustered-rf1:{number}"), len(orders)),
        })
        workloads["scattered"].append({
            "number": number, "orders": orders, "lines": lines,
            "rf2_keys": [key for _, key in block],
            "rf2_customers": [customer for customer, _ in block],
            "rf1_customers": [customers[scattered_rf1.randrange(len(customers))]
                              for _ in orders],
        })

    manifest: dict[str, Any] = {
        "seed": seed, "pairs": pair_count, "zipf_exponent": zipf_exponent,
        "base": {"customers": len(customers), "orders": order_count,
                 "rf2_highest_order_key": oldest[-1][1]},
        "inputs": {}, "workloads": {},
        "unused_dbgen_files": [f"delete.{n}" for n in range(1, pair_count + 1)],
    }
    for name, path in base_paths.items():
        manifest["inputs"][name] = {"path": str(path), **file_record(path)}
    for number in range(1, pair_count + 1):
        for table in ("orders", "lineitem"):
            path = updates / f"{table}.tbl.u{number}"
            manifest["inputs"][f"{table}_u{number}"] = {"path": str(path), **file_record(path)}
    for name in WORKLOADS:
        directory = out / name
        write_workload(directory, workloads[name])
        manifest["workloads"][name] = {
            "pairs": [describe_pair(pair) for pair in workloads[name]],
            "files": {path.name: file_record(path) for path in sorted(directory.iterdir())},
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
    parser.add_argument("--zipf-exponent", type=float, default=1.0,
                        help="Zipf exponent s of the clustered RF1 Customer draw (finite, > 0)")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    try:
        manifest = prepare(args.base, args.updates, args.out, args.seed, args.pairs,
                           args.zipf_exponent)
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
