# Compaction options for small transactions

> **Status (2026-11-19):** option 3, prefix filters, is built as an A/B arm, off by default ([decisions.md](decisions.md#2026-11-19-group-reads-skip-runs-by-a-prefix-filter)); a memtable that supersedes option 1 is designed in [memtable.md](memtable.md). The rest stays documentation only. The `sequence` order-key arm exposed cancelled rows piling up at the front of the key range; it is a watch item, below ([Watch for](#watch-for-cancelled-rows-at-the-front-of-the-key-range)).
>
> **Earlier status:** documentation only. The user deferred any compaction change on 2026-11-18 and asked for the options set against measurements, respecting the current spine design and aiming at a good layout for every spine, not only the merged index ([DECISIONS.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/DECISIONS.md#2026-11-18-compaction-is-deferred-its-options-are-documented-with-measurements)). Nothing here is decided. The figures are from the K = 10 pilots of 2026-11-18 (measurement version 14, one repetition, SF 10, about 12.5 rows per transaction), at DBSP's `critical` memory pressure throughout. The measurement version 15 counters (`run_counts`) will fill in the rows marked *pending*.

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
| 1 | *Superseded by the memtable ([memtable.md](memtable.md), phase 1 built 2026-11-20).* **Spill floor.** No file below X bytes even at high or critical pressure; smaller runs stay in memory until merges make them X. In effect a memtable bounded per spine. | `Runtime::min_insert_storage_bytes` and `min_step_storage_bytes` return X instead of 0. | The ~9 to 10 writes per transaction, and the L0 file runs each group read seeks. | Memory up to X per spine at critical pressure, against the resident cost of a small file run (its reader, trailer, index root, and range filter), which version 15 can measure. |
| 2 | **Skip runs by key bounds** on `seek_key`. | The seek loops in `CursorList` and `RunMerge` check each run's resident key range first. | Per-run seeks where a run's key range is narrow: the order-keyed traces (RF1's new keys interleave, RF2's are the lowest). | Little for the customer-folded index, whose small runs span many customers. |
| 3 | **Prefix filters** (RocksDB's prefix Bloom): a filter over a key prefix, such as the customer part of the index's folded key, so group seeks skip runs without that prefix. | A writer option, the probe in the seek path; stored partitioned like the Bloom filters (decision of 2026-11-18). | The index's 11 to 19 per-run seeks per group. | The prefix length per spine; for the index it comes from the `.mi` spec. |
| 4 | **Bound the run count** (RocksDB's universal compaction trigger): when a spine holds more than R runs, merge its youngest adjacent runs regardless of level thresholds. | `merge_is_eligible` and `try_start_merge`. | Runs per spine, hence per-run seeks. | More write amplification, competing for the one merger thread. |
| 5 | **Retune L0 and the merger**: a lower L0/L1 boundary, a lower L0 `MERGE_COUNTS` start, `merger_threads` above 1. | Existing dev tweaks and constants. | Merge backlog (*pending*: version 15's merges and merge time per refresh). | Does not remove small files; more merge I/O. |
| 6 | **Leveled compaction** (RocksDB's default): one sorted run per level, key-partitioned. | A new merger and key-partitioned runs. | Best read amplification. | A large departure from the spine; not proposed now. |

## Next measurements

The measurement version 15 rerun (A-seek clustered first) gives, per refresh: each spine's runs after each transaction, its merges and merge time, and device reads by reading thread and owner. With those, the table above can say how much of the reads per transaction comes from the merged index's group reads, from the base tables' exact probes, and from merges, which decides between options 1, 3, and 4.

## Watch for: cancelled rows at the front of the key range

Observed on 2026-11-19 in the `sequence` order-key arm; not fixed. The user decided to wait for the `uuid` arm before choosing a fix ([DECISIONS.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/DECISIONS.md#2026-11-19-the-sequence-arm-is-stopped-cancelled-rows-are-a-watch-item)).

### The phenomenon

A deletion in DBSP is a row of weight −1 in a newer run. The row disappears only when a merge brings it together with the +1 row it cancels. Until then a cursor over the spine sees both, and `CursorList` steps past every key whose weights sum to zero: `CursorList::new` calls `skip_zero_weight_keys_forward` before any seek, and `seek_key` calls it after landing (`trace/cursor/cursor_list.rs`).

Size-tiered merging keeps the pairs apart for a long time. The +1 rows of base data sit in a spine's largest run, at a high level; the −1 rows sit in the small recent runs that small transactions write. The two meet only when the levels below grow to the largest run's size and merge into it. At about 12.5 rows per transaction that does not happen within a pilot, and the levels hold no key range of their own (option 6 above), so no smaller merge can reach the cancelled rows.

When deletions remove the lowest keys and inserts land elsewhere, as a queue that ages out its oldest entries does, the cancelled pairs form one solid stretch at the front of the key range, longer after every deletion. Every new cursor walks the whole stretch before it reaches a live key.

### What the `sequence` pilot showed

The `sequence` arm keys Orders 1..N, RF1 appends above N, and RF2 deletes the lowest keys. In the 512/512 A-seek clustered K = 10 pilot (measurement version 16), the native run was stopped after 3 h 37 min; the same pilot with dbgen keys ran in 23 min.

- **Who read.** The process had read 86 GB, at about 7 MB/s, of a 19 GB native image. 85 GB came from the driver's main thread and 1 GB from the DBSP worker. The driver's main thread does the RF1/RF2 input checks and RF2 discovery (`BaseState` in `base_state.rs`), which open a new cursor over a base table's runs for each lookup.
- **What it read.** A 5 s trace of that thread (`strace -e pread64`) logged 4,060 reads (33 MB). Nearly all were from base Lineitem's largest run, a 5.26 GB file, and most of the rest from base Orders'. Each lookup read the run's trailer and index, then its data blocks in file order from offset 0, about 8 MB of them: the 15,000 Orders (about 60,000 Lineitems) of the RF2 refresh before. Each block was read about four times in the 5 s.
- **Why from the device.** Each base transaction evicts the base tables' blocks from the buffer cache (measurement version 16), so the next transaction's checks read the stretch again.
- **Why dbgen does not show it.** dbgen's RF1 keys fill the gaps of the very range RF2 deletes from (RF1 pair 1 inserts keys 9 to 60,008; RF2 pair 1 deletes 1 to 60,000), so live keys interleave with the cancelled ones and a walk ends within a few rows. The driver's main thread read 1.7 to 2.7 GB per refresh in the dbgen pilot (`run_counts.device_reads.by_read_owner.other`).

The checks are outside the measured phases, so the pilot measured nothing about the view. Whether DBSP's own operators pay the walk, wherever they open a cursor over a spine whose front is cancelled, was not measured.

### Existing solutions

| | System | Mechanism | Fit with DBSP's spine |
| --- | --- | --- | --- |
| 1 | RocksDB `CompactOnDeletionCollector` (`NewCompactOnDeletionCollectorFactory(sliding_window_size, deletion_trigger, deletion_ratio)`) | A table-properties collector marks a file for compaction when a sliding window of its entries holds at least `deletion_trigger` deletions, or a whole file at least `deletion_ratio` of them. | The trigger fits: a run whose −1 rows exceed a share would be merged down into the run holding their +1 rows. Without key-partitioned levels, that merge rewrites the whole largest run (5.26 GB here). |
| 2 | Cassandra `tombstone_threshold` (default 0.2), `tombstone_compaction_interval`, `unchecked_tombstone_compaction` | Compacts a single SSTable on its own when its share of droppable tombstones exceeds the threshold. | Close to 1. A DBSP run cannot drop a −1 on its own, since its +1 is in another run; the merge must include that run. |
| 3 | RocksDB leveled compaction | Each level is a set of files of about 64 MB with disjoint key ranges; a compaction rewrites only the files of the next level that overlap. | Pushes deletions down at the cost of the overlapping files only: the stretch at the front of the key range lives in one or two files. It is option 6 above, a large departure from the spine. 1 with 3 is RocksDB's design. |
| 4 | RocksDB's queue guidance (`iterate_lower_bound`, `DeleteRange`) | The application keeps the queue's head key and starts every iterator there, so iterators never walk the deleted prefix; a range deletion replaces per-key tombstones. | A read-side remedy: the cancelled rows stay, and every reader must know the head. Not a root-cause fix. |

### What to look for in later arms

- The driver's main-thread device reads (`run_counts.device_reads.by_read_owner.other`) and `checks_input_preparation_ms` or `discovery_ms` growing from refresh to refresh.
- `base_ms` or `ivm_ms` growing across refreshes while the transaction size stays fixed, in a workload whose deletions cluster in key order.
- The `uuid` arm deletes the oldest Orders too, but their keys are scattered, so its cancelled pairs should be spread over the key range rather than stretched at its front. If it still shows the growth, choose among 1 to 4.
