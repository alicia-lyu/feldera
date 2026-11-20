#!/usr/bin/env python3
"""Build and compare the two R5 Q3 maintenance executables."""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone
from typing import Any


EXPERIMENT = Path(__file__).resolve().parent
MODES = ("native", "merged-index")
# The owners of `Component::ALL` (measurement version 11): the components that write storage and
# whose reads are counted. `requested_slice` (the joins' reads of the readers' requested slices of
# the index, reconstructed mode) and `partner_trace` (the joins' reads of their traces, native
# mode) are read owners only, so no write is ever counted under them.
WRITE_OWNERS = ("query", "base_relation", "merged_index", "requested_slice", "partner_trace")
READ_ONLY_OWNERS = ("requested_slice", "partner_trace")
# The merged index's readers, and the run counts reported for each (measurement version 8). The
# view phase reads the index only through its scan session, so there is one reader. The decodes
# are file runs'; the memory reads are in-memory runs', kept apart from them.
INDEX_READERS = ("session",)
# `filtered_probes`: exact probes that a file run's filters (key range, then Roaring bitmap or
# Bloom filter) answer with no seek.
RUN_READ_FIELDS = ("seeks", "filtered_probes", "filtered_prefix_probes", "key_decodes",
                   "value_decodes", "weight_decodes",
                   "memory_key_reads", "memory_value_reads", "memory_weight_reads")
# The scan session's moves per transaction (`SessionCounts`, measurement version 8).
SESSION_COUNT_FIELDS = ("groups", "cursor_opens", "logical_seeks", "logical_steps", "positions",
                        "payload_decodes_old", "payload_decodes_dlt", "cancelled")
# How the reconstructed build's scan session passes untouched groups (`--session-pass`). The
# step pass was retired on 2026-11-18 (user decisions): a small transaction walks half the index
# under it. The driver keeps `--session-pass step`; the runner no longer offers it.
SESSION_PASSES = ("seek",)
# The version of the driver report that this runner and `summarize.py` understand. Version 12
# adds `block_expansions` (the blocks rebuilt from their disk encoding on cache misses). Version
# 15 writes Bloom filters storage-resident (`configuration.filter_residency`) and adds each
# refresh's `run_counts` (runs per spine after each transaction, merges, device reads by owner).
# 16 evicts the base tables' blocks from the cache after each base transaction (`base_eviction_ms`,
# `base_evicted_bytes`, `configuration.evict_base_after_transaction`).
MEASUREMENT_VERSION = 18
# The fields of `BlockExpansions` (measurement version 12), by encoding. Prefix truncation is
# `--front-code-byte-keys` (the `front_code_byte_keys` configuration); elision is
# `--elide-value-headers`.
PREFIX_TRUNCATED_EXPANSION_FIELDS = ("prefix_truncated_blocks", "prefix_truncated_encoded_bytes",
                                     "prefix_truncated_expanded_bytes")
ELIDED_EXPANSION_FIELDS = ("elided_blocks", "elided_encoded_bytes", "elided_expanded_bytes")
BLOCK_EXPANSION_FIELDS = PREFIX_TRUNCATED_EXPANSION_FIELDS + ELIDED_EXPANSION_FIELDS
# The threads whose expansions a report gives: the view worker per owner, the driver's thread
# per owner (reconstructed mode), and base maintenance's reads of the index (one count,
# reconstructed mode).
EXPANSION_THREADS = ("view_worker", "driver", "index_upkeep")
# The values of `--device-label`.
DEVICE_LABELS = ("ssd", "hdd")
# The storage device block of the manifest (`storage_device`); every field is `null` when the
# device is not resolved.
STORAGE_DEVICE_FIELDS = ("path", "device", "filesystem", "logical_sector_bytes", "rotational",
                         "model", "label")


# An image is the loaded state of one mode with one storage format. The refresh program refuses an
# image whose settings differ from its own, so each mode has an image per prefix-truncation arm:
# arm A with --front-code-byte-keys, arm B without. Arm B was retired on 2026-11-18 (user
# decision), so only arm A's images are built; `image_name` still names B for older directories.
IMAGE_ARMS = (("A", True),)
IMAGES_MANIFEST = "images.json"


def image_name(mode: str, front_code_byte_keys: bool) -> str:
    """The directory name of the image of `mode` (`native` or `merged-index`) for an arm."""
    return f"{mode}-{'A' if front_code_byte_keys else 'B'}"


def tree_size(root: Path) -> int:
    """The total length of the regular files under `root`."""
    return sum(path.stat().st_size for path in root.rglob("*") if path.is_file())


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def refresh_inputs(path: Path) -> tuple[list[dict[str, Any]], dict[str, Path]]:
    """The refreshes of a refresh list with their paths resolved, and every input file. A
    refresh may carry `transaction_orders` (`prepare_inputs.py --transaction-rows`): the
    Order count of each consecutive transaction it is cut into, which must cover its rows."""
    entries = json.loads(path.read_text())
    if not isinstance(entries, list):
        raise ValueError("refresh list must be a JSON array")
    resolved: list[dict[str, Any]] = []
    inputs: dict[str, Path] = {}
    for number, entry in enumerate(entries):
        if not isinstance(entry, dict) or set(entry) - {"transaction_orders"} not in (
            {"rf1_orders", "rf1_lines"}, {"rf2"}
        ):
            raise ValueError(f"refresh {number} must contain RF1 paths or one RF2 path")
        item: dict[str, Any] = {}
        if "transaction_orders" in entry:
            counts = entry["transaction_orders"]
            if not isinstance(counts, list) or not counts or not all(
                    isinstance(count, int) and not isinstance(count, bool) and count > 0
                    for count in counts):
                raise ValueError(f"refresh {number} transaction_orders must be a non-empty "
                                 "list of positive integers")
            item["transaction_orders"] = counts
        for field, raw in entry.items():
            if field == "transaction_orders":
                continue
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


def counter_problems(report: dict[str, Any], key: str, fields: tuple[str, ...]) -> list[str]:
    """List where a driver report's per-refresh merged-index counters `key` are malformed.

    A reconstructed report gives, for each refresh and for the sequence, every one of
    `fields` as a non-negative integer, and the sequence's counts are the sums of the
    refreshes'. A native report has no merged index, so all of them are `null`. A
    report that names neither mode is not checked here.
    """
    mode = report.get("mode")
    if mode not in ("native", "reconstructed"):
        return []
    places = [(f"refresh {item.get('refresh')}", item.get(key))
              for item in report.get("refreshes", [])]
    sequence = report.get("sequence", {}).get(key)
    if mode == "native":
        return [f"{place}: a native run reports {key}"
                for place, value in [*places, ("sequence", sequence)] if value is not None]
    problems: list[str] = []

    def counts(place: str, found: Any) -> dict[str, int] | None:
        values = {}
        for field in fields:
            value = found.get(field) if isinstance(found, dict) else None
            if isinstance(value, bool) or not isinstance(value, int) or value < 0:
                problems.append(f"{place}: {key}.{field} is {value!r}")
                return None
            values[field] = value
        return values

    total = {field: 0 for field in fields}
    for place, found in places:
        for field, value in (counts(place, found) or {}).items():
            total[field] += value
    summed = counts("sequence", sequence)
    if summed is not None and not problems and summed != total:
        problems.append(f"sequence: {key} are not the sums of the refreshes'")
    return problems


def profiled_cpu_problems(report: dict[str, Any]) -> list[str]:
    """List the refreshes whose CPU-profiler times (`profiled_cpu_ms.base` and `.q3`, both
    modes, measurement version 6) are missing or not finite non-negative numbers."""
    if report.get("mode") not in ("native", "reconstructed"):
        return []
    problems: list[str] = []
    for item in report.get("refreshes", []):
        cpu = item.get("profiled_cpu_ms")
        for circuit in ("base", "q3"):
            value = cpu.get(circuit) if isinstance(cpu, dict) else None
            if isinstance(value, bool) or not isinstance(value, (int, float)) \
                    or not math.isfinite(value) or value < 0:
                problems.append(f"refresh {item.get('refresh')}: "
                                f"profiled_cpu_ms.{circuit} is {value!r}")
    return problems


def delta_phase_problems(report: dict[str, Any]) -> list[str]:
    """List the refreshes whose `delta_ms` (the preparation of the view circuit's transaction
    delta, both modes, measurement version 11) is missing or not a finite non-negative number,
    or exceeds `ivm_ms`, which includes it."""
    if report.get("mode") not in ("native", "reconstructed"):
        return []
    problems: list[str] = []
    for item in report.get("refreshes", []):
        delta = item.get("delta_ms")
        if isinstance(delta, bool) or not isinstance(delta, (int, float)) \
                or not math.isfinite(delta) or delta < 0:
            problems.append(f"refresh {item.get('refresh')}: delta_ms is {delta!r}")
            continue
        ivm = item.get("ivm_ms")
        if isinstance(ivm, (int, float)) and not isinstance(ivm, bool) and ivm < delta:
            problems.append(f"refresh {item.get('refresh')}: ivm_ms {ivm!r} is below "
                            f"delta_ms {delta!r}, which it includes")
    return problems


def session_pass_problems(report: dict[str, Any], session_pass: str) -> list[str]:
    """List a mismatch between the pass the runner asked for and the one the driver
    reports: a reconstructed run reports it, and a native run, which has no scan
    session, reports `null`."""
    mode = report.get("mode")
    if mode not in ("native", "reconstructed"):
        return []
    expected = session_pass if mode == "reconstructed" else None
    reported = report.get("configuration", {}).get("session_pass")
    if reported != expected:
        return [f"configuration.session_pass is {reported!r}, expected {expected!r}"]
    return []


def measurement_version_problems(report: dict[str, Any]) -> list[str]:
    """List a driver report of another measurement version than this runner understands."""
    if report.get("mode") not in ("native", "reconstructed"):
        return []
    version = report.get("measurement_version")
    if version != MEASUREMENT_VERSION:
        return [f"measurement_version is {version!r}, expected {MEASUREMENT_VERSION}"]
    return []


def layout_fallback_problems(report: dict[str, Any]) -> list[str]:
    """List the disk encodings that were switched on and wrote a block as it was because the
    block's layout did not match what the encoder understands.

    That is a bug or an unexpected archive layout, and the run would then measure a mix of
    formats. When `configuration.front_code_byte_keys` is true,
    `front_coding.layout_fallbacks` must be 0, and when `configuration.elide_value_headers` is
    true, `value_header_elision.layout_fallbacks` must be 0. Each `size_fallbacks` is not
    checked: a block that an encoding would not shrink is left as it is on purpose.
    """
    if report.get("mode") not in ("native", "reconstructed"):
        return []
    configuration = report.get("configuration") or {}
    problems: list[str] = []
    for flag, section in (("front_code_byte_keys", "front_coding"),
                          ("elide_value_headers", "value_header_elision")):
        if not configuration.get(flag):
            continue
        fallbacks = (report.get(section) or {}).get("layout_fallbacks")
        if isinstance(fallbacks, bool) or not isinstance(fallbacks, int) or fallbacks < 0:
            problems.append(f"{section}.layout_fallbacks is {fallbacks!r}")
        elif fallbacks:
            problems.append(f"{section}.layout_fallbacks is {fallbacks}, expected 0 with "
                            f"configuration.{flag}: those blocks were written in the plain layout")
    return problems


def block_expansion_problems(report: dict[str, Any]) -> list[str]:
    """List where a driver report's `block_expansions` (measurement version 12) are malformed.

    Each refresh and the sequence give `block_expansions` with three entries: `view_worker`
    and `driver`, each with every owner of `WRITE_OWNERS`, and `index_upkeep`, one count. A
    count has every `BLOCK_EXPANSION_FIELDS` as a non-negative integer, and a block count of 0
    means 0 bytes, and an expanded block is no smaller than its encoded form. A native run has
    no merged index, so its `driver` and `index_upkeep` are `null`. The sequence's counts are the
    sums of the refreshes'. The `prefix_truncated_*` fields are 0 unless
    `configuration.front_code_byte_keys` is true, and the `elided_*` fields are 0 unless
    `configuration.elide_value_headers` is true: a file written in neither encoding has no block
    to expand. A report that names neither mode is not checked here.
    """
    mode = report.get("mode")
    if mode not in ("native", "reconstructed"):
        return []
    configuration = report.get("configuration") or {}
    must_be_zero = ((() if configuration.get("front_code_byte_keys")
                     else PREFIX_TRUNCATED_EXPANSION_FIELDS)
                    + (() if configuration.get("elide_value_headers")
                       else ELIDED_EXPANSION_FIELDS))
    problems: list[str] = []

    def counts(place: str, name: str, found: Any) -> dict[str, int] | None:
        values = {}
        for field in BLOCK_EXPANSION_FIELDS:
            value = found.get(field) if isinstance(found, dict) else None
            if isinstance(value, bool) or not isinstance(value, int) or value < 0:
                problems.append(f"{place}: block_expansions.{name}.{field} is {value!r}")
                return None
            values[field] = value
        for field in must_be_zero:
            if values[field]:
                problems.append(f"{place}: block_expansions.{name}.{field} is {values[field]}, "
                                "expected 0 with the encoding off")
        for blocks, encoded, expanded in (
                ("prefix_truncated_blocks", "prefix_truncated_encoded_bytes",
                 "prefix_truncated_expanded_bytes"),
                ("elided_blocks", "elided_encoded_bytes", "elided_expanded_bytes")):
            if not values[blocks] and (values[encoded] or values[expanded]):
                problems.append(f"{place}: block_expansions.{name} counts bytes of no {blocks}")
            if values[expanded] < values[encoded]:
                problems.append(f"{place}: block_expansions.{name}.{expanded} is below "
                                f"{encoded}")
        return values

    def place_counts(place: str, found: Any) -> dict[tuple[str, str | None], dict[str, int]] | None:
        if not isinstance(found, dict):
            problems.append(f"{place}: block_expansions is {found!r}")
            return None
        values: dict[tuple[str, str | None], dict[str, int]] = {}
        for thread in EXPANSION_THREADS:
            part = found.get(thread)
            if thread != "view_worker" and mode == "native":
                if part is not None:
                    problems.append(f"{place}: a native run reports block_expansions.{thread}")
                continue
            if thread == "index_upkeep":
                one = counts(place, thread, part)
                if one is None:
                    return None
                values[(thread, None)] = one
                continue
            for owner in WRITE_OWNERS:
                one = counts(place, f"{thread}.{owner}",
                             part.get(owner) if isinstance(part, dict) else None)
                if one is None:
                    return None
                values[(thread, owner)] = one
        return values

    totals: dict[tuple[str, str | None], dict[str, int]] = {}
    for item in report.get("refreshes", []):
        found = place_counts(f"refresh {item.get('refresh')}", item.get("block_expansions"))
        for key, one in (found or {}).items():
            total = totals.setdefault(key, {field: 0 for field in BLOCK_EXPANSION_FIELDS})
            for field, value in one.items():
                total[field] += value
    summed = place_counts("sequence", report.get("sequence", {}).get("block_expansions"))
    if summed is not None and not problems and summed != totals:
        problems.append("sequence: block_expansions are not the sums of the refreshes'")
    return problems


LATENCY_PHASES = ("total_ms", "base_ms", "ivm_ms")


def transaction_latency_problems(report: dict[str, Any]) -> list[str]:
    """List where a report's per-transaction latencies (measurement version 14) are malformed:
    each refresh's `transaction_latency` gives, for `total_ms`, `base_ms` and `ivm_ms`, one
    finite non-negative value per transaction of the refresh, and `max` is their largest."""
    problems = []
    for number, refresh in enumerate(report.get("refreshes", []), start=1):
        latency = refresh.get("transaction_latency")
        transactions = refresh.get("transactions")
        if not isinstance(latency, dict):
            problems.append(f"refresh {number}: transaction_latency is {latency!r}")
            continue
        if latency.get("transactions") != transactions:
            problems.append(f"refresh {number}: transaction_latency counts "
                            f"{latency.get('transactions')!r} transactions, the refresh "
                            f"{transactions!r}")
        for phase in LATENCY_PHASES:
            values = (latency.get(phase) or {}).get("values")
            if not isinstance(values, list) or len(values) != transactions or not all(
                    isinstance(value, (int, float)) and not isinstance(value, bool)
                    and math.isfinite(value) and value >= 0 for value in values):
                problems.append(f"refresh {number}: transaction_latency.{phase}.values must "
                                f"hold {transactions!r} finite non-negative values")
            elif values and latency[phase].get("max") != max(values):
                problems.append(f"refresh {number}: transaction_latency.{phase}.max is not the "
                                "largest value")
    return problems


def index_prefix_filter_problems(report: dict[str, Any], asked: bool) -> list[str]:
    """List a mismatch between the prefix-filter arm the runner asked for and the one the driver
    reports. Native runs report it too, though they have no index to apply it to. A report that
    names neither mode is not checked here."""
    if report.get("mode") not in ("native", "reconstructed"):
        return []
    reported = report.get("configuration", {}).get("index_prefix_filter")
    if reported is not asked:
        return [f"configuration.index_prefix_filter is {reported!r}, expected {asked!r}"]
    return []


def memtable_mib_problems(report: dict[str, Any], asked: int | None) -> list[str]:
    """List a mismatch between the memtable budget the runner asked for (`--memtable-mib`, `None`
    for off) and the one the driver reports. A report that names neither mode is not checked."""
    if report.get("mode") not in ("native", "reconstructed"):
        return []
    configuration = report.get("configuration", {})
    if "memtable_mib" not in configuration:
        return [f"configuration.memtable_mib is missing, expected {asked!r}"]
    if configuration["memtable_mib"] != asked:
        return [f"configuration.memtable_mib is {configuration['memtable_mib']!r}, "
                f"expected {asked!r}"]
    return []


def report_problems(report: dict[str, Any], session_pass: str) -> list[str]:
    """Every consistency check of one driver report."""
    return (measurement_version_problems(report) + layout_fallback_problems(report)
            + transaction_latency_problems(report)
            + block_expansion_problems(report)
            + owner_write_problems(report) + index_read_problems(report)
            + view_read_problems(report) + driver_read_problems(report)
            + counter_problems(report, "index_session_counts", SESSION_COUNT_FIELDS)
            + counter_problems(report, "index_upkeep_run_reads", RUN_READ_FIELDS)
            + profiled_cpu_problems(report) + delta_phase_problems(report)
            + session_pass_problems(report, session_pass))


def view_read_problems(report: dict[str, Any]) -> list[str]:
    """List where a driver report's view-worker run reads are malformed.

    Each refresh and the sequence give `view_run_reads` for every owner in
    `WRITE_OWNERS`, with every `RUN_READ_FIELDS` a non-negative integer, and the
    sequence's counts are the sums of the refreshes'. Nothing the view worker runs
    reads under `merged_index`: the scan session reads the index on the driver's
    thread, and its reads are in `index_run_reads`, not here. The joins' reads of the
    readers' requested slices are under `requested_slice` (measurement version 11): a
    reconstructed run must report some over the sequence, and a native run none. The joins'
    reads of their traces are under `partner_trace`: a native run must report some over the
    sequence, and a reconstructed run none. A report that names neither mode is not
    checked here.
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
            if index_part != 0:
                problems.append(f"{place}: view_run_reads.merged_index.{field} is {index_part}, "
                                "expected 0")
            slice_part = found[("requested_slice", field)]
            if mode == "native" and slice_part != 0:
                problems.append(f"{place}: view_run_reads.requested_slice.{field} is "
                                f"{slice_part}, expected 0 in a native run")
            trace_part = found[("partner_trace", field)]
            if mode == "reconstructed" and trace_part != 0:
                problems.append(f"{place}: view_run_reads.partner_trace.{field} is "
                                f"{trace_part}, expected 0 in a reconstructed run")
    summed = counts("sequence", report.get("sequence", {}).get("view_run_reads"))
    if summed is not None and not problems and summed != total:
        problems.append("sequence: view_run_reads are not the sums of the refreshes'")
    if summed is not None and not problems:
        partner = "requested_slice" if mode == "reconstructed" else "partner_trace"
        if not any(summed[(partner, field)] for field in RUN_READ_FIELDS):
            problems.append(f"sequence: a {mode} run reports no view_run_reads.{partner} reads")
    return problems


def driver_read_problems(report: dict[str, Any]) -> list[str]:
    """List where a driver report's `driver_run_reads` are malformed.

    `driver_run_reads` (measurement version 11) are the reads the driver's thread made while
    collecting the readers, per owner of `WRITE_OWNERS`. A native run has none (`null`). A
    reconstructed run reports every owner with every `RUN_READ_FIELDS` a non-negative integer,
    the sequence's counts are the sums of the refreshes', and the driver reads under no owner
    but the index's (`merged_index`) and the requested slice's (`requested_slice`).
    """
    mode = report.get("mode")
    if mode not in ("native", "reconstructed"):
        return []
    places = [("sequence", report.get("sequence", {}).get("driver_run_reads"))]
    places += [(f"refresh {item.get('refresh')}", item.get("driver_run_reads"))
               for item in report.get("refreshes", [])]
    problems: list[str] = []
    if mode == "native":
        return [f"{place}: a native run reports driver_run_reads"
                for place, reads in places if reads is not None]
    totals: dict[tuple[str, str], int] = {}
    for place, reads in places:
        for owner in WRITE_OWNERS:
            for field in RUN_READ_FIELDS:
                value = reads.get(owner, {}).get(field) if isinstance(reads, dict) else None
                if isinstance(value, bool) or not isinstance(value, int) or value < 0:
                    problems.append(f"{place}: driver_run_reads.{owner}.{field} is {value!r}")
                    return problems
                if owner not in ("merged_index", "requested_slice") and value != 0:
                    problems.append(f"{place}: driver_run_reads.{owner}.{field} is {value}, "
                                    "expected 0")
                if place != "sequence":
                    totals[(owner, field)] = totals.get((owner, field), 0) + value
    sequence = places[0][1]
    # The sequence comes first in `places`, so its sums are checked against the refreshes' here.
    for owner in WRITE_OWNERS:
        for field in RUN_READ_FIELDS:
            if sequence[owner][field] != totals.get((owner, field), 0):
                problems.append("sequence: driver_run_reads are not the sums of the refreshes'")
                return problems
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
        if any(owners["requested_slice"].values()):
            problems.append(f"{place}: writes are attributed to the read owner requested_slice")
        if report.get("mode") == "native" and any(owners["merged_index"].values()):
            problems.append(f"{place}: a native run reports merged-index writes")
    return problems


def unescape_mountinfo(field: str) -> str:
    """Undo the octal escapes (`\\040` for a space) in a `mountinfo` path."""
    return re.sub(r"\\([0-7]{3})", lambda match: chr(int(match.group(1), 8)), field)


def mount_of(path: Path, mountinfo: str) -> tuple[str, str] | None:
    """The file system type and the source of the mount that holds `path`, from the text of
    `/proc/self/mountinfo`: the mount with the longest mount point that contains `path`, the
    later line when two mounts share it. `None` if no mount contains it."""
    found: tuple[int, str, str] | None = None
    for line in mountinfo.splitlines():
        before, separator, after = line.partition(" - ")
        fields, rest = before.split(), after.split()
        if not separator or len(fields) < 5 or len(rest) < 2:
            continue
        mount_point = unescape_mountinfo(fields[4])
        inside = (mount_point == "/" or str(path) == mount_point
                  or str(path).startswith(mount_point.rstrip("/") + "/"))
        if inside and (found is None or len(mount_point) >= found[0]):
            found = (len(mount_point), rest[0], unescape_mountinfo(rest[1]))
    return None if found is None else (found[1], found[2])


def read_sysfs(path: Path) -> str | None:
    try:
        return path.read_text().strip()
    except OSError:
        return None


def block_disk_name(source: str, sys_root: Path) -> str | None:
    """The sysfs name of the whole disk behind the mount source `source`, or `None`.

    A partition resolves to its parent disk. A device-mapper device, an md array and a loop
    device have no disk of their own in this sense, so they are not resolved, and neither is a
    source outside `/dev` (a network or memory file system).
    """
    if not source.startswith("/dev/") or source.startswith("/dev/mapper/"):
        return None
    name = source.removeprefix("/dev/")
    if "/" in name or re.match(r"(dm-|md|loop|zram|ram)\d+$", name):
        return None
    block = sys_root / "block"
    if (block / name / "dm").exists():
        return None
    if (block / name / "queue").is_dir():
        return name
    if block.is_dir():
        for disk in sorted(block.iterdir()):
            if (disk / name).is_dir() and (disk / "queue").is_dir():
                return disk.name
    return None


def storage_device(path: Path, label: str | None = None, system: str | None = None,
                   proc_root: Path = Path("/proc"), sys_root: Path = Path("/sys")) -> dict[str, Any]:
    """Describe the block device behind `path` for the manifest (`storage_device`).

    On Linux it reads the mount of `path` from `<proc_root>/self/mountinfo`, then the disk's
    `queue/rotational`, `queue/logical_block_size` and `device/model` under
    `<sys_root>/block/<disk>`; a partition is described by its parent disk. Every field is `None`
    elsewhere, and when the device is not resolved (an unreadable file, a device-mapper device or
    another virtual device, a network file system): the fields are never guessed. `label` is the
    operator's `--device-label` and is kept as given; `device` is the disk's `/dev` path.
    """
    unresolved: dict[str, Any] = {field: None for field in STORAGE_DEVICE_FIELDS}
    unresolved["label"] = label
    if (system or platform.system()) != "Linux":
        return unresolved
    try:
        mountinfo = (proc_root / "self" / "mountinfo").read_text()
    except OSError:
        return unresolved
    mount = mount_of(path, mountinfo)
    disk = block_disk_name(mount[1], sys_root) if mount else None
    if mount is None or disk is None:
        return unresolved
    queue = sys_root / "block" / disk / "queue"
    rotational = read_sysfs(queue / "rotational")
    sector = read_sysfs(queue / "logical_block_size")
    return {
        "path": str(path),
        "device": f"/dev/{disk}",
        "filesystem": mount[0],
        "logical_sector_bytes": int(sector) if sector and sector.isdigit() else None,
        "rotational": {"0": False, "1": True}.get(rotational or ""),
        "model": read_sysfs(sys_root / "block" / disk / "device" / "model"),
        "label": label,
    }


def device_label_problem(device: dict[str, Any]) -> str | None:
    """Describe how the operator's label contradicts the device's `rotational` flag, if it does.
    Nothing is checked when either is unknown."""
    label, rotational = device.get("label"), device.get("rotational")
    if label is None or rotational is None:
        return None
    if (label == "ssd") == rotational:
        return (f"--device-label {label} contradicts {device.get('device')}, which reports "
                f"rotational={int(rotational)}")
    return None


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


def file_identity(path: Path) -> dict[str, int]:
    """The size and modification time of an input, which a run checks in place of a hash."""
    status = path.stat()
    return {"size": status.st_size, "mtime_ns": status.st_mtime_ns}


def unchanged_inputs(spec: dict[str, Any]) -> None:
    """Check that every shared input is the file the campaign hashed.

    Every run of every campaign reads the same input files in place, so none is copied
    (user decision of 2026-11-11). The campaign hashes each input once; a run checks
    only that its size and modification time are those recorded beside the hash.
    """
    for name, raw in spec["inputs"].items():
        if file_identity(Path(raw)) != spec["input_identities"][name]:
            raise RuntimeError(f"input {name} changed after the campaign hashed it")


def evict_from_page_cache(paths: list[Path]) -> dict[str, Any]:
    """Drop the page-cache pages of `paths` (Linux `posix_fadvise(DONTNEED)`).

    Each file is first flushed with `fsync`, because dirty pages (of a file just
    written) are not dropped. Failures are recorded per file, not raised, so
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


def tree_files(root: Path) -> list[Path]:
    """Every regular file under `root`."""
    return sorted(path for path in root.rglob("*") if path.is_file())


def run_inside_cgroup(spec_path: Path) -> None:
    spec = json.loads(spec_path.read_text())
    group = Path(spec["cgroup"])
    verify_membership(group)
    if (group / "memory.max").read_text().strip() != str(spec["total_memory_bytes"]):
        raise RuntimeError("run cgroup has an unexpected total memory limit")
    unchanged_inputs(spec)
    # Each run starts cold: no page of the shared inputs that an earlier run, or the
    # campaign's hashing, left in the page cache survives into this one. The driver's
    # reads then fault the pages again, charged to this run's cgroup.
    # A run started from an image also starts with its copy of the image out of the page cache,
    # which the copy left there in the runner's cgroup.
    evicted = [Path(raw) for raw in spec["inputs"].values()]
    if spec.get("image"):
        evicted += tree_files(Path(spec["image"]))
    eviction = evict_from_page_cache(evicted)
    write_json(Path(spec["run_dir"]) / "input-eviction.json", eviction)
    write_json(Path(spec["run_dir"]) / "cgroup-proof.json", {
        "cgroup": str(group), "memory_max": spec["total_memory_bytes"],
        "memory_swap_max": 0, "membership_verified": True,
        "input_identities_verified": True, "inputs_evicted": eviction["evicted"],
    })
    command = driver_command(spec, spec["inputs"])
    os.execv(command[0], command)


def driver_command(spec: dict[str, Any], inputs: dict[str, str]) -> list[str]:
    """The driver's command line for one run: the single-process driver, which loads the base
    files into `<run dir>/storage`, or, when `spec` names an image, the refresh program, which
    reopens the run's copy of that image."""
    args = spec["args"]
    command = [spec["executable"], "--r5"]
    if spec.get("image"):
        command += ["--from-image", spec["image"]]
        if args.get("warm_up"):
            command += ["--warm-up", args["warm_up"]]
    else:
        command += [
            "--customer", inputs["customer"],
            "--orders", inputs["orders"],
            "--lineitem", inputs["lineitem"],
            "--storage-path", str(Path(spec["run_dir"]) / "storage"),
        ]
    command += [
        "--refresh-list", inputs["refresh_list"],
        "--report", str(Path(spec["run_dir"]) / "driver.json"),
        "--cache-bytes", str(args["cache_bytes"]),
        "--memory-target-bytes", str(args["memory_target_bytes"]),
        "--compaction-timeout-secs", str(args["compaction_timeout_secs"]),
        "--batch", str(args.get("batch", 10000)),
        "--workers", "1", "--runs", "1",
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
    if args.get("index_prefix_filter"):
        command.append("--index-prefix-filter")
    if args.get("memtable_mib") is not None:
        command.extend(["--memtable-mib", str(args["memtable_mib"])])
    command.extend(["--session-pass", args.get("session_pass", "seek")])
    return command


def build_executables(destination: Path) -> dict[str, Path]:
    destination.mkdir(parents=True, exist_ok=True)
    cargo_target = Path(os.environ.get("CARGO_TARGET_DIR", EXPERIMENT / "target"))
    if not cargo_target.is_absolute():
        cargo_target = (EXPERIMENT / cargo_target).resolve()
    binaries: dict[str, Path] = {}
    # One generation serves both builds: the reconstructed view circuit is written by hand.
    subprocess.run([str(EXPERIMENT / "generate.sh")], cwd=EXPERIMENT, check=True)
    for mode in MODES:
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
                  verify: bool = False, enforced: bool = False,
                  cold: bool = False) -> dict[str, Any]:
    if system != "Linux":
        return {"status": "smoke", "reason": "total memory is not enforced on macOS"}
    if limit is None:
        return {"status": "nonqualifying", "reason": "no cgroup v2 total memory limit"}
    if not enforced:
        return {"status": "nonqualifying", "reason": "cgroup enforcement was not verified"}
    if not cold:
        return {"status": "nonqualifying",
                "reason": "the shared inputs were not evicted from the page cache before the run"}
    if verify:
        return {"status": "nonqualifying", "reason": "independent verification was enabled"}
    # A refresh program's report carries the loaded size in its image's load record.
    load = result.get("load") or (result.get("image") or {}).get("load") or {}
    loaded = load.get("observed_storage_bytes")
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
    # The proof's `inputs_evicted` is checked by `qualification`, not here: it is the
    # run's cold start, not the cgroup's enforcement.
    enforced = isinstance(proof, dict) and {
        key: value for key, value in proof.items() if key != "inputs_evicted"} == {
        "cgroup": str(group), "memory_max": limit, "memory_swap_max": 0,
        "membership_verified": True, "input_identities_verified": True,
    } and memory_max == str(limit) and swap_max == "0"
    counters = {}
    for name in ("memory.peak", "memory.events", "io.stat"):
        path = group / name
        counters[name] = path.read_text().strip() if path.is_file() else None
    return {"enforced": enforced, "proof": proof, "memory_max": memory_max,
            "memory_swap_max": swap_max, "counters": counters}


# The load program's defaults (`--build-images`). Loading is not measured, so its settings serve
# speed and never reach a refresh run (decision of 2026-11-17). The view phase of every load
# transaction scans the touched customers' groups of the merged index; storage files bypass the
# page cache (`O_DIRECT`), so a buffer-cache miss reads the device. With the campaign's 128 MiB
# and 10,000-row transactions a reconstructed SF 10 load read 0.35% of `lineitem.tbl` per
# minute; with 16 GiB per load (four at once) and 1,000,000-row transactions about 2% until the
# caches filled, then about 0.9% while it read 17 to 29 GB back from the SSD. So each load's
# cache is sized from the host: `LOAD_MEMORY_SHARE` of `MemAvailable`, split evenly among the
# loads that run at once (all four, or one with `--sequential-loads`), leaving the rest for the
# loads' working memory. `--cache-bytes` overrides it; a host without `/proc/meminfo` falls back
# to `LOAD_CACHE_FALLBACK_BYTES`.
LOAD_MEMORY_SHARE = 0.75
LOAD_CACHE_FALLBACK_BYTES = 16 * 1024 ** 3
MIB = 1024 * 1024
LOAD_BATCH = 1_000_000
LOAD_COMPACTION_TIMEOUT_SECS = 3600.0
# The single-process driver's defaults, which also load.
CAMPAIGN_BATCH = 10000
CAMPAIGN_COMPACTION_TIMEOUT_SECS = 300.0


def parse_args(argv: list[str]) -> argparse.Namespace:
    if argv[:1] == ["--adopt-image-sources"] and len(argv) == 2:
        return argparse.Namespace(adopt_image_sources=Path(argv[1]))
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("customer", "orders", "lineitem", "refresh-list", "report"):
        parser.add_argument("--" + name, type=Path)
    parser.add_argument("--cache-bytes", type=int,
                        help="the buffer cache in bytes, a whole MiB; required for a campaign. "
                             "With --build-images it is each load's own cache (default: "
                             f"{LOAD_MEMORY_SHARE:.0%}% of MemAvailable split among the loads "
                             "that run at once) and never reaches a refresh run")
    parser.add_argument("--memory-target-bytes", type=int)
    parser.add_argument("--build-images", type=Path, metavar="IMAGES_DIR",
                        help="load the base files once per mode and prefix-truncation arm, "
                             "outside any cgroup and with no memory target, and write the "
                             "images and images.json into this new directory; runs no refresh")
    parser.add_argument("--adopt-image-sources", type=Path, metavar="IMAGES_DIR",
                        help="record the image sources digest in an image directory built "
                             "before it existed, if git shows no image source changed since")
    parser.add_argument("--complete-images", type=Path, metavar="IMAGES_DIR",
                        help="load, all at once, the images that IMAGES_DIR/images.json lacks, "
                             "with the executables that wrote the others; each load's cache is "
                             f"{LOAD_MEMORY_SHARE:.0%}% of MemAvailable split by mode weight "
                             "(merged-index twice native) unless --cache-bytes is given")
    parser.add_argument("--images", type=Path, metavar="IMAGES_DIR",
                        help="start every run from a copy of the matching image in this "
                             "directory (built by --build-images) instead of loading")
    parser.add_argument("--sequential-loads", action="store_true",
                        help="with --build-images, run the loads one at a time, each with the "
                             "whole load cache budget (default: all four at once)")
    parser.add_argument("--keep-run-storage", action="store_true",
                        help="with --images, keep each completed run's copy of its image "
                             "(default: delete it once the run's report is recorded)")
    parser.add_argument("--warm-up", type=Path, metavar="REFRESH_LIST",
                        help="with --images, apply these refreshes untimed before the window, "
                             "so that the window starts with their buffer cache")
    parser.add_argument("--compaction-timeout-secs", type=float,
                        help=f"default {CAMPAIGN_COMPACTION_TIMEOUT_SECS:g}, or "
                             f"{LOAD_COMPACTION_TIMEOUT_SECS:g} with --build-images")
    parser.add_argument("--batch", type=int,
                        help=f"rows per load transaction: default {CAMPAIGN_BATCH}, or "
                             f"{LOAD_BATCH} with --build-images")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--storage-path", type=Path)
    parser.add_argument("--device-label", choices=DEVICE_LABELS,
                        help="the kind of device that holds the storage path, recorded in the "
                             "manifest; on Linux it must agree with the device's rotational flag")
    parser.add_argument("--no-verify", action="store_true")
    parser.add_argument("--diagnostics", action="store_true",
                        help="record the flush log, the compaction log and the run contents "
                             "of each run in a diagnostics directory beside its driver report")
    parser.add_argument("--setup-full-compaction", action="store_true",
                        help="fully compact the circuit and the merged index after loading, "
                             "before the refreshes; its cost is reported in the load section")
    parser.add_argument("--front-code-byte-keys", action="store_true",
                        help="prefix-truncate byte-string keys in storage files (the merged "
                             "index's key level); native runs have no such keys")
    parser.add_argument("--elide-value-headers", action="store_true",
                        help="drop the redundant 16-byte header of every byte-string value in "
                             "storage files, and rebuild it on read (the merged index's value "
                             "level); native runs have no such values")
    parser.add_argument("--index-prefix-filter", action="store_true",
                        help="give the merged index's files a prefix filter over the customer "
                             "group instead of the whole-key filter, so that group seeks skip "
                             "the runs it rules out (an A/B arm, measurement version 17); "
                             "images record it, so each arm needs its own images; native runs "
                             "ignore it")
    parser.add_argument("--memtable-mib", type=int, metavar="MIB",
                        help="a campaign's memtable budget for the integral spines, in MiB "
                             "(measurement version 18; off by default); memtables count in RSS, "
                             "so the budget must stay below half of --memory-target-bytes; not "
                             "a storage setting, so images need no rebuild per budget")
    parser.add_argument("--session-pass", choices=SESSION_PASSES, default="seek",
                        help="how the reconstructed build's scan session passes the untouched "
                             "groups (default seek); native runs have no scan session")
    parser.add_argument("--cgroup-root", type=Path)
    parser.add_argument("--total-memory-bytes", type=int)
    args = parser.parse_args(argv)
    loading = args.build_images or args.complete_images
    if args.build_images and args.complete_images:
        parser.error("--build-images and --complete-images are alternatives")
    if loading:
        # The cache stays None unless given: the load program sizes it from the host.
        defaults = (args.cache_bytes, LOAD_BATCH, LOAD_COMPACTION_TIMEOUT_SECS)
    else:
        if args.cache_bytes is None:
            parser.error("a campaign needs --cache-bytes")
        defaults = (args.cache_bytes, CAMPAIGN_BATCH, CAMPAIGN_COMPACTION_TIMEOUT_SECS)
    for name, default in zip(("cache_bytes", "batch", "compaction_timeout_secs"), defaults):
        if getattr(args, name) is None:
            setattr(args, name, default)
    base_files = [args.customer, args.orders, args.lineitem]
    if loading:
        if None in base_files:
            parser.error("--build-images needs --customer, --orders and --lineitem")
        if args.complete_images and args.sequential_loads:
            parser.error("--complete-images runs its loads at once")
        if args.images or args.refresh_list or args.report or args.storage_path or \
                args.cgroup_root or args.warm_up or args.keep_run_storage or \
                args.memory_target_bytes is not None:
            parser.error("--build-images loads only: it takes the base files and the loading "
                         "settings, and no refresh, report, storage, cgroup or memory target")
    else:
        if args.refresh_list is None or args.report is None:
            parser.error("a campaign needs --refresh-list and --report")
        if args.memory_target_bytes is None:
            parser.error("a campaign needs --memory-target-bytes")
        if args.images:
            if any(path is not None for path in base_files):
                parser.error("--images replaces the base files: an image holds the loaded state")
        elif None in base_files:
            parser.error("a campaign needs --customer, --orders and --lineitem, or --images")
        if args.warm_up and not args.images:
            parser.error("--warm-up applies to runs started from --images")
        if args.sequential_loads:
            parser.error("--sequential-loads applies to --build-images")
        if args.keep_run_storage and not args.images:
            parser.error("--keep-run-storage applies to runs started from --images")
    if args.runs < 1 or args.batch < 1 or (args.cache_bytes is not None
                                           and args.cache_bytes < 1) or \
            (args.memory_target_bytes is not None and args.memory_target_bytes < 1):
        parser.error("runs, batch, cache bytes, and memory target bytes must be positive")
    if args.memtable_mib is not None:
        if loading:
            parser.error("--memtable-mib applies to a campaign: images load without memtables")
        if args.memtable_mib < 1:
            parser.error("--memtable-mib must be positive")
        if args.memory_target_bytes is not None and \
                args.memtable_mib * MIB * 2 >= args.memory_target_bytes:
            parser.error("--memtable-mib must be below half of --memory-target-bytes, which the "
                         "driver's runtime refuses at startup")
    if args.cache_bytes is not None and args.cache_bytes % MIB:
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


def git_state() -> tuple[str, str]:
    """The checkout's revision and `git status --short`."""
    revision = subprocess.run(["git", "rev-parse", "HEAD"], cwd=EXPERIMENT,
                              check=True, capture_output=True, text=True)
    status = subprocess.run(["git", "status", "--short"], cwd=EXPERIMENT,
                            check=True, capture_output=True, text=True)
    return revision.stdout.strip(), status.stdout


def memory_available_bytes(meminfo: Path = Path("/proc/meminfo")) -> int | None:
    """The host's `MemAvailable`, or None where `/proc/meminfo` does not give it."""
    try:
        for line in meminfo.read_text().splitlines():
            name, _, value = line.partition(":")
            if name == "MemAvailable":
                return int(value.split()[0]) * 1024
    except (OSError, ValueError, IndexError):
        return None
    return None


def load_cache_bytes(available: int | None, concurrent: int) -> int:
    """Each load's buffer cache: `LOAD_MEMORY_SHARE` of `available` split among `concurrent`
    loads, in whole MiB and at least 2 MiB (the driver's minimum), or the fallback."""
    if available is None:
        return LOAD_CACHE_FALLBACK_BYTES
    return max(int(available * LOAD_MEMORY_SHARE / concurrent) // MIB * MIB, 2 * MIB)


def load_image(root: Path, binaries: dict[str, Path], inputs: dict[str, Path], mode: str,
               front_code: bool, cache_bytes: int, args: argparse.Namespace) -> dict[str, Any]:
    """Runs the load program for one image in `root` and returns its `images.json` entry."""
    name = image_name(mode, front_code)
    directory = root / name
    command = [str(binaries[mode]), "--write-image", str(directory),
               "--customer", str(inputs["customer"]),
               "--orders", str(inputs["orders"]),
               "--lineitem", str(inputs["lineitem"]),
               "--runs", "1", "--no-verify", "--batch", str(args.batch),
               "--cache-bytes", str(cache_bytes),
               "--compaction-timeout-secs", str(args.compaction_timeout_secs)]
    if args.setup_full_compaction:
        command.append("--setup-full-compaction")
    if front_code:
        command.append("--front-code-byte-keys")
    if args.elide_value_headers:
        command.append("--elide-value-headers")
    if args.index_prefix_filter:
        command.append("--index-prefix-filter")
    started = time.monotonic()
    completed = subprocess.run(command, cwd=EXPERIMENT, capture_output=True, text=True)
    (root / f"{name}.stdout.log").write_text(completed.stdout)
    (root / f"{name}.stderr.log").write_text(completed.stderr)
    if completed.returncode:
        raise RuntimeError(f"loading image {name} exited {completed.returncode}: "
                           f"{completed.stderr[-2000:]}")
    image = json.loads((directory / "image.json").read_text())
    return {
        "directory": str(directory), "mode": mode,
        "front_code_byte_keys": front_code, "settings": image["settings"],
        "checkpoint": image["checkpoint"]["uuid"], "load": image["load"],
        "size_bytes": tree_size(directory / "storage"),
        "elapsed_seconds": time.monotonic() - started,
        "cache_bytes": cache_bytes,
    }


# `--complete-images` gives each load a share of the load budget in proportion to its mode's
# weight: a reconstructed load reads the merged index's touched groups in every load transaction,
# so it gains more from cache than a native load does (user direction of 2026-11-18).
LOAD_MODE_WEIGHTS = {"native": 1, "merged-index": 2}


def weighted_load_cache_bytes(available: int | None, modes: list[str]) -> list[int]:
    """Each pending load's cache when `modes` load at once: `LOAD_MEMORY_SHARE` of `available`
    split in proportion to `LOAD_MODE_WEIGHTS`, in whole MiB and at least 2 MiB, or the
    fallback each."""
    if available is None:
        return [LOAD_CACHE_FALLBACK_BYTES for _ in modes]
    total = sum(LOAD_MODE_WEIGHTS[mode] for mode in modes)
    return [max(int(available * LOAD_MEMORY_SHARE * LOAD_MODE_WEIGHTS[mode] / total) // MIB * MIB,
                2 * MIB) for mode in modes]


def complete_images(args: argparse.Namespace) -> int:
    """Loads the images that `images.json` in an existing image directory lacks, all at once,
    with the executables that wrote the others, so that every image in the directory comes from
    the same executables. A missing image's directory may exist only empty (a placeholder that
    kept an interrupted sequential build from starting it). Marks the manifest complete when all
    four images are present."""
    root = args.complete_images.resolve()
    path = root / IMAGES_MANIFEST
    manifest = json.loads(path.read_text())
    binaries = {mode: Path(entry["path"]) for mode, entry in manifest["executables"].items()}
    for mode, entry in manifest["executables"].items():
        if sha256(binaries[mode]) != entry["sha256"]:
            raise ValueError(f"{binaries[mode]} no longer matches images.json")
    inputs = {name: Path(getattr(args, name)).resolve()
              for name in ("customer", "orders", "lineitem")}
    for name, source in inputs.items():
        if sha256(source) != manifest["inputs"][name]["sha256"]:
            raise ValueError(f"{source} is not the input the other images loaded")
    arms = [(mode, front_code) for mode in MODES for _, front_code in IMAGE_ARMS]
    pending = [arm for arm in arms if image_name(*arm) not in manifest["images"]]
    for arm in pending:
        directory = root / image_name(*arm)
        if directory.exists():
            if any(directory.iterdir()):
                raise ValueError(f"{directory} exists and is not empty")
            directory.rmdir()
    available = memory_available_bytes()
    caches = ([args.cache_bytes] * len(pending) if args.cache_bytes
              else weighted_load_cache_bytes(available, [mode for mode, _ in pending]))
    manifest["configuration"]["completion"] = {
        "images": [image_name(*arm) for arm in pending], "mem_available_bytes": available,
        "cache_bytes": dict(zip((image_name(*arm) for arm in pending), caches)),
        "cache_source": "--cache-bytes" if args.cache_bytes else (
            f"{LOAD_MEMORY_SHARE} of MemAvailable, weighted by mode {LOAD_MODE_WEIGHTS}"),
        "concurrent_loads": len(pending)}
    manifest["errors"] = []
    try:
        with concurrent.futures.ThreadPoolExecutor(max_workers=max(len(pending), 1)) as pool:
            futures = {image_name(*arm): pool.submit(load_image, root, binaries, inputs, *arm,
                                                     cache, args)
                       for arm, cache in zip(pending, caches)}
        for name, future in futures.items():
            try:
                manifest["images"][name] = future.result()
            except Exception as error:
                manifest["errors"].append(f"{type(error).__name__}: {error}")
        manifest["status"] = ("complete" if not manifest["errors"]
                              and all(image_name(*arm) in manifest["images"] for arm in arms)
                              else "failed")
    finally:
        try:
            subprocess.run([str(EXPERIMENT / "generate.sh")], cwd=EXPERIMENT, check=True)
        except Exception as error:
            manifest["errors"].append(f"native generation restoration failed: {error}")
            manifest["status"] = "failed"
        write_json(path, manifest)
    return 0 if manifest["status"] == "complete" else 1


def build_images(args: argparse.Namespace) -> int:
    """The load program: load the base files once per mode and prefix-truncation arm into an
    image (`--write-image`), outside any cgroup and with no memory target, since loading is not
    measured (user decision of 2026-11-17). Writes `images.json` beside the images."""
    root = args.build_images.resolve()
    if root.exists():
        print(f"measure: {root} already exists", file=sys.stderr)
        return 2
    root.mkdir(parents=True)
    path = root / IMAGES_MANIFEST
    manifest: dict[str, Any] = {
        "status": "failed", "revision": None, "git_status_short": None,
        "platform": platform.platform(),
        "storage_device": storage_device(root, args.device_label),
        "configuration": {"cache_bytes": None, "batch": args.batch,
                          "compaction_timeout_secs": args.compaction_timeout_secs,
                          "setup_full_compaction": args.setup_full_compaction,
                          "elide_value_headers": args.elide_value_headers,
                          "index_prefix_filter": args.index_prefix_filter,
                          "memory_target_bytes": None, "cgroup": None},
        "inputs": {}, "executables": {}, "images": {}, "errors": [],
    }
    # The loads are independent and unmeasured, and each uses one DBSP worker, so by default
    # they run at once and share the host's memory; `--sequential-loads` gives each the whole
    # budget instead. Every load finishes before any failure is reported, and each image that
    # loads is recorded.
    arms = [(mode, front_code) for mode in MODES for _, front_code in IMAGE_ARMS]
    concurrent_loads = 1 if args.sequential_loads else len(arms)
    available = memory_available_bytes()
    cache_bytes = args.cache_bytes or load_cache_bytes(available, concurrent_loads)
    manifest["configuration"].update({
        "cache_bytes": cache_bytes, "concurrent_loads": concurrent_loads,
        "cache_source": "--cache-bytes" if args.cache_bytes else (
            f"{LOAD_MEMORY_SHARE} of MemAvailable / concurrent_loads" if available is not None
            else "fallback: MemAvailable unknown"),
        "mem_available_bytes": available})
    try:
        manifest["revision"], manifest["git_status_short"] = git_state()
        inputs = {name: Path(getattr(args, name)).resolve()
                  for name in ("customer", "orders", "lineitem")}
        for name, source in inputs.items():
            if not source.is_file():
                raise FileNotFoundError(source)
            manifest["inputs"][name] = {"path": str(source), "sha256": sha256(source),
                                        **file_identity(source)}
        binaries = build_executables(root / "executables")
        manifest["executables"] = {mode: {"path": str(binary), "sha256": sha256(binary)}
                                   for mode, binary in binaries.items()}
        # Taken after `build_executables`, whose `generate.sh` wrote the native circuits.
        manifest["image_sources_sha256"] = image_sources_sha256()
        def load(mode: str, front_code: bool) -> dict[str, Any]:
            return load_image(root, binaries, inputs, mode, front_code, cache_bytes, args)

        with concurrent.futures.ThreadPoolExecutor(max_workers=concurrent_loads) as pool:
            futures = {image_name(*arm): pool.submit(load, *arm) for arm in arms}
        failures = []
        for name, future in futures.items():
            try:
                manifest["images"][name] = future.result()
            except Exception as error:
                failures.append(f"{type(error).__name__}: {error}")
        write_json(path, manifest)
        if failures:
            raise RuntimeError("; ".join(failures))
        manifest["status"] = "complete"
    except Exception as error:  # Keep every image built so far.
        manifest["errors"].append(f"{type(error).__name__}: {error}")
    finally:
        try:
            subprocess.run([str(EXPERIMENT / "generate.sh")], cwd=EXPERIMENT, check=True)
        except Exception as error:
            manifest["errors"].append(f"native generation restoration failed: {error}")
            manifest["status"] = "failed"
        write_json(path, manifest)
    return 0 if manifest["status"] == "complete" else 1


def read_images(root: Path) -> dict[str, Any]:
    """The manifest of the images in `root`, which must be complete."""
    manifest = json.loads((root / IMAGES_MANIFEST).read_text())
    if manifest.get("status") != "complete":
        raise ValueError(f"{root / IMAGES_MANIFEST} is not complete")
    return manifest


PREPARE_MANIFEST = "prepare-manifest.json"


def prepare_manifest_for(refresh_list: Path) -> Path | None:
    """The `prepare_inputs.py` manifest of a refresh list: `<out>/<workload>/refreshes.json` or
    `<out>/<workload>/warm-up/refreshes.json` has it at `<out>`. None for a hand-made list."""
    for directory in list(refresh_list.resolve().parents)[:3]:
        if (directory / PREPARE_MANIFEST).is_file():
            return directory / PREPARE_MANIFEST
    return None


def refresh_base_problems(refresh_list: Path, image_inputs: dict[str, Any]) -> list[str]:
    """Why the refreshes in `refresh_list` do not belong to the base tables the images loaded.

    The refreshes of one `--order-keys` scheme carry keys that exist only in that scheme's base
    tables, so they must not run against images loaded from another. A manifest written before
    `refresh_base` existed (dbgen keys only) names its base files under `inputs`.
    """
    path = prepare_manifest_for(refresh_list)
    if path is None:
        return []
    prepared = json.loads(path.read_text())
    base = prepared.get("refresh_base") or prepared["inputs"]
    return [f"{refresh_list} was prepared for {base[name]['path']}, but the images loaded "
            f"{image_inputs[name]['path']} (SHA-256 differs)"
            for name in ("customer", "orders", "lineitem")
            if base[name]["sha256"] != image_inputs[name]["sha256"]]


# The sources that define what an image holds (decision of 2026-11-18, replacing the
# byte-identical executable rule of 2026-11-17): DBSP's storage format, its batches, the merged
# index, the spine's merging and save/restore, the checkpointer, the circuits, the COL index, and
# the driver's reconstruction and workload code. A campaign reopens an image only when these
# files are unchanged (`image_sources_problems`). The driver's other code (measurement and
# reporting) may change freely; the driver's `ImageManifest::FORMAT` covers its own load and
# save path, the circuit fingerprint (DBSP) covers the circuits' shape, and `ImageSettings`
# covers the storage settings. Paths are relative to the repository root.
IMAGE_SOURCE_PATHS = (
    "crates/dbsp/src/storage/file",
    "crates/dbsp/src/trace/ord",
    "crates/dbsp/src/trace/merged_index",
    "crates/dbsp/src/trace/spine_async.rs",
    "crates/dbsp/src/trace/spine_async",
    "crates/dbsp/src/circuit/checkpointer.rs",
    "sql-to-dbsp-compiler/experiments/q3-ivm-baseline/q3-base-circuit/src",
    "sql-to-dbsp-compiler/experiments/q3-ivm-baseline/q3-view-circuit/src",
    "sql-to-dbsp-compiler/experiments/q3-ivm-baseline/q3-trie-view-circuit/src",
    "sql-to-dbsp-compiler/experiments/merged-index-col/col.mi",
    "sql-to-dbsp-compiler/experiments/merged-index-col/src",
    "sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/reconstruction.rs",
    "sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/workload.rs",
)
# What generates the two native circuit crates' untracked `lib.rs` (`generate.sh`): adopting a
# digest for existing images checks that these did not change either.
IMAGE_GENERATOR_PATHS = (
    "sql-to-dbsp-compiler/experiments/q3-ivm-baseline/base.sql",
    "sql-to-dbsp-compiler/experiments/q3-ivm-baseline/q3.sql",
    "sql-to-dbsp-compiler/experiments/q3-ivm-baseline/generate.sh",
    "sql-to-dbsp-compiler/SQL-compiler/src",
)
REPOSITORY = EXPERIMENT.parents[2]


def image_source_files(repository: Path, paths: tuple[str, ...]) -> list[Path]:
    """The regular files under `paths` (files or directories), relative to `repository` and
    sorted; build outputs and caches are skipped."""
    files = set()
    for name in paths:
        path = repository / name
        if path.is_file():
            files.add(path.relative_to(repository))
        elif path.is_dir():
            for found in path.rglob("*"):
                parts = found.relative_to(repository).parts
                if found.is_file() and not any(part in ("target", "__pycache__")
                                               or part.startswith(".") for part in parts):
                    files.add(found.relative_to(repository))
    return sorted(files)


def image_sources_sha256(repository: Path = REPOSITORY,
                         paths: tuple[str, ...] = IMAGE_SOURCE_PATHS) -> str:
    """A digest of the image sources: each file's relative path and contents, in path order, so
    that a changed, added, removed, or renamed file changes it."""
    digest = hashlib.sha256()
    for relative in image_source_files(repository, paths):
        digest.update(str(relative).encode() + b"\0")
        digest.update(sha256(repository / relative).encode() + b"\n")
    return digest.hexdigest()


def image_sources_problems(images: dict[str, Any], sources_sha256: str) -> list[str]:
    """Why this checkout may not reopen the images: their sources digest is missing or differs.
    The executables may differ; only the sources that define an image's content must match."""
    written_with = images.get("image_sources_sha256")
    if written_with is None:
        return ["the images record no image_sources_sha256: adopt it with "
                "--adopt-image-sources if the image sources are unchanged, or rebuild them"]
    if written_with != sources_sha256:
        return [f"the images were written from image sources {written_with}, but this checkout "
                f"has {sources_sha256}: a file in IMAGE_SOURCE_PATHS changed, so rebuild the "
                "images with --build-images"]
    return []


def status_path(line: str) -> str:
    """The repository-relative path of a `git status --short` line, which `git_state` takes in
    this directory, so its paths are relative to it; a rename gives its new path."""
    path = line[3:].strip().split(" -> ")[-1].strip('"')
    try:
        return str((EXPERIMENT / path).resolve().relative_to(REPOSITORY))
    except ValueError:
        return path


def adopt_image_sources(args: argparse.Namespace) -> int:
    """Records the sources digest in an image directory written before the digest existed,
    after checking with git that no image source or circuit generator changed since the
    images' revision, and that the images were not built from a modified tree."""
    root = args.adopt_image_sources.resolve()
    path = root / IMAGES_MANIFEST
    manifest = json.loads(path.read_text())
    if manifest.get("status") != "complete":
        print(f"measure: {path} is not complete", file=sys.stderr)
        return 2
    if manifest.get("image_sources_sha256"):
        print(f"measure: {path} already records image_sources_sha256", file=sys.stderr)
        return 2
    watched = IMAGE_SOURCE_PATHS + IMAGE_GENERATOR_PATHS
    dirty = [line for line in (manifest.get("git_status_short") or "").splitlines()
             if any(status_path(line).startswith(prefix) for prefix in watched)]
    if dirty:
        print(f"measure: the images were built with uncommitted image sources: {dirty}",
              file=sys.stderr)
        return 1
    changed = subprocess.run(["git", "diff", "--quiet", manifest["revision"], "--", *watched],
                             cwd=REPOSITORY)
    if changed.returncode != 0:
        print(f"measure: image sources changed since {manifest['revision']}; rebuild the images",
              file=sys.stderr)
        return 1
    # The native circuits' lib.rs are generated, not tracked: regenerate them from the
    # unchanged generators before taking the digest.
    subprocess.run([str(EXPERIMENT / "generate.sh")], cwd=EXPERIMENT, check=True)
    manifest["image_sources_sha256"] = image_sources_sha256()
    manifest["image_sources_adopted"] = {"revision": git_state()[0],
                                         "images_revision": manifest["revision"]}
    write_json(path, manifest)
    print(f"measure: adopted image_sources_sha256 {manifest['image_sources_sha256']}")
    return 0


def run(argv: list[str]) -> int:
    args = parse_args(argv)
    if getattr(args, "adopt_image_sources", None):
        return adopt_image_sources(args)
    if args.build_images:
        return build_images(args)
    if args.complete_images:
        return complete_images(args)
    report = args.report.resolve()
    campaign_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
    root = (args.storage_path or EXPERIMENT / "target" / "r5-measurements" /
            campaign_id).resolve()
    device = storage_device(root, args.device_label)
    problem = device_label_problem(device)
    if problem:
        print(f"measure: {problem}", file=sys.stderr)
        return 2
    manifest: dict[str, Any] = {
        "status": "failed", "revision": None, "git_status_short": None,
        "platform": platform.platform(),
        "storage_device": device,
        "configuration": {"runs": args.runs, "cache_bytes": args.cache_bytes,
                          "batch": args.batch,
                          "memory_target_bytes": args.memory_target_bytes,
                          "compaction_timeout_secs": args.compaction_timeout_secs,
                          "verify": not args.no_verify,
                          "diagnostics": args.diagnostics,
                          "setup_full_compaction": args.setup_full_compaction,
                          "front_code_byte_keys": args.front_code_byte_keys,
                          "elide_value_headers": args.elide_value_headers,
                          "index_prefix_filter": args.index_prefix_filter,
                          "memtable_mib": args.memtable_mib,
                          "session_pass": args.session_pass,
                          "total_memory_bytes": args.total_memory_bytes,
                          "cgroup_root": str(args.cgroup_root) if args.cgroup_root else None,
                          "images": str(args.images.resolve()) if args.images else None,
                          "warm_up": str(args.warm_up.resolve()) if args.warm_up else None},
        "inputs": {}, "executables": {}, "images": None, "runs": [], "errors": [],
    }
    try:
        manifest["revision"], manifest["git_status_short"] = git_state()
        images = None
        if args.images:
            images = read_images(args.images.resolve())
            manifest["images"] = {
                "directory": str(args.images.resolve()),
                "manifest_sha256": sha256(args.images.resolve() / IMAGES_MANIFEST),
                "inputs": images["inputs"], "images": images["images"]}
        if images:
            lists = [args.refresh_list] + ([args.warm_up] if args.warm_up else [])
            problems = [problem for refresh_list in lists
                        for problem in refresh_base_problems(refresh_list, images["inputs"])]
            if problems:
                raise ValueError("; ".join(problems))
            manifest["images"]["refresh_base_checked"] = [
                str(prepare_manifest_for(refresh_list)) for refresh_list in lists
                if prepare_manifest_for(refresh_list) is not None]
        refreshes, refresh_paths = refresh_inputs(args.refresh_list.resolve())
        inputs = {} if images else {name: Path(getattr(args, name)).resolve()
                                    for name in ("customer", "orders", "lineitem")}
        inputs.update(refresh_paths)
        if args.warm_up:
            _, warm_up_paths = refresh_inputs(args.warm_up.resolve())
            inputs.update({f"warm_up_{name}": path for name, path in warm_up_paths.items()})
            inputs["warm_up_list"] = args.warm_up.resolve()
        inputs["refresh_list"] = args.refresh_list.resolve()
        for name, path in inputs.items():
            if not path.is_file():
                raise FileNotFoundError(path)
            manifest["inputs"][name] = {"path": str(path), "sha256": sha256(path),
                                        **file_identity(path)}
        if args.cgroup_root:
            cgroup_available(args.cgroup_root)
        binaries = build_executables(EXPERIMENT / "target" / "r5-executables" / campaign_id)
        manifest["executables"] = {mode: {"path": str(path), "sha256": sha256(path)}
                                   for mode, path in binaries.items()}
        if images:
            manifest["image_sources_sha256"] = image_sources_sha256()
            problems = image_sources_problems(images, manifest["image_sources_sha256"])
            if problems:
                raise ValueError("; ".join(problems))
        for repetition in range(1, args.runs + 1):
            for mode in MODES:
                run_dir = root / f"run-{repetition}-{mode}"
                run_dir.mkdir(parents=True, exist_ok=False)
                record: dict[str, Any] = {"mode": mode, "repetition": repetition,
                                          "status": "failed", "storage_path": str(run_dir / "storage")}
                manifest["runs"].append(record)
                spec = {"run_dir": str(run_dir), "executable": str(binaries[mode]),
                        "refreshes": refreshes, "inputs": {k: str(v) for k, v in inputs.items()},
                        "input_identities": {
                            k: {"size": item["size"], "mtime_ns": item["mtime_ns"]}
                            for k, item in manifest["inputs"].items()},
                        "args": {"cache_bytes": args.cache_bytes,
                                 "batch": args.batch,
                                 "memory_target_bytes": args.memory_target_bytes,
                                 "compaction_timeout_secs": args.compaction_timeout_secs,
                                 "no_verify": args.no_verify,
                                 "diagnostics": args.diagnostics,
                                 "setup_full_compaction": args.setup_full_compaction,
                                 "front_code_byte_keys": args.front_code_byte_keys,
                                 "elide_value_headers": args.elide_value_headers,
                                 "index_prefix_filter": args.index_prefix_filter,
                                 "memtable_mib": args.memtable_mib,
                                 "session_pass": args.session_pass,
                                 "warm_up": str(args.warm_up.resolve()) if args.warm_up else None}}
                if images:
                    # Each run refreshes its own full copy of its mode's and arm's image, made
                    # outside the run's cgroup; the run evicts the copy before the driver starts.
                    name = image_name(mode, args.front_code_byte_keys)
                    if name not in images["images"]:
                        raise ValueError(f"{args.images} has no image {name}")
                    copy = run_dir / "image"
                    subprocess.run(["cp", "-a", images["images"][name]["directory"], str(copy)],
                                   check=True)
                    spec["image"] = str(copy)
                    record["storage_path"] = str(copy / "storage")
                    record["image"] = {"name": name,
                                       "checkpoint": images["images"][name]["checkpoint"]}
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
                    command = driver_command(spec, spec["inputs"])
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
                problems = (report_problems(raw, args.session_pass)
                            + index_prefix_filter_problems(raw, args.index_prefix_filter)
                            + memtable_mib_problems(raw, args.memtable_mib))
                if problems:
                    raise RuntimeError(f"{mode} run {repetition} report is inconsistent: "
                                       + "; ".join(problems))
                record["qualification"] = qualification(platform.system(),
                    args.total_memory_bytes, raw, verify=not args.no_verify,
                    enforced=record.get("cgroup", {}).get("enforced", False),
                    cold=(record.get("cgroup", {}).get("proof") or {}).get("inputs_evicted", False))
                record["status"] = "complete"
                if images and not args.keep_run_storage:
                    # The copy holds only the state after the run's refreshes, which no report
                    # reads; at SF 10 a native copy is about 19 GB, so the campaign's copies do
                    # not fit on the disks. A failed run keeps its copy for diagnosis.
                    shutil.rmtree(run_dir / "image")
                    record["storage_deleted"] = True
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
