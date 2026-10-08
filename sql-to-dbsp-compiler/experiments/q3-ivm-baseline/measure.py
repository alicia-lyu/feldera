#!/usr/bin/env python3
"""Build and compare the two R5 Q3 maintenance executables."""

from __future__ import annotations

import argparse
import hashlib
import json
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
        copied[name] = str(target)
    refreshes = []
    for number, entry in enumerate(spec["refreshes"]):
        refreshes.append({key: copied[f"refresh_{number}_{key}"] for key in entry})
    refresh_path = destination / "refreshes.json"
    write_json(refresh_path, refreshes)
    copied["refresh_list"] = str(refresh_path)
    return copied


def run_inside_cgroup(spec_path: Path) -> None:
    spec = json.loads(spec_path.read_text())
    group = Path(spec["cgroup"])
    verify_membership(group)
    if (group / "memory.max").read_text().strip() != str(spec["total_memory_bytes"]):
        raise RuntimeError("run cgroup has an unexpected total memory limit")
    copied = private_inputs(spec)
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
        "--workers", "1", "--runs", "1",
        "--storage-path", str(Path(spec["run_dir"]) / "storage"),
    ]
    if args["no_verify"]:
        command.append("--no-verify")
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


def qualification(system: str, limit: int | None, result: dict[str, Any]) -> dict[str, Any]:
    if system != "Linux":
        return {"status": "smoke", "reason": "total memory is not enforced on macOS"}
    if limit is None:
        return {"status": "nonqualifying", "reason": "no cgroup v2 total memory limit"}
    settled = result.get("sequence", {}).get("settled_storage_bytes")
    if not isinstance(settled, int):
        return {"status": "nonqualifying", "reason": "settled storage size unavailable"}
    if settled <= limit:
        return {"status": "nonqualifying", "reason": "settled storage does not exceed total memory"}
    return {"status": "qualifying", "reason": "settled storage exceeds enforced total memory"}


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("customer", "orders", "lineitem", "refresh-list", "report"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--cache-bytes", type=int, required=True)
    parser.add_argument("--memory-target-bytes", type=int, required=True)
    parser.add_argument("--compaction-timeout-secs", type=float, default=300)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--storage-path", type=Path)
    parser.add_argument("--no-verify", action="store_true")
    parser.add_argument("--cgroup-root", type=Path)
    parser.add_argument("--total-memory-bytes", type=int)
    args = parser.parse_args(argv)
    if args.runs < 1 or args.cache_bytes < 1 or args.memory_target_bytes < 1:
        parser.error("runs, cache bytes, and memory target bytes must be positive")
    if args.compaction_timeout_secs <= 0:
        parser.error("compaction timeout must be positive")
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
        "status": "failed", "revision": None, "platform": platform.platform(),
        "configuration": {"runs": args.runs, "cache_bytes": args.cache_bytes,
                          "memory_target_bytes": args.memory_target_bytes,
                          "compaction_timeout_secs": args.compaction_timeout_secs,
                          "verify": not args.no_verify,
                          "total_memory_bytes": args.total_memory_bytes,
                          "cgroup_root": str(args.cgroup_root) if args.cgroup_root else None},
        "inputs": {}, "executables": {}, "runs": [], "errors": [],
    }
    try:
        revision = subprocess.run(["git", "rev-parse", "HEAD"], cwd=EXPERIMENT,
                                  check=True, capture_output=True, text=True)
        manifest["revision"] = revision.stdout.strip()
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
                        "args": {"cache_bytes": args.cache_bytes,
                                 "memory_target_bytes": args.memory_target_bytes,
                                 "compaction_timeout_secs": args.compaction_timeout_secs,
                                 "no_verify": args.no_verify}}
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
                if completed.returncode:
                    raise RuntimeError(f"{mode} run {repetition} exited {completed.returncode}: "
                                       f"{completed.stderr[-2000:]}")
                raw = json.loads((run_dir / "driver.json").read_text())
                record["driver"] = raw
                record["qualification"] = qualification(platform.system(),
                    args.total_memory_bytes, raw)
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
