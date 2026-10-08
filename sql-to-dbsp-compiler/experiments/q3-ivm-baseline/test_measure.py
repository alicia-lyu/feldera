"""Tests for the R5 comparison runner."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
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
            driver_report.write_text('{"sequence":{"settled_storage_bytes":4096}}')
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
        below = {"sequence": {"settled_storage_bytes": 100}}
        above = {"sequence": {"settled_storage_bytes": 101}}
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

    def test_cli_rejects_invalid_cache_and_timeout(self) -> None:
        with self.assertRaises(SystemExit):
            measure.parse_args([*self.args, "--cache-bytes", "1024"])
        with self.assertRaises(SystemExit):
            measure.parse_args([*self.args, "--compaction-timeout-secs", "nan"])
        parsed = measure.parse_args([*self.args, "--batch", "512"])
        self.assertEqual(parsed.batch, 512)


if __name__ == "__main__":
    unittest.main()
