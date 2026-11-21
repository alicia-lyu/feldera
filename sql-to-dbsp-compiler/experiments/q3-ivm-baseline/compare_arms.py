#!/usr/bin/env python3
"""Compare pilot arms, view circuit first, as Markdown ready for MEASUREMENT.md.

Usage: python3 compare_arms.py [--first-refresh N] MANIFEST [MANIFEST ...]

Each MANIFEST is a `measure.py` manifest.json (one arm). Every run of every manifest
becomes one row per phase, over refreshes N and later (default 2: refresh 1 starts from a
cold cache and is left out, as in MEASUREMENT.md). For each row it prints:

1. the view circuit: `ivm_ms` p50 / p95 / p99 per transaction (nearest rank, as
   `summarize.py`), and the worker thread's device reads per transaction;
2. for merged-index runs, the index's per-run seeks, the runs the prefix filter skipped
   (`filtered_prefix_probes`), and the index's device reads (`other/merged_index`), per
   transaction;
3. `base_ms` p50 (amortized over the views an index serves, so it comes last);

then, per run: pressure per refresh, completed merges and the mergers' real time, runs at
the end per spine, the memtable fields (measurement version 18; "—" before), and the
window peak and end RSS. A run whose status is not `complete` is listed, not measured.

Writes by kind come from `spill_kinds.py MANIFEST` on a `--diagnostics` rerun.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys
from typing import Any

from summarize import nearest_rank

MIB = 1 << 20
DASH = "—"


def arm_label(manifest_path: Path) -> str:
    """The arm's name: its directory, without the shared `pilot-` prefix."""
    return manifest_path.parent.name.removeprefix("pilot-")


def per_transaction(total: float, transactions: int) -> float:
    return total / transactions if transactions else 0.0


def read_operations(refresh: dict[str, Any], thread: str, owner: str | None = None) -> int:
    """Device read operations of one thread group, for one read owner or all of them."""
    owners = refresh["run_counts"]["device_reads"]["by_read_owner"].get(thread, {})
    return sum(value["operations"] for name, value in owners.items()
               if owner is None or name == owner)


def phase_row(refreshes: list[dict[str, Any]], merged_index: bool) -> dict[str, Any]:
    """The per-transaction figures of one phase's refreshes."""
    transactions = sum(refresh["transactions"] for refresh in refreshes)
    latency = {part: sorted(value for refresh in refreshes
                            for value in refresh["transaction_latency"][part]["values"])
               for part in ("ivm_ms", "base_ms")}
    row: dict[str, Any] = {
        "transactions": transactions,
        "ivm": tuple(nearest_rank(latency["ivm_ms"], p) for p in (50, 95, 99)),
        "base_p50": nearest_rank(latency["base_ms"], 50),
        "worker_reads": per_transaction(
            sum(read_operations(refresh, "foreground") for refresh in refreshes), transactions),
    }
    if merged_index:
        session = [refresh["index_run_reads"]["session"] for refresh in refreshes]
        row["index_seeks"] = per_transaction(sum(s["seeks"] for s in session), transactions)
        row["skipped_runs"] = per_transaction(
            sum(s.get("filtered_prefix_probes", 0) for s in session), transactions)
        row["index_reads"] = per_transaction(
            sum(read_operations(refresh, "other", "merged_index") for refresh in refreshes),
            transactions)
    return row


def counted_spines(refresh: dict[str, Any]) -> list[dict[str, Any]]:
    """Spines with full counts; transient spines report only their runs."""
    return [spine for spine in refresh["run_counts"]["spines"] if "runs_at_end" in spine]


def memtable_summary(refreshes: list[dict[str, Any]]) -> str:
    spines = [counted_spines(refresh) for refresh in refreshes]
    if not any("memtable_flushes" in spine for group in spines for spine in group):
        return DASH
    flushes = sum(spine.get("memtable_flushes", 0) for group in spines for spine in group)
    if flushes == 0 and not any(spine.get("memtable_entries_at_end") for group in spines
                                for spine in group):
        return "off"
    held = [sum(spine.get("memtable_bytes_at_end", 0) for spine in group) / MIB
            for group in spines]
    copies = [sum(spine.get("memtable_node_copies", 0) for spine in group) for group in spines]
    copy_ms = [sum(spine.get("memtable_copy_ms", 0.0) for spine in group) for group in spines]
    return (f"{flushes} flushes; {min(held):.0f}–{max(held):.0f} MiB held at refresh ends; "
            f"{min(copies):,}–{max(copies):,} node copies, "
            f"{min(copy_ms) / 1e3:.1f}–{max(copy_ms) / 1e3:.1f} s per refresh")


def run_summary(driver: dict[str, Any], refreshes: list[dict[str, Any]]) -> dict[str, str]:
    pressure = " ".join(refresh["memory_pressure"]["after"]["runtime"] for refresh in refreshes)
    merges = sum(spine["completed_merges"] for refresh in refreshes
                 for spine in counted_spines(refresh))
    merge_seconds = sum(spine["merge_real_ms"] for refresh in refreshes
                        for spine in counted_spines(refresh)) / 1e3
    runs = ", ".join(f"{spine['owner']} {spine['runs_at_end']}"
                     for spine in counted_spines(refreshes[-1]))
    sequence = driver.get("sequence", {})
    peak, end = sequence.get("window_peak_rss_bytes"), sequence.get("window_end_rss_bytes")
    rss = f"{peak / MIB:.0f} / {end / MIB:.0f}" if peak and end else DASH
    return {"pressure": pressure, "merges": f"{merges:,} ({merge_seconds:.0f} s)",
            "runs": runs, "memtable": memtable_summary(refreshes), "rss": rss}


def manifest_rows(manifest_path: Path, first_refresh: int) -> tuple[list[str], list[str]]:
    """Markdown rows of one manifest: (phase rows, run rows)."""
    manifest = json.loads(manifest_path.read_text())
    arm = arm_label(manifest_path)
    phase_lines, run_lines = [], []
    for run in manifest["runs"]:
        mode = run["mode"]
        if run.get("status") != "complete":
            run_lines.append(f"| {arm} | {mode} | status `{run.get('status')}`, not measured "
                             f"| | | | |")
            continue
        driver = run["driver"]
        refreshes = [refresh for refresh in driver["refreshes"]
                     if refresh["refresh"] >= first_refresh]
        for phase in ("rf1", "rf2"):
            chosen = [refresh for refresh in refreshes if refresh["phase"] == phase]
            if not chosen:
                continue
            row = phase_row(chosen, mode == "merged-index")
            index = (f"{row['index_seeks']:.1f} | {row['skipped_runs']:.1f} | "
                     f"{row['index_reads']:.2f}" if "index_seeks" in row
                     else f"{DASH} | {DASH} | {DASH}")
            p50, p95, p99 = row["ivm"]
            phase_lines.append(
                f"| {arm} | {mode} | {phase.upper()} | {p50:.2f} / {p95:.2f} / {p99:.2f} "
                f"| {row['worker_reads']:.1f} | {index} | {row['base_p50']:.2f} |")
        summary = run_summary(driver, refreshes)
        run_lines.append(f"| {arm} | {mode} | {summary['pressure']} | {summary['merges']} "
                         f"| {summary['runs']} | {summary['memtable']} | {summary['rss']} |")
    return phase_lines, run_lines


def render(manifest_paths: list[Path], first_refresh: int) -> str:
    phase_lines, run_lines = [], []
    for path in manifest_paths:
        phases, runs = manifest_rows(path, first_refresh)
        phase_lines += phases
        run_lines += runs
    return "\n".join([
        f"Refreshes {first_refresh} and later, per transaction.",
        "",
        "| Arm | Mode | Phase | `ivm_ms` p50 / p95 / p99 | worker device reads "
        "| index per-run seeks | runs skipped by the filter | index device reads "
        "| `base_ms` p50 (amortized) |",
        "| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: |",
        *phase_lines,
        "",
        "| Arm | Mode | Pressure after each refresh | Completed merges (merger real time) "
        "| Runs at the end, per spine | Memtable | Window peak / end RSS (MiB) |",
        "| --- | --- | --- | --- | --- | --- | --- |",
        *run_lines,
    ])


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--first-refresh", type=int, default=2,
                        help="the first refresh counted (default 2)")
    parser.add_argument("manifests", nargs="+", type=Path)
    args = parser.parse_args(argv)
    print(render(args.manifests, args.first_refresh))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
