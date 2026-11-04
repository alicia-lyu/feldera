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


def index_reads(seeks: int) -> dict[str, dict[str, int]]:
    """Every reader with `seeks` seeks and twice as many key, value and weight reads."""
    return {reader: {"seeks": seeks, "filter_rejections": 0, "key_decodes": 2 * seeks,
                     "value_decodes": 2 * seeks, "weight_decodes": 2 * seeks}
            for reader in summarize.INDEX_READERS}


def view_reads(index: dict | None, query: int) -> dict[str, dict[str, int]]:
    """The view worker's reads: `query` of every count under `query`, and the index readers'
    sums under `merged_index` (zero without an index)."""
    return {
        "query": {field: query for field in summarize.RUN_READ_FIELDS},
        "base_relation": {field: 0 for field in summarize.RUN_READ_FIELDS},
        "merged_index": {field: sum(reads[field] for reads in index.values()) if index else 0
                         for field in summarize.RUN_READ_FIELDS},
    }


LAYOUT = {snapshot: {"file": {"runs": 1, "bytes": 10}, "memory": {"runs": 1, "bytes": 5}}
          for snapshot in ("before", "after")}


def driver_report(mode: str, scale: float, writes: tuple[int, int, int] = (1, 2, 0)) -> dict:
    """A driver report with two refreshes; `scale` multiplies every timing, and, in the
    merged-index mode, refresh `n` makes `n * scale` index seeks per reader."""
    split = owners(*writes)
    native = mode == "native"
    refreshes = [{"refresh": number, "foreground_ms": 10.0 * scale * number,
                  "base_ms": 4.0 * scale * number, "forward_ms": 0.5 * scale * number,
                  "index_ms": 1.0 * scale * number, "q3_ms": 2.0 * scale * number,
                  "ivm_ms": 2.0 * scale * number,
                  "index_run_reads": None if native else index_reads(int(number * scale)),
                  "index_layout": None if native else LAYOUT}
                 for number in (1, 2)]
    for refresh in refreshes:
        refresh["view_run_reads"] = view_reads(refresh["index_run_reads"],
                                               10 * refresh["refresh"])
    return {
        "status": "complete", "mode": "native" if mode == "native" else "reconstructed",
        "measurement_version": 5, "os": "linux",
        "refreshes": refreshes,
        "sequence": {
            "index_run_reads": None if native else index_reads(int(3 * scale)),
            "view_run_reads": view_reads(None if native else index_reads(int(3 * scale)), 30),
            "end_to_end_ms": 100.0 * scale, "observed_storage_bytes": 1000 + int(scale),
            "window_peak_rss_bytes": 7_000_000 + int(scale),
            "window_end_rss_bytes": 6_000_000 + int(scale), "cgroup": None,
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
        self.assertEqual(native["metrics"]["refresh_ivm_ms"]["max"], 18.0)
        self.assertNotIn("refresh_combined_maintenance_ms", native["metrics"])
        # The base phase and the IVM figure are summed side by side, and index_ms apart from
        # both: base_ms is 4 * scale * (1 + 2), index_ms is 1 * scale * (1 + 2), and
        # forward_ms is 0.5 * scale * (1 + 2).
        self.assertEqual(native["metrics"]["refresh_index_ms"]["median"], 6.0)
        self.assertEqual(native["metrics"]["refresh_base_ms"]["median"], 24.0)
        self.assertEqual(native["metrics"]["refresh_forward_ms"]["median"], 3.0)
        self.assertEqual(native["metrics"]["write_operations_base_relation"]["median"], 2)
        self.assertEqual(native["metrics"]["write_bytes_total"]["median"], 300)
        self.assertEqual(native["metrics"]["observed_storage_bytes"]["max"], 1003)
        self.assertEqual(native["metrics"]["peak_rss_bytes"]["min"], 5_000_001)
        self.assertEqual(native["metrics"]["window_peak_rss_bytes"]["max"], 7_000_003)
        self.assertEqual(native["metrics"]["window_end_rss_bytes"]["min"], 6_000_001)
        self.assertNotIn("cgroup_delta_pgscan", native["metrics"])
        self.assertEqual(set(summary["workloads"]), {"clustered", "scattered"})
        markdown = (out / "summary.md").read_text()
        self.assertIn("## clustered", markdown)
        self.assertIn("Runs per mode: native 3, merged-index 3.", markdown)
        self.assertIn("| `sequence_wall_ms` | 200 | 100 | 300 | 600 | 500 | 700 |", markdown)
        # The index's run reads exist for the merged-index mode alone. Its sequence makes
        # 3 * scale seeks per reader over four readers, so 12 * scale.
        self.assertNotIn("index_run_reads", native)
        merged = summary["workloads"]["clustered"]["modes"]["merged-index"]
        seeks = merged["index_run_reads"]["index_seeks"]
        self.assertEqual((seeks["median"], seeks["min"], seeks["max"]), (72, 60, 84))
        self.assertEqual(merged["index_run_reads"]["index_value_decodes"]["median"], 144)
        self.assertNotIn("index_seeks", merged["metrics"])
        self.assertIn("Merged-index run reads (merged-index)", markdown)
        self.assertIn("| `index_seeks` | 72 | 60 | 84 |", markdown)
        # Both modes report the view worker's reads, summed over the owners: 30 query seeks,
        # plus the index's 12 * scale in the merged-index mode.
        self.assertEqual(native["metrics"]["view_seeks"]["median"], 30)
        self.assertEqual(merged["metrics"]["view_seeks"]["median"], 30 + 72)
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

    def test_window_rss_may_be_null_only_off_linux(self) -> None:
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        for run in content["runs"]:
            run["driver"]["os"] = "macos"
            run["driver"]["sequence"]["window_peak_rss_bytes"] = None
            run["driver"]["sequence"]["window_end_rss_bytes"] = None
        summary = summarize.summarize([self.write_manifest("mac", content)])
        metrics = summary["workloads"]["mac"]["modes"]["native"]["metrics"]
        self.assertIn("peak_rss_bytes", metrics)
        self.assertNotIn("window_peak_rss_bytes", metrics)
        self.assertNotIn("window_end_rss_bytes", metrics)
        # The same nulls on Linux (for example a failed clear_refs write) are an error.
        for run in content["runs"]:
            run["driver"]["os"] = "linux"
        self.failing(content, "native run 1: field sequence.window_peak_rss_bytes is None")

    def test_report_must_name_its_os(self) -> None:
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        del content["runs"][0]["driver"]["os"]
        self.failing(content, "native run 1: the report does not name its os")

    @staticmethod
    def with_cgroup(run: dict, **changes: object) -> None:
        cgroup = {
            "delta": {
                "memory_events": {"high": 0, "max": 4, "oom": 0, "oom_kill": 0},
                "memory_stat": {"pgscan": 600, "pgsteal": 70, "anon": -5,
                                "workingset_refault_anon": 8, "workingset_refault_file": 9},
                "memory_pressure_total_microseconds": {"some": 1234, "full": 0}},
            "peak": {"window_peak_supported": True, "window_peak_bytes": 9_000_000,
                     "max_sampled_memory_current_bytes": 8_000_000}}
        cgroup.update(changes)
        run["driver"]["sequence"]["cgroup"] = cgroup

    def test_cgroup_deltas_become_metrics_when_present(self) -> None:
        content = manifest({"native": [1.0, 2.0], "merged-index": [1.0, 2.0]})
        for run in content["runs"]:
            self.with_cgroup(run)
        summary = summarize.summarize([self.write_manifest("cg", content)])
        metrics = summary["workloads"]["cg"]["modes"]["native"]["metrics"]
        self.assertEqual(metrics["cgroup_delta_pgscan"]["median"], 600)
        self.assertEqual(metrics["cgroup_delta_workingset_refault_file"]["max"], 9)
        self.assertEqual(metrics["cgroup_delta_memory_events_max"]["min"], 4)
        self.assertEqual(metrics["cgroup_delta_pressure_some_microseconds"]["median"], 1234)
        self.assertEqual(metrics["cgroup_window_peak_bytes"]["median"], 9_000_000)
        self.assertEqual(metrics["cgroup_max_sampled_memory_current_bytes"]["max"], 8_000_000)
        # A gauge's signed delta and a counter the kernel lacks contribute nothing.
        self.assertNotIn("cgroup_delta_anon", metrics)
        self.assertNotIn("cgroup_delta_workingset_refault", metrics)

    def test_unsupported_window_peak_leaves_only_the_sampled_maximum(self) -> None:
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        for run in content["runs"]:
            self.with_cgroup(run, peak={"window_peak_supported": False,
                                        "window_peak_bytes": None,
                                        "max_sampled_memory_current_bytes": 5})
        metrics = summarize.summarize([self.write_manifest("np", content)])[
            "workloads"]["np"]["modes"]["native"]["metrics"]
        self.assertNotIn("cgroup_window_peak_bytes", metrics)
        self.assertEqual(metrics["cgroup_max_sampled_memory_current_bytes"]["median"], 5)

    def test_invalid_present_cgroup_delta_is_an_error(self) -> None:
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        for run in content["runs"]:
            self.with_cgroup(run)
        content["runs"][0]["driver"]["sequence"]["cgroup"]["delta"]["memory_stat"]["pgscan"] = -1
        self.failing(content, "native run 1 sequence.cgroup: field delta.memory_stat.pgscan is -1")

    def test_runs_with_different_metric_sets_are_an_error(self) -> None:
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        self.with_cgroup(content["runs"][0])
        self.failing(content, "different sets of metrics")

    def test_older_measurement_versions_are_an_error(self) -> None:
        # Version 2 timed the base tables' maintenance inside q3_ms; version 3 booked the merged
        # index in combined_maintenance_ms and not in base_ms.
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        content["runs"][0]["driver"]["measurement_version"] = 2
        self.failing(content, "native run 1: measurement_version is 2, not 5")
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        content["runs"][0]["driver"]["measurement_version"] = 3
        self.failing(content, "native run 1: measurement_version is 3, not 5")
        # Version 4 has no merged-index run reads.
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        content["runs"][1]["driver"]["measurement_version"] = 4
        self.failing(content, "merged-index run 1: measurement_version is 4, not 5")
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        del content["runs"][1]["driver"]["measurement_version"]
        self.failing(content, "merged-index run 1: measurement_version is None, not 5")
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        del content["runs"][0]["driver"]["refreshes"][0]["base_ms"]
        self.failing(content, "native run 1 refresh 1: missing field base_ms")

    def test_inconsistent_write_attribution_is_an_error(self) -> None:
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        content["runs"][0]["driver"]["sequence"]["resources"]["backend_write_operations"] += 1
        self.failing(content, "inconsistent write attribution")

    def test_inconsistent_index_run_reads_are_an_error(self) -> None:
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        content["runs"][1]["driver"]["sequence"]["index_run_reads"]["orders_before"]["seeks"] += 1
        self.failing(content, "inconsistent run reads")
        # The view worker's index part must match the index readers'.
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        refresh = content["runs"][1]["driver"]["refreshes"][0]
        refresh["view_run_reads"]["merged_index"]["seeks"] += 1
        self.failing(content, "view_run_reads.merged_index.seeks")

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
