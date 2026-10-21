"""Tests for the R5 manifest summarizer, on synthetic manifests."""

from __future__ import annotations

import contextlib
import copy
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import summarize  # noqa: E402


def owners(query: int, base: int, index: int) -> dict[str, dict[str, int]]:
    return {"query": {"operations": query, "bytes": query * 100},
            "base_relation": {"operations": base, "bytes": base * 100},
            "merged_index": {"operations": index, "bytes": index * 100}}


def driver_report(mode: str, scale: float, writes: tuple[int, int, int] = (1, 2, 0)) -> dict:
    """A driver report with two refreshes; `scale` multiplies every timing."""
    split = owners(*writes)
    refreshes = [{"refresh": number, "foreground_ms": 10.0 * scale * number,
                  "index_ms": 1.0 * scale * number, "q3_ms": 2.0 * scale * number,
                  "combined_maintenance_ms": 3.0 * scale * number}
                 for number in (1, 2)]
    return {
        "status": "complete", "mode": "native" if mode == "native" else "reconstructed",
        "refreshes": refreshes,
        "sequence": {
            "end_to_end_ms": 100.0 * scale, "observed_storage_bytes": 1000 + int(scale),
            "resources": {
                "cpu_microseconds": 90_000 * scale, "peak_rss_bytes": 5_000_000 + int(scale),
                "backend_write_operations": sum(writes),
                "backend_write_bytes": 100 * sum(writes),
                "backend_write_by_owner": split}}}


def manifest(scales: dict[str, list[float]]) -> dict:
    runs = []
    for mode, values in scales.items():
        for repetition, scale in enumerate(values, start=1):
            runs.append({"mode": mode, "repetition": repetition, "status": "complete",
                         "returncode": 0, "driver": driver_report(mode, scale)})
    return {"status": "complete", "errors": [], "platform": "test", "revision": "abc",
            "configuration": {"runs": 3}, "runs": runs}


class SummarizeTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def write_manifest(self, workload: str, content: dict) -> Path:
        path = self.root / workload / "manifest.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(content))
        return path

    def test_golden_path_medians_extremes_and_files(self) -> None:
        clustered = self.write_manifest("clustered", manifest(
            {"native": [3.0, 1.0, 2.0], "merged-index": [5.0, 7.0, 6.0]}))
        scattered = self.write_manifest("scattered", manifest(
            {"native": [1.0, 1.0, 1.0], "merged-index": [2.0, 2.0, 2.0]}))
        out = self.root / "out"
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(summarize.main([str(clustered), str(scattered), "--out", str(out)]), 0)
        summary = json.loads((out / "summary.json").read_text())
        native = summary["workloads"]["clustered"]["modes"]["native"]
        self.assertEqual(native["runs"], 3)
        wall = native["metrics"]["sequence_wall_ms"]
        self.assertEqual((wall["median"], wall["min"], wall["max"]), (200.0, 100.0, 300.0))
        self.assertEqual(native["metrics"]["sequence_cpu_ms"]["median"], 180.0)
        # Sum over the two refreshes of foreground_ms: 10 * scale * (1 + 2).
        self.assertEqual(native["metrics"]["refresh_foreground_ms"]["median"], 60.0)
        self.assertEqual(native["metrics"]["refresh_combined_maintenance_ms"]["max"], 27.0)
        self.assertEqual(native["metrics"]["write_operations_base_relation"]["median"], 2)
        self.assertEqual(native["metrics"]["write_bytes_total"]["median"], 300)
        self.assertEqual(native["metrics"]["observed_storage_bytes"]["max"], 1003)
        self.assertEqual(native["metrics"]["peak_rss_bytes"]["min"], 5_000_001)
        self.assertEqual(set(summary["workloads"]), {"clustered", "scattered"})
        markdown = (out / "summary.md").read_text()
        self.assertIn("## clustered", markdown)
        self.assertIn("Runs per mode: native 3, merged-index 3.", markdown)
        self.assertIn("| `sequence_wall_ms` | 200 | 100 | 300 | 600 | 500 | 700 |", markdown)
        self.assertTrue(markdown.endswith("|\n"))
        self.assertNotIn("  \n", markdown)

    def test_median_of_even_count_is_the_mean_of_the_middle_two(self) -> None:
        path = self.write_manifest("even", manifest(
            {"native": [1.0, 4.0, 2.0, 10.0], "merged-index": [1.0, 1.0, 1.0, 1.0]}))
        summary = summarize.summarize([path])
        wall = summary["workloads"]["even"]["modes"]["native"]["metrics"]["sequence_wall_ms"]
        self.assertEqual(wall["median"], 300.0)
        self.assertEqual(summary["workloads"]["even"]["modes"]["native"]["runs"], 4)

    def failing(self, content: dict, pattern: str) -> None:
        path = self.write_manifest("broken", content)
        with self.assertRaisesRegex(summarize.SummaryError, pattern):
            summarize.summarize([path])
        out = self.root / "unwritten"
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(summarize.main([str(path), "--out", str(out)]), 1)
        self.assertIn("summarize:", stderr.getvalue())
        self.assertFalse(out.exists(), "a failed summary must write nothing")

    def test_failed_run_or_campaign_is_an_error(self) -> None:
        content = manifest({"native": [1.0, 2.0], "merged-index": [1.0, 2.0]})
        content["runs"][1]["status"] = "failed"
        self.failing(content, "native run 2: run status 'failed'")
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        content["status"] = "failed"
        content["errors"] = ["RuntimeError: native run 1 exited 7"]
        self.failing(content, "campaign status is 'failed'")
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        content["runs"][0]["returncode"] = 7
        self.failing(content, "return code 7")

    def test_missing_or_invalid_field_is_an_error_not_a_zero(self) -> None:
        base = manifest({"native": [1.0, 2.0], "merged-index": [1.0, 2.0]})
        for path, pattern in [
            (("sequence", "end_to_end_ms"), "missing field sequence.end_to_end_ms"),
            (("sequence", "resources", "peak_rss_bytes"), "missing field .*peak_rss_bytes"),
            (("sequence", "observed_storage_bytes"), "missing field .*observed_storage_bytes"),
        ]:
            content = copy.deepcopy(base)
            driver = content["runs"][3]["driver"]
            holder = driver
            for step in path[:-1]:
                holder = holder[step]
            del holder[path[-1]]
            self.failing(content, f"merged-index run 2: {pattern}")
        content = copy.deepcopy(base)
        del content["runs"][0]["driver"]["refreshes"][1]["q3_ms"]
        self.failing(content, "native run 1 refresh 2: missing field q3_ms")
        content = copy.deepcopy(base)
        content["runs"][0]["driver"]["sequence"]["end_to_end_ms"] = None
        self.failing(content, "end_to_end_ms is None")
        content = copy.deepcopy(base)
        content["runs"][0]["driver"]["sequence"]["resources"] = {
            "unavailable": "independent validation enabled; counters omitted to exclude validation"}
        self.failing(content, "missing field sequence.resources.cpu_microseconds")
        content = copy.deepcopy(base)
        content["runs"][0]["driver"]["refreshes"] = []
        self.failing(content, "no refreshes")

    def test_inconsistent_write_attribution_is_an_error(self) -> None:
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        content["runs"][0]["driver"]["sequence"]["resources"]["backend_write_operations"] += 1
        self.failing(content, "inconsistent write attribution")

    def test_repetition_count_mismatch_is_an_error(self) -> None:
        self.failing(manifest({"native": [1.0, 2.0, 3.0], "merged-index": [1.0, 2.0]}),
                     "modes have different repetitions")
        self.failing(manifest({"native": [1.0, 2.0]}), "modes have different repetitions")

    def test_duplicate_workload_labels_are_an_error(self) -> None:
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        first = self.write_manifest("same", content)
        second = first.with_name("other.json")
        second.write_text(first.read_text())
        with self.assertRaisesRegex(summarize.SummaryError, "two manifests are named 'same'"):
            summarize.summarize([first, second])


if __name__ == "__main__":
    unittest.main()
