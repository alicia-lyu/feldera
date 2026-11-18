# Compaction options for small transactions

> **Status:** documentation only. The user deferred any compaction change on 2026-11-18 and asked for the options set against measurements, respecting the current spine design and aiming at a good layout for every spine, not only the merged index ([DECISIONS.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/DECISIONS.md#2026-11-18-compaction-is-deferred-its-options-are-documented-with-measurements)). Nothing here is decided. The figures are from the K = 10 pilots of 2026-11-18 (measurement version 14, one repetition, SF 10, about 12.5 rows per transaction), at DBSP's `critical` memory pressure throughout. The measurement version 15 counters (`run_counts`) will fill in the rows marked *pending*.

## The current policy, from the code

All in `crates/dbsp/src/trace/spine_async.rs` unless noted.

- **Levels by record count.** `Spine::size_to_level` puts a run at L0 up to 14,999 records (`MAX_LEVEL0_BATCH_SIZE_RECORDS`), at L1 up to 99,999, and ten times larger at each level up to L8. A merge output is placed by its own size, so a small output goes back into L0's queue.
- **Size-tiered merging.** A level merges once it holds enough runs (`MERGE_COUNTS`: 8 at L0 and L1, 3 from L2, 2 from L6; `min_batches_to_merge`), taking up to 64 or 128 of them, one merge per level at a time. Nothing keeps one sorted run per level. With nothing in flight, about 25 runs can wait in an integral spine with no merge due (`loose_batches_with_no_merge_due`).
- **One merger thread per worker.** `num_merger_threads` is `workers × DEFAULT_MERGER_THREAD_RATIO` (1), so with one worker one thread merges every spine of both circuits and the merged index.
- **Placement by pressure** (`circuit/runtime.rs`). At high or critical pressure every inserted run is written to a file at once (`min_insert_storage_bytes` is 0), and at critical every non-empty run passed between operators within a step too (`min_step_storage_bytes`). From moderate pressure every merge output is a file. Nothing coalesces transactions before writing: a run never holds more than one transaction until a merge combines runs.
- **Reads seek every run.** `CursorList::seek_key` (`trace/cursor/cursor_list.rs`) and the merged index's `RunMerge::seek` (`trace/merged_index/run_merge.rs`) seek each run not yet past the target, with no key-bounds check. Only exact probes (`seek_key_exact`) consult a run's range and membership filters. The merged index's group reads use no exact probe (decision of 2026-11-09).

## What the pilots measured

**Runs at the end of the window** (A-seek; loose and merging; from the circuit profiles and `index_layout`):

| Spine | native, clustered | native, scattered | reconstructed, clustered | reconstructed, scattered |
| --- | ---: | ---: | ---: | ---: |
| `LINEITEM` / `ORDERS` / `CUSTOMER` (base) | 39 / 22 / 2 | 45 / 27 / 2 | 32 / 36 / 2 | 26 / 30 / 2 |
| join traces (native only) | 51, 2, 50, 17 | 31, 2, 39, 14 | none | none |
| `V` | 34 | 25 | 21 | 23 |
| merged index | none | none | 25 | 35 |
| **total** | **217** | **185** | **116** | **116** |

The first join trace held 51 runs with only 9 waiting at a level, so most were inputs of merges in progress: the one merger thread was behind. The merged index held 75, 20, 55, 37, 33, and 25 runs (clustered) and 20, 14, 23, 33, 39, and 35 (scattered) at the ends of the six refreshes; its count after each transaction is *pending* (version 15).

**Cost per transaction** (reconstructed A-seek, per refresh, after the first):

| | clustered RF1 | scattered RF1 | RF2 |
| --- | ---: | ---: | ---: |
| touched groups | 1.00 | 2.49 | 2.47 |
| per-run seeks per group (`index_run_reads.session.seeks`) | 15 to 19 | 11 to 16 | 13 to 15 |
| device reads per transaction (process-wide) | 24 to 27 | 42 to 48 | 35 to 38 |
| storage writes per transaction (process-wide) | about 9.3 | about 9.4 | about 9.3 |

Native did about 22 device reads and 10.4 writes per transaction after its first refresh. A device read averaged 5.3 to 6.0 KiB (reconstructed) and 7.6 KiB (native). The split of reads by owner (base tables, index, view, merges) is *pending* (version 15, `device_reads`).

## The options

Each is generic to every spine and applies to both modes alike (the research rule of a fair mechanism).

| | Option | Change | Measured quantity it targets | Cost or risk |
| --- | --- | --- | --- | --- |
| 1 | **Spill floor.** No file below X bytes even at high or critical pressure; smaller runs stay in memory until merges make them X. In effect a memtable bounded per spine. | `Runtime::min_insert_storage_bytes` and `min_step_storage_bytes` return X instead of 0. | The ~9 to 10 writes per transaction, and the L0 file runs each group read seeks. | Memory up to X per spine at critical pressure, against the resident cost of a small file run (its reader, trailer, index root, and range filter), which version 15 can measure. |
| 2 | **Skip runs by key bounds** on `seek_key`. | The seek loops in `CursorList` and `RunMerge` check each run's resident key range first. | Per-run seeks where a run's key range is narrow: the order-keyed traces (RF1's new keys interleave, RF2's are the lowest). | Little for the customer-folded index, whose small runs span many customers. |
| 3 | **Prefix filters** (RocksDB's prefix Bloom): a filter over a key prefix, such as the customer part of the index's folded key, so group seeks skip runs without that prefix. | A writer option, the probe in the seek path; stored partitioned like the Bloom filters (decision of 2026-11-18). | The index's 11 to 19 per-run seeks per group. | The prefix length per spine; for the index it comes from the `.mi` spec. |
| 4 | **Bound the run count** (RocksDB's universal compaction trigger): when a spine holds more than R runs, merge its youngest adjacent runs regardless of level thresholds. | `merge_is_eligible` and `try_start_merge`. | Runs per spine, hence per-run seeks. | More write amplification, competing for the one merger thread. |
| 5 | **Retune L0 and the merger**: a lower L0/L1 boundary, a lower L0 `MERGE_COUNTS` start, `merger_threads` above 1. | Existing dev tweaks and constants. | Merge backlog (*pending*: version 15's merges and merge time per refresh). | Does not remove small files; more merge I/O. |
| 6 | **Leveled compaction** (RocksDB's default): one sorted run per level, key-partitioned. | A new merger and key-partitioned runs. | Best read amplification. | A large departure from the spine; not proposed now. |

## Next measurements

The measurement version 15 rerun (A-seek clustered first) gives, per refresh: each spine's runs after each transaction, its merges and merge time, and device reads by reading thread and owner. With those, the table above can say how much of the reads per transaction comes from the merged index's group reads, from the base tables' exact probes, and from merges, which decides between options 1, 3, and 4.
