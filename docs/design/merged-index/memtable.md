# A memtable for DBSP spines

**Status:** design only (user, 2026-11-19). Nothing here is built. The current campaign plan measures order-key assignment and prefix Bloom filters first. The measurements that motivate this design are in [MEASUREMENT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/MEASUREMENT.md#one-budget-at-512-mib-base-tables-evicted-2026-11-19) (writes by kind at critical pressure) and in [compaction.md](compaction.md).

**Goal.**
- Initial run generation spans transactions. Out-of-order inserts go into an in-memory structure under its own budget, so the memtable's contents never depend on RSS pressure (user, 2026-11-19).
- The memtable is the in-memory form of a file run (user, 2026-11-19): it is built from the layer file's own data blocks, made insertable, so that sealing assembles a file run from them. Items are not re-serialized, and no merge pass is needed.
- This replaces the `imbl` design, so there is no new dependency and no licence question. An ART for byte-keyed spines stays a possible later arm.
- No prefix truncation in memory: sealed blocks are written as they are (user: not worth it now).

**Terms.**
- A *Z-set* maps each row to an integer weight: positive for insertions, negative for deletions.
- Feldera's code calls the generic form, with any weight type `R`, a *W-set* (weighted set): `OrdWSet`, and `OrdZSet` when `R` is the `i64` weight.
- An *indexed* W-set maps each key to values with weights. Join traces and the merged index are indexed W-sets.
- In a file, an indexed W-set is two columns: column 0 holds the keys, column 1 the `(value, weight)` items. Each column is a B-tree of data blocks under index blocks.

## What is reused, and how the memtable becomes a file run

**What a file run is today** (`storage/file/format.rs`).
- **A data block** holds four things:
  - a `DataBlockHeader`;
  - the items, each serialized on its own with rkyv;
  - a *value map*: the byte offset of each item, in logical order;
  - in every column but the last, *row groups*: for row `i`, the range of rows in the next column.

  The order of the items' bytes inside a block does not matter; the value map gives their logical order.
- **An index block** holds, for each child, its bounds, cumulative row totals, child offsets and child sizes.
- **The blocks' order** in a file is unspecified, and the trailer comes last.

| Reused piece | Where | Role in the memtable |
| --- | --- | --- |
| The data block format: header, rkyv items, value map, row groups | `format.rs`, `writer.rs` | **A memtable leaf is a data block in the file format.** Its value map and row groups use a fixed 4-byte width in memory, which the format allows (`Varint`). |
| The data block decoder (`DataBlock`: item access by value map, row groups, key decoding) | `reader.rs` | The memtable cursor reads leaves with the file reader's own decoder. Only the navigation between leaves is new. |
| `Item<K, A>`, the concrete key, value and weight types, and the factories | `storage/file/mod.rs`, `trace.rs` | Items are serialized with the same rkyv serializer the writer uses, so the bytes in a leaf are the bytes a file would hold. |
| The column writer's index building, block padding, checksums, compression, trailer, and filters (Bloom, prefix, once built, range) | `writer.rs` | At seal, the leaves are handed to the writer as finished data blocks. The writer builds the index blocks, filters and trailer as it does for any file. |
| Insertion of written blocks into the buffer cache (`from_raw_with_cache`) | `writer.rs`, buffer cache | A sealed leaf's buffer goes into the cache as it is written, so the memtable's memory becomes cache entries without a copy. |
| Fallback batch enums (`Inner { Vec, File }`) and `DelegatingCursor` | `trace/ord/fallback/wset.rs`, `indexed_wset.rs` | The frozen memtable is a third variant, `Inner::Memtable`. Every reader (`CursorList`, `SpineCursor`, `RunMerge`, replay) takes it unchanged. |
| Spine snapshots under the `SharedState` lock; `size_to_level`, `MERGE_COUNTS`, the merger runtime | `spine_async.rs` | A snapshot includes the frozen memtable. The sealed file run is added at its level and compacted from then on like any other. |
| `persisted()`, `Spine::save`/`restore` | Fallback batches, `spine_async.rs` | `save` seals first. The checkpoint lists files only; its format is unchanged. |
| `Accumulator` and `AccumulateUntimedTraceAppend` | `operator/dynamic/` | The per-transaction accumulator spine gets a memtable too, and the integral adopts or absorbs it at flush. |
| Pressure levels, the `StorageOptions` plumbing, `RunWriteKind`, `SpineRunCounts` | `circuit/runtime.rs`, `feldera-types`, `storage/diagnostics.rs` | The budget, the knob and the counters follow these patterns. |

The life of an update:
1. A transaction's delta reaches `Spine::insert`.
2. Its items are serialized into the right leaves, in any order.
3. A snapshot sees the frozen memtable as one in-memory run.
4. At the memtable's budget or its spine's cap, the leaves are written to a file **as they are**. The only changes are the row-group rebase, the header, and the checksum.
5. The writer adds index blocks, filters and the trailer, and the file run enters the spine's levels.

## What is genuinely new

**(a) The slotted leaf.** This is the classic slotted page, after Graefe, "Modern B-Tree Techniques" (Foundations and Trends in Databases, 2011). LMDB likewise keeps its pages in the same format in memory and on disk.
- **Insert.** The new item is serialized with rkyv and appended to the leaf's item area. Its offset is inserted into the value map at its sorted position, a binary search plus a memmove of the 4-byte offsets. Items stay valid when moved, because each is serialized on its own.
- **Weight update.** The archived weight is updated in place; it is fixed-size.
- **Zero weight.** The offset is removed from the value map. The item's bytes become garbage, reclaimed when the leaf splits or is sealed.
- **Split.** A leaf over the target block size (the writer's data block size) splits in two. Each half is rebuilt by copying its live items' bytes, with no re-serialization.
- **Two columns** (indexed W-sets): each key leaf owns the value leaves that hold its keys' values.
  - The key leaf's row groups are *local*: rows in its own value leaves, counted from 0.
  - Inserting a value for key `i` puts it in that key's row range and adds 1 to the later row-group entries of the same key leaf, a fixed-width array.
  - A W-set has only column 0, with the weight as its auxiliary data.

**Single writer.** Each memtable has one writer, the thread that owns its spine (user, 2026-11-19). Frozen views are immutable and readable from any thread: the merged index's `Old` snapshot is read on the view worker, and background merges read snapshots on the merger thread. A spine written by more than one thread falls back to default run generation; none is today, in DBSP or the Q3 driver. So the active memtable needs no lock, and only its frozen view enters `SharedState`. Copy-on-write is still needed on one thread: a join's delayed trace holds the previous transaction's snapshot across yields while the same worker appends to the spine.

**(b) The leaf index.** A two-level copy-on-write vector of `Arc<Leaf>`, kept in key order, with each leaf's bounds and row count.
- A seek is a binary search on bounds, then the leaf's own value map.
- A snapshot clones the top-level `Arc`.
- Writing a leaf after a snapshot copies that leaf (`Arc::make_mut`) and its chunk of the vector; nothing else is copied.
- Concurrency is as today: one worker writes each spine, frozen views are immutable, and one mutex guards the run list.

**(c) The memtable cursor.** The leaf index for navigation, plus `DataBlock` for item access. In an indexed W-set, a key's local row group is rebased by its key leaf's offset into the value rows.
- `seek_key_exact` is exact, with no filter. `seek_key_in_prefix` (the prefix-filter plan) answers from the leaves.
- Read counters follow `VecIndexedWSetCursor`'s rules.

**(d) The seal: assemble, don't merge.**
- Walk the leaves in order. For each leaf:
  1. drop its garbage (copying live item bytes) if it has any;
  2. rebase its row groups by the running row total;
  3. set the header and recompute the checksum;
  4. pass it to the writer as a finished data block.
- The bridge is the writer's new `write_data_block(raw, bounds, n_rows)`: the column writer's existing index building (bounds, row totals, offsets) takes a block it did not pack.
- Filters are built at seal from the keys, since a Bloom filter cannot delete. The writer hashes each key it decodes through `DataBlock`.
- **Placement.**
  - At low pressure, the seal can keep the leaves as an in-memory run. By then they are no longer the memtable's: they count in RSS under the normal rules.
  - At moderate pressure or above, the seal writes a file.
  - The rule is `pick_merge_destination`'s.

**(e) Seals follow RocksDB.**
- At any time there is one active (mutable) memtable per spine, plus at most one immutable memtable being flushed (RocksDB's `max_write_buffer_number` = 2).
- When the active memtable fills while the previous one is still flushing, the insert stalls until the flush ends, as in RocksDB's write stall. The stall is recorded with the backpressure metric.
- Flushes run on the merger runtime. `is_merging()` counts a flush in progress.

**(f) Spine hooks** (`spine_async.rs`).
- **The fields.** `Spine.memtable`, plus `memtable_view` and `flushing` in `SharedState`.
- **`get_batches()`** returns the runs, then the flushing memtable, then the active memtable's frozen view.
- **`insert`.**
  - A batch above the cap / 4 bypasses the memtable as a normal run.
  - Otherwise its items are inserted.
  - `spill_if_due`, `min_insert_storage_bytes` and `min_step_storage_bytes` are not consulted.
- **The other operations.**
  - `save` flushes synchronously.
  - `fork` shares the leaves, copy-on-write.
  - `consolidate`, `complete_merges` and `initiate_compaction` flush first.
  - `restore` and `insert_without_blocking` stay run-based.
- **The hook.** `Batch::new_memtable(&Factories)` defaults to `None`. Only the Fallback W-set and indexed W-set override it. Batches with timestamps get none: a stated limit.

**(g) Unsorted input.** `Spine::insert_tuples` inserts tuple by tuple, which is where the sort is handed to the memtable.
- The sort can be skipped only where the trace is the sole consumer.
- Operator outputs and input handles feed other operators' cursors and stay sorted.
- `ManualIndex`'s `Dlt` is also published to the readers, so its sort stays, but its insert becomes inserts into the memtable.

**(h) The budget, exempt from pressure.**
- **Knob.** `StorageOptions::memtable_mib` (off by default; `openapi.json` hand-edited). A per-spine cap dev tweak defaults to the budget / 4, clamped to [1, 256] MiB.
- **The budget.** `MemtableBudget` tracks the live leaf bytes, of the active and the flushing memtables. Above the budget, it flushes the largest active memtable.
- **The pressure rule.** `raw_memory_pressure` compares `rss − live_memtable` against `max_rss − memtable_budget`. So filling a memtable never raises pressure, and RSS stays within `max_rss`.
- **Startup** refuses a budget of `max_rss / 2` or more.

**(i) Diagnostics.**
- `RunWriteKind::MemtableFlush` and `MemtableCheckpointFlush`.
- `SpineRunCounts` and q3 `run_counts` gain `memtable_entries`, `memtable_bytes`, `memtable_garbage_bytes`, `flushing` and `memtable_flushes`.
- Metrics `MEMTABLE_*`.
- `spill_kinds.py` moves into `q3-ivm-baseline/`, with the flush kinds added.

## The bridges

- **`Writer::write_data_block`.** The one change to the writer: the column writer accepts a finished data block, together with its bounds and row count, and feeds its existing index building. A test writes the same rows both ways and compares the files' reads.
- **`DataBlock` over a memory leaf.** The decoder is already a view over raw bytes. The memtable cursor builds it over `Arc<Leaf>` bytes, with a row-group base offset.
- **The rkyv serializer.** The writer's per-item serializer is factored out, so leaves and the writer produce identical item bytes.
- **The q3 arm keeps one budget:** target 512 MiB = cache 448 MiB + memtable 64 MiB. A cache equal to the target stays at critical pressure by construction (RSS about 720 MiB in the 512/512 pilot). The split is recorded in `DECISIONS.md` before measuring. A 448 MiB control with the memtable off separates the memtable's effect from the smaller cache.
- **What it will not remove.** Step-built files: operators build them straight into files at critical pressure, since `min_step_storage_bytes` is 0. They were 6.6 of native's 8.7 view files per transaction. They are reported, not changed here.

## Stages, tests, run

1. **The slotted leaf.**
   - Insert, update, zero removal, split and garbage reclaim, as unit tests.
   - A proptest against a `BTreeMap<(K,V),R>` model.
   - `DataBlock` decodes every leaf state.
2. **Leaf index and cursor.**
   - Cursor conformance against `VecIndexedWSet`, every `Cursor` method included.
   - Snapshot isolation across later inserts.
3. **The seal.**
   - A sealed memtable and a file built by today's builder from the same rows read identically: proptest, both W-set kinds, and Bloom and prefix filters.
   - Sealed blocks reach the buffer cache.
   - Compression on and off.
4. **Run variant and hook.**
   - Mixed Vec, File and Memtable runs in `CursorList` and `RunMerge`.
   - `persisted` round trip.
5. **Budget and pressure.**
   - Extend `memory_pressure_thresholds_and_spill_behavior`.
   - The largest memtable is flushed first.
   - Startup validation.
6. **Spine.**
   - The `trace/test.rs` spine proptests, with the knob on and off.
   - No runs below the cap, then one file run at its level.
   - No `InsertSpill` at forced critical pressure.
   - The bypass.
   - One active and one flushing memtable, then a stall.
   - Save and restore with a non-empty memtable.
   - Fork while flushing.
   - A held snapshot unchanged by later inserts.
7. **`insert_tuples`**: a proptest against `dyn_from_tuples` + `insert`.
8. **Harness.**
   - `verify.sh` transcripts are byte-identical with the knob on, in both modes.
   - The images are rebuilt as `images-v18`, since `spine_async.rs`, `trace/ord/**` and `storage/file/**` are in the digest. Measurement version 18.
9. **Docs.**
   - compaction.md option 1 (spill floor) is superseded.
   - A memtable section in `crates/dbsp/CLAUDE.md`.
   - Dated entries in `decisions.md` and `DECISIONS.md`.

**Risks, measured in the arm:**
- the CPU of a per-item rkyv serialization and an offset memmove, against a batch sort plus a build;
- garbage from repeated updates and cancellations (reported as `memtable_garbage_bytes`; reclaimed at split and flush);
- leaf copies after a snapshot (one leaf per touched leaf per transaction);
- accounting drift against RSS (report the delta).

**Run.** A-seek clustered, K = 10, prefix filters on, three arms:
- 512/512 with the memtable off;
- 448 cache with the memtable off;
- 448 cache + 64 MiB memtable.

Report, view first: IVM percentiles, runs per spine, per-run seeks per group, view writes by kind, flush count and size, and pressure per refresh.

