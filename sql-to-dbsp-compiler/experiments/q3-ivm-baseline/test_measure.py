"""Tests for the R5 comparison runner."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import types
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location("measure", Path(__file__).with_name("measure.py"))
assert SPEC and SPEC.loader
measure = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(measure)


class MeasureTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        for name in ("customer", "orders", "lineitem", "rf1_orders", "rf1_lines", "rf2"):
            (self.root / name).write_text(name)
        self.refresh = self.root / "refresh.json"
        self.refresh.write_text(json.dumps([
            {"rf1_orders": "rf1_orders", "rf1_lines": "rf1_lines"},
            {"rf2": "rf2"}]))
        self.report = self.root / "report.json"
        self.args = ["--customer", str(self.root / "customer"),
                     "--orders", str(self.root / "orders"),
                     "--lineitem", str(self.root / "lineitem"),
                     "--refresh-list", str(self.refresh),
                     "--report", str(self.report),
                     "--storage-path", str(self.root / "campaign"),
                     "--cache-bytes", str(1024 * 1024), "--memory-target-bytes", "2048",
                     "--runs", "1"]

    def test_refresh_paths_and_manifest_validation(self) -> None:
        refreshes, files = measure.refresh_inputs(self.refresh)
        self.assertEqual(refreshes[0]["rf1_orders"], str((self.root / "rf1_orders").resolve()))
        self.assertEqual(files["refresh_1_rf2"], (self.root / "rf2").resolve())
        self.refresh.write_text('[{"rf1_orders":"rf1_orders"}]')
        with self.assertRaisesRegex(ValueError, "RF1 paths"):
            measure.refresh_inputs(self.refresh)

    def test_failure_records_manifest_and_restores_native_generation(self) -> None:
        calls = []

        def process(command, **kwargs):
            calls.append(command)
            if command[:3] == ["git", "rev-parse", "HEAD"]:
                return subprocess.CompletedProcess(command, 0, stdout="revision\n")
            if command[0] == str(binaries["native"]):
                return subprocess.CompletedProcess(command, 7, stdout="", stderr="failed")
            return subprocess.CompletedProcess(command, 0, stdout="", stderr="")

        binaries = {mode: self.root / mode for mode in measure.MODES}
        for binary in binaries.values():
            binary.write_bytes(b"binary")
        with patch.object(measure, "build_executables", return_value=binaries), \
             patch.object(measure.subprocess, "run", side_effect=process):
            self.assertEqual(measure.run(self.args), 1)
        manifest = json.loads(self.report.read_text())
        self.assertEqual(manifest["status"], "failed")
        self.assertEqual(manifest["runs"][0]["returncode"], 7)
        self.assertIn("exited 7", manifest["errors"][0])
        self.assertEqual(calls[-1], [str(measure.EXPERIMENT / "generate.sh")])

    def test_invalid_refresh_list_records_failure_before_build(self) -> None:
        self.refresh.write_text('[{"rf2":"missing"}]')

        def process(command, **kwargs):
            if command[:3] == ["git", "rev-parse", "HEAD"]:
                return subprocess.CompletedProcess(command, 0, stdout="revision\n")
            return subprocess.CompletedProcess(command, 0)

        with patch.object(measure, "build_executables") as build, \
             patch.object(measure.subprocess, "run", side_effect=process) as launch:
            self.assertEqual(measure.run(self.args), 1)
        build.assert_not_called()
        self.assertEqual(launch.call_args.args[0], [str(measure.EXPERIMENT / "generate.sh")])
        manifest = json.loads(self.report.read_text())
        self.assertEqual(manifest["status"], "failed")
        self.assertEqual(manifest["runs"], [])
        self.assertIn("FileNotFoundError", manifest["errors"][0])

    def test_restoration_failure_invalidates_success(self) -> None:
        calls = []

        def process(command, **kwargs):
            calls.append(command)
            if command[:3] == ["git", "rev-parse", "HEAD"]:
                return subprocess.CompletedProcess(command, 0, stdout="revision\n")
            if command[:3] == ["git", "status", "--short"]:
                return subprocess.CompletedProcess(command, 0, stdout=" M source.rs\n")
            if command[0].endswith("generate.sh"):
                raise subprocess.CalledProcessError(1, command)
            driver_report = Path(command[command.index("--report") + 1])
            driver_report.write_text('{"measurement_version":2,"load":'
                                     '{"observed_storage_bytes":4096},"sequence":'
                                     '{"observed_storage_bytes":4096}}')
            return subprocess.CompletedProcess(command, 0, stdout="", stderr="")

        binaries = {mode: self.root / mode for mode in measure.MODES}
        for binary in binaries.values():
            binary.write_bytes(b"binary")
        with patch.object(measure, "build_executables", return_value=binaries), \
             patch.object(measure.subprocess, "run", side_effect=process):
            self.assertEqual(measure.run(self.args), 1)
        manifest = json.loads(self.report.read_text())
        self.assertEqual(len(manifest["runs"]), 2)
        self.assertEqual(manifest["status"], "failed")
        self.assertIn("restoration failed", manifest["errors"][0])
        self.assertEqual(manifest["git_status_short"], " M source.rs\n")

    def test_linux_qualification_requires_storage_larger_than_limit(self) -> None:
        below = {"load": {"observed_storage_bytes": 100}}
        above = {"load": {"observed_storage_bytes": 101}}
        self.assertEqual(measure.qualification("Linux", 100, below, enforced=True)["status"],
                         "nonqualifying")
        self.assertEqual(measure.qualification("Linux", 100, above, enforced=True)["status"],
                         "qualifying")
        self.assertEqual(measure.qualification("Darwin", 100, above)["status"], "smoke")
        self.assertEqual(measure.qualification("Linux", None, above)["status"],
                         "nonqualifying")
        self.assertEqual(measure.qualification("Linux", 100, above, verify=True,
                                               enforced=True)["status"],
                         "nonqualifying")
        self.assertEqual(measure.qualification("Linux", 100, above)["status"],
                         "nonqualifying")

    def test_end_of_sequence_growth_does_not_qualify_small_loaded_state(self) -> None:
        report = {"measurement_version": 2,
                  "load": {"observed_storage_bytes": 99},
                  "sequence": {"observed_storage_bytes": 1_000_000}}
        result = measure.qualification("Linux", 100, report, enforced=True)
        self.assertEqual(result["status"], "nonqualifying")
        self.assertIn("loaded storage", result["reason"])

    def test_missing_or_invalid_loaded_size_does_not_qualify(self) -> None:
        for loaded in (None, True, -1, "101"):
            with self.subTest(loaded=loaded):
                report = {"load": {"observed_storage_bytes": loaded}}
                result = measure.qualification("Linux", 100, report, enforced=True)
                self.assertEqual(result["status"], "nonqualifying")

    def test_private_input_copy_rewrites_refresh_paths(self) -> None:
        refreshes, files = measure.refresh_inputs(self.refresh)
        spec = {"run_dir": str(self.root / "private-run"), "refreshes": refreshes,
                "inputs": {**{key: str(self.root / key)
                               for key in ("customer", "orders", "lineitem")},
                           **{key: str(path) for key, path in files.items()}}}
        spec["input_hashes"] = {key: measure.sha256(Path(path))
                                for key, path in spec["inputs"].items()}
        copied = measure.private_inputs(spec)
        self.assertEqual(Path(copied["customer"]).read_text(), "customer")
        private_refresh = json.loads(Path(copied["refresh_list"]).read_text())
        self.assertEqual(Path(private_refresh[1]["rf2"]).read_text(), "rf2")
        self.assertTrue(all(str(self.root / "private-run") in value
                            for entry in private_refresh for value in entry.values()))

    def test_private_copy_rejects_changed_source(self) -> None:
        spec = {"run_dir": str(self.root / "changed"),
                "inputs": {"customer": str(self.root / "customer")},
                "input_hashes": {"customer": "0" * 64}, "refreshes": []}
        with self.assertRaisesRegex(RuntimeError, "differs from recorded input hash"):
            measure.private_inputs(spec)

    def test_eviction_advises_dontneed_on_every_file_after_flushing(self) -> None:
        paths = [self.root / "customer", self.root / "orders", self.root / "customer"]
        events: list[tuple[str, int]] = []
        with patch.object(measure.os, "posix_fadvise", create=True,
                          side_effect=lambda fd, offset, length, advice:
                          events.append(("fadvise", advice))) as advise, \
             patch.object(measure.os, "POSIX_FADV_DONTNEED", 4, create=True), \
             patch.object(measure.os, "fsync",
                          side_effect=lambda fd: events.append(("fsync", fd))):
            result = measure.evict_from_page_cache(paths)
        self.assertEqual(advise.call_count, 2, "duplicates are advised once")
        self.assertTrue(all(call.args[1:] == (0, 0, 4) for call in advise.call_args_list))
        self.assertEqual([kind for kind, _ in events], ["fsync", "fadvise"] * 2)
        self.assertTrue(result["evicted"])
        self.assertEqual(len(result["files"]), 2)
        self.assertEqual(result["failures"], [])

    def test_eviction_records_a_failing_file_without_raising(self) -> None:
        with patch.object(measure.os, "posix_fadvise", create=True), \
             patch.object(measure.os, "POSIX_FADV_DONTNEED", 4, create=True), \
             patch.object(measure.os, "fsync"):
            result = measure.evict_from_page_cache(
                [self.root / "customer", self.root / "missing"])
        self.assertFalse(result["evicted"])
        self.assertEqual(result["files"], [str(self.root / "customer")])
        self.assertEqual([item["path"] for item in result["failures"]],
                         [str(self.root / "missing")])

    def test_eviction_is_a_noop_without_posix_fadvise(self) -> None:
        with patch.object(measure, "os", types.SimpleNamespace()):
            result = measure.evict_from_page_cache([self.root / "customer"])
        self.assertFalse(result["evicted"])
        self.assertIn("unavailable", result["reason"])

    def test_cgroup_child_evicts_sources_and_copies(self) -> None:
        refreshes, files = measure.refresh_inputs(self.refresh)
        group = self.root / "cgroup-evict"
        group.mkdir()
        (group / "memory.max").write_text("4096")
        spec = {"run_dir": str(self.root / "evict-run"), "cgroup": str(group),
                "total_memory_bytes": 4096, "executable": "/bin/driver",
                "refreshes": refreshes,
                "inputs": {**{key: str(self.root / key)
                               for key in ("customer", "orders", "lineitem")},
                           **{key: str(path) for key, path in files.items()}},
                "args": {"cache_bytes": 1024, "memory_target_bytes": 2048,
                         "compaction_timeout_secs": 5, "no_verify": True}}
        spec["input_hashes"] = {key: measure.sha256(Path(path))
                                for key, path in spec["inputs"].items()}
        spec_path = self.root / "evict-spec.json"
        spec_path.write_text(json.dumps(spec))
        with patch.object(measure, "verify_membership"), \
             patch.object(measure.os, "execv", side_effect=RuntimeError("stop")), \
             patch.object(measure, "evict_from_page_cache",
                          return_value={"evicted": True}) as evict:
            with self.assertRaisesRegex(RuntimeError, "stop"):
                measure.run_inside_cgroup(spec_path)
        evicted = {str(path) for path in evict.call_args.args[0]}
        run_input = self.root / "evict-run" / "input"
        self.assertIn(str(self.root / "customer"), evicted)
        self.assertIn(str(run_input / "customer"), evicted)
        self.assertIn(str(run_input / "refreshes.json"), evicted)
        self.assertTrue((self.root / "evict-run" / "input-eviction.json").is_file())

    def test_cgroup_limit_and_swap_are_checked(self) -> None:
        group = measure.configure_cgroup(self.root, "run", 1024)
        self.assertEqual((group / "memory.max").read_text(), "1024")
        self.assertEqual((group / "memory.swap.max").read_text(), "0")
        with self.assertRaises(ValueError):
            measure.cgroup_available(self.root)

    def test_cgroup_child_copies_before_driver_exec(self) -> None:
        refreshes, files = measure.refresh_inputs(self.refresh)
        group = self.root / "cgroup"
        group.mkdir()
        (group / "memory.max").write_text("4096")
        run_dir = self.root / "run"
        spec = {"run_dir": str(run_dir), "cgroup": str(group),
                "total_memory_bytes": 4096, "executable": "/bin/driver",
                "refreshes": refreshes,
                "inputs": {**{key: str(self.root / key)
                               for key in ("customer", "orders", "lineitem")},
                           **{key: str(path) for key, path in files.items()}},
                "args": {"cache_bytes": 1024, "memory_target_bytes": 2048,
                         "compaction_timeout_secs": 5, "no_verify": True}}
        spec["input_hashes"] = {key: measure.sha256(Path(path))
                                for key, path in spec["inputs"].items()}
        spec_path = self.root / "spec.json"
        spec_path.write_text(json.dumps(spec))
        with patch.object(measure, "verify_membership") as membership, \
             patch.object(measure.os, "execv", side_effect=RuntimeError("stop")) as execute:
            with self.assertRaisesRegex(RuntimeError, "stop"):
                measure.run_inside_cgroup(spec_path)
        membership.assert_called_once_with(group)
        command = execute.call_args.args[1]
        self.assertEqual(command[0], "/bin/driver")
        customer = Path(command[command.index("--customer") + 1])
        self.assertEqual(customer.read_text(), "customer")
        self.assertTrue(customer.is_relative_to(run_dir / "input"))
        self.assertEqual(command[command.index("--batch") + 1], "10000")
        evidence = measure.cgroup_evidence(group, run_dir, 4096)
        self.assertFalse(evidence["enforced"])
        (group / "memory.swap.max").write_text("0")
        (group / "memory.peak").write_text("12345\n")
        (group / "memory.events").write_text("oom 0\n")
        (group / "io.stat").write_text("8:0 rbytes=9 wbytes=10\n")
        evidence = measure.cgroup_evidence(group, run_dir, 4096)
        self.assertTrue(evidence["enforced"])
        self.assertEqual(evidence["counters"], {
            "memory.peak": "12345", "memory.events": "oom 0",
            "io.stat": "8:0 rbytes=9 wbytes=10"})

    @staticmethod
    def owner_report(mode: str = "reconstructed", **changes: object) -> dict:
        """A report whose per-owner writes add up to its totals."""
        resources = {
            "backend_write_operations": 10, "backend_write_bytes": 1000,
            "backend_write_by_owner": {
                "query": {"operations": 2, "bytes": 200},
                "base_relation": {"operations": 3, "bytes": 300},
                "merged_index": {"operations": 5, "bytes": 500}}}
        if mode == "native":
            resources["backend_write_by_owner"]["query"] = {"operations": 7, "bytes": 700}
            resources["backend_write_by_owner"]["merged_index"] = {"operations": 0, "bytes": 0}
        resources.update(changes)
        return {"mode": mode, "sequence": {"resources": resources},
                "refreshes": [{"refresh": 1, "resources": resources}]}

    @staticmethod
    def index_report(mode: str = "reconstructed") -> dict:
        """A report whose sequence index reads are the sums of its two refreshes'."""
        if mode == "native":
            return {"mode": "native", "sequence": {"index_run_reads": None},
                    "refreshes": [{"refresh": 1, "index_run_reads": None, "index_layout": None}]}

        def reads(seeks: int) -> dict:
            return {reader: {field: seeks for field in measure.RUN_READ_FIELDS}
                    for reader in measure.INDEX_READERS}
        layout = {snapshot: {location: {"runs": 1, "bytes": 8}
                             for location in ("file", "memory")}
                  for snapshot in ("before", "after")}
        return {"mode": mode, "sequence": {"index_run_reads": reads(3)},
                "refreshes": [{"refresh": number, "index_run_reads": reads(number),
                               "index_layout": layout} for number in (1, 2)]}

    def test_index_reads_are_complete_and_sum_to_the_sequence(self) -> None:
        self.assertEqual(measure.index_read_problems(self.index_report()), [])
        self.assertEqual(measure.index_read_problems(self.index_report("native")), [])

        unsummed = self.index_report()
        unsummed["sequence"]["index_run_reads"]["customers_after"]["seeks"] = 4
        self.assertIn("not the sums", measure.index_read_problems(unsummed)[0])

        negative = self.index_report()
        negative["refreshes"][0]["index_run_reads"]["lineitems_before"]["key_decodes"] = -1
        self.assertIn("lineitems_before.key_decodes is -1",
                      measure.index_read_problems(negative)[0])

        no_layout = self.index_report()
        del no_layout["refreshes"][1]["index_layout"]["after"]["file"]
        self.assertIn("index_layout.after.file is malformed",
                      measure.index_read_problems(no_layout)[0])

        native = self.index_report("native")
        native["refreshes"][0]["index_run_reads"] = {}
        self.assertIn("a native run reports merged-index reads",
                      measure.index_read_problems(native)[0])

    def test_view_reads_match_the_index_and_sum_to_the_sequence(self) -> None:
        def view(index_seeks: int, query_seeks: int) -> dict:
            reads = {owner: {field: 0 for field in measure.RUN_READ_FIELDS}
                     for owner in measure.WRITE_OWNERS}
            reads["merged_index"]["seeks"] = index_seeks
            reads["query"]["seeks"] = query_seeks
            return reads
        report = self.index_report()
        for refresh in report["refreshes"]:
            # Four readers with `refresh` seeks each.
            refresh["view_run_reads"] = view(4 * refresh["refresh"], 5)
            for reader in measure.INDEX_READERS:
                for field in measure.RUN_READ_FIELDS:
                    refresh["index_run_reads"][reader][field] = 0
                refresh["index_run_reads"][reader]["seeks"] = refresh["refresh"]
        report["sequence"]["view_run_reads"] = view(12, 10)
        self.assertEqual(measure.view_read_problems(report), [])

        report["sequence"]["view_run_reads"]["query"]["seeks"] = 11
        self.assertIn("not the sums", measure.view_read_problems(report)[0])

        report["refreshes"][0]["view_run_reads"]["merged_index"]["seeks"] = 5
        self.assertIn("view_run_reads.merged_index.seeks is 5, expected 4",
                      measure.view_read_problems(report)[0])

        native = {"mode": "native", "sequence": {"view_run_reads": view(0, 3)},
                  "refreshes": [{"refresh": 1, "view_run_reads": view(0, 3)}]}
        self.assertEqual(measure.view_read_problems(native), [])
        native["refreshes"][0]["view_run_reads"]["merged_index"]["seeks"] = 1
        self.assertIn("expected 0", measure.view_read_problems(native)[0])

        missing = {"mode": "native", "sequence": {}, "refreshes": [{"refresh": 1}]}
        self.assertIn("view_run_reads.query.seeks is None",
                      measure.view_read_problems(missing)[0])

    def test_owner_writes_must_sum_to_totals_in_every_report_block(self) -> None:
        self.assertEqual(measure.owner_write_problems(self.owner_report()), [])
        self.assertEqual(measure.owner_write_problems(self.owner_report("native")), [])
        # Validation runs omit counters and are not checked.
        validation = {"mode": "native", "sequence": {"resources": {"unavailable": "x"}}}
        self.assertEqual(measure.owner_write_problems(validation), [])

        wrong_total = measure.owner_write_problems(
            self.owner_report(backend_write_bytes=1001))
        self.assertEqual(len(wrong_total), 2)  # the sequence and the refresh
        self.assertIn("owners write 1000 bytes", wrong_total[0])

        no_owners = self.owner_report()
        del no_owners["refreshes"][0]["resources"]["backend_write_by_owner"]
        self.assertIn("missing", measure.owner_write_problems(no_owners)[0])

    def test_native_runs_report_no_merged_index_writes(self) -> None:
        report = self.owner_report("native")
        owners = report["sequence"]["resources"]["backend_write_by_owner"]
        owners["query"] = {"operations": 5, "bytes": 500}
        owners["merged_index"] = {"operations": 2, "bytes": 200}
        problems = measure.owner_write_problems(report)
        # The sequence and the refresh share the changed counters in this fixture.
        self.assertEqual(len(problems), 2)
        for problem in problems:
            self.assertIn("native run reports merged-index writes", problem)

    def test_inconsistent_owner_report_fails_the_campaign(self) -> None:
        def process(command, **kwargs):
            if command[:3] == ["git", "rev-parse", "HEAD"]:
                return subprocess.CompletedProcess(command, 0, stdout="revision\n")
            if command[0] in map(str, binaries.values()):
                report = self.owner_report("native", backend_write_bytes=1)
                Path(command[command.index("--report") + 1]).write_text(json.dumps(report))
            return subprocess.CompletedProcess(command, 0, stdout="", stderr="")

        binaries = {mode: self.root / mode for mode in measure.MODES}
        for binary in binaries.values():
            binary.write_bytes(b"binary")
        with patch.object(measure, "build_executables", return_value=binaries), \
             patch.object(measure.subprocess, "run", side_effect=process):
            self.assertEqual(measure.run(self.args), 1)
        manifest = json.loads(self.report.read_text())
        self.assertEqual(manifest["status"], "failed")
        self.assertIn("report is inconsistent", manifest["errors"][0])

    def test_diagnostics_flag_reaches_the_driver(self) -> None:
        spec = {"run_dir": str(self.root / "run"), "executable": "/bin/driver",
                "args": {"cache_bytes": 1024, "memory_target_bytes": 2048,
                         "compaction_timeout_secs": 5, "no_verify": True,
                         "diagnostics": True}}
        inputs = {key: key for key in ("customer", "orders", "lineitem", "refresh_list")}
        command = measure.driver_command(spec, inputs)
        self.assertEqual(command[command.index("--diagnostics") + 1],
                         str(self.root / "run" / "diagnostics"))
        spec["args"]["diagnostics"] = False
        self.assertNotIn("--diagnostics", measure.driver_command(spec, inputs))
        self.assertTrue(measure.parse_args([*self.args, "--diagnostics"]).diagnostics)
        self.assertFalse(measure.parse_args(self.args).diagnostics)

    def test_setup_full_compaction_flag_reaches_the_driver(self) -> None:
        spec = {"run_dir": str(self.root / "run"), "executable": "/bin/driver",
                "args": {"cache_bytes": 1024, "memory_target_bytes": 2048,
                         "compaction_timeout_secs": 5, "no_verify": True,
                         "setup_full_compaction": True}}
        inputs = {key: key for key in ("customer", "orders", "lineitem", "refresh_list")}
        self.assertIn("--setup-full-compaction", measure.driver_command(spec, inputs))
        spec["args"]["setup_full_compaction"] = False
        self.assertNotIn("--setup-full-compaction", measure.driver_command(spec, inputs))
        self.assertTrue(
            measure.parse_args([*self.args, "--setup-full-compaction"]).setup_full_compaction)
        self.assertFalse(measure.parse_args(self.args).setup_full_compaction)

    def test_front_code_byte_keys_flag_reaches_the_driver(self) -> None:
        spec = {"run_dir": str(self.root / "run"), "executable": "/bin/driver",
                "args": {"cache_bytes": 1024, "memory_target_bytes": 2048,
                         "compaction_timeout_secs": 5, "no_verify": True,
                         "front_code_byte_keys": True}}
        inputs = {key: key for key in ("customer", "orders", "lineitem", "refresh_list")}
        self.assertIn("--front-code-byte-keys", measure.driver_command(spec, inputs))
        spec["args"]["front_code_byte_keys"] = False
        self.assertNotIn("--front-code-byte-keys", measure.driver_command(spec, inputs))
        self.assertTrue(
            measure.parse_args([*self.args, "--front-code-byte-keys"]).front_code_byte_keys)
        self.assertFalse(measure.parse_args(self.args).front_code_byte_keys)

    def test_elide_value_headers_flag_reaches_the_driver(self) -> None:
        spec = {"run_dir": str(self.root / "run"), "executable": "/bin/driver",
                "args": {"cache_bytes": 1024, "memory_target_bytes": 2048,
                         "compaction_timeout_secs": 5, "no_verify": True,
                         "elide_value_headers": True}}
        inputs = {key: key for key in ("customer", "orders", "lineitem", "refresh_list")}
        self.assertIn("--elide-value-headers", measure.driver_command(spec, inputs))
        spec["args"]["elide_value_headers"] = False
        self.assertNotIn("--elide-value-headers", measure.driver_command(spec, inputs))
        self.assertTrue(
            measure.parse_args([*self.args, "--elide-value-headers"]).elide_value_headers)
        self.assertFalse(measure.parse_args(self.args).elide_value_headers)
        # Key front coding and value-header elision are independent switches.
        self.assertFalse(
            measure.parse_args([*self.args, "--elide-value-headers"]).front_code_byte_keys)

    def test_cli_rejects_invalid_cache_and_timeout(self) -> None:
        with self.assertRaises(SystemExit):
            measure.parse_args([*self.args, "--cache-bytes", "1024"])
        with self.assertRaises(SystemExit):
            measure.parse_args([*self.args, "--compaction-timeout-secs", "nan"])
        parsed = measure.parse_args([*self.args, "--batch", "512"])
        self.assertEqual(parsed.batch, 512)


if __name__ == "__main__":
    unittest.main()
