#!/usr/bin/env python3
"""Measures Q14's refreshes from images under a cgroup memory limit, as the Q3 harness does.

Two steps, each its own command:

* ``build``: builds both release drivers (native and ``merged-index``) and runs their load
  programs at once (``--write-image``), with no cgroup limit and no memory target. Loading is
  not measured. It writes ``<storage>/images/{native,reconstructed}`` and
  ``<storage>/images/images.json``.
* ``run``: runs each mode's refresh program ``--runs`` times, alternating which mode goes first.
  Each run copies its mode's image (outside the cgroup), drops the inputs and the copy from the
  page cache, and starts the driver in a new cgroup v2 child with ``memory.max`` set and no swap.
  The window starts with an empty buffer cache. The copy is deleted after the run.

The runner must sit in a cgroup that shares a user-writable ancestor with the run cgroups (see
``CLAUDE.md``, "Measuring from images"). Linux only.
"""
import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
MODES = {"native": [], "reconstructed": ["--features", "merged-index"]}
TOOLCHAIN = "+1.96.1"
GIB = 1 << 30
MIB = 1 << 20


def build_drivers(out: Path) -> dict:
    """Builds both release drivers and copies them into ``out``, so a run never sees a rebuild."""
    out.mkdir(parents=True, exist_ok=True)
    drivers = {}
    for mode, features in MODES.items():
        subprocess.run(["cargo", TOOLCHAIN, "build", "--release", *features], cwd=HERE, check=True)
        driver = out / f"q14-{mode}"
        shutil.copy2(HERE / "target/release/q14-ivm-baseline", driver)
        drivers[mode] = str(driver)
    return drivers


def storage_flags(args) -> list:
    return ["--cache-bytes", str(args.cache_bytes), "--front-code-byte-keys",
            "--elide-value-headers"]


def build(args) -> None:
    images = args.storage / "images"
    if images.exists():
        sys.exit(f"{images} already exists")
    drivers = build_drivers(images / "bin")
    tables = {table: str(args.inputs / f"{table}.tbl") for table in ["part", "partsupp", "lineitem"]}
    loads = {}
    for mode, driver in drivers.items():
        command = [driver, "--part", tables["part"], "--partsupp", tables["partsupp"],
                   "--lineitem", tables["lineitem"], "--write-image", str(images / mode),
                   "--batch", str(args.batch), "--no-verify", *storage_flags(args)]
        log = open(images / f"{mode}.log", "w")
        loads[mode] = (subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT), log)
    failed = []
    for mode, (process, log) in loads.items():
        if process.wait() != 0:
            failed.append(mode)
        log.close()
    if failed:
        sys.exit(f"loading failed for {failed}; see {images}/<mode>.log")
    manifest = {"drivers": drivers, "inputs": tables, "batch": args.batch,
                "cache_bytes": args.cache_bytes,
                "images": {mode: {"size_bytes": tree_bytes(images / mode),
                                  "load": json.loads((images / mode / "image.json").read_text())["load"]}
                           for mode in MODES}}
    (images / "images.json").write_text(json.dumps(manifest, indent=2))
    print(json.dumps(manifest["images"], indent=2))


def tree_bytes(root: Path) -> int:
    return sum(path.stat().st_size for path in root.rglob("*") if path.is_file())


def evict_from_page_cache(paths) -> dict:
    """Drops the page-cache pages of every file under ``paths`` (``posix_fadvise(DONTNEED)``)."""
    files, failures = 0, []
    for root in paths:
        for path in [root] if root.is_file() else [p for p in root.rglob("*") if p.is_file()]:
            try:
                with open(path, "rb") as handle:
                    os.fsync(handle.fileno())
                    os.posix_fadvise(handle.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
                files += 1
            except OSError as error:
                failures.append(f"{path}: {error}")
    return {"evicted": not failures, "files": files, "failures": failures}


class Cgroup:
    """A cgroup v2 child with ``memory.max`` set and swap off."""

    def __init__(self, root: Path, name: str, memory_max: int):
        self.path = root / name
        self.path.mkdir()
        for file, value in [("memory.max", memory_max), ("memory.swap.max", 0)]:
            (self.path / file).write_text(str(value))
            if (self.path / file).read_text().strip() != str(value):
                raise RuntimeError(f"{self.path / file} did not take {value}")

    def read(self, file: str) -> dict:
        return dict(line.split() for line in (self.path / file).read_text().splitlines())

    def enter(self) -> None:
        (self.path / "cgroup.procs").write_text(str(os.getpid()))

    def remove(self) -> None:
        self.path.rmdir()


class Sampler(threading.Thread):
    """Samples a cgroup's ``memory.current`` (kernel 5.15 has no ``memory.peak``)."""

    def __init__(self, cgroup: Cgroup):
        super().__init__(daemon=True)
        self.file = cgroup.path / "memory.current"
        self.peak = 0
        self.done = threading.Event()

    def run(self) -> None:
        while not self.done.wait(0.05):
            try:
                self.peak = max(self.peak, int(self.file.read_text()))
            except OSError:
                return


def run_one(args, images: dict, mode: str, name: str, out: Path) -> dict:
    run_dir = out / name
    run_dir.mkdir()
    copy = args.storage / "runs" / out.name / name
    copy.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(["cp", "-a", str(args.storage / "images" / mode), str(copy)], check=True)
    inputs = [Path(path) for path in images["inputs"].values()] + [args.refresh_list.parent]
    evicted = evict_from_page_cache(inputs + [copy])

    cgroup = Cgroup(args.cgroup_root, f"{out.name}-{name}", args.memory_max)
    command = ["/usr/bin/time", "-f", "peak_rss_kb=%M wall_s=%e", "-o", str(run_dir / "time.txt"),
               images["drivers"][mode], "--from-image", str(copy),
               "--refresh-list", str(args.refresh_list), "--report", str(run_dir / "report.json"),
               "--memory-target-bytes", str(args.memory_target_bytes), "--no-verify",
               "--session-pass", "seek", *storage_flags(args)]
    sampler = Sampler(cgroup)
    sampler.start()
    with open(run_dir / "stdout.txt", "w") as log:
        status = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT,
                                preexec_fn=cgroup.enter).returncode
    sampler.done.set()
    sampler.join()
    record = {
        "name": name, "mode": mode, "status": status,
        "cgroup": {"path": str(cgroup.path), "memory_max": args.memory_max, "swap_max": 0,
                   "sampled_peak_memory_current": sampler.peak,
                   "events": cgroup.read("memory.events"),
                   "stat": {key: int(value) for key, value in cgroup.read("memory.stat").items()
                            if key in ("anon", "file", "pgscan", "pgsteal", "pgmajfault",
                                       "workingset_refault_file")}},
        "inputs_evicted": evicted,
        "image_bytes": images["images"][mode]["size_bytes"],
    }
    record["qualifying"] = (status == 0 and evicted["evicted"]
                            and record["image_bytes"] > args.memory_max)
    cgroup.remove()
    if status == 0:
        shutil.rmtree(copy)
    (run_dir / "run.json").write_text(json.dumps(record, indent=2))
    print(f"{name}: status={status} qualifying={record['qualifying']} "
          f"cgroup_peak={sampler.peak / MIB:.0f}MiB oom={record['cgroup']['events'].get('oom_kill')} "
          f"{(run_dir / 'time.txt').read_text().strip()}", flush=True)
    if status != 0:
        sys.exit(f"{name} failed; its copy is kept at {copy}")
    return record


def run(args) -> None:
    images = json.loads((args.storage / "images" / "images.json").read_text())
    out = args.out / time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    out.mkdir(parents=True)
    configuration = {key: str(value) if isinstance(value, Path) else value
                     for key, value in vars(args).items() if key != "func"}
    runs = []
    for rep in range(1, args.runs + 1):
        modes = list(MODES) if rep % 2 else list(reversed(MODES))
        for mode in modes:
            runs.append(run_one(args, images, mode, f"run-{rep}-{mode}", out))
            (out / "manifest.json").write_text(json.dumps(
                {"configuration": configuration, "images": images, "runs": runs}, indent=2))
    summarize(out)


def summarize(out: Path) -> None:
    """Prints, per mode, each refresh's median over runs, and each run's window evidence."""
    by_mode = {}
    for run_dir in sorted(out.glob("run-*")):
        record = json.loads((run_dir / "run.json").read_text())
        report = json.loads((run_dir / "report.json").read_text())
        by_mode.setdefault(record["mode"], []).append((record, report))
    for mode, runs in by_mode.items():
        print(f"== {mode}: {len(runs)} runs")
        for record, report in runs:
            print(f"  {record['name']}: qualifying={record['qualifying']} "
                  f"restore={report['image']['restore_ms']:.0f}ms "
                  f"window_peak_rss={(report['window_peak_rss_bytes'] or 0) / MIB:.0f}MiB "
                  f"cgroup_peak={record['cgroup']['sampled_peak_memory_current'] / MIB:.0f}MiB "
                  f"pgmajfault={record['cgroup']['stat'].get('pgmajfault')} "
                  f"pressure_end={report['refreshes'][-1]['memory_pressure']['after']}")
        for index, refresh in enumerate(runs[0][1]["refreshes"]):
            def median(field):
                return statistics.median(r["refreshes"][index][field] for _, r in runs)

            def reads(r):
                return sum(owner["operations"] for thread in r["refreshes"][index]["device_reads"].values()
                           for owner in thread.values())
            print(f"    refresh {index + 1} {refresh['phase']} rows={refresh['rows']}: "
                  + " ".join(f"{field[:-3]}={median(field):.1f}" for field in
                             ["total_ms", "base_ms", "index_ms", "ivm_ms", "delta_ms", "q14_ms",
                              "base_eviction_ms"])
                  + f" reads={statistics.median(reads(r) for _, r in runs):.0f}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(required=True)
    for name, func in [("build", build), ("run", run)]:
        command = commands.add_parser(name)
        command.set_defaults(func=func)
        command.add_argument("--storage", type=Path, required=True,
                             help="a local ext4 or xfs directory for the images and run copies")
        command.add_argument("--cache-bytes", type=int, default=256 * MIB)
    build_command, run_command = commands.choices["build"], commands.choices["run"]
    build_command.add_argument("--inputs", type=Path, required=True,
                               help="the directory of dbgen's part, partsupp and lineitem .tbl")
    build_command.add_argument("--batch", type=int, default=1_000_000)
    run_command.add_argument("--refresh-list", type=Path, required=True)
    run_command.add_argument("--out", type=Path, required=True, help="where reports go")
    run_command.add_argument("--runs", type=int, default=3)
    run_command.add_argument("--cgroup-root", type=Path, default=Path("/sys/fs/cgroup/q14"))
    run_command.add_argument("--memory-max", type=int, default=GIB)
    run_command.add_argument("--memory-target-bytes", type=int, default=512 * MIB)
    summary = commands.add_parser("summarize")
    summary.add_argument("out", type=Path)
    summary.set_defaults(func=lambda args: summarize(args.out))
    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
