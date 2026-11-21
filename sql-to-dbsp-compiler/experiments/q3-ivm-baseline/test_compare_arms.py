from __future__ import annotations

import json
from pathlib import Path
import tempfile
import unittest

import compare_arms


def refresh(number: int, phase: str, ivm: list[float], *, pressure: str = "critical",
            memtable: dict | None = None, merges: int = 3) -> dict:
    spine = {"owner": "query", "runs_at_end": 4, "completed_merges": merges,
             "merge_real_ms": 1000.0}
    if memtable is not None:
        spine.update(memtable)
    return {
        "refresh": number, "phase": phase, "transactions": len(ivm),
        "transaction_latency": {"ivm_ms": {"values": ivm},
                                "base_ms": {"values": [10.0] * len(ivm)}},
        "memory_pressure": {"before": {"runtime": "low"}, "after": {"runtime": pressure}},
        "index_run_reads": {"session": {"seeks": 4 * len(ivm),
                                        "filtered_prefix_probes": 2 * len(ivm)}},
        "run_counts": {
            "device_reads": {"by_read_owner": {
                "foreground": {"query": {"operations": 3 * len(ivm), "bytes": 0}},
                "other": {"merged_index": {"operations": len(ivm), "bytes": 0}}}},
            "spines": [spine, {"owner": "query", "runs_at_start": 1}],   # one transient
        },
    }


def run(mode: str, refreshes: list[dict], status: str = "complete") -> dict:
    return {"mode": mode, "status": status,
            "driver": {"refreshes": refreshes,
                       "sequence": {"window_peak_rss_bytes": 700 << 20,
                                    "window_end_rss_bytes": 650 << 20}}}


class CompareArmsTest(unittest.TestCase):
    def write(self, runs: list[dict]) -> Path:
        directory = Path(self.enterContext(tempfile.TemporaryDirectory())) / "pilot-arm-x"
        directory.mkdir()
        path = directory / "manifest.json"
        path.write_text(json.dumps({"runs": runs}))
        return path

    def test_refresh_one_is_left_out_and_percentiles_are_nearest_rank(self) -> None:
        values = [float(v) for v in range(1, 101)]
        path = self.write([run("merged-index", [refresh(1, "rf1", [999.0] * 100),
                                                refresh(2, "rf1", values)])])
        text = compare_arms.render([path], first_refresh=2)
        self.assertIn("| arm-x | merged-index | RF1 | 50.00 / 95.00 / 99.00 | 3.0 | 4.0 | 2.0 "
                      "| 1.00 | 10.00 |", text)
        self.assertNotIn("999", text)

    def test_a_single_transaction_gives_every_percentile(self) -> None:
        path = self.write([run("native", [refresh(2, "rf2", [7.0])])])
        text = compare_arms.render([path], first_refresh=2)
        self.assertIn("| arm-x | native | RF2 | 7.00 / 7.00 / 7.00 | 3.0 | — | — | — |", text)

    def test_reports_before_version_18_show_no_memtable(self) -> None:
        path = self.write([run("native", [refresh(2, "rf1", [1.0])])])
        self.assertIn("| 3 (1 s) | query 4 | — | 700 / 650 |",
                      compare_arms.render([path], first_refresh=2))

    def test_memtable_fields_are_summarized(self) -> None:
        fields = {"memtable_flushes": 2, "memtable_bytes_at_end": 8 << 20,
                  "memtable_entries_at_end": 10, "memtable_node_copies": 1500,
                  "memtable_copy_ms": 2500.0}
        path = self.write([run("native", [refresh(2, "rf1", [1.0], memtable=fields),
                                          refresh(3, "rf2", [1.0], memtable=fields)])])
        self.assertIn("4 flushes; 8–8 MiB held at refresh ends; 1,500–1,500 node copies, "
                      "2.5–2.5 s per refresh", compare_arms.render([path], first_refresh=2))

    def test_an_idle_memtable_reads_off(self) -> None:
        fields = {"memtable_flushes": 0, "memtable_bytes_at_end": 0,
                  "memtable_entries_at_end": 0, "memtable_node_copies": 0,
                  "memtable_copy_ms": 0.0}
        path = self.write([run("native", [refresh(2, "rf1", [1.0], memtable=fields)])])
        self.assertIn("| off |", compare_arms.render([path], first_refresh=2))

    def test_an_incomplete_run_is_listed_not_measured(self) -> None:
        path = self.write([run("native", [], status="failed"),
                           run("merged-index", [refresh(2, "rf1", [2.0])])])
        text = compare_arms.render([path], first_refresh=2)
        self.assertIn("| arm-x | native | status `failed`, not measured |", text)
        self.assertNotIn("| arm-x | native | RF1", text)
        self.assertIn("| arm-x | merged-index | RF1 |", text)


if __name__ == "__main__":
    unittest.main()
