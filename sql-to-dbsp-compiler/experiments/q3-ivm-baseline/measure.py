#!/usr/bin/env python3
"""Build and compare the two R5 Q3 maintenance executables."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
from datetime import datetime, timezone
from typing import Any


EXPERIMENT = Path(__file__).resolve().parent
MODES = ("native", "merged-index")
WRITE_OWNERS = ("query", "base_relation", "merged_index")
# The merged index's readers, and the run counts reported for each (measurement version 5).
INDEX_READERS = ("customers_after", "orders_before", "joined_orders_after", "lineitems_before")
RUN_READ_FIELDS = ("seeks", "filter_rejections", "key_decodes", "value_decodes", "weight_decodes")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def refresh_inputs(path: Path) -> tuple[list[dict[str, str]], dict[str, Path]]:
    entries = json.loads(path.read_text())
    if not isinstance(entries, list):
        raise ValueError("refresh list must be a JSON array")
    resolved: list[dict[str, str]] = []
    inputs: dict[str, Path] = {}
    for number, entry in enumerate(entries):
        if not isinstance(entry, dict) or set(entry) not in (
            {"rf1_orders", "rf1_lines"}, {"rf2"}
        ):
            raise ValueError(f"refresh {number} must contain RF1 paths or one RF2 path")
        item: dict[str, str] = {}
        for field, raw in entry.items():
            if not isinstance(raw, str) or not raw:
                raise ValueError(f"refresh {number} {field} must be a path")
            source = (path.parent / raw).resolve()
            if not source.is_file():
                raise FileNotFoundError(source)
            item[field] = str(source)
            inputs[f"refresh_{number}_{field}"] = source
        resolved.append(item)
    return resolved, inputs


def index_read_problems(report: dict[str, Any]) -> list[str]:
    """List where a driver report's merged-index run reads are malformed.

    A reconstructed report gives, for each refresh and for the sequence, every
    reader's `RUN_READ_FIELDS` as non-negative integers, and each refresh's
    index layout; the sequence's counts are the sums of the refreshes'. A
    native report has no merged index, so all of these are `null`. A report that
    names neither mode is not checked here.
    """
    if report.get("mode") not in ("native", "reconstructed"):
        return []
    refreshes = report.get("refreshes", [])
    sequence = report.get("sequence", {}).get("index_run_reads")
    problems: list[str] = []
    if report.get("mode") == "native":
        places = [("sequence", sequence)]
        places += [(f"refresh {item.get('refresh')}", item.get("index_run_reads"))
                   for item in refreshes]
        places += [(f"refresh {item.get('refresh')} layout", item.get("index_layout"))
                   for item in refreshes]
        return [f"{place}: a native run reports merged-index reads"
                for place, value in places if value is not None]

    def counts(place: str, reads: Any) -> dict[tuple[str, str], int] | None:
        found = {}
        for reader in INDEX_READERS:
            for field in RUN_READ_FIELDS:
                value = reads.get(reader, {}).get(field) if isinstance(reads, dict) else None
                if isinstance(value, bool) or not isinstance(value, int) or value < 0:
                    problems.append(f"{place}: index_run_reads.{reader}.{field} is {value!r}")
                    return None
                found[(reader, field)] = value
        return found

    total: dict[tuple[str, str], int] = {}
    for item in refreshes:
        place = f"refresh {item.get('refresh')}"
        found = counts(place, item.get("index_run_reads"))
        for key, value in (found or {}).items():
            total[key] = total.get(key, 0) + value
        layout = item.get("index_layout")
        for snapshot in ("before", "after"):
            for location in ("file", "memory"):
                block = layout.get(snapshot, {}).get(location) if isinstance(layout, dict) \
                    else None
                if not isinstance(block, dict) or not all(
                        isinstance(block.get(field), int) for field in ("runs", "bytes")):
                    problems.append(f"{place}: index_layout.{snapshot}.{location} is malformed")
    summed = counts("sequence", sequence)
    if summed is not None and not problems and summed != total:
        problems.append("sequence: index_run_reads are not the sums of the refreshes'")
    return problems


def view_read_problems(report: dict[str, Any]) -> list[str]:
    """List where a driver report's view-worker run reads are malformed.

    Each refresh and the sequence give `view_run_reads` for every owner in
    `WRITE_OWNERS`, with every `RUN_READ_FIELDS` a non-negative integer, and the
    sequence's counts are the sums of the refreshes'. Only the index readers read
    under `merged_index`: a native report has none, and a reconstructed report's
    `merged_index` counts are its `index_run_reads` summed over the readers. A
    report that names neither mode is not checked here.
    """
    mode = report.get("mode")
    if mode not in ("native", "reconstructed"):
        return []
    problems: list[str] = []

    def counts(place: str, reads: Any) -> dict[tuple[str, str], int] | None:
        found = {}
        for owner in WRITE_OWNERS:
            for field in RUN_READ_FIELDS:
                value = reads.get(owner, {}).get(field) if isinstance(reads, dict) else None
                if isinstance(value, bool) or not isinstance(value, int) or value < 0:
                    problems.append(f"{place}: view_run_reads.{owner}.{field} is {value!r}")
                    return None
                found[(owner, field)] = value
        return found

    total: dict[tuple[str, str], int] = {}
    for item in report.get("refreshes", []):
        place = f"refresh {item.get('refresh')}"
        found = counts(place, item.get("view_run_reads"))
        if found is None:
            continue
        for key, value in found.items():
            total[key] = total.get(key, 0) + value
        for field in RUN_READ_FIELDS:
            index_part = found[("merged_index", field)]
            if mode == "native":
                expected = 0
            else:
                readers = item.get("index_run_reads") or {}
                expected = sum(readers.get(reader, {}).get(field, 0) for reader in INDEX_READERS)
            if index_part != expected:
                problems.append(f"{place}: view_run_reads.merged_index.{field} is {index_part}, "
                                f"expected {expected}")
    summed = counts("sequence", report.get("sequence", {}).get("view_run_reads"))
    if summed is not None and not problems and summed != total:
        problems.append("sequence: view_run_reads are not the sums of the refreshes'")
    return problems


def owner_write_problems(report: dict[str, Any]) -> list[str]:
    """List where a driver report's per-owner writes disagree with its totals.

    Every resource block that carries backend write totals (the sequence and
    each refresh) must also split them by owner, and the owners must add up to
    the totals. A native report must show no merged-index writes: the native
    circuit has no merged index.
    """
    blocks = [("sequence", report.get("sequence", {}).get("resources"))]
    blocks += [(f"refresh {item.get('refresh')}", item.get("resources"))
               for item in report.get("refreshes", [])]
    problems: list[str] = []
    for place, resources in blocks:
        if not isinstance(resources, dict) or "backend_write_operations" not in resources:
            continue  # Validation runs omit counters.
        owners = resources.get("backend_write_by_owner")
        if not isinstance(owners, dict) or set(owners) != set(WRITE_OWNERS):
            problems.append(f"{place}: backend_write_by_owner is missing or incomplete")
            continue
        for field, total_name in (("operations", "backend_write_operations"),
                                  ("bytes", "backend_write_bytes")):
            by_owner = sum(owners[owner][field] for owner in WRITE_OWNERS)
            if by_owner != resources[total_name]:
                problems.append(f"{place}: owners write {by_owner} {field}, "
                                f"total is {resources[total_name]}")
        if report.get("mode") == "native" and any(owners["merged_index"].values()):
            problems.append(f"{place}: a native run reports merged-index writes")
    return problems


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def cgroup_available(root: Path) -> None:
    if not root.resolve().is_relative_to(Path("/sys/fs/cgroup")):
        raise ValueError("cgroup root must be under /sys/fs/cgroup")
    if not (root / "cgroup.controllers").is_file():
        raise ValueError(f"{root} is not a cgroup v2 directory")
    if "memory" not in (root / "cgroup.controllers").read_text().split():
        raise ValueError(f"memory controller is unavailable in {root}")
    if "memory" not in (root / "cgroup.subtree_control").read_text().split():
        raise ValueError(f"memory controller is not enabled for children of {root}")
    if not (root / "cgroup.procs").exists():
        raise ValueError(f"{root} cannot accept processes")
    if not os.access(root, os.W_OK):
        raise PermissionError(f"{root} is not writable")


def configure_cgroup(root: Path, name: str, limit: int) -> Path:
    ancestor = root.resolve()
    while ancestor.is_relative_to(Path("/sys/fs/cgroup")):
        maximum = ancestor / "memory.max"
        if maximum.is_file():
            value = maximum.read_text().strip()
            if value != "max" and int(value) < limit:
                raise ValueError(f"ancestor {ancestor} limits memory below {limit} bytes")
        if ancestor == Path("/sys/fs/cgroup"):
            break
        ancestor = ancestor.parent
    group = root / name
    group.mkdir()
    (group / "memory.max").write_text(str(limit))
    (group / "memory.swap.max").write_text("0")
    if (group / "memory.max").read_text().strip() != str(limit):
        raise RuntimeError("cgroup total memory limit was not applied")
    if (group / "memory.swap.max").read_text().strip() != "0":
        raise RuntimeError("cgroup swap limit was not applied")
    return group


def enter_cgroup(group: Path) -> None:
    (group / "cgroup.procs").write_text(str(os.getpid()))


def verify_membership(group: Path) -> None:
    membership = Path("/proc/self/cgroup").read_text().splitlines()
    expected = "/" + group.relative_to(Path("/sys/fs/cgroup")).as_posix()
    if f"0::{expected}" not in membership:
        raise RuntimeError(f"process is not in expected cgroup {expected}")
    if (group / "memory.swap.max").read_text().strip() != "0":
        raise RuntimeError("swap is enabled in run cgroup")


def private_inputs(spec: dict[str, Any]) -> dict[str, str]:
    """Copy all inputs inside the process cgroup, before driver startup."""
    destination = Path(spec["run_dir"]) / "input"
    destination.mkdir(parents=True)
    copied: dict[str, str] = {}
    for name, raw in spec["inputs"].items():
        source = Path(raw)
        target = destination / f"{name}{source.suffix}"
        shutil.copyfile(source, target)
        expected = spec["input_hashes"][name]
        if sha256(target) != expected:
            raise RuntimeError(f"private copy of {name} differs from recorded input hash")
        copied[name] = str(target)
    refreshes = []
    for number, entry in enumerate(spec["refreshes"]):
        refreshes.append({key: copied[f"refresh_{number}_{key}"] for key in entry})
    refresh_path = destination / "refreshes.json"
    write_json(refresh_path, refreshes)
    copied["refresh_list"] = str(refresh_path)
    return copied


def evict_from_page_cache(paths: list[Path]) -> dict[str, Any]:
    """Drop the page-cache pages of `paths` (Linux `posix_fadvise(DONTNEED)`).

    Each file is first flushed with `fsync`, because dirty pages (the private copies,
    just written) are not dropped. Failures are recorded per file, not raised, so
    that an unsupported filesystem cannot abort a campaign. Where `os.posix_fadvise`
    does not exist (macOS), nothing is done and `evicted` is false.
    """
    if not hasattr(os, "posix_fadvise"):
        return {"evicted": False, "reason": "os.posix_fadvise is unavailable on this platform",
                "files": [], "failures": []}
    evicted: list[str] = []
    failures: list[dict[str, str]] = []
    for path in sorted(set(paths)):
        try:
            with path.open("rb") as handle:
                os.fsync(handle.fileno())
                os.posix_fadvise(handle.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
            evicted.append(str(path))
        except OSError as error:
            failures.append({"path": str(path), "error": str(error)})
    return {"evicted": bool(evicted) and not failures, "reason": None,
            "files": evicted, "failures": failures}


def run_inside_cgroup(spec_path: Path) -> None:
    spec = json.loads(spec_path.read_text())
    group = Path(spec["cgroup"])
    verify_membership(group)
    if (group / "memory.max").read_text().strip() != str(spec["total_memory_bytes"]):
        raise RuntimeError("run cgroup has an unexpected total memory limit")
    copied = private_inputs(spec)
    # Start the driver with neither the sources nor their copies in the page cache.
    write_json(Path(spec["run_dir"]) / "input-eviction.json", evict_from_page_cache(
        [Path(raw) for raw in spec["inputs"].values()] + [Path(raw) for raw in copied.values()]))
    write_json(Path(spec["run_dir"]) / "cgroup-proof.json", {
        "cgroup": str(group), "memory_max": spec["total_memory_bytes"],
        "memory_swap_max": 0, "membership_verified": True,
        "private_input_hashes_verified": True,
    })
    command = driver_command(spec, copied)
    os.execv(command[0], command)


def driver_command(spec: dict[str, Any], inputs: dict[str, str]) -> list[str]:
    args = spec["args"]
    command = [
        spec["executable"], "--r5",
        "--customer", inputs["customer"],
        "--orders", inputs["orders"],
        "--lineitem", inputs["lineitem"],
        "--refresh-list", inputs["refresh_list"],
        "--report", str(Path(spec["run_dir"]) / "driver.json"),
        "--cache-bytes", str(args["cache_bytes"]),
        "--memory-target-bytes", str(args["memory_target_bytes"]),
        "--compaction-timeout-secs", str(args["compaction_timeout_secs"]),
        "--batch", str(args.get("batch", 10000)),
        "--workers", "1", "--runs", "1",
        "--storage-path", str(Path(spec["run_dir"]) / "storage"),
    ]
    if args["no_verify"]:
        command.append("--no-verify")
    if args.get("diagnostics"):
        # Opt-in flush log, compaction log and run contents, written beside the
        # run's driver report.
        command.extend(["--diagnostics", str(Path(spec["run_dir"]) / "diagnostics")])
    if args.get("setup_full_compaction"):
        command.append("--setup-full-compaction")
    if args.get("front_code_byte_keys"):
        command.append("--front-code-byte-keys")
    if args.get("elide_value_headers"):
        command.append("--elide-value-headers")
    return command


def build_executables(destination: Path) -> dict[str, Path]:
    destination.mkdir(parents=True, exist_ok=True)
    cargo_target = Path(os.environ.get("CARGO_TARGET_DIR", EXPERIMENT / "target"))
    if not cargo_target.is_absolute():
        cargo_target = (EXPERIMENT / cargo_target).resolve()
    binaries: dict[str, Path] = {}
    for mode in MODES:
        generate = [str(EXPERIMENT / "generate.sh")]
        if mode == "merged-index":
            generate.append("--q3-merged-index")
        subprocess.run(generate, cwd=EXPERIMENT, check=True)
        build = ["cargo", "+1.96.1", "build", "--release", "--offline", "--locked", "-j2"]
        if mode == "merged-index":
            build.extend(["--features", "merged-index"])
        subprocess.run(build, cwd=EXPERIMENT, check=True)
        source = cargo_target / "release" / "q3-ivm-baseline"
        target = destination / f"q3-ivm-{mode}"
        shutil.copy2(source, target)
        binaries[mode] = target
    return binaries


def qualification(system: str, limit: int | None, result: dict[str, Any],
                  verify: bool = False, enforced: bool = False) -> dict[str, Any]:
    if system != "Linux":
        return {"status": "smoke", "reason": "total memory is not enforced on macOS"}
    if limit is None:
        return {"status": "nonqualifying", "reason": "no cgroup v2 total memory limit"}
    if not enforced:
        return {"status": "nonqualifying", "reason": "cgroup enforcement was not verified"}
    if verify:
        return {"status": "nonqualifying", "reason": "independent verification was enabled"}
    loaded = result.get("load", {}).get("observed_storage_bytes")
    if isinstance(loaded, bool) or not isinstance(loaded, int) or loaded < 0:
        return {"status": "nonqualifying", "reason": "loaded storage size unavailable"}
    if loaded <= limit:
        return {"status": "nonqualifying", "reason": "loaded storage does not exceed total memory"}
    return {"status": "qualifying", "reason":
            "loaded state exceeds enforced total memory; runtime pressure is not established"}


def cgroup_evidence(group: Path, run_dir: Path, limit: int) -> dict[str, Any]:
    """Record kernel counters and prove the child used the configured limit."""
    proof_path = run_dir / "cgroup-proof.json"
    proof = json.loads(proof_path.read_text()) if proof_path.is_file() else None
    memory_max_path = group / "memory.max"
    swap_max_path = group / "memory.swap.max"
    memory_max = memory_max_path.read_text().strip() if memory_max_path.is_file() else None
    swap_max = swap_max_path.read_text().strip() if swap_max_path.is_file() else None
    enforced = proof == {
        "cgroup": str(group), "memory_max": limit, "memory_swap_max": 0,
        "membership_verified": True, "private_input_hashes_verified": True,
    } and memory_max == str(limit) and swap_max == "0"
    counters = {}
    for name in ("memory.peak", "memory.events", "io.stat"):
        path = group / name
        counters[name] = path.read_text().strip() if path.is_file() else None
    return {"enforced": enforced, "proof": proof, "memory_max": memory_max,
            "memory_swap_max": swap_max, "counters": counters}


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("customer", "orders", "lineitem", "refresh-list", "report"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--cache-bytes", type=int, required=True)
    parser.add_argument("--memory-target-bytes", type=int, required=True)
    parser.add_argument("--compaction-timeout-secs", type=float, default=300)
    parser.add_argument("--batch", type=int, default=10000)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--storage-path", type=Path)
    parser.add_argument("--no-verify", action="store_true")
    parser.add_argument("--diagnostics", action="store_true",
                        help="record the flush log, the compaction log and the run contents "
                             "of each run in a diagnostics directory beside its driver report")
    parser.add_argument("--setup-full-compaction", action="store_true",
                        help="fully compact the circuit and the merged index after loading, "
                             "before the refreshes; its cost is reported in the load section")
    parser.add_argument("--front-code-byte-keys", action="store_true",
                        help="front-code byte-string keys in storage files (the merged "
                             "index's key level); native runs have no such keys")
    parser.add_argument("--elide-value-headers", action="store_true",
                        help="drop the redundant 16-byte header of every byte-string value in "
                             "storage files, and rebuild it on read (the merged index's value "
                             "level); native runs have no such values")
    parser.add_argument("--cgroup-root", type=Path)
    parser.add_argument("--total-memory-bytes", type=int)
    args = parser.parse_args(argv)
    if args.runs < 1 or args.batch < 1 or args.cache_bytes < 1 or args.memory_target_bytes < 1:
        parser.error("runs, batch, cache bytes, and memory target bytes must be positive")
    if args.cache_bytes % (1024 * 1024):
        parser.error("cache bytes must be a whole MiB")
    if not math.isfinite(args.compaction_timeout_secs) or args.compaction_timeout_secs <= 0:
        parser.error("compaction timeout must be finite and positive")
    if (args.cgroup_root is None) != (args.total_memory_bytes is None):
        parser.error("--cgroup-root and --total-memory-bytes must be supplied together")
    if args.total_memory_bytes is not None and args.total_memory_bytes < 1:
        parser.error("total memory limit must be positive")
    if args.cgroup_root and platform.system() != "Linux":
        parser.error("cgroup qualification requires Linux")
    return args


def run(argv: list[str]) -> int:
    args = parse_args(argv)
    report = args.report.resolve()
    campaign_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
    root = (args.storage_path or EXPERIMENT / "target" / "r5-measurements" /
            campaign_id).resolve()
    manifest: dict[str, Any] = {
        "status": "failed", "revision": None, "git_status_short": None,
        "platform": platform.platform(),
        "configuration": {"runs": args.runs, "cache_bytes": args.cache_bytes,
                          "batch": args.batch,
                          "memory_target_bytes": args.memory_target_bytes,
                          "compaction_timeout_secs": args.compaction_timeout_secs,
                          "verify": not args.no_verify,
                          "diagnostics": args.diagnostics,
                          "setup_full_compaction": args.setup_full_compaction,
                          "front_code_byte_keys": args.front_code_byte_keys,
                          "elide_value_headers": args.elide_value_headers,
                          "total_memory_bytes": args.total_memory_bytes,
                          "cgroup_root": str(args.cgroup_root) if args.cgroup_root else None},
        "inputs": {}, "executables": {}, "runs": [], "errors": [],
    }
    try:
        revision = subprocess.run(["git", "rev-parse", "HEAD"], cwd=EXPERIMENT,
                                  check=True, capture_output=True, text=True)
        manifest["revision"] = revision.stdout.strip()
        status = subprocess.run(["git", "status", "--short"], cwd=EXPERIMENT,
                                check=True, capture_output=True, text=True)
        manifest["git_status_short"] = status.stdout
        refreshes, refresh_paths = refresh_inputs(args.refresh_list.resolve())
        inputs = {name: Path(getattr(args, name)).resolve()
                  for name in ("customer", "orders", "lineitem")}
        inputs.update(refresh_paths)
        inputs["refresh_list"] = args.refresh_list.resolve()
        for name, path in inputs.items():
            if not path.is_file():
                raise FileNotFoundError(path)
            manifest["inputs"][name] = {"path": str(path), "sha256": sha256(path)}
        if args.cgroup_root:
            cgroup_available(args.cgroup_root)
        binaries = build_executables(EXPERIMENT / "target" / "r5-executables" / campaign_id)
        manifest["executables"] = {mode: {"path": str(path), "sha256": sha256(path)}
                                   for mode, path in binaries.items()}
        for repetition in range(1, args.runs + 1):
            for mode in MODES:
                run_dir = root / f"run-{repetition}-{mode}"
                run_dir.mkdir(parents=True, exist_ok=False)
                record: dict[str, Any] = {"mode": mode, "repetition": repetition,
                                          "status": "failed", "storage_path": str(run_dir / "storage")}
                manifest["runs"].append(record)
                spec = {"run_dir": str(run_dir), "executable": str(binaries[mode]),
                        "refreshes": refreshes, "inputs": {k: str(v) for k, v in inputs.items()},
                        "input_hashes": {k: item["sha256"]
                                         for k, item in manifest["inputs"].items()},
                        "args": {"cache_bytes": args.cache_bytes,
                                 "batch": args.batch,
                                 "memory_target_bytes": args.memory_target_bytes,
                                 "compaction_timeout_secs": args.compaction_timeout_secs,
                                 "no_verify": args.no_verify,
                                 "diagnostics": args.diagnostics,
                                 "setup_full_compaction": args.setup_full_compaction,
                                 "front_code_byte_keys": args.front_code_byte_keys,
                                 "elide_value_headers": args.elide_value_headers}}
                group = None
                if args.cgroup_root:
                    group = configure_cgroup(args.cgroup_root, f"r5-{campaign_id}-{run_dir.name}",
                                             args.total_memory_bytes)
                    spec["cgroup"] = str(group)
                    spec["total_memory_bytes"] = args.total_memory_bytes
                    write_json(run_dir / "spec.json", spec)
                    command = [sys.executable, str(Path(__file__).resolve()),
                               "--internal-run", str(run_dir / "spec.json")]
                    completed = subprocess.run(command, cwd=EXPERIMENT, capture_output=True,
                                               text=True, preexec_fn=lambda: enter_cgroup(group))
                else:
                    command = driver_command(spec, {**spec["inputs"],
                        "refresh_list": str(args.refresh_list.resolve())})
                    completed = subprocess.run(command, cwd=EXPERIMENT,
                                               capture_output=True, text=True)
                record["returncode"] = completed.returncode
                (run_dir / "stdout.log").write_text(completed.stdout)
                (run_dir / "stderr.log").write_text(completed.stderr)
                if group:
                    record["cgroup"] = cgroup_evidence(group, run_dir, args.total_memory_bytes)
                if completed.returncode:
                    raise RuntimeError(f"{mode} run {repetition} exited {completed.returncode}: "
                                       f"{completed.stderr[-2000:]}")
                raw = json.loads((run_dir / "driver.json").read_text())
                record["driver"] = raw
                if raw.get("status") not in (None, "complete"):
                    raise RuntimeError(f"{mode} run {repetition} reported {raw['status']}")
                problems = (owner_write_problems(raw) + index_read_problems(raw)
                            + view_read_problems(raw))
                if problems:
                    raise RuntimeError(f"{mode} run {repetition} report is inconsistent: "
                                       + "; ".join(problems))
                record["qualification"] = qualification(platform.system(),
                    args.total_memory_bytes, raw, verify=not args.no_verify,
                    enforced=record.get("cgroup", {}).get("enforced", False))
                record["status"] = "complete"
                write_json(report, manifest)
        manifest["status"] = "complete"
    except Exception as error:  # Preserve every partial result for a failed campaign.
        manifest["errors"].append(f"{type(error).__name__}: {error}")
    finally:
        try:
            subprocess.run([str(EXPERIMENT / "generate.sh")], cwd=EXPERIMENT, check=True)
        except Exception as error:
            manifest["errors"].append(f"native generation restoration failed: {error}")
            manifest["status"] = "failed"
        write_json(report, manifest)
    return 0 if manifest["status"] == "complete" else 1


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--internal-run":
        run_inside_cgroup(Path(sys.argv[2]))
    else:
        sys.exit(run(sys.argv[1:]))
