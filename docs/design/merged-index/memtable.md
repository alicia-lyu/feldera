# A memtable for DBSP spines

**Status:** design only. Nothing here is built. The first version (user, 2026-11-19) built the leaves from the layer file's own data blocks. The user revised it the same day: the leaves hold typed items, and the seal copies them into a file. The current campaign plan measures order-key assignment and prefix Bloom filters first. The measurements that motivate this design are in [MEASUREMENT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/MEASUREMENT.md#one-budget-at-512-mib-base-tables-evicted-2026-11-19) (writes by kind at critical pressure) and in [compaction.md](compaction.md).

**Goal.**

- Initial run generation spans transactions. Out-of-order inserts go into an in-memory structure under its own budget, so the memtable's contents never depend on RSS pressure (user, 2026-11-19).
- The memtable is a B+-tree whose leaves are small, typed, sorted runs. The seal is one ordered pass that copies the leaves into the standard file writer (user, 2026-11-19). Sealed files are packed like any other file.
- No new dependency. `imbl::OrdMap` and an ART were considered; see [Why the seal copies](#why-the-seal-copies).
- No prefix truncation in memory. Sealed files get prefix truncation, value-header elision and compression from the writer's settings, as any file does.

**Terms.**

- A *Z-set* maps each row to an integer weight: positive for insertions, negative for deletions.
- Feldera's code calls the generic form, with any weight type `R`, a *W-set* (weighted set): `OrdWSet`, and `OrdZSet` when `R` is the `i64` weight.
- An *indexed* W-set maps each key to values with weights. Join traces and the merged index are indexed W-sets.
- *Key leaf*, *row boundaries*, *value branch*, *value leaf* and *item-offset map* are the file-run terms of the advisor deck's Appendix E, below.

## The file run a seal must produce

The advisor deck's Appendix E ([figure](slides-oct-8/figures/row-traversal.tex), sources in [SOURCE-EVIDENCE.md](slides-oct-8/SOURCE-EVIDENCE.md)) follows one key through a file run. An indexed W-set is two columns: keys, and `(value, weight)` items. Each column is a B-tree: its leaves are data blocks, its inner nodes index blocks ("branches"), and the file's trailer names each column's root.

1. **Key leaf.** Key 43 is item 1 of its leaf. The leaf's row boundaries `[0, 2, 4, 6]` give key 43 the value rows `[2, 4)`. These are row numbers in the whole value column, not in one block.
2. **Value branch.** Row 2 falls in child 1, by the cumulative row totals `[2, 4, 6]`. The branch's child offsets and sizes (512-byte units, compressed) locate that leaf in the file.
3. **Value leaf.** Its item-offset map points to each serialized item, `(8, +1)` and `(15, +1)`; the walk stops before row 4.

A reader descends from the root, keeping the branches it passed; moving to the next leaf goes back through that path, since leaves are not linked. The writer closes a leaf at `min_data_block` (8 KiB) once `min_branch` (32) items fit (`DataBlockBuilder::try_add_item`, `writer.rs`), so a column has about its bytes / 8 KiB leaves, and a branch of small keys has hundreds of children.

Three of the appendix's numbers exist only once the file is laid out in key order: global row boundaries (they count every earlier key's values), cumulative row totals, and child offsets and sizes (they depend on each compressed leaf). Any seal therefore makes one ordered pass that assigns them. Today's writer is that pass.

## Why the seal copies

**The first design** made each memtable leaf a slotted key leaf or value leaf in the appendix's format, so that the seal only assigned the three laid-out numbers: it rebased each key leaf's local row boundaries and let the writer build the branches over finished leaves. Assembly without a copy works only at leaf granularity, and an insert-efficient leaf does not make a good file leaf:

- **Packing.** A leaf that splits on insert averages about two-thirds of the writer's 8 KiB. A sealed file would get smaller, uneven leaves and more branch entries.
- **Garbage.** Updates and zero weights leave items that the item-offset map no longer points to. They must be removed at seal, which is a copy after all.
- **Encodings.** Prefix truncation and value-header elision rewrite a whole leaf, so slotted leaves could not use them.
- **Cost of the bridges.** It needed a writer entry point for finished leaves, a `DataBlock` over memory leaves, and a shared rkyv serializer.

No insert-efficient structure produces packed leaves, so good files need a pass at seal, and the pass already exists. The structure to choose is the one whose in-order scan is cheapest:

- **B+-tree leaves.** Each leaf is a set of contiguous sorted vectors. Leaves are disjoint and in key order, so the seal reads leaf after leaf, with no comparisons between leaves and no pointer chasing inside one.
- **`imbl::OrdMap`.** Also a B-tree, but its nodes are sized by entry count, not bytes. Every node is reference-counted, and after a snapshot each insert copies a path of nodes. Its iterator yields single pairs, not slices.
- **ART** (byte keys, so merged indexes only). An in-order scan walks the inner nodes, and each entry is a separate heap leaf.

These are expected costs, not measurements. The arm measures the seal's CPU.

## What is reused, and how the memtable becomes a file run

| Reused piece | Where | Role in the memtable |
| --- | --- | --- |
| `ListMerger::merge` and the file builder | `trace/spine_async/list_merger.rs`, `trace/ord/file/` | **The seal.** The memtable cursor is the merger's one input; with one cursor, its heaps are trivial, and the merge is a copy in key order. The builder packs blocks and builds the index blocks, filters (Bloom, prefix once built, range) and trailer as for any file. |
| The `VecIndexedWSet` layout (keys, offsets, values, weights) and the factories' `DynVec` | `trace/ord/vec/`, `trace.rs` | **A leaf is a small run in this layout.** Items are typed, as in any in-memory run. |
| Fallback batch enums (`Inner { Vec, File }`) and `DelegatingCursor` | `trace/ord/fallback/wset.rs`, `indexed_wset.rs` | The frozen memtable is a third variant, `Inner::Memtable`. Every reader (`CursorList`, `SpineCursor`, `RunMerge`, replay) takes it unchanged. |
| Spine snapshots under the `SharedState` lock; `size_to_level`, `MERGE_COUNTS`, the merger runtime | `spine_async.rs` | A snapshot includes the frozen memtable. The sealed file run is added at its level and compacted from then on like any other. |
| `pick_merge_destination` | `trace/ord/fallback/utils.rs` | It decides whether a seal writes a file or keeps the frozen memtable as an in-memory run. |
| `persisted()`, `Spine::save`/`restore` | Fallback batches, `spine_async.rs` | `save` seals first. The checkpoint lists files only; its format is unchanged. |
| `Accumulator` and `AccumulateUntimedTraceAppend` | `operator/dynamic/` | The per-transaction accumulator spine gets a memtable too, and the integral adopts or absorbs it at flush. |
| Pressure levels, the `StorageOptions` plumbing, `RunWriteKind`, `SpineRunCounts` | `circuit/runtime.rs`, `feldera-types`, `storage/diagnostics.rs` | The budget, the knob and the counters follow these patterns. |

The life of an update:

1. A transaction's delta reaches `Spine::insert`.
2. Its items are inserted into the right leaves, in any order.
3. A snapshot sees the frozen memtable as one in-memory run.
4. At the memtable's budget or its spine's cap, the seal streams the leaves, in order, through `ListMerger` into the file builder.
5. The file run enters the spine's levels.

## What is genuinely new

**(a) The leaf.** A small run in the `VecIndexedWSet` layout; a W-set's leaf has keys and weights only. A leaf splits at a byte target. In Appendix E's terms, one memtable leaf holds a key leaf's keys and their value rows together:

- its offsets vector is the key leaf's row boundaries, but local: they count from the leaf's first value, so an insert shifts only the later offsets of its own leaf;
- its values and weights are the value rows themselves, typed, so it needs no item-offset map;
- the seal turns the local boundaries into global ones as the writer counts rows.

- **Insert.** A binary search, then a vector insert of the typed item.
- **Weight update.** In place.
- **Zero weight.** The item is removed, so leaves hold no garbage.
- **A key across two leaves.** A split may fall between the values of one key. That key is then repeated as the next leaf's first key, and the cursor joins the two parts.

```text
insert(k, v, w):
    leaf = leaf_index.find(k, v)          // binary search on leaf bounds
    leaf = make_mut(leaf)                 // copy once if a snapshot holds it
    i = leaf.position(k)                  // existing key, or insert k
    j = leaf.value_position(i, v)         // within k's value range
    if found: leaf.weight[j] += w; if zero: remove j (and k if now empty)
    else:     insert (v, w) at j; shift the later offsets of the leaf
    if leaf.bytes > target: split leaf
```

**Single writer.** Each memtable has one writer, the thread that owns its spine (user, 2026-11-19). Frozen views are immutable and readable from any thread: the merged index's `Old` snapshot is read on the view worker, and background merges read snapshots on the merger thread. A spine written by more than one thread falls back to default run generation; none is today, in DBSP or the Q3 driver. So the active memtable needs no lock, and only its frozen view enters `SharedState`. Copy-on-write is still needed on one thread: a join's delayed trace holds the previous transaction's snapshot across yields while the same worker appends to the spine.

**(b) The leaf index.** A two-level copy-on-write vector of `Arc<Leaf>`, kept in key order, with each leaf's bounds and row count. It plays the value branch's role, with pointers in place of child offsets and sizes.

- A seek is Appendix E's walk in memory: a binary search on bounds, then a search in the leaf, whose offsets give the key's values directly.
- A snapshot clones the top-level `Arc`.
- Writing a leaf after a snapshot copies that leaf (`Arc::make_mut`) and its chunk of the vector; nothing else is copied.
- Concurrency is as today: one worker writes each spine, frozen views are immutable, and one mutex guards the run list.

**(c) The memtable cursor.** It implements `Cursor` and `MergeCursor`: the leaf index for navigation, then the leaf's vectors.

- It joins a key that spans two leaves into one key with one value sequence.
- `seek_key_exact` is exact, with no filter. `seek_key_in_prefix` (the prefix-filter plan) answers from the leaves.
- Read counters count `memory_*` reads, following `VecIndexedWSetCursor`'s rules.

**(d) The seal: one ordered copy.**

- `ListMerger::merge(factories, builder, vec![memtable_cursor])`, with the file builder.
- The writer serializes each item once, packs the key and value leaves, and assigns the three laid-out numbers: global row boundaries, cumulative row totals, and child offsets and sizes. Filters, prefix truncation and compression follow its settings, unchanged.
- **Placement.**
  - At low pressure, the seal keeps the frozen memtable as an in-memory run, with no copy. From then on it counts in RSS under the normal rules.
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
- **The budget.** `MemtableBudget` tracks the live leaf bytes of the active and the flushing memtables. A flushing memtable counts until its file is closed. Above the budget, it flushes the largest active memtable.
- **The pressure rule.** `raw_memory_pressure` compares `rss − live_memtable` against `max_rss − memtable_budget`. So filling a memtable never raises pressure, and RSS stays within `max_rss`.
- **Startup** refuses a budget of `max_rss / 2` or more.

**(i) Diagnostics.**

- `RunWriteKind::MemtableFlush` and `MemtableCheckpointFlush`.
- `SpineRunCounts` and q3 `run_counts` gain `memtable_entries`, `memtable_bytes`, `flushing` and `memtable_flushes`.
- Metrics `MEMTABLE_*`.
- `spill_kinds.py` moves into `q3-ivm-baseline/`, with the flush kinds added.

## The arm and its limits

- **The q3 arm keeps one budget:** target 512 MiB = cache 448 MiB + memtable 64 MiB. A cache equal to the target stays at critical pressure by construction (RSS about 720 MiB in the 512/512 pilot). The split is recorded in `DECISIONS.md` before measuring. A 448 MiB control with the memtable off separates the memtable's effect from the smaller cache.
- **What it will not remove.** Step-built files: operators build them straight into files at critical pressure, since `min_step_storage_bytes` is 0. They were 6.6 of native's 8.7 view files per transaction. They are reported, not changed here.

## Stages, tests, run

1. **The leaf.**
   - Insert, update, zero removal and split, as unit tests, including a split between the values of one key.
   - A proptest against a `BTreeMap<(K, V), R>` model.
2. **Leaf index and cursor.**
   - Cursor conformance against `VecIndexedWSet`, every `Cursor` and `MergeCursor` method included, with keys that span leaves.
   - Snapshot isolation across later inserts.
3. **The seal.**
   - A sealed memtable and a file built by today's builder from the same rows read identically: proptest, both W-set kinds, and Bloom and prefix filters.
   - Compression and prefix truncation on and off.
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

- the seal's CPU: one serialization per item, the same work as any file build today, against a batch sort plus a build per transaction;
- the CPU of per-item inserts, and the vector memmove for wide rows;
- peak memory during a seal, since the flushing memtable counts in the budget until its file is closed;
- leaf copies after a snapshot (one copy per touched leaf per transaction);
- accounting drift against RSS (report the delta).

**Run.** A-seek clustered, K = 10, prefix filters on, three arms:

- 512/512 with the memtable off;
- 448 cache with the memtable off;
- 448 cache + 64 MiB memtable.

Report, view first: IVM percentiles, runs per spine, per-run seeks per group, view writes by kind, flush count and size, the seal's CPU, and pressure per refresh.
