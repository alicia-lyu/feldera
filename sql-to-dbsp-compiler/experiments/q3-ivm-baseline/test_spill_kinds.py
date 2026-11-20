from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("spill_kinds",
                                              Path(__file__).with_name("spill_kinds.py"))
spill_kinds = importlib.util.module_from_spec(SPEC)
# Dataclasses look their module up in sys.modules.
sys.modules["spill_kinds"] = spill_kinds
SPEC.loader.exec_module(spill_kinds)

WINDOW = {"started_micros": 100, "finished_micros": 200}


def flush(file: str, size: int, finished: int, owner: str = "query") -> dict:
    return {"file": file, "bytes": size, "blocks": 1, "started_micros": finished - 1,
            "finished_micros": finished, "owner": owner, "succeeded": True}


def record(kind: str, file: str | None, started: int = 110, finished: int = 130) -> dict:
    return {"kind": kind, "owner": "query", "spine": "s", "level": 0,
            "started_micros": started, "finished_micros": finished, "input_runs": 1,
            "input_records": 1, "output_records": 1,
            "output_location": "storage" if file else "memory", "output_file": file,
            "output_file_bytes": None}


class SpillKindsTest(unittest.TestCase):
    def test_every_kind_is_classed_and_unnamed_files_are_step_built(self) -> None:
        flushes = [flush("/a", 1000, 150), flush("/a", 24, 160),   # one file, two writes
                   flush("/b", 10, 150), flush("/m", 4096, 170), flush("/s", 5, 180),
                   flush("/base", 7, 150, owner="base_relation"),
                   flush("/late", 9, 201), flush("/early", 9, 99)]
        records = [record("background_compaction", "a"), record("insert_spill", "/b"),
                   record("memtable_flush", "/m", started=120, finished=170),
                   record("background_compaction", "/base"),
                   record("background_compaction", None)]
        totals = spill_kinds.classify(flushes, records, WINDOW)
        self.assertEqual(set(totals), {("view", "background_compaction"),
                                       ("view", "insert_spill"), ("view", "memtable_flush"),
                                       ("view", "step_built"), ("base", "background_compaction")})
        compaction = totals[("view", "background_compaction")]
        self.assertEqual((compaction.files, compaction.writes, compaction.bytes), (1, 2, 1024))
        memtable = totals[("view", "memtable_flush")]
        self.assertEqual((memtable.files, memtable.bytes, memtable.seal_micros), (1, 4096, [50]))
        self.assertEqual(totals[("view", "step_built")].bytes, 5)
        self.assertEqual(totals[("base", "background_compaction")].files, 1)
        self.assertEqual(totals[("view", "insert_spill")].seal_micros, [])

    def test_flushes_outside_the_window_are_left_out(self) -> None:
        self.assertEqual(spill_kinds.classify([flush("/x", 1, 99), flush("/y", 1, 201)], [],
                                              WINDOW), {})

    def test_report_is_per_transaction_with_the_seal_time(self) -> None:
        totals = spill_kinds.classify([flush("/m", 2048, 150)],
                                      [record("memtable_flush", "/m", 100, 104)], WINDOW)
        lines = spill_kinds.report_lines("native", totals, transactions=2)
        self.assertEqual(lines[0], "native: 1 files, 2 transactions in the window")
        self.assertIn("files/txn   0.50", lines[1])
        self.assertIn("KiB/txn     1.0", lines[1])
        self.assertIn("mean seal ms    0.00", lines[1])

    def test_manifest_reads_each_runs_diagnostics(self) -> None:
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            diagnostics = root / "runs" / "run-1-native" / "diagnostics"
            diagnostics.mkdir(parents=True)
            (diagnostics / "summary.json").write_text(json.dumps({"window": WINDOW}))
            (diagnostics / "flush-log.jsonl").write_text(json.dumps(flush("/s", 10, 150)) + "\n")
            (diagnostics / "compaction-log.jsonl").write_text("")
            manifest = {"runs": [
                {"mode": "native", "repetition": 1,
                 "driver": {"refreshes": [{"transactions": 3}, {"transactions": 2}]}},
                {"mode": "reconstructed", "repetition": 1, "driver": {"refreshes": []}}]}
            (root / "manifest.json").write_text(json.dumps(manifest))
            lines = spill_kinds.manifest_lines(root / "manifest.json")
        self.assertEqual(lines[0], "native: 1 files, 5 transactions in the window")
        self.assertIn("view step_built", lines[1])
        self.assertTrue(lines[2].startswith("reconstructed: no diagnostics at"))


if __name__ == "__main__":
    unittest.main()
