"""Class the files a diagnostics pilot wrote in its measured window by why they were written.

Usage: python3 spill_kinds.py MANIFEST [MANIFEST ...]

Each manifest is a `measure.py --diagnostics` campaign report. For every run, every file in
`flush-log.jsonl` whose flush finished inside the window (`summary.json`) is classed by the
`compaction-log.jsonl` record that names it as its output:

- `background_compaction` or `requested_compaction`: a compaction output;
- `insert_spill`: an inserted run that the spine wrote to storage at once;
- `memtable_flush` or `memtable_checkpoint_flush`: a sealed memtable;
- `step_built`, for a file no record names: a file built straight to storage during a step. That
  is an inserted run that was a file from the start, or a temporary run passed between
  operators; the logs cannot tell those two apart.

Files are grouped by their writer's owner into the view (every owner but `base_relation`) and
the base tables, and counted per transaction of the window. A memtable flush also reports its
mean seal time, from its compaction record.
"""
from __future__ import annotations

import collections
from dataclasses import dataclass, field
import json
from pathlib import Path
import sys
from typing import Any, Iterable

STEP_BUILT = "step_built"
BASE_OWNER = "base_relation"
MEMTABLE_KINDS = ("memtable_flush", "memtable_checkpoint_flush")


@dataclass
class KindTotals:
    """The files of one write kind and owner group."""
    files: int = 0
    writes: int = 0
    bytes: int = 0
    seal_micros: list[int] = field(default_factory=list)


def read_jsonl(path: Path) -> list[dict[str, Any]]:
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def owner_group(owner: str) -> str:
    return "base" if owner == BASE_OWNER else "view"


def classify(flushes: Iterable[dict[str, Any]], records: Iterable[dict[str, Any]],
             window: dict[str, int]) -> dict[tuple[str, str], KindTotals]:
    """Totals per (owner group, kind) of the files whose flushes finished inside the window."""
    start, end = window["started_micros"], window["finished_micros"]
    named = {}
    for record in records:
        if record.get("output_file"):
            named[record["output_file"].lstrip("/")] = record
    per_file: dict[str, list[Any]] = collections.defaultdict(lambda: [0, 0, None])
    for flush in flushes:
        if start <= flush["finished_micros"] <= end:
            entry = per_file[flush["file"].lstrip("/")]
            entry[0] += 1
            entry[1] += flush["bytes"]
            entry[2] = flush["owner"]
    totals: dict[tuple[str, str], KindTotals] = collections.defaultdict(KindTotals)
    for name, (writes, size, owner) in per_file.items():
        record = named.get(name)
        kind = record["kind"] if record else STEP_BUILT
        total = totals[(owner_group(owner), kind)]
        total.files += 1
        total.writes += writes
        total.bytes += size
        if kind in MEMTABLE_KINDS:
            total.seal_micros.append(record["finished_micros"] - record["started_micros"])
    return dict(totals)


def report_lines(mode: str, totals: dict[tuple[str, str], KindTotals],
                 transactions: int) -> list[str]:
    lines = [f"{mode}: {sum(t.files for t in totals.values())} files, "
             f"{transactions} transactions in the window"]
    for group, kind in sorted(totals):
        total = totals[(group, kind)]
        line = (f"  {group:4s} {kind:26s} files/txn {total.files / transactions:6.2f}  "
                f"writes/txn {total.writes / transactions:6.2f}  "
                f"KiB/txn {total.bytes / transactions / 1024:7.1f}  "
                f"mean KiB/file {total.bytes / max(total.files, 1) / 1024:8.1f}")
        if total.seal_micros:
            mean_ms = sum(total.seal_micros) / len(total.seal_micros) / 1000
            line += f"  mean seal ms {mean_ms:7.2f}"
        lines.append(line)
    return lines


def manifest_lines(manifest_path: Path) -> list[str]:
    manifest = json.loads(manifest_path.read_text())
    lines = []
    for run in manifest["runs"]:
        directory = (manifest_path.parent / "runs" / f"run-{run.get('repetition', 1)}-{run['mode']}"
                     / "diagnostics")
        if not directory.is_dir():
            lines.append(f"{run['mode']}: no diagnostics at {directory}")
            continue
        window = json.loads((directory / "summary.json").read_text())["window"]
        totals = classify(read_jsonl(directory / "flush-log.jsonl"),
                          read_jsonl(directory / "compaction-log.jsonl"), window)
        transactions = sum(refresh["transactions"] for refresh in run["driver"]["refreshes"])
        lines.extend(report_lines(run["mode"], totals, transactions))
    return lines


def main(argv: list[str]) -> int:
    if not argv:
        print(__doc__.splitlines()[2], file=sys.stderr)
        return 2
    for path in argv:
        print(f"\n{path}")
        print("\n".join(manifest_lines(Path(path))))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
