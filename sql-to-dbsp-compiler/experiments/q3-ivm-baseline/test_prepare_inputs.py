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

    def test_the_warm_up_pair_follows_the_measured_pairs_and_leaves_them_unchanged(self) -> None:
        measured = PAIRS - 1
        plain = self.root / "plain"
        prepare_inputs.prepare(self.base, self.updates, plain, 11, measured)
        warmed = self.root / "warmed"
        manifest = prepare_inputs.prepare(self.base, self.updates, warmed, 11, measured,
                                          warm_up=True)
        self.assertTrue(manifest["warm_up"])
        for workload in prepare_inputs.WORKLOADS:
            with self.subTest(workload=workload):
                files = sorted(path.name for path in (plain / workload).iterdir())
                for name in files:
                    self.assertEqual((plain / workload / name).read_bytes(),
                                     (warmed / workload / name).read_bytes(), name)
                warm = warmed / workload / "warm-up"
                refreshes = json.loads((warm / "refreshes.json").read_text())
                self.assertEqual(refreshes, [
                    {"rf1_orders": f"rf1-orders-{PAIRS}.tbl",
                     "rf1_lines": f"rf1-lines-{PAIRS}.tbl"},
                    {"rf2": f"rf2-delete-{PAIRS}.tbl"}])
                warm_rf2 = set(read_keys(warm / f"rf2-delete-{PAIRS}.tbl"))
                measured_rf2 = {key for keys in self.rf2_sets(warmed, workload, measured)
                                for key in keys}
                self.assertTrue(warm_rf2)
                self.assertFalse(warm_rf2 & measured_rf2)
                self.assertEqual(len(manifest["workloads"][workload]["pairs"]), measured)
                self.assertEqual(len(manifest["workloads"][workload]["warm_up"]["pairs"]), 1)

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

    def test_rf2_deletes_the_lowest_order_keys_in_both_workloads(self) -> None:
        out = self.prepare()
        clustered = self.rf2_sets(out, "clustered")
        # Both workloads delete the oldest Orders, so they share their RF2 sets.
        self.assertEqual(clustered, self.rf2_sets(out, "scattered"))
        deleted = [key for keys in clustered for key in keys]
        # Pair K takes the next block of the lowest base Order keys, in key order.
        self.assertEqual(deleted, sorted(self.base_orders)[:len(deleted)])
        manifest = json.loads((out / "prepare-manifest.json").read_text())
        self.assertEqual(manifest["base"]["rf2_highest_order_key"], deleted[-1])

    def test_clustered_rf1_customers_do_not_depend_on_the_rf2_sets(self) -> None:
        before = self.prepare("before")
        # Rotate which Customer owns which base Order; the Customer keys and the seed stay the
        # same. Neither the RF1 Customers nor the RF2 keys depend on the Orders' Customers.
        rows = (self.base / "orders.tbl").read_text().splitlines()
        rotated = [order_row(int(row.split("|")[0]), CUSTOMERS - int(row.split("|")[1]) + 1)
                   for row in rows]
        write_lines(self.base / "orders.tbl", rotated)
        after = self.prepare("after")
        self.assertEqual(self.rf2_sets(before, "clustered"), self.rf2_sets(after, "clustered"))
        for number in range(1, PAIRS + 1):
            name = Path("clustered") / f"rf1-orders-{number}.tbl"
            self.assertEqual((before / name).read_bytes(), (after / name).read_bytes())

    def test_clustered_rf1_customers_are_valid_unsorted_and_seeded(self) -> None:
        out = self.prepare()
        manifest = json.loads((out / "prepare-manifest.json").read_text())
        valid = {key for key in range(1, CUSTOMERS + 1) if key % 3}
        for number in range(1, PAIRS + 1):
            customers = read_customers(out / "clustered" / f"rf1-orders-{number}.tbl")
            self.assertEqual(len(customers), UPDATE_ORDERS)
            self.assertTrue(set(customers) <= valid)
        # The seed changes the draws.
        other = self.prepare("other", seed=12)
        pooled = [read_customers(directory / "clustered" / f"rf1-orders-{n}.tbl")
                  for directory in (out, other) for n in range(1, PAIRS + 1)]
        self.assertNotEqual(pooled[:PAIRS], pooled[PAIRS:])
        self.assertIn("rf1_top_customer_orders", manifest["workloads"]["clustered"]["pairs"][0])

    def test_only_customers_outside_the_multiples_of_3_receive_orders(self) -> None:
        out = self.prepare()
        manifest = json.loads((out / "prepare-manifest.json").read_text())
        eligible = [key for key in range(1, CUSTOMERS + 1) if key % 3]
        self.assertEqual(manifest["base"]["eligible_customers"], len(eligible))
        self.assertEqual(prepare_inputs.eligible_customers(list(range(1, CUSTOMERS + 1))),
                         eligible)
        for workload in prepare_inputs.WORKLOADS:
            for number in range(1, PAIRS + 1):
                customers = read_customers(out / workload / f"rf1-orders-{number}.tbl")
                self.assertTrue(all(customer % 3 for customer in customers), workload)
        with self.assertRaisesRegex(prepare_inputs.InputError, "multiples of 3"):
            prepare_inputs.eligible_customers([3, 6, 9])

    def test_nurand_follows_tpcc(self) -> None:
        class Fixed:
            """Returns the queued values from randint, checking each requested range."""

            def __init__(self, values: list[tuple[int, int, int]]) -> None:
                self.values = values

            def randint(self, low: int, high: int) -> int:
                expected_low, expected_high, value = self.values.pop(0)
                assert (low, high) == (expected_low, expected_high), (low, high)
                return value

        # (((5 | 9) + 7) % 10) + 1 = ((13 + 7) % 10) + 1 = 1
        self.assertEqual(prepare_inputs.nurand(Fixed([(0, 15, 5), (1, 10, 9)]), 15, 1, 10, 7), 1)
        # (((2 | 4) + 0) % 10) + 1 = 7
        self.assertEqual(prepare_inputs.nurand(Fixed([(0, 15, 2), (1, 10, 4)]), 15, 1, 10, 0), 7)
        generator = random.Random("nurand")
        drawn = [prepare_inputs.nurand(generator, 1023, 1, 3000, 123) for _ in range(30000)]
        self.assertTrue(all(1 <= value <= 3000 for value in drawn))
        top = collections.Counter(drawn).most_common(1)[0][1]
        self.assertGreater(top, 5 * len(drawn) / 3000, "NURand should be skewed")

    def test_nurand_customers_stay_in_proportional_blocks(self) -> None:
        eligible = list(range(1, 7001))  # blocks of 3,000, 3,000 and 1,000
        generator = random.Random("blocks")
        drawn = [prepare_inputs.draw_nurand_customer(eligible, generator, 5)
                 for _ in range(70000)]
        self.assertTrue(set(drawn) <= set(eligible))
        blocks = collections.Counter((customer - 1) // prepare_inputs.NURAND_BLOCK
                                     for customer in drawn)
        for block, share in ((0, 3 / 7), (1, 3 / 7), (2, 1 / 7)):
            self.assertAlmostEqual(blocks[block] / len(drawn), share, delta=0.01)

    def test_transactions_close_at_the_row_target_and_keep_orders_whole(self) -> None:
        self.assertEqual(prepare_inputs.cut_transactions([3, 2, 5, 1, 1], 4), [2, 1, 2])
        self.assertEqual(prepare_inputs.cut_transactions([3, 2, 5, 1, 1], None), [5])
        self.assertEqual(prepare_inputs.cut_transactions([9, 9], 1), [1, 1])

    def test_sessions_give_each_drawn_customer_up_to_m_orders(self) -> None:
        draws = iter([10, 20, 30, 40])
        self.assertEqual(prepare_inputs.session_customers([5, 2], 2, lambda: next(draws)),
                         [10, 10, 20, 20, 30, 40, 40])
        expected = collections.Counter()
        for count in range(1, 9):
            customers = prepare_inputs.session_customers([count], 4, iter(range(100)).__next__)
            self.assertEqual(len(customers), count)
            self.assertEqual(len(set(customers)), -(-count // 4))
            expected[count] = max(collections.Counter(customers).values())
        self.assertTrue(all(value <= 4 for value in expected.values()))

    def test_transaction_rows_cut_each_set_in_file_order(self) -> None:
        out = self.root / "cut"
        manifest = prepare_inputs.prepare(self.base, self.updates, out, 11, PAIRS,
                                          transaction_rows=5, orders_per_session=2)
        self.assertEqual((manifest["transaction_rows"], manifest["orders_per_session"]), (5, 2))
        base_lines = collections.Counter(read_keys(self.base / "lineitem.tbl"))
        for workload in prepare_inputs.WORKLOADS:
            refreshes = json.loads((out / workload / "refreshes.json").read_text())
            self.assertEqual([sorted(item) for item in refreshes],
                             [["rf1_lines", "rf1_orders", "transaction_orders"],
                              ["rf2", "transaction_orders"]] * PAIRS)
            for number in range(1, PAIRS + 1):
                rf1, rf2 = refreshes[2 * number - 2], refreshes[2 * number - 1]
                orders = read_keys(out / workload / rf1["rf1_orders"])
                lines = collections.Counter(read_keys(out / workload / rf1["rf1_lines"]))
                deleted = read_keys(out / workload / rf2["rf2"])
                for keys, counts, rows_of in ((orders, rf1["transaction_orders"], lines),
                                              (deleted, rf2["transaction_orders"], base_lines)):
                    self.assertEqual(sum(counts), len(keys))
                    cursor = 0
                    for index, count in enumerate(counts):
                        rows = sum(1 + rows_of[key] for key in keys[cursor:cursor + count])
                        if index < len(counts) - 1:
                            self.assertGreaterEqual(rows, 5)
                            # Without its last Order the transaction was below the target.
                            last = keys[cursor + count - 1]
                            self.assertLess(rows - 1 - rows_of[last], 5)
                        cursor += count
                pair = manifest["workloads"][workload]["pairs"][number - 1]
                self.assertEqual(pair["rf1_transactions"]["count"],
                                 len(rf1["transaction_orders"]))
                if workload == "clustered":
                    customers = read_customers(out / workload / rf1["rf1_orders"])
                    cursor = 0
                    for count in rf1["transaction_orders"]:
                        self.assertLessEqual(len(set(customers[cursor:cursor + count])),
                                             -(-count // 2))
                        cursor += count
        import measure

        resolved, _ = measure.refresh_inputs(out / "clustered" / "refreshes.json")
        self.assertEqual(resolved[0]["transaction_orders"],
                         json.loads((out / "clustered" / "refreshes.json").read_text())[0][
                             "transaction_orders"])
        # Without the option each set is one transaction and the field is absent.
        plain = self.prepare("plain")
        refreshes = json.loads((plain / "clustered" / "refreshes.json").read_text())
        self.assertTrue(all("transaction_orders" not in item for item in refreshes))

    def test_invalid_transaction_sizes_are_rejected(self) -> None:
        for index, arguments in enumerate(({"transaction_rows": 0},
                                           {"orders_per_session": 0})):
            with self.assertRaisesRegex(prepare_inputs.InputError, "must be positive"):
                prepare_inputs.prepare(self.base, self.updates, self.root / f"bad{index}", 11,
                                       PAIRS, **arguments)
            self.assertFalse((self.root / f"bad{index}").exists())

    def test_top_customer_orders_counts_the_most_popular_customers(self) -> None:
        customers = [5] * 7 + [6] * 3 + [7, 8]
        self.assertEqual(prepare_inputs.top_customer_orders(customers),
                         {"1": 7, "10": 12, "100": 12})

    def test_manifest_records_the_nurand_constants_and_the_rf1_skew(self) -> None:
        out = self.prepare()
        manifest = json.loads((out / "prepare-manifest.json").read_text())
        self.assertEqual(manifest["nurand"]["a"], 1023)
        self.assertEqual(manifest["nurand"]["block"], 3000)
        self.assertTrue(0 <= manifest["nurand"]["c"] <= 1023)
        self.assertEqual((manifest["transaction_rows"], manifest["orders_per_session"]),
                         (None, 4))
        for workload in prepare_inputs.WORKLOADS:
            for number, pair in enumerate(manifest["workloads"][workload]["pairs"], start=1):
                customers = read_customers(out / workload / f"rf1-orders-{number}.tbl")
                self.assertEqual(pair["rf1_distinct_customers"], len(set(customers)))
                top = pair["rf1_top_customer_orders"]
                self.assertEqual(sorted(top), ["1", "10", "100"])
                self.assertEqual(top["100"], UPDATE_ORDERS)
                self.assertLessEqual(top["1"], top["10"])
                self.assertEqual(pair["rf1_transactions"]["count"], 1)

    def test_same_seed_gives_identical_bytes_and_other_seed_differs(self) -> None:
        first, second = self.prepare("first"), self.prepare("second")
        names = sorted(path.relative_to(first) for path in first.rglob("*") if path.is_file())
        self.assertIn(Path("scattered/refreshes.json"), names)
        for name in names:
            self.assertEqual((first / name).read_bytes(), (second / name).read_bytes(), name)
        other = self.prepare("other", seed=12)
        # The seed drives only RF1; RF2 is the oldest Orders whatever the seed.
        for workload in ("clustered", "scattered"):
            self.assertEqual(self.rf2_sets(first, workload), self.rf2_sets(other, workload))
        self.assertNotEqual((first / "scattered" / "rf1-orders-1.tbl").read_bytes(),
                            (other / "scattered" / "rf1-orders-1.tbl").read_bytes())

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
        parsed = prepare_inputs.parse_args(argv + ["--transaction-rows", "10"])
        self.assertEqual((parsed.transaction_rows, parsed.orders_per_session), (10, 4))
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(prepare_inputs.main(
                argv[:-4] + ["--out", str(self.root / "cli-bad"), "--seed", "3",
                             "--transaction-rows", "0"]), 1)
        self.assertIn("transaction-rows", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
