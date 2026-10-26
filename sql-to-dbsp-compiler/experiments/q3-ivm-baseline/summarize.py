#!/usr/bin/env python3
"""Summarize `measure.py` manifests: median, minimum and maximum over repetitions.

Each manifest is the `--report` file of one `measure.py` campaign (one
workload). For each workload and mode (`native`, `merged-index`) the script
reports, over the repetitions:

- `sequence_wall_ms` (`sequence.end_to_end_ms`), `sequence_cpu_ms`
  (`sequence.resources.cpu_microseconds` / 1000);
- the sums over the refreshes of `foreground_ms` and the measurement-version-4 phases:
  `base_ms` (the base circuit plus the merged index's upkeep), `index_ms` (that upkeep, a
  sub-phase of `base_ms` that is never added to it again), `forward_ms` (reading the base net
  delta and feeding the view circuit), `q3_ms` (the view circuit alone) and `ivm_ms` (equal to
  `q3_ms`, the IVM figure). Base and IVM are always reported side by side;
- `sequence.resources.backend_write_by_owner`: write operations and bytes of
  `query`, `base_relation` and `merged_index`, plus the totals
  `backend_write_operations` and `backend_write_bytes`;
- `sequence.observed_storage_bytes`, and `sequence.resources.peak_rss_bytes`
  (the process-lifetime high-water mark, which includes loading);
- the maintenance-window memory evidence, which is evidence of memory use and not proof
  of memory pressure: `window_peak_rss_bytes` and `window_end_rss_bytes`
  (`sequence.window_peak_rss_bytes`, `sequence.window_end_rss_bytes`; `null` is accepted only
  when the report's `os` is not `linux`, and then the two metrics are left out), and, when
  the driver ran in a cgroup v2 group, the window deltas of `sequence.cgroup` that the kernel
  provides (`cgroup_delta_*`: `pgscan`, `pgsteal`, the workingset refaults, the memory events
  and the pressure stall totals) and the cgroup's window peak (`cgroup_window_peak_bytes` if the
  reset worked) and the maximum sampled `memory.current`.

It writes `summary.json` and `summary.md` to `--out`. It exits nonzero and
writes nothing if a campaign or run did not complete, a driver report is not
measurement version 4, a field is missing or not a finite non-negative number, a
mode has no runs, or the modes' repetitions differ. It never substitutes a zero for a missing value.
"""

from __future__ import annotations

import argparse
import math
from pathlib import Path
import json
import statistics
import sys
from typing import Any

from measure import MODES, WRITE_OWNERS, owner_write_problems

MEASUREMENT_VERSION = 4
REFRESH_SUMS = ("foreground_ms", "base_ms", "index_ms", "forward_ms", "q3_ms", "ivm_ms")
STATISTICS = ("median", "min", "max")
# `memory.stat` counters and `memory.events` entries reported as `cgroup_delta_*` metrics when
# the window's cgroup delta has them.
CGROUP_STAT_DELTAS = ("pgscan", "pgsteal", "workingset_refault", "workingset_refault_anon",
                      "workingset_refault_file")
CGROUP_EVENT_DELTAS = ("high", "max", "oom", "oom_kill")


class SummaryError(ValueError):
    """A manifest cannot be summarized without guessing."""


def number(container: Any, path: tuple[str, ...], where: str) -> float:
    """Return the finite non-negative number at `path` in `container`, or fail naming it."""
    value = container
    for step in path:
        if not isinstance(value, dict) or step not in value:
            raise SummaryError(f"{where}: missing field {'.'.join(path)}")
        value = value[step]
    if isinstance(value, bool) or not isinstance(value, (int, float)) \
            or not math.isfinite(value) or value < 0:
        raise SummaryError(f"{where}: field {'.'.join(path)} is {value!r}, "
                           "not a finite non-negative number")
    return value


def run_metrics(run: dict[str, Any], where: str) -> dict[str, float]:
    """Extract one run's metrics from its driver report."""
    driver = run.get("driver")
    if not isinstance(driver, dict):
        raise SummaryError(f"{where}: no driver report")
    if driver.get("measurement_version") != MEASUREMENT_VERSION:
        # Version 2 timed base-table maintenance inside q3_ms, and version 3 booked the merged
        # index in combined_maintenance_ms instead of base_ms, so their sums are not comparable.
        raise SummaryError(f"{where}: measurement_version is "
                           f"{driver.get('measurement_version')!r}, not {MEASUREMENT_VERSION}")
    problems = owner_write_problems(driver)
    if problems:
        raise SummaryError(f"{where}: inconsistent write attribution: " + "; ".join(problems))
    metrics = {
        "sequence_wall_ms": number(driver, ("sequence", "end_to_end_ms"), where),
        "sequence_cpu_ms": number(driver, ("sequence", "resources", "cpu_microseconds"),
                                  where) / 1000,
    }
    refreshes = driver.get("refreshes")
    if not isinstance(refreshes, list) or not refreshes:
        raise SummaryError(f"{where}: no refreshes in the driver report")
    for field in REFRESH_SUMS:
        metrics[f"refresh_{field}"] = sum(
            number(refresh, (field,), f"{where} refresh {index}")
            for index, refresh in enumerate(refreshes, start=1))
    resources = ("sequence", "resources")
    for owner in WRITE_OWNERS:
        for field in ("operations", "bytes"):
            metrics[f"write_{field}_{owner}"] = number(
                driver, (*resources, "backend_write_by_owner", owner, field), where)
    metrics["write_operations_total"] = number(
        driver, (*resources, "backend_write_operations"), where)
    metrics["write_bytes_total"] = number(driver, (*resources, "backend_write_bytes"), where)
    metrics["observed_storage_bytes"] = number(driver, ("sequence", "observed_storage_bytes"), where)
    metrics["peak_rss_bytes"] = number(driver, (*resources, "peak_rss_bytes"), where)
    metrics.update(window_memory_metrics(driver, where))
    return metrics


def window_memory_metrics(driver: dict[str, Any], where: str) -> dict[str, float]:
    """The maintenance-window memory metrics of one driver report.

    The RSS figures are required on Linux and left out elsewhere. The cgroup figures are
    taken from whatever the report's `sequence.cgroup` provides; a missing cgroup or a
    missing kernel counter contributes no metric, and a present one must be valid.
    """
    operating_system = driver.get("os")
    if not isinstance(operating_system, str):
        raise SummaryError(f"{where}: the report does not name its os")
    sequence = driver.get("sequence", {})
    metrics: dict[str, float] = {}
    for field in ("window_peak_rss_bytes", "window_end_rss_bytes"):
        if operating_system != "linux" and sequence.get(field) is None:
            continue
        metrics[field] = number(driver, ("sequence", field), where)
    cgroup = sequence.get("cgroup")
    if cgroup is None:
        return metrics
    if not isinstance(cgroup, dict):
        raise SummaryError(f"{where}: sequence.cgroup is {cgroup!r}, not an object")
    delta = cgroup.get("delta") or {}
    for name in CGROUP_STAT_DELTAS:
        if (delta.get("memory_stat") or {}).get(name) is not None:
            metrics[f"cgroup_delta_{name}"] = number(
                cgroup, ("delta", "memory_stat", name), where + " sequence.cgroup")
    for name in CGROUP_EVENT_DELTAS:
        if (delta.get("memory_events") or {}).get(name) is not None:
            metrics[f"cgroup_delta_memory_events_{name}"] = number(
                cgroup, ("delta", "memory_events", name), where + " sequence.cgroup")
    for kind in ("some", "full"):
        if (delta.get("memory_pressure_total_microseconds") or {}).get(kind) is not None:
            metrics[f"cgroup_delta_pressure_{kind}_microseconds"] = number(
                cgroup, ("delta", "memory_pressure_total_microseconds", kind),
                where + " sequence.cgroup")
    peak = cgroup.get("peak") or {}
    for field, name in (("window_peak_bytes", "cgroup_window_peak_bytes"),
                        ("max_sampled_memory_current_bytes",
                         "cgroup_max_sampled_memory_current_bytes")):
        if peak.get(field) is not None:
            metrics[name] = number(cgroup, ("peak", field), where + " sequence.cgroup")
    return metrics


def summarize_values(values: list[float]) -> dict[str, float]:
    return {"median": statistics.median(values), "min": min(values), "max": max(values)}


def summarize_manifest(manifest: dict[str, Any], label: str) -> dict[str, Any]:
    """Summarize one campaign's manifest; the modes must have the same repetitions."""
    if manifest.get("status") != "complete" or manifest.get("errors"):
        raise SummaryError(f"{label}: campaign status is {manifest.get('status')!r}, "
                           f"errors {manifest.get('errors')!r}")
    runs = manifest.get("runs")
    if not isinstance(runs, list):
        raise SummaryError(f"{label}: manifest has no runs list")
    by_mode: dict[str, dict[int, dict[str, float]]] = {mode: {} for mode in MODES}
    for run in runs:
        mode, repetition = run.get("mode"), run.get("repetition")
        where = f"{label} {mode} run {repetition}"
        if mode not in by_mode:
            raise SummaryError(f"{where}: unknown mode")
        if run.get("status") != "complete" or run.get("returncode") not in (0, None):
            raise SummaryError(f"{where}: run status {run.get('status')!r}, "
                               f"return code {run.get('returncode')!r}")
        if not isinstance(repetition, int) or repetition in by_mode[mode]:
            raise SummaryError(f"{where}: repetition is missing or repeated")
        by_mode[mode][repetition] = run_metrics(run, where)
    names_by_run = {tuple(metrics) for found in by_mode.values() for metrics in found.values()}
    if len(names_by_run) > 1:
        raise SummaryError(f"{label}: runs report different sets of metrics (for example, a "
                           "kernel without some cgroup counters, or a non-Linux run among "
                           "Linux runs)")
    repetitions = {mode: sorted(found) for mode, found in by_mode.items()}
    if any(not found for found in repetitions.values()) \
            or len({tuple(found) for found in repetitions.values()}) != 1:
        raise SummaryError(f"{label}: modes have different repetitions: {repetitions}")
    modes: dict[str, Any] = {}
    for mode, found in by_mode.items():
        names = next(iter(found.values())).keys()
        modes[mode] = {
            "runs": len(found),
            "metrics": {name: summarize_values([metrics[name] for metrics in found.values()])
                        for name in names},
        }
    return {"platform": manifest.get("platform"), "revision": manifest.get("revision"),
            "configuration": manifest.get("configuration"), "modes": modes}


def workload_label(path: Path) -> str:
    return path.resolve().parent.name


def summarize(paths: list[Path]) -> dict[str, Any]:
    workloads: dict[str, Any] = {}
    for path in paths:
        label = workload_label(path)
        if label in workloads:
            raise SummaryError(f"two manifests are named {label!r} (by directory); "
                               "give each campaign its own directory")
        try:
            manifest = json.loads(path.read_text())
        except (OSError, json.JSONDecodeError) as error:
            raise SummaryError(f"{path}: cannot read manifest: {error}") from error
        if not isinstance(manifest, dict):
            raise SummaryError(f"{path}: manifest is not a JSON object")
        workloads[label] = {"source": str(path), **summarize_manifest(manifest, label)}
    return {"workloads": workloads}


def format_value(value: float) -> str:
    return f"{int(value):,}" if float(value).is_integer() else f"{value:,.3f}"


def render_markdown(summary: dict[str, Any]) -> str:
    lines = ["# R5 summary", "",
             "Median, minimum and maximum over the repetitions of each mode. "
             "Times are milliseconds; sizes are bytes.", ""]
    for label, workload in summary["workloads"].items():
        runs = {mode: workload["modes"][mode]["runs"] for mode in MODES}
        lines += [f"## {label}", "",
                  f"Source: `{workload['source']}`. Runs per mode: "
                  + ", ".join(f"{mode} {count}" for mode, count in runs.items()) + ".", ""]
        lines.append("| Metric | " + " | ".join(f"{mode} {stat}" for mode in MODES
                                                 for stat in STATISTICS) + " |")
        lines.append("| --- | " + " | ".join("---:" for _ in MODES for _ in STATISTICS) + " |")
        for name in workload["modes"][MODES[0]]["metrics"]:
            cells = [format_value(workload["modes"][mode]["metrics"][name][stat])
                     for mode in MODES for stat in STATISTICS]
            lines.append(f"| `{name}` | " + " | ".join(cells) + " |")
        lines.append("")
    return "\n".join(lines)


def write_outputs(summary: dict[str, Any], out: Path) -> None:
    out.mkdir(parents=True, exist_ok=True)
    (out / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
    (out / "summary.md").write_text(render_markdown(summary))


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("manifests", type=Path, nargs="+",
                        help="measure.py --report files; each one's directory names its workload")
    parser.add_argument("--out", type=Path, required=True,
                        help="directory for summary.json and summary.md")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    try:
        summary = summarize(args.manifests)
    except SummaryError as error:
        print(f"summarize: {error}", file=sys.stderr)
        return 1
    write_outputs(summary, args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
