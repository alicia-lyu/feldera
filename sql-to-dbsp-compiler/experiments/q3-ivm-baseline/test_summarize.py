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
            "merged_index": {"operations": index, "bytes": index * 100},
            "requested_slice": {"operations": 0, "bytes": 0},
            "partner_trace": {"operations": 0, "bytes": 0}}


def index_reads(seeks: int) -> dict[str, dict[str, int]]:
    """The scan session with `seeks` seeks and twice as many key, value and weight decodes, and
    as many memory reads. `filtered_probes` counts exact probes that a file run's filters (key
    range, then Roaring bitmap or Bloom filter) answer with no seek."""
    return {reader: {"seeks": seeks, "filtered_probes": 0, "key_decodes": 2 * seeks,
                     "value_decodes": 2 * seeks, "weight_decodes": 2 * seeks,
                     "memory_key_reads": seeks, "memory_value_reads": seeks,
                     "memory_weight_reads": seeks}
            for reader in summarize.INDEX_READERS}


def view_reads(query: int, requested_slice: int = 0,
               partner_trace: int = 0) -> dict[str, dict[str, int]]:
    """The view worker's reads: `query` of every count under `query`, `requested_slice` under
    `requested_slice`, and `partner_trace` under `partner_trace`. The scan session reads the index
    on the driver's thread, so the worker reads nothing under `merged_index`."""
    return {
        "query": {field: query for field in summarize.RUN_READ_FIELDS},
        "base_relation": {field: 0 for field in summarize.RUN_READ_FIELDS},
        "merged_index": {field: 0 for field in summarize.RUN_READ_FIELDS},
        "requested_slice": {field: requested_slice for field in summarize.RUN_READ_FIELDS},
        "partner_trace": {field: partner_trace for field in summarize.RUN_READ_FIELDS},
    }


def driver_reads(requested_slice: int, index: int = 1) -> dict[str, dict[str, int]]:
    """The driver thread's reads while collecting: the index's (`merged_index`) and the routing's
    `requested_slice` ones, and nothing under any other owner."""
    reads = view_reads(0, requested_slice)
    reads["merged_index"] = {field: index for field in summarize.RUN_READ_FIELDS}
    return reads


def session_counts(groups: int) -> dict[str, int]:
    """The scan session's moves: `groups` groups, and twice as many of every other count."""
    return {field: groups if field == "groups" else 2 * groups
            for field in summarize.SESSION_COUNT_FIELDS}


def upkeep_reads(seeks: int) -> dict[str, int]:
    """Base maintenance's reads of the index: `seeks` of every count."""
    return {field: seeks for field in summarize.RUN_READ_FIELDS}


def expansion(prefix_truncated: int = 0, elided: int = 0) -> dict[str, int]:
    """A block-expansion count: each prefix-truncated block is 100 bytes encoded and 400
    expanded, each elided block 50 and 200."""
    return {"prefix_truncated_blocks": prefix_truncated,
            "prefix_truncated_encoded_bytes": 100 * prefix_truncated,
            "prefix_truncated_expanded_bytes": 400 * prefix_truncated,
            "elided_blocks": elided, "elided_encoded_bytes": 50 * elided,
            "elided_expanded_bytes": 200 * elided}


def owner_expansions(**by_owner: dict[str, int]) -> dict[str, dict[str, int]]:
    """A per-owner block-expansion report: the owners named, the others zero."""
    return {owner: by_owner.get(owner, expansion()) for owner in summarize.WRITE_OWNERS}


def block_expansions(mode: str, number: int) -> dict:
    """The block expansions of refresh `number` (or, passed the sum, the sequence). A native run
    has no byte-string columns, so it expands nothing. A reconstructed run expands `number`
    prefix-truncated and `2 * number` elided blocks on the view worker's requested slices,
    `number` of each kind in the driver's scan, and `number` prefix-truncated blocks in base
    maintenance."""
    if mode == "native":
        return {"view_worker": owner_expansions(), "driver": None, "index_upkeep": None}
    return {"view_worker": owner_expansions(
                requested_slice=expansion(number, 2 * number)),
            "driver": owner_expansions(merged_index=expansion(number, number)),
            "index_upkeep": expansion(number, 0)}


STORAGE_DEVICE = {"path": "/mnt/ssd/campaign", "device": "/dev/nvme0n1", "filesystem": "ext4",
                  "logical_sector_bytes": 512, "rotational": False, "model": "Samsung SSD 990",
                  "label": "ssd"}

LAYOUT = {snapshot: {"file": {"runs": 1, "bytes": 10}, "memory": {"runs": 1, "bytes": 5}}
          for snapshot in ("before", "after")}


def driver_report(mode: str, scale: float, writes: tuple[int, int, int] = (1, 2, 0)) -> dict:
    """A driver report with two refreshes; `scale` multiplies every timing, and, in the
    merged-index mode, refresh `n` makes `n * scale` index seeks per reader."""
    split = owners(*writes)
    native = mode == "native"
    refreshes = [{"refresh": number, "foreground_ms": 10.0 * scale * number,
                  "base_ms": 4.0 * scale * number, "forward_ms": 0.5 * scale * number,
                  "index_ms": 1.0 * scale * number, "delta_ms": 0.25 * scale * number, "q3_ms": 1.75 * scale * number,
                  "ivm_ms": 2.0 * scale * number,
                  "index_run_reads": None if native else index_reads(int(number * scale)),
                  "index_layout": None if native else LAYOUT,
                  "index_session_counts": None if native else session_counts(number),
                  "index_upkeep_run_reads": None if native else upkeep_reads(number),
                  "profiled_cpu_ms": {"base": 3.0 * scale * number,
                                      "q3": 1.5 * scale * number}}
                 for number in (1, 2)]
    for refresh in refreshes:
        refresh["view_run_reads"] = view_reads(
            10 * refresh["refresh"], 0 if native else 3 * refresh["refresh"],
            4 * refresh["refresh"] if native else 0)
        refresh["driver_run_reads"] = None if native else driver_reads(refresh["refresh"])
        refresh["block_expansions"] = block_expansions(mode, refresh["refresh"])
    return {
        "status": "complete", "mode": "native" if mode == "native" else "reconstructed",
        "measurement_version": 13, "os": "linux",
        "configuration": {"front_code_byte_keys": True, "elide_value_headers": True},
        "front_coding": {"blocks": 5, "layout_fallbacks": 0, "size_fallbacks": 1},
        "value_header_elision": {"blocks": 6, "layout_fallbacks": 0, "size_fallbacks": 0},
        "refreshes": refreshes,
        "sequence": {
            "block_expansions": block_expansions(mode, 3),
            "index_run_reads": None if native else index_reads(int(3 * scale)),
            "index_session_counts": None if native else session_counts(3),
            "index_upkeep_run_reads": None if native else upkeep_reads(3),
            "view_run_reads": view_reads(30, 0 if native else 9, 12 if native else 0),
            "driver_run_reads": None if native else driver_reads(3, 2),
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
            "configuration": {"runs": 3}, "storage_device": STORAGE_DEVICE, "runs": runs}


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
        # delta_ms (0.25 * scale * 3) is a part of ivm_ms (2 * scale * 3) and beside forward_ms.
        self.assertEqual(native["metrics"]["refresh_delta_ms"]["median"], 1.5)
        self.assertEqual(native["metrics"]["refresh_q3_ms"]["median"], 10.5)
        # The CPU profiler's times are summed per circuit in both modes: 3 and 1.5 * scale * 3.
        self.assertEqual(native["metrics"]["refresh_profiled_cpu_base_ms"]["median"], 18.0)
        self.assertEqual(native["metrics"]["refresh_profiled_cpu_q3_ms"]["max"], 13.5)
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
        self.assertIn("Runs per mode: native 3, merged-index 3. Session pass: `seek`.", markdown)
        # The storage device and its label stand beside the campaign.
        self.assertIn("Storage device: ssd (/dev/nvme0n1, Samsung SSD 990, ext4, non-rotational, "
                      "512-byte sectors).", markdown)
        self.assertEqual(summary["workloads"]["clustered"]["storage_device"], STORAGE_DEVICE)
        # Block expansions are summed over the threads and owners: a reconstructed run expands
        # 3 + 3 + 3 prefix-truncated and 6 + 3 elided blocks over its two refreshes, and a native
        # run none. The bytes follow the blocks.
        self.assertEqual(native["metrics"]["expansion_prefix_truncated_blocks"]["max"], 0)
        merged_metrics = summary["workloads"]["clustered"]["modes"]["merged-index"]["metrics"]
        self.assertEqual(merged_metrics["expansion_prefix_truncated_blocks"]["median"], 9)
        self.assertEqual(merged_metrics["expansion_prefix_truncated_expanded_bytes"]["median"],
                         3600)
        self.assertEqual(merged_metrics["expansion_elided_blocks"]["median"], 9)
        self.assertEqual(merged_metrics["expansion_elided_encoded_bytes"]["median"], 450)
        self.assertIn("`expansion_*` are the blocks", markdown)
        self.assertIn("| `sequence_wall_ms` | 200 | 100 | 300 | 600 | 500 | 700 |", markdown)
        # The index's run reads exist for the merged-index mode alone. Its sequence makes
        # 3 * scale seeks for the one reader, the scan session.
        self.assertNotIn("index_run_reads", native)
        merged = summary["workloads"]["clustered"]["modes"]["merged-index"]
        seeks = merged["index_run_reads"]["index_seeks"]
        self.assertEqual((seeks["median"], seeks["min"], seeks["max"]), (18, 15, 21))
        self.assertEqual(merged["index_run_reads"]["index_value_decodes"]["median"], 36)
        # The memory reads are separate metrics, one per seek, not added to the decodes.
        self.assertEqual(merged["index_run_reads"]["index_memory_value_reads"]["median"], 18)
        self.assertNotIn("index_seeks", merged["metrics"])
        # The session's moves and base maintenance's reads of the index sit beside them.
        self.assertEqual(merged["index_run_reads"]["index_session_groups"]["median"], 3)
        self.assertEqual(merged["index_run_reads"]["index_session_positions"]["median"], 6)
        self.assertEqual(merged["index_run_reads"]["index_upkeep_seeks"]["median"], 3)
        self.assertIn("Merged-index counters (merged-index)", markdown)
        self.assertIn("| `index_seeks` | 18 | 15 | 21 |", markdown)
        # Both modes report the DBSP operators' own reads on the view worker: 30 query seeks. The
        # scan session reads the index on the driver's thread, so the worker adds none.
        self.assertEqual(native["metrics"]["dbsp_operator_seeks"]["median"], 30)
        self.assertEqual(merged["metrics"]["dbsp_operator_seeks"]["median"], 30)
        # The joins' reads of the requested slices are the reconstructed mode's alone.
        self.assertEqual(native["metrics"]["requested_slice_seeks"]["median"], 0)
        # The worker's 9 and the driver's routing reads, which the raw report keeps apart, 3.
        self.assertEqual(merged["metrics"]["requested_slice_seeks"]["median"], 12)
        self.assertEqual(merged["metrics"]["requested_slice_memory_weight_reads"]["median"], 12)
        # The native joins' trace reads are the partner trace's, and none in reconstructed mode.
        self.assertEqual(native["metrics"]["partner_trace_seeks"]["median"], 12)
        self.assertEqual(merged["metrics"]["partner_trace_seeks"]["median"], 0)
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
        del content["runs"][0]["driver"]["refreshes"][1]["delta_ms"]
        self.failing(content, "native run 1: inconsistent counters: refresh 2: delta_ms is None")
        content = copy.deepcopy(base)
        content["runs"][3]["driver"]["refreshes"][0]["delta_ms"] = 5.0
        self.failing(content, "merged-index run 2: inconsistent counters: refresh 1: "
                              "ivm_ms 4.0 is below delta_ms 5.0")
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
        self.failing(content, "native run 1: measurement_version is 2, not 13")
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        content["runs"][0]["driver"]["measurement_version"] = 3
        self.failing(content, "native run 1: measurement_version is 3, not 13")
        # Version 4 has no merged-index run reads.
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        content["runs"][1]["driver"]["measurement_version"] = 4
        self.failing(content, "merged-index run 1: measurement_version is 4, not 13")
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        del content["runs"][1]["driver"]["measurement_version"]
        self.failing(content, "merged-index run 1: measurement_version is None, not 13")
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        del content["runs"][0]["driver"]["refreshes"][0]["base_ms"]
        self.failing(content, "native run 1 refresh 1: missing field base_ms")

    def test_inconsistent_write_attribution_is_an_error(self) -> None:
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        content["runs"][0]["driver"]["sequence"]["resources"]["backend_write_operations"] += 1
        self.failing(content, "inconsistent write attribution")

    def test_inconsistent_index_run_reads_are_an_error(self) -> None:
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        content["runs"][1]["driver"]["sequence"]["index_run_reads"]["session"]["seeks"] += 1
        self.failing(content, "inconsistent counters")
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        content["runs"][1]["driver"]["sequence"]["index_session_counts"]["groups"] += 1
        self.failing(content, "index_session_counts are not the sums")
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        del content["runs"][0]["driver"]["refreshes"][1]["profiled_cpu_ms"]
        self.failing(content, "profiled_cpu_ms.base is None")
        # The view worker reads nothing under `merged_index`.
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        refresh = content["runs"][1]["driver"]["refreshes"][0]
        refresh["view_run_reads"]["merged_index"]["seeks"] += 1
        self.failing(content, "view_run_reads.merged_index.seeks")
        # A native run has no requested-slice reads, and a reconstructed run has some.
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        refresh = content["runs"][0]["driver"]["refreshes"][0]
        refresh["view_run_reads"]["requested_slice"]["seeks"] += 1
        self.failing(content, "view_run_reads.requested_slice.seeks")
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        driver = content["runs"][1]["driver"]
        for item in (*driver["refreshes"], driver["sequence"]):
            item["view_run_reads"]["requested_slice"] = {
                field: 0 for field in summarize.RUN_READ_FIELDS}
        self.failing(content, "reports no view_run_reads.requested_slice reads")
        # The partner trace is the native mode's alone, and a native run has some.
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        refresh = content["runs"][1]["driver"]["refreshes"][0]
        refresh["view_run_reads"]["partner_trace"]["seeks"] += 1
        self.failing(content, "view_run_reads.partner_trace.seeks")
        # The driver thread reads under no owner but the index's and the requested slice's.
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        refresh = content["runs"][1]["driver"]["refreshes"][0]
        refresh["driver_run_reads"]["query"]["seeks"] = 1
        self.failing(content, "driver_run_reads.query.seeks")

    def test_a_manifest_without_a_device_is_summarized_as_not_recorded(self) -> None:
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        del content["storage_device"]
        path = self.write_manifest("old", content)
        out = self.root / "out"
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(summarize.main([str(path), "--out", str(out)]), 0)
        self.assertIn("Storage device: not recorded.", (out / "summary.md").read_text())

    def test_the_device_description_shows_the_label_first(self) -> None:
        self.assertEqual(summarize.describe_device(STORAGE_DEVICE),
                         "ssd (/dev/nvme0n1, Samsung SSD 990, ext4, non-rotational, "
                         "512-byte sectors)")
        unresolved = {field: None for field in STORAGE_DEVICE}
        self.assertEqual(summarize.describe_device(unresolved), "unlabelled (device not resolved)")
        self.assertEqual(summarize.describe_device({**unresolved, "label": "hdd"}),
                         "hdd (device not resolved)")
        self.assertEqual(summarize.describe_device({**STORAGE_DEVICE, "label": "hdd",
                                                    "rotational": True}).split(",")[3],
                         " rotational")

    def test_a_layout_fallback_or_a_malformed_expansion_is_an_error(self) -> None:
        content = manifest({"native": [1.0], "merged-index": [1.0]})
        content["runs"][1]["driver"]["front_coding"]["layout_fallbacks"] = 2
        self.failing(content, "front_coding.layout_fallbacks is 2")

        content = manifest({"native": [1.0], "merged-index": [1.0]})
        content["runs"][0]["driver"]["sequence"]["block_expansions"] = None
        self.failing(content, "sequence: block_expansions is None")

        content = manifest({"native": [1.0], "merged-index": [1.0]})
        content["runs"][1]["driver"]["configuration"]["elide_value_headers"] = False
        self.failing(content, "expected 0 with the encoding off")

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
