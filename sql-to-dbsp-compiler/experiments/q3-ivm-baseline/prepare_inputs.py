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

RF2 selection:

- clustered: order the base Orders by (o_custkey, o_orderkey). Let T be the sum
  of the N(K). The Orders of the lowest-keyed Customers, taken until they hold
  at least T Orders, are the candidates (the narrowest Customer-key prefix that
  holds T Orders). Pair K takes the next N(K) Orders in that order. Nothing is
  random.
- scattered: a uniform sample of T distinct row positions of `orders.tbl`,
  drawn with `random.Random(f"{seed}:scattered-rf2")` and dealt in order to the
  pairs. It samples existing rows, not integers in the sparse key range.

RF1 remapping. Only o_custkey of each RF1 Order changes. Let C be the sorted
list of all Customer keys in `customer.tbl`:

- clustered: pair K covers the Customers from the lowest to the highest
  o_custkey of its own RF2 set, which is the contiguous slice C[a..b] of width
  W = b - a + 1. The j-th Order of the file (0-based, ascending Order key) gets
  C[a + (j * W) // N(K)], so Orders sit on consecutive Customers in ascending
  order, and the RF1 and RF2 sets of a pair touch the same Customers.
- scattered: every Order gets an independent uniform draw from all of C, from
  `random.Random(f"{seed}:scattered-rf1")` consumed in pair and file order.

Equal seeds and inputs give byte-identical outputs on one Python version.
"""

from __future__ import annotations

import argparse
from array import array
from bisect import bisect_left
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


def scan_base_orders(path: Path, customers: list[int],
                     rf1_keys: set[int]) -> tuple[int, array]:
    """One pass over the base Orders: validate, count, and tally Orders per Customer.

    Returns the Order count and an array indexed by position in `customers`.
    """
    known = set(customers)
    per_customer = array("I", [0]) * len(customers)
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
        per_customer[bisect_left(customers, customer)] += 1
        count += 1
    return count, per_customer


def clustered_prefix(customers: list[int], per_customer: array, needed: int) -> int:
    """Return the highest Customer key of the narrowest prefix holding `needed` Orders."""
    held = 0
    for customer, orders in zip(customers, per_customer):
        held += orders
        if held >= needed:
            return customer
    raise InputError(f"base Orders hold {held}, fewer than the {needed} needed")


def select_rf2_candidates(path: Path, prefix_bound: int, sampled: dict[int, int]
                          ) -> tuple[list[tuple[int, int]], dict[int, tuple[int, int]]]:
    """One pass: gather the prefix Orders and the sampled rows.

    Returns [(o_custkey, o_orderkey)] for Customers up to `prefix_bound`, and
    {row position: (o_custkey, o_orderkey)} for the sampled positions.
    """
    prefix: list[tuple[int, int]] = []
    picked: dict[int, tuple[int, int]] = {}
    for position, row in enumerate(tbl_rows(path)):
        customer = order_customer(row, path)
        if customer <= prefix_bound or position in sampled:
            pair = (customer, order_key(row, path))
            if customer <= prefix_bound:
                prefix.append(pair)
            if position in sampled:
                picked[position] = pair
    prefix.sort()
    return prefix, picked


def write_rows(path: Path, rows: Iterable[bytes]) -> None:
    with path.open("wb") as target:
        for row in rows:
            target.write(row + b"\n")


def file_record(path: Path) -> dict[str, Any]:
    return {"sha256": sha256(path), "bytes": path.stat().st_size}


def customer_slice(customers: list[int], low: int, high: int) -> list[int]:
    return customers[bisect_left(customers, low):bisect_left(customers, high) + 1]


def remap_clustered(orders: list[bytes], slice_: list[int]) -> list[int]:
    """Assign consecutive Orders to consecutive Customers of `slice_`."""
    width = len(slice_)
    return [slice_[(index * width) // len(orders)] for index in range(len(orders))]


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
        "rf1_customer_min": min(pair["rf1_customers"]),
        "rf1_customer_max": max(pair["rf1_customers"]),
        "rf1_distinct_customers": len(set(pair["rf1_customers"])),
        "rf2_orders": len(pair["rf2_keys"]),
        "rf2_customer_min": min(pair["rf2_customers"]),
        "rf2_customer_max": max(pair["rf2_customers"]),
        "rf2_distinct_customers": len(set(pair["rf2_customers"])),
    }


def prepare(base: Path, updates: Path, out: Path, seed: int, pair_count: int) -> dict[str, Any]:
    if pair_count < 1:
        raise InputError("--pairs must be at least 1")
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
    order_count, per_customer = scan_base_orders(base_paths["orders"], customers, rf1_keys)
    if order_count < needed:
        raise InputError(f"base Orders hold {order_count}, fewer than the {needed} "
                         f"RF2 deletions needed for {pair_count} pairs")

    prefix_bound = clustered_prefix(customers, per_customer, needed)
    sample = random.Random(f"{seed}:scattered-rf2").sample(range(order_count), needed)
    owner_of_position: dict[int, int] = {}
    cursor = 0
    for index, size in enumerate(sizes):
        for position in sample[cursor:cursor + size]:
            owner_of_position[position] = index
        cursor += size
    prefix, picked = select_rf2_candidates(base_paths["orders"], prefix_bound,
                                           owner_of_position)

    scattered_rf1 = random.Random(f"{seed}:scattered-rf1")
    workloads: dict[str, list[dict[str, Any]]] = {name: [] for name in WORKLOADS}
    cursor = 0
    for index, (size, (orders, lines)) in enumerate(zip(sizes, update_sets)):
        number = index + 1
        block = prefix[cursor:cursor + size]
        cursor += size
        clustered_slice = customer_slice(customers, min(c for c, _ in block),
                                         max(c for c, _ in block))
        workloads["clustered"].append({
            "number": number, "orders": orders, "lines": lines,
            "rf2_keys": [key for _, key in block],
            "rf2_customers": [customer for customer, _ in block],
            "rf1_customers": remap_clustered(orders, clustered_slice),
        })
        scattered = [pair for position, pair in sorted(picked.items())
                     if owner_of_position[position] == index]
        workloads["scattered"].append({
            "number": number, "orders": orders, "lines": lines,
            "rf2_keys": [key for _, key in scattered],
            "rf2_customers": [customer for customer, _ in scattered],
            "rf1_customers": [customers[scattered_rf1.randrange(len(customers))]
                              for _ in orders],
        })

    manifest: dict[str, Any] = {
        "seed": seed, "pairs": pair_count,
        "base": {"customers": len(customers), "orders": order_count,
                 "clustered_rf2_customer_prefix_bound": prefix_bound},
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
                        help="seed of the scattered RF2 sample and the scattered RF1 remapping")
    parser.add_argument("--pairs", type=int, default=3, help="RF1/RF2 pairs per workload")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    try:
        manifest = prepare(args.base, args.updates, args.out, args.seed, args.pairs)
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
