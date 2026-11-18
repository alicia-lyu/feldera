"""Tests for the R5 comparison runner."""

from __future__ import annotations

import importlib.util
import hashlib
import json
from pathlib import Path
import shutil
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
        self.assertEqual(measure.qualification("Linux", 100, below, enforced=True,
                                               cold=True)["status"], "nonqualifying")
        self.assertEqual(measure.qualification("Linux", 100, above, enforced=True,
                                               cold=True)["status"], "qualifying")
        warm = measure.qualification("Linux", 100, above, enforced=True)
        self.assertEqual(warm["status"], "nonqualifying")
        self.assertIn("not evicted", warm["reason"])
        self.assertEqual(measure.qualification("Darwin", 100, above)["status"], "smoke")
        self.assertEqual(measure.qualification("Linux", None, above)["status"],
                         "nonqualifying")
        self.assertEqual(measure.qualification("Linux", 100, above, verify=True,
                                               enforced=True, cold=True)["status"],
                         "nonqualifying")
        self.assertEqual(measure.qualification("Linux", 100, above)["status"],
                         "nonqualifying")

    def test_end_of_sequence_growth_does_not_qualify_small_loaded_state(self) -> None:
        report = {"measurement_version": 2,
                  "load": {"observed_storage_bytes": 99},
                  "sequence": {"observed_storage_bytes": 1_000_000}}
        result = measure.qualification("Linux", 100, report, enforced=True, cold=True)
        self.assertEqual(result["status"], "nonqualifying")
        self.assertIn("loaded storage", result["reason"])

    def test_missing_or_invalid_loaded_size_does_not_qualify(self) -> None:
        for loaded in (None, True, -1, "101"):
            with self.subTest(loaded=loaded):
                report = {"load": {"observed_storage_bytes": loaded}}
                result = measure.qualification("Linux", 100, report, enforced=True, cold=True)
                self.assertEqual(result["status"], "nonqualifying")

    def shared_input_spec(self, run_dir: Path) -> dict:
        """A run spec over the shared inputs, with the identities the campaign recorded."""
        refreshes, files = measure.refresh_inputs(self.refresh)
        spec = {"run_dir": str(run_dir), "refreshes": refreshes,
                "inputs": {**{key: str(self.root / key)
                               for key in ("customer", "orders", "lineitem")},
                           **{key: str(path) for key, path in files.items()},
                           "refresh_list": str(self.refresh)}}
        spec["input_identities"] = {key: measure.file_identity(Path(path))
                                    for key, path in spec["inputs"].items()}
        return spec

    def test_shared_inputs_are_read_in_place_and_checked_unchanged(self) -> None:
        spec = self.shared_input_spec(self.root / "shared-run")
        measure.unchanged_inputs(spec)
        self.assertFalse((self.root / "shared-run").exists(), "nothing is copied")
        (self.root / "customer").write_text("customer, rewritten")
        with self.assertRaisesRegex(RuntimeError, "input customer changed"):
            measure.unchanged_inputs(spec)

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

    def image_args(self, images: Path) -> list[str]:
        """A campaign over images, with no base files."""
        return ["--refresh-list", str(self.refresh), "--report", str(self.report),
                "--storage-path", str(self.root / "campaign"), "--images", str(images),
                "--cache-bytes", str(1024 * 1024), "--memory-target-bytes", "2048",
                "--runs", "1"]

    def test_images_are_named_by_mode_and_prefix_truncation_arm(self) -> None:
        self.assertEqual([measure.image_name(mode, front) for mode in measure.MODES
                          for _, front in measure.IMAGE_ARMS],
                         ["native-A", "native-B", "merged-index-A", "merged-index-B"])

    def test_build_images_and_campaigns_take_disjoint_arguments(self) -> None:
        base = ["--customer", str(self.root / "customer"), "--orders", str(self.root / "orders"),
                "--lineitem", str(self.root / "lineitem"), "--cache-bytes", str(1024 * 1024)]
        build = measure.parse_args(base + ["--build-images", str(self.root / "images")])
        self.assertIsNone(build.memory_target_bytes)
        for extra in (["--refresh-list", str(self.refresh)], ["--report", str(self.report)],
                      ["--memory-target-bytes", "2048"], ["--images", "i"],
                      ["--cgroup-root", "/sys/fs/cgroup/r5", "--total-memory-bytes", "1"]):
            with self.subTest(extra=extra), patch("sys.stderr"), self.assertRaises(SystemExit):
                measure.parse_args(base + ["--build-images", "i"] + extra)
        images = measure.parse_args(self.image_args(self.root / "images"))
        self.assertIsNone(images.customer)
        with patch("sys.stderr"), self.assertRaises(SystemExit):
            measure.parse_args(self.image_args(self.root / "images") + base[:2])
        with patch("sys.stderr"), self.assertRaises(SystemExit):
            measure.parse_args(self.args + ["--warm-up", str(self.refresh)])

    def test_loading_has_its_own_cache_batch_and_timeout_defaults(self) -> None:
        base = ["--customer", str(self.root / "customer"), "--orders", str(self.root / "orders"),
                "--lineitem", str(self.root / "lineitem")]
        build = measure.parse_args(base + ["--build-images", "i"])
        self.assertEqual(build.cache_bytes, 16 * 1024 ** 3)
        self.assertEqual(build.batch, 1_000_000)
        self.assertEqual(build.compaction_timeout_secs, 3600)
        chosen = measure.parse_args(base + ["--build-images", "i", "--cache-bytes",
                                            str(1024 * 1024), "--batch", "7",
                                            "--compaction-timeout-secs", "9"])
        self.assertEqual((chosen.cache_bytes, chosen.batch, chosen.compaction_timeout_secs),
                         (1024 * 1024, 7, 9))
        # A campaign keeps its measured settings: the cache is required, and the load
        # defaults do not apply.
        campaign = measure.parse_args(self.image_args(self.root / "images"))
        self.assertEqual((campaign.batch, campaign.compaction_timeout_secs), (10000, 300))
        without_cache = self.image_args(self.root / "images")
        index = without_cache.index("--cache-bytes")
        del without_cache[index:index + 2]
        with patch("sys.stderr"), self.assertRaises(SystemExit):
            measure.parse_args(without_cache)

    def test_a_run_from_an_image_starts_the_refresh_program(self) -> None:
        spec = {"run_dir": str(self.root), "executable": "/bin/driver",
                "image": str(self.root / "image"),
                "args": {"cache_bytes": 1024, "memory_target_bytes": 2048,
                         "compaction_timeout_secs": 5, "no_verify": True,
                         "warm_up": str(self.refresh)}}
        command = measure.driver_command(spec, {"refresh_list": str(self.refresh)})
        self.assertEqual(command[command.index("--from-image") + 1], str(self.root / "image"))
        self.assertEqual(command[command.index("--warm-up") + 1], str(self.refresh))
        for absent in ("--customer", "--orders", "--lineitem", "--storage-path"):
            self.assertNotIn(absent, command)

    def test_qualification_reads_the_loaded_size_of_an_image(self) -> None:
        report = {"load": None, "image": {"load": {"observed_storage_bytes": 101}}}
        self.assertEqual(measure.qualification("Linux", 100, report, enforced=True,
                                               cold=True)["status"], "qualifying")

    def test_a_run_from_an_image_evicts_its_copy(self) -> None:
        group = self.root / "cgroup-image"
        group.mkdir()
        (group / "memory.max").write_text("4096")
        run_dir = self.root / "image-run"
        (run_dir / "image" / "storage" / "uuid").mkdir(parents=True)
        (run_dir / "image" / "image.json").write_text("{}")
        (run_dir / "image" / "storage" / "uuid" / "batch.feldera").write_text("batch")
        spec = {**self.shared_input_spec(run_dir), "cgroup": str(group),
                "total_memory_bytes": 4096, "executable": "/bin/driver",
                "image": str(run_dir / "image"),
                "args": {"cache_bytes": 1024, "memory_target_bytes": 2048,
                         "compaction_timeout_secs": 5, "no_verify": True}}
        spec_path = self.root / "image-spec.json"
        spec_path.write_text(json.dumps(spec))
        with patch.object(measure, "verify_membership"), \
             patch.object(measure.os, "execv", side_effect=RuntimeError("stop")), \
             patch.object(measure, "evict_from_page_cache",
                          return_value={"evicted": True}) as evict:
            with self.assertRaisesRegex(RuntimeError, "stop"):
                measure.run_inside_cgroup(spec_path)
        evicted = {str(path) for path in evict.call_args.args[0]}
        self.assertIn(str(run_dir / "image" / "storage" / "uuid" / "batch.feldera"), evicted)
        self.assertIn(str(run_dir / "image" / "image.json"), evicted)
        self.assertTrue(set(spec["inputs"].values()) <= evicted)

    def test_images_must_be_complete(self) -> None:
        images = self.root / "images"
        images.mkdir()
        (images / measure.IMAGES_MANIFEST).write_text(json.dumps(
            {"status": "complete", "revision": "abc", "images": {}}))
        self.assertEqual(measure.read_images(images)["revision"], "abc")
        (images / measure.IMAGES_MANIFEST).write_text(json.dumps(
            {"status": "failed", "revision": "abc", "images": {}}))
        with self.assertRaisesRegex(ValueError, "not complete"):
            measure.read_images(images)

    def test_images_reopen_only_with_the_executables_that_wrote_them(self) -> None:
        images = {"executables": {"native": {"sha256": "n"}, "merged-index": {"sha256": "m"}}}
        same = {"native": {"sha256": "n"}, "merged-index": {"sha256": "m"}}
        self.assertEqual(measure.image_executable_problems(images, same), [])
        rebuilt = {"native": {"sha256": "n"}, "merged-index": {"sha256": "x"}}
        problems = measure.image_executable_problems(images, rebuilt)
        self.assertEqual(len(problems), 1)
        self.assertIn("merged-index images were written by an executable with sha256 m",
                      problems[0])
        self.assertEqual(len(measure.image_executable_problems({}, same)), 2)

    def image_campaign(self, name: str, written_by: bytes = b"binary",
                       extra: list[str] = ()) -> tuple[int, dict, Path]:
        """Runs a campaign over fake images, with drivers that report success."""
        images = self.root / f"{name}-images"
        binaries = {mode: self.root / f"{name}-{mode}" for mode in measure.MODES}
        for binary in binaries.values():
            binary.write_bytes(b"binary")
        entries = {}
        for mode in measure.MODES:
            image = images / measure.image_name(mode, False)
            (image / "storage").mkdir(parents=True)
            (image / "storage" / "batch.feldera").write_text("batch")
            entries[image.name] = {"directory": str(image), "checkpoint": "u"}
        (images / measure.IMAGES_MANIFEST).write_text(json.dumps({
            "status": "complete", "revision": "older", "inputs": {}, "images": entries,
            "executables": {mode: {"sha256": hashlib.sha256(written_by).hexdigest()}
                            for mode in measure.MODES}}))
        launched = []

        def process(command, **kwargs):
            if command[:3] == ["git", "rev-parse", "HEAD"]:
                return subprocess.CompletedProcess(command, 0, stdout="revision\n")
            if command[0] in map(str, binaries.values()):
                launched.append(command)
                mode = "native" if command[0] == str(binaries["native"]) else "reconstructed"
                report = self.owner_report(mode)
                Path(command[command.index("--report") + 1]).write_text(json.dumps(report))
                return subprocess.CompletedProcess(command, 0, stdout="", stderr="")
            if command[:2] == ["cp", "-a"]:
                shutil.copytree(command[2], command[3])
            return subprocess.CompletedProcess(command, 0, stdout="", stderr="")

        storage = self.root / f"{name}-campaign"
        args = [*self.image_args(images), "--storage-path", str(storage), *extra]
        with patch.object(measure, "build_executables", return_value=binaries), \
             patch.object(measure, "report_problems", return_value=[]), \
             patch.object(measure.subprocess, "run", side_effect=process):
            code = measure.run(args)
        return code, json.loads(self.report.read_text()), storage

    def test_a_campaign_refuses_images_written_by_other_executables(self) -> None:
        code, manifest, storage = self.image_campaign("other", written_by=b"older")
        self.assertEqual(code, 1)
        self.assertIn("rebuild the images", manifest["errors"][0])
        self.assertEqual(manifest["runs"], [])
        self.assertFalse(storage.exists() and any(storage.iterdir()))

    def test_a_completed_run_deletes_its_copy_unless_asked_to_keep_it(self) -> None:
        code, manifest, storage = self.image_campaign("delete")
        self.assertEqual(code, 0, manifest["errors"])
        for record in manifest["runs"]:
            self.assertTrue(record["storage_deleted"])
            self.assertFalse(Path(record["storage_path"]).exists())
        code, manifest, storage = self.image_campaign("keep", extra=["--keep-run-storage"])
        self.assertEqual(code, 0, manifest["errors"])
        for record in manifest["runs"]:
            self.assertNotIn("storage_deleted", record)
            self.assertTrue((Path(record["storage_path"]) / "batch.feldera").is_file())
        with patch("sys.stderr"), self.assertRaises(SystemExit):
            measure.parse_args(self.args + ["--keep-run-storage"])

    def test_build_images_loads_each_mode_and_arm_outside_any_cgroup(self) -> None:
        images = self.root / "images"
        commands = []

        def process(command: list[str], **kwargs: object) -> subprocess.CompletedProcess:
            if "--write-image" not in command:  # git, generate.sh, uname
                return subprocess.CompletedProcess(command, 0, stdout="rev\n", stderr="")
            commands.append(command)
            directory = Path(command[command.index("--write-image") + 1])
            (directory / "storage").mkdir(parents=True)
            (directory / "storage" / "file").write_text("12345")
            (directory / "image.json").write_text(json.dumps({
                "settings": {"mode": "m"}, "checkpoint": {"uuid": "u"},
                "load": {"observed_storage_bytes": 5}}))
            return subprocess.CompletedProcess(command, 0, stdout="", stderr="")

        binaries = {mode: self.root / mode for mode in measure.MODES}
        for binary in binaries.values():
            binary.write_bytes(b"binary")
        args = measure.parse_args([
            "--customer", str(self.root / "customer"), "--orders", str(self.root / "orders"),
            "--lineitem", str(self.root / "lineitem"), "--cache-bytes", str(1024 * 1024),
            "--build-images", str(images), "--setup-full-compaction", "--elide-value-headers"])
        with patch.object(measure, "build_executables", return_value=binaries), \
             patch.object(measure.subprocess, "run", side_effect=process):
            self.assertEqual(measure.build_images(args), 0)
        manifest = json.loads((images / measure.IMAGES_MANIFEST).read_text())
        self.assertEqual(manifest["status"], "complete")
        self.assertEqual(sorted(manifest["images"]),
                         ["merged-index-A", "merged-index-B", "native-A", "native-B"])
        self.assertEqual(manifest["images"]["native-A"]["size_bytes"], 5)
        self.assertEqual(len(commands), 4)
        for command in commands:
            # Loading has no memory target and no cgroup: it is not measured.
            self.assertNotIn("--memory-target-bytes", command)
            self.assertNotIn("--r5", command)
            self.assertIn("--setup-full-compaction", command)
            self.assertIn("--elide-value-headers", command)
            name = Path(command[command.index("--write-image") + 1]).name
            self.assertEqual("--front-code-byte-keys" in command, name.endswith("-A"))
        # An existing directory is refused.
        self.assertEqual(measure.build_images(args), 2)

    def test_build_images_records_every_load_that_succeeds_when_one_fails(self) -> None:
        images = self.root / "images-one-fails"

        def process(command: list[str], **kwargs: object) -> subprocess.CompletedProcess:
            if "--write-image" not in command:  # git, generate.sh, uname
                return subprocess.CompletedProcess(command, 0, stdout="rev\n", stderr="")
            directory = Path(command[command.index("--write-image") + 1])
            if directory.name == "merged-index-B":
                return subprocess.CompletedProcess(command, 101, stdout="", stderr="boom")
            (directory / "storage").mkdir(parents=True)
            (directory / "image.json").write_text(json.dumps({
                "settings": {}, "checkpoint": {"uuid": "u"}, "load": {}}))
            return subprocess.CompletedProcess(command, 0, stdout="", stderr="")

        binaries = {mode: self.root / mode for mode in measure.MODES}
        for binary in binaries.values():
            binary.write_bytes(b"binary")
        args = measure.parse_args([
            "--customer", str(self.root / "customer"), "--orders", str(self.root / "orders"),
            "--lineitem", str(self.root / "lineitem"), "--cache-bytes", str(1024 * 1024),
            "--build-images", str(images)])
        with patch.object(measure, "build_executables", return_value=binaries), \
             patch.object(measure.subprocess, "run", side_effect=process):
            self.assertNotEqual(measure.build_images(args), 0)
        manifest = json.loads((images / measure.IMAGES_MANIFEST).read_text())
        self.assertEqual(manifest["status"], "failed")
        self.assertEqual(sorted(manifest["images"]), ["merged-index-A", "native-A", "native-B"])
        self.assertEqual(len(manifest["errors"]), 1)
        self.assertIn("merged-index-B exited 101", manifest["errors"][0])
        self.assertIn("boom", (images / "merged-index-B.stderr.log").read_text())

    def test_cgroup_child_starts_cold_by_evicting_the_shared_inputs(self) -> None:
        group = self.root / "cgroup-evict"
        group.mkdir()
        (group / "memory.max").write_text("4096")
        run_dir = self.root / "evict-run"
        run_dir.mkdir()
        spec = {**self.shared_input_spec(run_dir), "cgroup": str(group),
                "total_memory_bytes": 4096, "executable": "/bin/driver",
                "args": {"cache_bytes": 1024, "memory_target_bytes": 2048,
                         "compaction_timeout_secs": 5, "no_verify": True}}
        spec_path = self.root / "evict-spec.json"
        spec_path.write_text(json.dumps(spec))
        with patch.object(measure, "verify_membership"), \
             patch.object(measure.os, "execv", side_effect=RuntimeError("stop")), \
             patch.object(measure, "evict_from_page_cache",
                          return_value={"evicted": True}) as evict:
            with self.assertRaisesRegex(RuntimeError, "stop"):
                measure.run_inside_cgroup(spec_path)
        evicted = {str(path) for path in evict.call_args.args[0]}
        self.assertEqual(evicted, set(spec["inputs"].values()))
        self.assertTrue((run_dir / "input-eviction.json").is_file())
        proof = json.loads((run_dir / "cgroup-proof.json").read_text())
        self.assertTrue(proof["inputs_evicted"])

    def test_cgroup_limit_and_swap_are_checked(self) -> None:
        group = measure.configure_cgroup(self.root, "run", 1024)
        self.assertEqual((group / "memory.max").read_text(), "1024")
        self.assertEqual((group / "memory.swap.max").read_text(), "0")
        with self.assertRaises(ValueError):
            measure.cgroup_available(self.root)

    def test_cgroup_child_runs_the_driver_on_the_shared_inputs(self) -> None:
        group = self.root / "cgroup"
        group.mkdir()
        (group / "memory.max").write_text("4096")
        run_dir = self.root / "run"
        run_dir.mkdir()
        spec = {**self.shared_input_spec(run_dir), "cgroup": str(group),
                "total_memory_bytes": 4096, "executable": "/bin/driver",
                "args": {"cache_bytes": 1024, "memory_target_bytes": 2048,
                         "compaction_timeout_secs": 5, "no_verify": True}}
        spec_path = self.root / "spec.json"
        spec_path.write_text(json.dumps(spec))
        with patch.object(measure, "verify_membership") as membership, \
             patch.object(measure.os, "execv", side_effect=RuntimeError("stop")) as execute:
            with self.assertRaisesRegex(RuntimeError, "stop"):
                measure.run_inside_cgroup(spec_path)
        membership.assert_called_once_with(group)
        command = execute.call_args.args[1]
        self.assertEqual(command[0], "/bin/driver")
        self.assertEqual(command[command.index("--customer") + 1], str(self.root / "customer"))
        self.assertEqual(command[command.index("--refresh-list") + 1], str(self.refresh))
        self.assertFalse((run_dir / "input").exists(), "nothing is copied")
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
                "merged_index": {"operations": 5, "bytes": 500},
                "requested_slice": {"operations": 0, "bytes": 0},
                "partner_trace": {"operations": 0, "bytes": 0}}}
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
        unsummed["sequence"]["index_run_reads"]["session"]["seeks"] = 4
        self.assertIn("not the sums", measure.index_read_problems(unsummed)[0])

        negative = self.index_report()
        negative["refreshes"][0]["index_run_reads"]["session"]["key_decodes"] = -1
        self.assertIn("session.key_decodes is -1",
                      measure.index_read_problems(negative)[0])

        # A report from before the memory reads (measurement version 6) is incomplete.
        old_report = self.index_report()
        del old_report["refreshes"][0]["index_run_reads"]["session"]["memory_key_reads"]
        self.assertIn("session.memory_key_reads is None",
                      measure.index_read_problems(old_report)[0])

        no_layout = self.index_report()
        del no_layout["refreshes"][1]["index_layout"]["after"]["file"]
        self.assertIn("index_layout.after.file is malformed",
                      measure.index_read_problems(no_layout)[0])

        native = self.index_report("native")
        native["refreshes"][0]["index_run_reads"] = {}
        self.assertIn("a native run reports merged-index reads",
                      measure.index_read_problems(native)[0])

    def test_view_reads_leave_the_index_to_the_session_and_sum_to_the_sequence(self) -> None:
        def view(index_seeks: int, query_seeks: int, slice_seeks: int = 0,
                 trace_seeks: int = 0) -> dict:
            reads = {owner: {field: 0 for field in measure.RUN_READ_FIELDS}
                     for owner in measure.WRITE_OWNERS}
            reads["merged_index"]["seeks"] = index_seeks
            reads["query"]["seeks"] = query_seeks
            reads["requested_slice"]["seeks"] = slice_seeks
            reads["partner_trace"]["seeks"] = trace_seeks
            return reads
        report = self.index_report()
        for refresh in report["refreshes"]:
            # The scan session reads the index on the driver's thread, and the view worker none.
            # The joins read the readers' requested slices.
            refresh["view_run_reads"] = view(0, 5, 2)
        report["sequence"]["view_run_reads"] = view(0, 10, 4)
        self.assertEqual(measure.view_read_problems(report), [])

        report["sequence"]["view_run_reads"]["query"]["seeks"] = 11
        self.assertIn("not the sums", measure.view_read_problems(report)[0])
        report["sequence"]["view_run_reads"]["query"]["seeks"] = 10

        report["refreshes"][0]["view_run_reads"]["merged_index"]["seeks"] = 5
        self.assertIn("view_run_reads.merged_index.seeks is 5, expected 0",
                      measure.view_read_problems(report)[0])

        # A reconstructed run reports requested-slice reads.
        no_slice = self.index_report()
        for refresh in no_slice["refreshes"]:
            refresh["view_run_reads"] = view(0, 5)
        no_slice["sequence"]["view_run_reads"] = view(0, 10)
        self.assertIn("reports no view_run_reads.requested_slice reads",
                      measure.view_read_problems(no_slice)[0])

        native = {"mode": "native", "sequence": {"view_run_reads": view(0, 3, 0, 2)},
                  "refreshes": [{"refresh": 1, "view_run_reads": view(0, 3, 0, 2)}]}
        self.assertEqual(measure.view_read_problems(native), [])
        native["refreshes"][0]["view_run_reads"]["merged_index"]["seeks"] = 1
        self.assertIn("expected 0", measure.view_read_problems(native)[0])
        # ...and none in a native run.
        native["refreshes"][0]["view_run_reads"]["merged_index"]["seeks"] = 0
        native["refreshes"][0]["view_run_reads"]["requested_slice"]["seeks"] = 1
        self.assertIn("view_run_reads.requested_slice.seeks is 1, expected 0 in a native run",
                      measure.view_read_problems(native)[0])
        native["refreshes"][0]["view_run_reads"]["requested_slice"]["seeks"] = 0

        # A native run reports partner-trace reads, and a reconstructed run none.
        no_trace = {"mode": "native", "sequence": {"view_run_reads": view(0, 3)},
                    "refreshes": [{"refresh": 1, "view_run_reads": view(0, 3)}]}
        self.assertIn("a native run reports no view_run_reads.partner_trace reads",
                      measure.view_read_problems(no_trace)[0])
        report["refreshes"][0]["view_run_reads"]["merged_index"]["seeks"] = 0
        report["refreshes"][0]["view_run_reads"]["partner_trace"]["seeks"] = 1
        self.assertIn("view_run_reads.partner_trace.seeks is 1, expected 0 in a reconstructed",
                      measure.view_read_problems(report)[0])

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

    def test_the_requested_slice_owner_writes_nothing(self) -> None:
        report = self.owner_report()
        owners = report["sequence"]["resources"]["backend_write_by_owner"]
        owners["requested_slice"] = {"operations": 1, "bytes": 0}
        owners["query"] = {"operations": 1, "bytes": 200}
        problems = measure.owner_write_problems(report)
        self.assertEqual(len(problems), 2)
        for problem in problems:
            self.assertIn("read owner requested_slice", problem)

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

    @staticmethod
    def counter_report(key: str, fields: tuple[str, ...], mode: str = "reconstructed") -> dict:
        """A report whose sequence counters `key` are the sums of its two refreshes'."""
        if mode == "native":
            return {"mode": "native", "sequence": {key: None},
                    "refreshes": [{"refresh": 1, key: None}]}
        return {"mode": mode, "sequence": {key: {field: 3 for field in fields}},
                "refreshes": [{"refresh": number, key: {field: number for field in fields}}
                              for number in (1, 2)]}

    def test_session_counts_and_upkeep_reads_are_complete_and_sum_to_the_sequence(self) -> None:
        for key, fields in (("index_session_counts", measure.SESSION_COUNT_FIELDS),
                            ("index_upkeep_run_reads", measure.RUN_READ_FIELDS)):
            with self.subTest(key=key):
                self.assertEqual(measure.counter_problems(
                    self.counter_report(key, fields), key, fields), [])
                self.assertEqual(measure.counter_problems(
                    self.counter_report(key, fields, "native"), key, fields), [])

                unsummed = self.counter_report(key, fields)
                unsummed["sequence"][key][fields[0]] = 4
                self.assertIn("not the sums",
                              measure.counter_problems(unsummed, key, fields)[0])

                missing = self.counter_report(key, fields)
                del missing["refreshes"][1][key][fields[-1]]
                self.assertIn(f"refresh 2: {key}.{fields[-1]} is None",
                              measure.counter_problems(missing, key, fields)[0])

                negative = self.counter_report(key, fields)
                negative["refreshes"][0][key][fields[0]] = -1
                self.assertIn("is -1", measure.counter_problems(negative, key, fields)[0])

                native = self.counter_report(key, fields, "native")
                native["sequence"][key] = {}
                self.assertIn(f"a native run reports {key}",
                              measure.counter_problems(native, key, fields)[0])

    def test_profiled_cpu_times_are_required_in_both_modes(self) -> None:
        for mode in ("native", "reconstructed"):
            report = {"mode": mode, "refreshes": [
                {"refresh": 1, "profiled_cpu_ms": {"base": 0.5, "q3": 0}}]}
            self.assertEqual(measure.profiled_cpu_problems(report), [])
            report["refreshes"][0]["profiled_cpu_ms"] = {"base": float("nan"), "q3": -1}
            self.assertEqual(len(measure.profiled_cpu_problems(report)), 2)
            report["refreshes"][0]["profiled_cpu_ms"] = None
            self.assertIn("profiled_cpu_ms.base is None",
                          measure.profiled_cpu_problems(report)[0])

    def test_delta_phase_is_required_and_within_ivm_in_both_modes(self) -> None:
        for mode in ("native", "reconstructed"):
            report = {"mode": mode, "refreshes": [
                {"refresh": 1, "delta_ms": 0.5, "q3_ms": 1.0, "ivm_ms": 1.5}]}
            self.assertEqual(measure.delta_phase_problems(report), [])
            report["refreshes"][0]["delta_ms"] = 0
            self.assertEqual(measure.delta_phase_problems(report), [])
            for bad in (-1, float("nan"), None, True):
                report["refreshes"][0]["delta_ms"] = bad
                self.assertIn("delta_ms is", measure.delta_phase_problems(report)[0])
            del report["refreshes"][0]["delta_ms"]
            self.assertIn("delta_ms is None", measure.delta_phase_problems(report)[0])
            report["refreshes"][0]["delta_ms"] = 2.0
            self.assertIn("ivm_ms 1.5 is below delta_ms 2.0",
                          measure.delta_phase_problems(report)[0])
        self.assertEqual(measure.delta_phase_problems({"mode": "other", "refreshes": [{}]}), [])

    def test_reported_session_pass_must_be_the_requested_one(self) -> None:
        def report(mode: str, reported: object) -> dict:
            return {"mode": mode, "configuration": {"session_pass": reported}}
        self.assertEqual(measure.session_pass_problems(report("reconstructed", "step"), "step"), [])
        self.assertIn("expected 'step'",
                      measure.session_pass_problems(report("reconstructed", "seek"), "step")[0])
        # Native has no scan session, whatever the runner asked for.
        self.assertEqual(measure.session_pass_problems(report("native", None), "step"), [])
        self.assertIn("expected None",
                      measure.session_pass_problems(report("native", "seek"), "seek")[0])

    def test_session_pass_flag_reaches_the_driver(self) -> None:
        spec = {"run_dir": str(self.root / "run"), "executable": "/bin/driver",
                "args": {"cache_bytes": 1024, "memory_target_bytes": 2048,
                         "compaction_timeout_secs": 5, "no_verify": True,
                         "session_pass": "step"}}
        inputs = {key: key for key in ("customer", "orders", "lineitem", "refresh_list")}
        command = measure.driver_command(spec, inputs)
        self.assertEqual(command[command.index("--session-pass") + 1], "step")
        del spec["args"]["session_pass"]
        command = measure.driver_command(spec, inputs)
        self.assertEqual(command[command.index("--session-pass") + 1], "seek")
        self.assertEqual(measure.parse_args(self.args).session_pass, "seek")
        self.assertEqual(
            measure.parse_args([*self.args, "--session-pass", "step"]).session_pass, "step")
        with self.assertRaises(SystemExit), patch("sys.stderr"):
            measure.parse_args([*self.args, "--session-pass", "Seek"])

    def test_both_builds_share_one_generation_without_options(self) -> None:
        calls = []

        def process(command, **kwargs):
            calls.append(command)
            return subprocess.CompletedProcess(command, 0)

        release = self.root / "cargo" / "release"
        release.mkdir(parents=True)
        (release / "q3-ivm-baseline").write_bytes(b"binary")
        with patch.dict(measure.os.environ, {"CARGO_TARGET_DIR": str(self.root / "cargo")}), \
             patch.object(measure.subprocess, "run", side_effect=process):
            binaries = measure.build_executables(self.root / "executables")
        generations = [command for command in calls if command[0].endswith("generate.sh")]
        self.assertEqual(generations, [[str(measure.EXPERIMENT / "generate.sh")]])
        self.assertEqual(set(binaries), set(measure.MODES))
        builds = [command for command in calls if "build" in command]
        self.assertEqual(["merged-index" in command for command in builds], [False, True])

    def test_cli_rejects_invalid_cache_and_timeout(self) -> None:
        with self.assertRaises(SystemExit):
            measure.parse_args([*self.args, "--cache-bytes", "1024"])
        with self.assertRaises(SystemExit):
            measure.parse_args([*self.args, "--compaction-timeout-secs", "nan"])
        parsed = measure.parse_args([*self.args, "--batch", "512"])
        self.assertEqual(parsed.batch, 512)

    # Storage device (manifest `storage_device`), on fixture /proc and /sys trees.

    def write_proc_and_sys(self, mountinfo: list[str], disks: dict[str, dict]) -> tuple[Path, Path]:
        """Write a fake `/proc/self/mountinfo` and `/sys/block` and return their roots.

        `disks` maps a disk name to its `rotational` and `sector` values (strings, as sysfs
        prints them), an optional `model`, optional `partitions`, and an optional `dm` flag that
        gives the disk a `dm` directory, as a device-mapper device has.
        """
        proc, sys_root = self.root / "proc", self.root / "sys"
        (proc / "self").mkdir(parents=True, exist_ok=True)
        (proc / "self" / "mountinfo").write_text("\n".join(mountinfo) + "\n")
        for name, disk in disks.items():
            base = sys_root / "block" / name
            (base / "queue").mkdir(parents=True, exist_ok=True)
            if "rotational" in disk:
                (base / "queue" / "rotational").write_text(disk["rotational"] + "\n")
            if "sector" in disk:
                (base / "queue" / "logical_block_size").write_text(disk["sector"] + "\n")
            if "model" in disk:
                (base / "device").mkdir(exist_ok=True)
                (base / "device" / "model").write_text(disk["model"] + "\n")
            for partition in disk.get("partitions", ()):
                (base / partition).mkdir(exist_ok=True)
            if disk.get("dm"):
                (base / "dm").mkdir(exist_ok=True)
        return proc, sys_root

    MOUNTS = [
        "26 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw",
        "40 26 259:3 / /mnt/ssd rw,noatime shared:5 - xfs /dev/nvme1n1p1 rw",
        "41 26 8:17 / /mnt/hdd rw,noatime shared:6 - ext4 /dev/sdb1 rw",
        "42 26 253:0 / /mnt/lvm rw,noatime shared:7 - ext4 /dev/mapper/vg-data rw",
        "43 26 252:0 / /mnt/dm0 rw,noatime shared:8 - ext4 /dev/dm-0 rw",
        "44 26 0:50 / /mnt/ramdisk rw - tmpfs tmpfs rw,size=1g",
        "45 26 8:33 / /mnt/with\\040space rw - ext4 /dev/sdc1 rw",
        "46 26 0:60 / /mnt/net rw - nfs4 server:/export rw",
    ]
    DISKS = {
        "nvme0n1": {"rotational": "0", "sector": "512", "model": "Boot NVMe",
                    "partitions": ["nvme0n1p1", "nvme0n1p2"]},
        "nvme1n1": {"rotational": "0", "sector": "512", "model": "Samsung SSD 990 PRO 1TB",
                    "partitions": ["nvme1n1p1"]},
        "sdb": {"rotational": "1", "sector": "512", "model": "ST4000DM004-2CV1   ",
                "partitions": ["sdb1"]},
        "sdc": {"rotational": "1", "sector": "4096", "partitions": ["sdc1"]},
        "dm-0": {"rotational": "0", "sector": "512", "dm": True},
    }

    def device(self, path: str, label: str | None = None) -> dict:
        proc, sys_root = self.write_proc_and_sys(self.MOUNTS, self.DISKS)
        return measure.storage_device(Path(path), label, system="Linux", proc_root=proc,
                                      sys_root=sys_root)

    def test_an_nvme_partition_resolves_to_its_parent_disk(self) -> None:
        self.assertEqual(self.device("/mnt/ssd/campaign", "ssd"), {
            "path": "/mnt/ssd/campaign", "device": "/dev/nvme1n1", "filesystem": "xfs",
            "logical_sector_bytes": 512, "rotational": False,
            "model": "Samsung SSD 990 PRO 1TB", "label": "ssd"})

    def test_a_sata_hard_disk_is_rotational_with_its_model_trimmed(self) -> None:
        self.assertEqual(self.device("/mnt/hdd/campaign", "hdd"), {
            "path": "/mnt/hdd/campaign", "device": "/dev/sdb", "filesystem": "ext4",
            "logical_sector_bytes": 512, "rotational": True,
            "model": "ST4000DM004-2CV1", "label": "hdd"})

    def test_the_longest_matching_mount_point_holds_the_path(self) -> None:
        # The root file system holds a path that no other mount contains, and a mount whose
        # name only starts like the path's is not a parent of it.
        self.assertEqual(self.device("/home/user/campaign")["device"], "/dev/nvme0n1")
        self.assertEqual(self.device("/mnt/ssd2/campaign")["device"], "/dev/nvme0n1")
        self.assertEqual(self.device("/mnt/ssd")["device"], "/dev/nvme1n1")
        self.assertEqual(self.device("/mnt/with space/campaign")["device"], "/dev/sdc")

    def test_a_missing_sysfs_file_leaves_only_its_field_null(self) -> None:
        found = self.device("/mnt/with space/campaign")
        self.assertEqual((found["logical_sector_bytes"], found["rotational"]), (4096, True))
        self.assertIsNone(found["model"])

    def test_device_mapper_and_other_virtual_devices_are_not_guessed(self) -> None:
        unresolved = {field: None for field in measure.STORAGE_DEVICE_FIELDS}
        for path in ("/mnt/lvm/campaign", "/mnt/dm0/campaign", "/mnt/ramdisk/campaign",
                     "/mnt/net/campaign"):
            with self.subTest(path=path):
                self.assertEqual(self.device(path), unresolved)
        # The operator's label is still recorded as given.
        self.assertEqual(self.device("/mnt/lvm/campaign", "ssd"),
                         {**unresolved, "label": "ssd"})

    def test_the_device_is_unresolved_off_linux_and_without_proc(self) -> None:
        unresolved = {field: None for field in measure.STORAGE_DEVICE_FIELDS}
        proc, sys_root = self.write_proc_and_sys(self.MOUNTS, self.DISKS)
        self.assertEqual(measure.storage_device(Path("/mnt/ssd/campaign"), system="Darwin",
                                                proc_root=proc, sys_root=sys_root), unresolved)
        self.assertEqual(measure.storage_device(Path("/mnt/ssd/campaign"), "hdd", system="Darwin"),
                         {**unresolved, "label": "hdd"})
        self.assertEqual(measure.storage_device(Path("/mnt/ssd/campaign"), system="Linux",
                                                proc_root=self.root / "absent",
                                                sys_root=sys_root), unresolved)

    def test_a_label_must_agree_with_a_known_rotational_flag(self) -> None:
        self.assertIsNone(measure.device_label_problem(self.device("/mnt/ssd/c", "ssd")))
        self.assertIsNone(measure.device_label_problem(self.device("/mnt/hdd/c", "hdd")))
        self.assertIn("--device-label hdd contradicts /dev/nvme1n1, which reports rotational=0",
                      measure.device_label_problem(self.device("/mnt/ssd/c", "hdd")))
        self.assertIn("--device-label ssd contradicts /dev/sdb, which reports rotational=1",
                      measure.device_label_problem(self.device("/mnt/hdd/c", "ssd")))
        # Nothing contradicts an absent label, an unresolved device, or an unknown flag.
        self.assertIsNone(measure.device_label_problem(self.device("/mnt/ssd/c")))
        self.assertIsNone(measure.device_label_problem(self.device("/mnt/lvm/c", "hdd")))
        unknown = {**self.device("/mnt/ssd/c", "hdd"), "rotational": None}
        self.assertIsNone(measure.device_label_problem(unknown))

    def test_the_manifest_records_the_device_and_a_contradicting_label_stops_the_run(self) -> None:
        def process(command, **kwargs):
            if command[:3] == ["git", "rev-parse", "HEAD"]:
                return subprocess.CompletedProcess(command, 0, stdout="revision\n")
            return subprocess.CompletedProcess(command, 0, stdout="", stderr="")

        binaries = {mode: self.root / mode for mode in measure.MODES}
        for binary in binaries.values():
            binary.write_bytes(b"binary")
        # Off Linux, nothing resolves and nothing can contradict the label.
        with patch.object(measure.platform, "system", return_value="Darwin"), \
             patch.object(measure, "build_executables", return_value=binaries), \
             patch.object(measure.subprocess, "run", side_effect=process):
            measure.run([*self.args, "--device-label", "hdd"])
        recorded = json.loads(self.report.read_text())["storage_device"]
        self.assertEqual(recorded, {**{field: None for field in measure.STORAGE_DEVICE_FIELDS},
                                    "label": "hdd"})

        self.report.unlink()
        known = {"path": "/x", "device": "/dev/nvme0n1", "filesystem": "ext4",
                 "logical_sector_bytes": 512, "rotational": False, "model": "m", "label": "hdd"}
        with patch.object(measure, "storage_device", return_value=known), \
             patch.object(measure, "build_executables") as build, \
             patch.object(measure.subprocess, "run", side_effect=process), \
             patch("sys.stderr") as stderr:
            self.assertEqual(measure.run([*self.args, "--device-label", "hdd"]), 2)
        build.assert_not_called()
        self.assertFalse(self.report.exists())
        self.assertIn("contradicts", "".join(call.args[0] for call in stderr.write.call_args_list))

    def test_the_device_label_is_ssd_or_hdd(self) -> None:
        self.assertIsNone(measure.parse_args(self.args).device_label)
        self.assertEqual(measure.parse_args([*self.args, "--device-label", "ssd"]).device_label,
                         "ssd")
        with self.assertRaises(SystemExit), patch("sys.stderr"):
            measure.parse_args([*self.args, "--device-label", "nvme"])

    # The layout-fallback gate and the block-expansion checks (measurement version 12).

    def gate_report(self, mode: str = "reconstructed", truncation: bool = True,
                    elision: bool = True, front: int = 0, value: int = 0) -> dict:
        return {"mode": mode,
                "configuration": {"front_code_byte_keys": truncation,
                                  "elide_value_headers": elision},
                "front_coding": {"blocks": 9, "layout_fallbacks": front, "size_fallbacks": 4},
                "value_header_elision": {"blocks": 9, "layout_fallbacks": value,
                                         "size_fallbacks": 4}}

    def test_the_gate_accepts_zero_layout_fallbacks_and_ignores_size_fallbacks(self) -> None:
        for mode in ("native", "reconstructed"):
            self.assertEqual(measure.layout_fallback_problems(self.gate_report(mode)), [])

    def test_the_gate_rejects_a_layout_fallback_of_each_encoding(self) -> None:
        problems = measure.layout_fallback_problems(self.gate_report(front=2))
        self.assertEqual(len(problems), 1)
        self.assertIn("front_coding.layout_fallbacks is 2, expected 0", problems[0])
        problems = measure.layout_fallback_problems(self.gate_report(value=1))
        self.assertEqual(len(problems), 1)
        self.assertIn("value_header_elision.layout_fallbacks is 1, expected 0", problems[0])
        self.assertEqual(len(measure.layout_fallback_problems(
            self.gate_report(front=1, value=1))), 2)

    def test_the_gate_checks_only_the_encodings_that_are_on(self) -> None:
        # A fallback of an encoding that was not asked for is not this run's problem.
        report = self.gate_report(truncation=False, elision=True, front=3)
        self.assertEqual(measure.layout_fallback_problems(report), [])
        report = self.gate_report(truncation=True, elision=False, value=3)
        self.assertEqual(measure.layout_fallback_problems(report), [])
        report = self.gate_report(truncation=False, elision=False, front=3, value=3)
        self.assertEqual(measure.layout_fallback_problems(report), [])

    def test_the_gate_needs_a_counter_when_the_encoding_is_on(self) -> None:
        report = self.gate_report()
        del report["front_coding"]
        self.assertIn("front_coding.layout_fallbacks is None",
                      measure.layout_fallback_problems(report)[0])
        report = self.gate_report()
        report["value_header_elision"]["layout_fallbacks"] = -1
        self.assertIn("is -1", measure.layout_fallback_problems(report)[0])
        self.assertEqual(measure.layout_fallback_problems({"mode": "other"}), [])

    def expansion_count(self, truncated: int = 0, elided: int = 0) -> dict[str, int]:
        return {"prefix_truncated_blocks": truncated,
                "prefix_truncated_encoded_bytes": 100 * truncated,
                "prefix_truncated_expanded_bytes": 400 * truncated,
                "elided_blocks": elided, "elided_encoded_bytes": 50 * elided,
                "elided_expanded_bytes": 200 * elided}

    def expansion_report(self, mode: str = "reconstructed", truncation: bool = True,
                         elision: bool = True) -> dict:
        """A report with two refreshes. A reconstructed run expands blocks on the view worker's
        requested slices, in the driver's scan, and in base maintenance; a native run none."""
        def block_expansions(number: int) -> dict:
            zero = {owner: self.expansion_count() for owner in measure.WRITE_OWNERS}
            if mode == "native":
                return {"view_worker": zero, "driver": None, "index_upkeep": None}
            view = {**zero, "requested_slice": self.expansion_count(number, 2 * number)}
            driver = {**zero, "merged_index": self.expansion_count(number, number)}
            return {"view_worker": view, "driver": driver,
                    "index_upkeep": self.expansion_count(number, 0)}
        return {"mode": mode, "measurement_version": 13,
                "configuration": {"front_code_byte_keys": truncation,
                                  "elide_value_headers": elision},
                "sequence": {"block_expansions": block_expansions(3)},
                "refreshes": [{"refresh": number, "block_expansions": block_expansions(number)}
                              for number in (1, 2)]}

    def test_block_expansions_are_complete_and_sum_to_the_sequence(self) -> None:
        for mode in ("native", "reconstructed"):
            self.assertEqual(measure.block_expansion_problems(self.expansion_report(mode)), [])
        self.assertEqual(measure.block_expansion_problems({"mode": "other"}), [])

        unsummed = self.expansion_report()
        unsummed["sequence"]["block_expansions"]["index_upkeep"]["elided_blocks"] = 1
        unsummed["sequence"]["block_expansions"]["index_upkeep"]["elided_encoded_bytes"] = 1
        unsummed["sequence"]["block_expansions"]["index_upkeep"]["elided_expanded_bytes"] = 2
        self.assertIn("not the sums", measure.block_expansion_problems(unsummed)[0])

        negative = self.expansion_report()
        negative["refreshes"][0]["block_expansions"]["view_worker"]["query"]["elided_blocks"] = -1
        self.assertIn("view_worker.query.elided_blocks is -1",
                      measure.block_expansion_problems(negative)[0])

        missing = self.expansion_report()
        del missing["refreshes"][1]["block_expansions"]["driver"]["merged_index"][
            "prefix_truncated_blocks"]
        self.assertIn("driver.merged_index.prefix_truncated_blocks is None",
                      measure.block_expansion_problems(missing)[0])

        flag = self.expansion_report()
        flag["sequence"]["block_expansions"]["view_worker"]["query"]["elided_blocks"] = True
        self.assertIn("is True", measure.block_expansion_problems(flag)[0])

        none = self.expansion_report()
        none["refreshes"][0]["block_expansions"] = None
        self.assertIn("refresh 1: block_expansions is None",
                      measure.block_expansion_problems(none)[0])

    def test_a_native_run_reports_no_driver_or_upkeep_expansions(self) -> None:
        native = self.expansion_report("native")
        native["refreshes"][0]["block_expansions"]["driver"] = {
            owner: self.expansion_count() for owner in measure.WRITE_OWNERS}
        self.assertIn("a native run reports block_expansions.driver",
                      measure.block_expansion_problems(native)[0])
        # A reconstructed run reports both.
        reconstructed = self.expansion_report()
        reconstructed["refreshes"][0]["block_expansions"]["index_upkeep"] = None
        self.assertIn("index_upkeep.prefix_truncated_blocks is None",
                      measure.block_expansion_problems(reconstructed)[0])

    def test_an_encoding_that_is_off_expands_nothing(self) -> None:
        # Prefix truncation off: the prefix-truncated fields must be 0.
        report = self.expansion_report(truncation=False)
        problems = measure.block_expansion_problems(report)
        self.assertTrue(problems)
        self.assertTrue(all("prefix_truncated" in problem and "encoding off" in problem
                            for problem in problems if "not the sums" not in problem))
        # Elision off: the elided fields must be 0.
        report = self.expansion_report(elision=False)
        self.assertTrue(any("elided_blocks" in problem for problem in
                            measure.block_expansion_problems(report)))
        # Both off, and a run that expanded nothing, is fine.
        quiet = self.expansion_report("native", truncation=False, elision=False)
        self.assertEqual(measure.block_expansion_problems(quiet), [])

    def test_expansion_bytes_follow_the_blocks(self) -> None:
        bytes_without_blocks = self.expansion_report("native")
        bytes_without_blocks["sequence"]["block_expansions"]["view_worker"]["query"][
            "elided_encoded_bytes"] = 5
        bytes_without_blocks["sequence"]["block_expansions"]["view_worker"]["query"][
            "elided_expanded_bytes"] = 9
        self.assertIn("counts bytes of no elided_blocks",
                      measure.block_expansion_problems(bytes_without_blocks)[0])
        shrunk = self.expansion_report("native")
        shrunk["refreshes"][0]["block_expansions"]["view_worker"]["query"] = {
            **self.expansion_count(1), "prefix_truncated_expanded_bytes": 99}
        self.assertIn("prefix_truncated_expanded_bytes is below prefix_truncated_encoded_bytes",
                      measure.block_expansion_problems(shrunk)[0])

    def test_report_problems_require_measurement_version_13(self) -> None:
        report = self.expansion_report()
        report.update(self.gate_report())
        self.assertEqual(measure.measurement_version_problems(report), [])
        report["measurement_version"] = 12
        self.assertEqual(measure.measurement_version_problems(report),
                         ["measurement_version is 12, expected 13"])
        del report["measurement_version"]
        self.assertIn("is None", measure.measurement_version_problems(report)[0])
        self.assertEqual(measure.measurement_version_problems({"mode": "other"}), [])
        # The combined check reports the gate and the expansions too.
        report = self.expansion_report()
        report.update(self.gate_report(front=1))
        problems = measure.report_problems(report, "seek")
        self.assertTrue(any("front_coding.layout_fallbacks is 1" in p for p in problems))


if __name__ == "__main__":
    unittest.main()
