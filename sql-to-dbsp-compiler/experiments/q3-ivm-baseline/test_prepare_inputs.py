"""Tests for the SF 10 campaign input preparation, on a tiny dbgen-shaped fixture."""

from __future__ import annotations

import collections
import contextlib
import io
import json
from pathlib import Path
import random
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import prepare_inputs  # noqa: E402

CUSTOMERS = 40
BASE_ORDERS = 160
UPDATE_ORDERS = 6
PAIRS = 3


def customer_row(key: int) -> str:
    return f"{key}|Customer#{key:09d}|addr|{key % 25}|phone|1.00|BUILDING|comment|"


def order_row(key: int, customer: int) -> str:
    return f"{key}|{customer}|O|1.00|1996-02-29|5-LOW|Clerk|0|comment|"


def line_row(order: int, number: int) -> str:
    return (f"{order}|2|3|{number}|5|1.00|0.04|0.02|N|O|"
            "1996-03-13|1996-02-12|1996-03-22|i|m|c|")


def sparse_key(index: int, update: int = 0) -> int:
    """dbgen's key layout: 8 used keys in every block of 32; update K uses offset 8K."""
    return (index // 8) * 32 + 8 * update + index % 8 + 1


def write_lines(path: Path, rows: list[str]) -> None:
    path.write_text("".join(row + "\n" for row in rows))


def make_dbgen_fixture(root: Path) -> tuple[Path, Path]:
    """Write base tables and `-U 3` updates shaped like dbgen's, with sparse Order keys."""
    base, updates = root / "base", root / "updates"
    base.mkdir()
    updates.mkdir()
    write_lines(base / "customer.tbl", [customer_row(key) for key in range(1, CUSTOMERS + 1)])
    # Customers whose key is a multiple of 3 place no Orders, as in dbgen.
    holders = [key for key in range(1, CUSTOMERS + 1) if key % 3]
    orders = [(sparse_key(i), holders[(i * 7) % len(holders)]) for i in range(BASE_ORDERS)]
    write_lines(base / "orders.tbl", [order_row(key, customer) for key, customer in orders])
    write_lines(base / "lineitem.tbl",
                [line_row(key, n) for key, _ in orders for n in range(1, 1 + key % 4 + 1)])
    for update in range(1, PAIRS + 1):
        keys = [sparse_key(i, update) for i in range(UPDATE_ORDERS)]
        write_lines(updates / f"orders.tbl.u{update}",
                    [order_row(key, 1 + key % CUSTOMERS) for key in keys])
        write_lines(updates / f"lineitem.tbl.u{update}",
                    [line_row(key, n) for key in keys for n in range(1, 1 + key % 3 + 1)])
        write_lines(updates / f"delete.{update}", [str(sparse_key(i)) for i in range(UPDATE_ORDERS)])
    return base, updates


def read_keys(path: Path) -> list[int]:
    return [int(line.split("|")[0]) for line in path.read_text().splitlines()]


def read_customers(path: Path) -> list[int]:
    return [int(line.split("|")[1]) for line in path.read_text().splitlines()]


class PrepareInputsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.base, self.updates = make_dbgen_fixture(self.root)
        self.base_orders = {
            int(line.split("|")[0]): int(line.split("|")[1])
            for line in (self.base / "orders.tbl").read_text().splitlines()}

    def prepare(self, name: str = "out", seed: int = 11, pairs: int = PAIRS) -> Path:
        out = self.root / name
        prepare_inputs.prepare(self.base, self.updates, out, seed, pairs)
        return out

    def rf2_sets(self, out: Path, workload: str, pairs: int = PAIRS) -> list[list[int]]:
        return [read_keys(out / workload / f"rf2-delete-{n}.tbl") for n in range(1, pairs + 1)]

    def test_rf2_sets_are_existing_distinct_and_never_reused(self) -> None:
        out = self.prepare()
        for workload in prepare_inputs.WORKLOADS:
            seen: set[int] = set()
            for keys in self.rf2_sets(out, workload):
                self.assertEqual(len(keys), UPDATE_ORDERS)
                self.assertEqual(len(set(keys)), UPDATE_ORDERS)
                self.assertTrue(set(keys) <= set(self.base_orders))
                self.assertFalse(seen & set(keys), f"{workload} reuses an Order across pairs")
                seen |= set(keys)

    def test_rf1_keeps_ascending_orders_and_grouped_lines(self) -> None:
        out = self.prepare()
        for workload in prepare_inputs.WORKLOADS:
            for number in range(1, PAIRS + 1):
                orders = read_keys(out / workload / f"rf1-orders-{number}.tbl")
                lines = read_keys(out / workload / f"rf1-lines-{number}.tbl")
                self.assertEqual(orders, sorted(set(orders)))
                self.assertEqual(sorted(set(lines)), orders)
                positions = [orders.index(key) for key in lines]
                self.assertEqual(positions, sorted(positions))
                # Remapping changes only o_custkey, and lines are copied byte for byte.
                original = (self.updates / f"orders.tbl.u{number}").read_text().splitlines()
                for before, after in zip(original, (out / workload / f"rf1-orders-{number}.tbl")
                                         .read_text().splitlines()):
                    self.assertEqual(before.split("|")[2:], after.split("|")[2:])
                    self.assertEqual(before.split("|")[0], after.split("|")[0])
                self.assertEqual((self.updates / f"lineitem.tbl.u{number}").read_text(),
                                 (out / workload / f"rf1-lines-{number}.tbl").read_text())

    def test_clustered_sets_use_the_lowest_customers(self) -> None:
        out = self.prepare()
        manifest = json.loads((out / "prepare-manifest.json").read_text())
        bound = manifest["base"]["clustered_rf2_customer_prefix_bound"]
        deleted = [key for keys in self.rf2_sets(out, "clustered") for key in keys]
        needed = len(deleted)
        # Every Order of a Customer below the bound is deleted or the bound's own customer holds
        # the rest, and the prefix below the bound holds fewer than `needed` Orders.
        below = [key for key, customer in self.base_orders.items() if customer < bound]
        self.assertLess(len(below), needed)
        self.assertTrue(set(below) <= set(deleted))
        self.assertLessEqual(max(self.base_orders[key] for key in deleted), bound)
        # Scattered sets spread far wider than the clustered prefix.
        scattered = [self.base_orders[key] for keys in self.rf2_sets(out, "scattered")
                     for key in keys]
        self.assertGreater(max(scattered), bound)

    def test_clustered_rf1_customers_do_not_depend_on_the_rf2_sets(self) -> None:
        before = self.prepare("before")
        # Rotate which Customer owns which base Order: the lowest-keyed Customers, and so the
        # clustered RF2 sets, change; the Customer keys and the seed stay the same.
        rows = (self.base / "orders.tbl").read_text().splitlines()
        rotated = [order_row(int(row.split("|")[0]), CUSTOMERS - int(row.split("|")[1]) + 1)
                   for row in rows]
        write_lines(self.base / "orders.tbl", rotated)
        after = self.prepare("after")
        self.assertNotEqual(self.rf2_sets(before, "clustered"), self.rf2_sets(after, "clustered"))
        for number in range(1, PAIRS + 1):
            name = Path("clustered") / f"rf1-orders-{number}.tbl"
            self.assertEqual((before / name).read_bytes(), (after / name).read_bytes())

    def test_clustered_rf1_customers_are_valid_unsorted_and_seeded(self) -> None:
        out = self.prepare()
        manifest = json.loads((out / "prepare-manifest.json").read_text())
        valid = set(range(1, CUSTOMERS + 1))
        for number in range(1, PAIRS + 1):
            customers = read_customers(out / "clustered" / f"rf1-orders-{number}.tbl")
            self.assertEqual(len(customers), UPDATE_ORDERS)
            self.assertTrue(set(customers) <= valid)
        # The permutation is shared by the pairs, and the seed changes it.
        other = self.prepare("other", seed=12)
        pooled = [read_customers(directory / "clustered" / f"rf1-orders-{n}.tbl")
                  for directory in (out, other) for n in range(1, PAIRS + 1)]
        self.assertNotEqual(pooled[:PAIRS], pooled[PAIRS:])
        self.assertIn("rf1_top_customer_orders", manifest["workloads"]["clustered"]["pairs"][0])

    def test_zipf_draw_favors_the_top_rank_and_skew_grows_with_the_exponent(self) -> None:
        permutation = list(range(100, 0, -1))  # rank 0 is Customer 100
        draws = 20000

        def counts(exponent: float) -> list[int]:
            cumulative = prepare_inputs.zipf_cumulative_weights(len(permutation), exponent)
            drawn = prepare_inputs.draw_zipf_customers(
                permutation, cumulative, random.Random("zipf-test"), draws)
            tally = collections.Counter(drawn)
            self.assertEqual(sum(tally.values()), draws)
            return [tally[customer] for customer in permutation]

        mild, steep = counts(0.5), counts(1.5)
        for tally in (mild, steep):
            self.assertEqual(tally[0], max(tally))
            self.assertGreater(tally[0], tally[9])
        self.assertGreater(steep[0], mild[0])
        # Rank 0 has weight 1 of the total, so its share is the reciprocal of that total.
        expected = 1 / sum(1 / (rank + 1) ** 1.5 for rank in range(len(permutation)))
        self.assertAlmostEqual(steep[0] / draws, expected, delta=0.02)

    def test_top_customer_orders_counts_the_most_popular_customers(self) -> None:
        customers = [5] * 7 + [6] * 3 + [7, 8]
        self.assertEqual(prepare_inputs.top_customer_orders(customers),
                         {"1": 7, "10": 12, "100": 12})

    def test_manifest_records_the_exponent_and_the_rf1_skew(self) -> None:
        out = self.prepare()
        manifest = json.loads((out / "prepare-manifest.json").read_text())
        self.assertEqual(manifest["zipf_exponent"], 1.0)
        for workload in prepare_inputs.WORKLOADS:
            for number, pair in enumerate(manifest["workloads"][workload]["pairs"], start=1):
                customers = read_customers(out / workload / f"rf1-orders-{number}.tbl")
                self.assertEqual(pair["rf1_distinct_customers"], len(set(customers)))
                top = pair["rf1_top_customer_orders"]
                self.assertEqual(sorted(top), ["1", "10", "100"])
                self.assertEqual(top["100"], UPDATE_ORDERS)
                self.assertLessEqual(top["1"], top["10"])
        steep = prepare_inputs.prepare(self.base, self.updates, self.root / "steep", 11,
                                       PAIRS, zipf_exponent=3.0)
        self.assertEqual(steep["zipf_exponent"], 3.0)

    def test_invalid_zipf_exponents_are_rejected(self) -> None:
        for index, exponent in enumerate((0.0, -1.0, float("nan"), float("inf"))):
            with self.assertRaisesRegex(prepare_inputs.InputError, "zipf-exponent"):
                prepare_inputs.prepare(self.base, self.updates, self.root / f"bad{index}", 11,
                                       PAIRS, zipf_exponent=exponent)
            self.assertFalse((self.root / f"bad{index}").exists())

    def test_same_seed_gives_identical_bytes_and_other_seed_differs(self) -> None:
        first, second = self.prepare("first"), self.prepare("second")
        names = sorted(path.relative_to(first) for path in first.rglob("*") if path.is_file())
        self.assertIn(Path("scattered/refreshes.json"), names)
        for name in names:
            self.assertEqual((first / name).read_bytes(), (second / name).read_bytes(), name)
        other = self.prepare("other", seed=12)
        self.assertNotEqual(self.rf2_sets(first, "scattered"), self.rf2_sets(other, "scattered"))
        self.assertEqual(self.rf2_sets(first, "clustered"), self.rf2_sets(other, "clustered"))

    def test_too_few_orders_is_an_error(self) -> None:
        for name in ("orders.tbl", "lineitem.tbl"):
            rows = (self.base / name).read_text().splitlines()
            kept = {key for key in read_keys(self.base / "orders.tbl")[:UPDATE_ORDERS * PAIRS - 1]}
            write_lines(self.base / name, [row for row in rows if int(row.split("|")[0]) in kept])
        with self.assertRaisesRegex(prepare_inputs.InputError, "fewer than the 18"):
            self.prepare()

    def test_invalid_inputs_are_rejected(self) -> None:
        orders = (self.updates / "orders.tbl.u1").read_text().splitlines()
        write_lines(self.updates / "orders.tbl.u1", orders[::-1])
        with self.assertRaisesRegex(prepare_inputs.InputError, "ascend"):
            self.prepare("descending")
        write_lines(self.updates / "orders.tbl.u1", orders)
        lines = (self.updates / "lineitem.tbl.u1").read_text().splitlines()
        write_lines(self.updates / "lineitem.tbl.u1", lines[::-1])
        with self.assertRaisesRegex(prepare_inputs.InputError, "not grouped"):
            self.prepare("ungrouped")
        write_lines(self.updates / "lineitem.tbl.u1", lines)
        # Give the first RF1 Order the key of an existing base Order, and its Lineitems with it.
        colliding, first = sparse_key(0), sparse_key(0, 1)
        write_lines(self.updates / "orders.tbl.u1", [order_row(colliding, 1)] + orders[1:])
        write_lines(self.updates / "lineitem.tbl.u1",
                    [row.replace(f"{first}|", f"{colliding}|", 1) if row.startswith(f"{first}|")
                     else row for row in lines])
        with self.assertRaisesRegex(prepare_inputs.InputError, "already exists"):
            self.prepare("collision")

    def test_refuses_an_existing_output_and_a_missing_update_set(self) -> None:
        out = self.prepare()
        with self.assertRaisesRegex(prepare_inputs.InputError, "already exists"):
            self.prepare("out")
        self.assertTrue(out.is_dir())
        with self.assertRaisesRegex(prepare_inputs.InputError, "missing dbgen update file"):
            self.prepare("too-many-pairs", pairs=PAIRS + 1)

    def test_refresh_list_matches_the_loader_format(self) -> None:
        # Mirrors measure.refresh_inputs and the driver's `deny_unknown_fields` Refresh struct.
        import measure

        out = self.prepare()
        for workload in prepare_inputs.WORKLOADS:
            refreshes, inputs = measure.refresh_inputs(out / workload / "refreshes.json")
            self.assertEqual([sorted(item) for item in refreshes],
                             [["rf1_lines", "rf1_orders"], ["rf2"]] * PAIRS)
            self.assertTrue(all(path.is_file() for path in inputs.values()))

    def test_manifest_records_seed_sizes_and_hashes(self) -> None:
        out = self.prepare()
        manifest = json.loads((out / "prepare-manifest.json").read_text())
        self.assertEqual((manifest["seed"], manifest["pairs"]), (11, PAIRS))
        self.assertEqual(manifest["base"]["orders"], BASE_ORDERS)
        self.assertEqual(manifest["base"]["customers"], CUSTOMERS)
        self.assertEqual(manifest["inputs"]["orders"]["sha256"],
                         prepare_inputs.sha256(self.base / "orders.tbl"))
        for workload in prepare_inputs.WORKLOADS:
            record = manifest["workloads"][workload]
            self.assertEqual([pair["rf2_orders"] for pair in record["pairs"]],
                             [UPDATE_ORDERS] * PAIRS)
            for name, file in record["files"].items():
                self.assertEqual(file["sha256"], prepare_inputs.sha256(out / workload / name))

    def test_command_line_reports_errors_without_a_traceback(self) -> None:
        argv = ["--base", str(self.base), "--updates", str(self.updates),
                "--out", str(self.root / "cli"), "--seed", "3"]
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(prepare_inputs.main(argv), 0)
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(prepare_inputs.main(argv), 1)
        self.assertIn("already exists", stderr.getvalue())
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            prepare_inputs.parse_args(["--base", "a", "--updates", "b", "--out", "c"])
        self.assertEqual(prepare_inputs.parse_args(
            argv + ["--zipf-exponent", "1.5"]).zipf_exponent, 1.5)
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(prepare_inputs.main(
                argv[:-4] + ["--out", str(self.root / "cli-bad"), "--seed", "3",
                             "--zipf-exponent", "0"]), 1)
        self.assertIn("zipf-exponent", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
