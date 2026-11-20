# A memtable for DBSP spines

**Status:** phase 1 is built (2026-11-20); phase 2, the measurement, is logged in [Phases](#phases). The first version (user, 2026-11-19) built the leaves from the layer file's own data blocks. The user revised it the same day: the seal copies the leaves into a file, so the memory layout is free of the file format. The current campaign plan measures order-key assignment and prefix Bloom filters first. The measurements that motivate this design are in [MEASUREMENT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/MEASUREMENT.md#one-budget-at-512-mib-base-tables-evicted-2026-11-19) (writes by kind at critical pressure) and in [compaction.md](compaction.md).

**Goal.**

- Initial run generation spans transactions. Out-of-order inserts go into an in-memory structure, soft-capped by its own budget. The memtable counts in RSS like any other memory (user, 2026-11-19), so it competes under the same pressure mechanism; the harness lowers the RSS target if needed.
- The memtable is a B+-tree whose leaves are small sorted arrays. The seal is one ordered pass that copies the leaves into the standard file writer (user, 2026-11-19). Sealed files are packed like any other file.
- No new dependency. A skiplist, `imbl::OrdMap` and an ART were considered; see [Choosing the in-memory structure](#choosing-the-in-memory-structure).
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

**The first design** made each memtable leaf a slotted key leaf or value leaf in the appendix's format, so that the seal only assigned the three laid-out numbers. Leaf size was never the obstacle: a file leaf is about 8 KiB, and so can an in-memory leaf be. What a seal without a copy cannot do is what a copy gives (user, 2026-11-19):

1. **Packing.** A leaf that splits on insert is half to two-thirds full. A copy combines such leaves into full 8 KiB file leaves, so the file has fewer leaves and branch entries.
2. **Encodings.** Prefix truncation, value-header elision and compression rewrite a whole leaf.
3. **Garbage and cancellations.** Items that updates or zero weights left behind are dropped.

The first design also needed three bridges: a writer entry point for finished leaves, a `DataBlock` over memory leaves, and a shared rkyv serializer. With a copy, the memory layout no longer has to match the file's.

### Where the copy happens

- **At seal (chosen).** The data is already in memory, and the file is written once. The seal runs on the merger runtime, off the worker's path. A skiplist or an ART can work only this way, since neither has leaves to keep.
- **At the first compaction.** The seal would write an unpacked file, without encodings. Seeks would read it until a merge rewrote it. That costs a second write and worse reads in the meantime, and gains only the seal's CPU, which is already in the background. It also needs a new rule: `size_to_level` places runs by size, so every sealed memtable would have to be forced into L1 for the rewrite to be its first merge.
- Whether the seal is the best place stays an open question (user, 2026-11-19); see the end of this doc.

### RocksDB's memtable

RocksDB copies at flush too.

- Its memtable is a skiplist (`InlineSkipList`) in an arena, insert-only. Every write appends an entry tagged with a sequence number; a delete appends a tombstone.
- A reader sees a snapshot by sequence number, with no copies and no locks.
- A flush iterates the skiplist, in order, through `CompactionIterator`, which drops versions no snapshot needs, into the table builder. The builder packs blocks, prefix-compresses keys with restart points, compresses, and builds the filters and the index.

A skiplist's scan chases pointers, and RocksDB accepts that. So the seal's scan cost is minor beside serialization and compression, and it does not decide the structure.

## Choosing the in-memory structure

The criteria, in order:

- **Seek cost.** DBSP operators seek the live memtable every transaction, so the memtable is read as much as any run.
- **Insert cost**, and **weight updates in place**: in a W-set, a later delta for the same `(key, value)` changes its weight, and a zero weight removes it.
- **Snapshot cost** (below).
- **Memory overhead** against the budget.
- **Fit with DBSP's type erasure.** Batches hold rows through the factories as `dyn` traits, not as concrete Rust types.

The candidates:

- **B+-tree with sorted-array leaves (chosen).** Seeks are binary searches over contiguous arrays. Weights update in place.
- **Skiplist, as in RocksDB.** Seeks chase pointers. Weights cannot be updated in place without breaking a reader's snapshot, so a weight change appends an entry tagged with its transaction, and a reader sums the entries its snapshot can see.
- **`imbl::OrdMap`.** A persistent B-tree: copying the whole map takes constant time, since the copy shares every node with the original, and each later insert copies the nodes from the root to the leaf it changes. That is the copy-on-write chosen below. It is generic over concrete Rust types, while DBSP's rows are type-erased, so every key would need its own heap allocation.
- **ART.** Byte keys only, so merged indexes only. It has no snapshots of its own; its nodes would need copy-on-write.

**The leaf's layout depends on the spine.** A leaf holds no particular form of data. It must keep the spine's order and meet DBSP's cursor contract, under which `Cursor::key()` and `val()` return references, `&K` and `&V`.

- **Native spines** (join traces, whose rows are Rust tuples): `DynVec` columns, so the cursor returns references into the leaf.
- **Merged-index spines.** A `FoldedKey` is one `Vec<u8>`, and a payload is packed bytes. A `DynVec` of byte vectors costs a heap allocation per key and per payload. A *byte leaf* avoids that: one contiguous byte area plus an offset array, compared with `memcmp`. It is the first design's slotted leaf without the file format. Its cost is that the cursor must build a `FoldedKey` to return `&K`, a copy per read, as a file cursor does in its key buffer.
- The `DynVec` leaf serves both. The byte leaf is not planned.

## What is reused, and how the memtable becomes a file run

| Reused piece | Where | Role in the memtable |
| --- | --- | --- |
| `ListMerger::merge` and the file builder | `trace/spine_async/list_merger.rs`, `trace/ord/file/` | **The seal.** The memtable cursor is the merger's one input; with one cursor, its heaps are trivial, and the merge is a copy in key order. The builder packs blocks and builds the index blocks, filters (Bloom, prefix once built, range) and trailer as for any file. |
| The `VecIndexedWSet` column layout (keys, offsets, values, weights) and the factories' `DynVec` | `trace/ord/vec/`, `dynamic/vec.rs` | **The leaf's layout.** `VecIndexedWSet` itself is immutable: a builder fills it once, and then it is only read. The leaf reuses its columns, not the batch. `DynVec` (the `Vector` trait) can push, truncate and sort, but not insert or remove at an index, so phase 1 added `insert_val`, `insert_ref` and `remove_at`. |
| Fallback batch enums (`Inner { Vec, File }`) and `DelegatingCursor` | `trace/ord/fallback/wset.rs`, `indexed_wset.rs` | The frozen memtable is a third variant, `Inner::Memtable`. Every reader (`CursorList`, `SpineCursor`, `RunMerge`, replay) takes it unchanged. |
| Spine snapshots under the `SharedState` lock; `size_to_level`, `MERGE_COUNTS`, the merger runtime | `spine_async.rs` | A snapshot includes the frozen memtable. The sealed file run is added at its level and compacted from then on like any other. |
| `pick_merge_destination` | `trace/ord/fallback/utils.rs` | It decides whether a seal writes a file or keeps the frozen memtable as an in-memory run. |
| `persisted()`, `Spine::save`/`restore` | Fallback batches, `spine_async.rs` | `save` first moves the memtable in as an in-memory run, which it persists like any other. The checkpoint lists files only; its format is unchanged. |
| Pressure levels, the `StorageOptions` plumbing, `RunWriteKind`, `SpineRunCounts` | `circuit/runtime.rs`, `feldera-types`, `storage/diagnostics.rs` | The budget, the knob and the counters follow these patterns. |

The life of an update:

1. A transaction's delta reaches `Spine::insert`.
2. Its items are inserted into the right leaves, in any order.
3. A snapshot sees the frozen memtable as one in-memory run.
4. When the budget asks for a seal, the seal streams the leaves, in order, through `ListMerger` into the file builder.
5. The file run enters the spine's levels.

## What is genuinely new

**(a) The leaf.** Columns in the `VecIndexedWSet` layout; a W-set's leaf has keys and weights only. A leaf splits at a byte target (see *Leaf size* below). In Appendix E's terms, one memtable leaf holds a key leaf's keys and their value rows together:

- its offsets vector is the key leaf's row boundaries, but local: they count from the leaf's first value, so an insert shifts only the later offsets of its own leaf;
- its values and weights are the value rows themselves, so it needs no item-offset map;
- the seal turns the local boundaries into global ones as the writer counts rows.

The operations:

- **Insert.** A binary search, then `insert_ref` in the columns.
- **Weight update.** In place.
- **Zero weight.** The item is removed (`remove_at`), so leaves hold no garbage.
- **A key across two leaves.** A split may fall between the values of one key. That key is then repeated as the next leaf's first key, and the cursor joins the two parts.

```text
insert(k, v, w):
    path = tree.descend(k, v)             // binary search at each inner node
    leaf = make_mut(path)                 // copy shared nodes on the path (Rodeh)
    i = leaf.position(k)                  // existing key, or insert k
    j = leaf.value_position(i, v)         // within k's value range
    if found: leaf.weight[j] += w; if zero: remove j (and k if now empty)
    else:     insert (v, w) at j; shift the later offsets of the leaf
    if leaf.bytes > target: split leaf, add its bound to the parent
```

**Snapshots: old versions, not concurrency control.** Each memtable has one writer, the thread that owns its spine (user, 2026-11-19). The active tree sits in the spine's `SharedState`, under the one state lock, so that a background seal can take it. The owner takes the lock once per inserted batch, and a snapshot or a seal takes the same lock, so no lock order arises. A second writer would only be serialized by that lock; none exists today, in DBSP or the Q3 driver. What one writer still needs is old versions:

- A held snapshot must keep seeing the state before later inserts. Key-value locking orders concurrent writers; it cannot give a reader the values from before an update, so there is no concurrency control to build.
- **A snapshot is held at nearly every insert.** A join's `delay_trace()` snapshot is taken before `UntimedTraceAppend` inserts the same step's delta into the same spine, and the join may read it afterwards (`integrate_trace_inner`, `JoinTrace::eval`). The merged index's `Old` snapshot is read on the view worker in the same way.
- **Copy-on-write (chosen)**, after Rodeh, "B-trees, Shadowing, and Clones" (ACM Transactions on Storage, 2008). A snapshot shares the root, and an update copies each shared node on its path. Only touched nodes are copied: a node a snapshot shares is copied once per step, and later updates in the same step reuse the copy.
- **The cost is the one copy-on-write B-trees pay.** LMDB likewise copies every page a write transaction touches. Row-level MVCC (Postgres, Hekaton) copies only the row, but pays for it with version chains, visibility checks and garbage collection. Here a step copies about one leaf per touched key, plus its inner nodes. A leaf copy is a deep copy: an `SqlString` (an `ArcStr`) clones by a reference-count increment, while a `FoldedKey` (a `Vec<u8>`) allocates. Old copies live only until the snapshot is released.
- **Measured, not assumed.** The counters report node copies and the time spent copying per transaction. The leaf size is a parameter, so smaller leaves can be measured if copies matter.
- *Note.* Inserts could be kept pending while a snapshot holds the tree and applied in place once it is released, avoiding copies. This is not planned.

**Accounting.** Leaf memory is counted when a leaf is allocated and released when it is dropped, through a counter in each leaf's `Drop`. The budget therefore counts every live leaf, including old copies that a snapshot still holds.

**Leaf size** is free of the file's 8 KiB, since the seal repacks. Smaller leaves cut the cost of a copy at the price of more inner entries. The default starts at 8 KiB.

**(b) The inner nodes.** A standard B+-tree: each inner node holds its children's bounds, row counts and `Arc` pointers. They play the value branch's role, with pointers in place of child offsets and sizes. The height grows as splits propagate, and the budget keeps it small: a 64 MiB memtable at 4–8 KiB per leaf has at most 16k leaves, so a fan-out of about 256 gives height 2.

- A seek is Appendix E's walk in memory: a binary search at each inner node, then a search in the leaf, whose offsets give the key's values directly.
- A snapshot clones the root's `Arc`.
- Concurrency is as today: one worker writes each spine, frozen memtables are immutable, and one mutex guards the run list and the memtable.

**(c) The memtable cursor.** It implements `Cursor` and `MergeCursor`: the inner nodes for navigation, then the leaf's columns.

- It joins a key that spans two leaves into one key with one value sequence.
- The memtable has no filters. `seek_key_exact` is a seek in the tree, which ignores the hash. `Batch::maybe_contains_prefix` answers from the memtable's key bounds alone, as for any in-memory run. Bloom and prefix filters exist only on file runs, which the seal builds through the writer.
- Read counters follow the Vec cursor of the same kind: an indexed memtable counts `memory_*` reads and seeks by `VecIndexedWSetCursor`'s rules, and a W-set memtable counts none, as `VecWSetCursor` counts none.

**(d) The seal: one ordered copy.**

- `ListMerger::merge(factories, builder, vec![frozen.merge_cursor(key_filter, value_filter)])`, with the file builder; the spine's retention filters apply as in any merge.
- The copy gives the three benefits above: full 8 KiB file leaves, the writer's encodings, and no garbage or cancelled items.
- The writer serializes each item once, packs the key and value leaves, and assigns the three laid-out numbers: global row boundaries, cumulative row totals, and child offsets and sizes. Filters, prefix truncation and compression follow its settings, unchanged.
- **Placement.**
  - The rule is `pick_merge_destination`'s, as for any merge output.
  - At low pressure, a memtable below `min_storage_bytes` (10 MiB by default) stays an in-memory run, with no copy; from then on it counts in RSS as a run, not in the budget.
  - At moderate pressure or above, or at or above `min_storage_bytes`, the seal writes a file.

**(e) Seals, after RocksDB.**

- At any time there is one active (mutable) memtable per spine, plus at most one immutable memtable being sealed (RocksDB's `max_write_buffer_number` = 2).
- Seals run on the merger runtime (`MergeWorkers::seal_memtable`). `is_merging()` counts a seal in progress.
- **No write stall** (user, 2026-11-20). Unlike RocksDB, inserts never wait for a seal: while a spine's memtable is sealing, its next inserts go into a new active memtable, and the total may overshoot the budget until the seal ends. The budget is a soft cap, and the overshoot shows in RSS like any memory. `MEMTABLE_STALL_NANOSECONDS` stays zero.

**(f) Spine hooks, as built** (`spine_async.rs`).

- **The fields.** `SharedState` holds the active memtable (`Box<dyn BatchMemtable<B>>`), the flushing one with its leaf-memory counter, and a seal count, all under the one state lock. `MergeWorkers` holds the spine's budget handle.
- **`get_batches()`** returns the runs, then the flushing memtable, then a frozen memtable of the active tree, made at snapshot time (lazy freeze). Every read path (`cursor`, `ro_snapshot`, `fetch`, `fork`, the counters) goes through it.
- **`insert`.** A batch above the budget / 4 goes in as a normal run. Otherwise its tuples are inserted into the tree, with no run, no `spill_if_due` and no backpressure wait.
- **The seal.** Under one lock, the memtable is taken and shown as flushing. The copy runs with no lock held. Under one lock again, the flushing memtable is replaced by its run. A snapshot therefore sees every tuple exactly once.
- **The other operations.** `save`, `consolidate`, `complete_merges` and `initiate_compaction` first move the memtable into the spine as an in-memory run, with no copy (`memtable_into_run`); `save` then persists it like any in-memory run. `fork` sees the memtable through `get_batches`. `restore` and `insert_without_blocking` stay run-based.
- **The hook.** `Batch::new_memtable(&Factories)` defaults to `None`. Only the fallback W-set and indexed W-set override it, through `LeafBatchMemtable`. Batches with timestamps get none: a stated limit.

**(g) The budget: a soft cap, counted in RSS.**

- **Knob.** `StorageOptions::memtable_mib` (off by default; `openapi.json` hand-edited).
- **The budget.** `MemtableBudget` tracks the live leaf bytes of the active and the flushing memtables, including old copies a snapshot still holds. A flushing memtable counts until its run replaces it. Above the budget, a background seal takes the largest memtable that is not flushing. There is no per-spine cap. The total may overshoot the budget while seals run, since inserts never wait (see (e)); a batch above the budget / 4 bypasses the memtable as a normal run.
- **Pressure is unchanged** (user, 2026-11-19). The memtable counts in RSS, and `raw_memory_pressure` is Feldera's own. A filling memtable therefore raises pressure, so other runs spill a little earlier when it is full and later after a seal. The harness reports pressure per refresh to show that sawtooth, and lowers the RSS target if the memtable's share needs room. An exemption (pressure computed on `rss − memtable` against `max_rss − budget`) was considered and rejected: it changes the pressure mechanism for one side of the comparison.
- **Startup** refuses a budget of `max_rss / 2` or more.

**(h) Diagnostics.**

- `RunWriteKind::MemtableFlush`, logged by every seal. `MemtableCheckpointFlush` is defined but not logged in phase 1: `save` persists the memtable's run as it persists any in-memory run.
- `SpineRunCounts` gains `memtable_entries`, `memtable_bytes`, `memtable_node_copies`, `memtable_copy_nanos`, `flushing` and `memtable_flushes`; q3 `run_counts` reports them per spine (`memtable_*_at_end`, and per-refresh differences of the counts).
- Metrics `MEMTABLE_FLUSHES` and `MEMTABLE_STALL_NANOSECONDS`.
- `spill_kinds.py` is rewritten in phase 2.

## Phases

The build has two phases. The plan for phase 1 lists its stages; phase 2 is logged here. A phase of cheaper inserts (an unsorted-input path, a per-leaf merge, a byte leaf) was planned and then dropped (user, 2026-11-20); see [history.md](history.md#2026-11-20-the-memtables-dropped-phase-of-cheaper-inserts).

1. **Phase 1: the memtable.**
   - The B+-tree with copy-on-write nodes, the memtable cursor, and `Inner::Memtable`.
   - Integral spines only. The per-transaction accumulator gets no memtable (user, 2026-11-19): it never spans transactions, so it is not where small runs pile up.
   - One budget for the runtime. A background seal takes the largest memtable, so a spine that stops inserting is still sealed. There is no per-spine cap.
   - Diagnostics, and the harness's `--memtable-mib` flag.
   - Each transaction's delta still arrives as a sorted batch. Its tuples are inserted one by one into the tree, between the tuples earlier transactions left there, using the previous tuple's leaf as a finger.
2. **Phase 2: measurement, on Linux** (per `LINUX-HANDOFF.md`).
   - First compile the adapters crate (`cargo check -p dbsp_adapters`). Its memtable metrics export (`memtable_flushes_total`, `memtable_stall_duration_seconds` in `controller.rs`) has never been compiled: `rdkafka-sys`'s build script fails on the Mac before reaching it (2026-11-20).
   - Rebuild the images: measurement version 18 changed `spine_async.rs` and `trace/ord/**`, which are in the image digest.
   - `measure.py --memtable-mib`, passed through to the driver's flag (the driver has it since measurement version 18).
   - The three arms of the run below (**Run**).
   - Rewrite `spill_kinds.py` in `q3-ivm-baseline/`; it was never committed. It tallies `compaction-log.jsonl` by write kind, the flush kinds included.

## The arm and its limits

- **The q3 arm keeps one budget:** target 512 MiB = cache 448 MiB + memtable 64 MiB. A cache equal to the target stays at critical pressure by construction (RSS about 720 MiB in the 512/512 pilot). The split is recorded in `DECISIONS.md` before measuring. A 448 MiB control with the memtable off separates the memtable's effect from the smaller cache.
- **What it will not remove.** Step-built files: operators build them straight into files at critical pressure, since `min_step_storage_bytes` is 0. They were 6.6 of native's 8.7 view files per transaction. They are reported, not changed here.

## Tests (phase 1)

Run with `cargo +1.96.1 test --offline -p dbsp --lib memtable`.

- **Vector.** `dynamic::vec::test::dyn_vec_insert_and_remove_*`.
- **Tree and leaves** (`trace/ord/memtable/test.rs`): a proptest against a `BTreeMap<(K, V), R>` model, with cancellations and splits inside a key; exact counts, and leaf memory that counts old copies until the snapshot goes; snapshot isolation, with copies only while a frozen memtable is held.
- **Cursor and batch** (`cursor_test.rs`): conformance against `VecIndexedWSet` and `VecWSet` under random operations, read counts included; mixed Vec, File and Memtable runs in `CursorList` and `ListMerger`; a sealed copy reads like a built file, with compression on and off; the `persisted` round trip.
- **Budget** (`circuit/memtable_budget.rs`, `circuit/runtime.rs`): largest first, a memtable that stopped inserting is still chosen, flushing and empty ones are skipped; startup validation.
- **Spine** (`trace::spine_async::memtable_test`): no memtable without the knob or on an accumulator; interleaving transactions live in the memtable with no runs; a held snapshot is unchanged by later inserts; the bypass; the budget seals into a file run, or keeps an in-memory run where merges stay in memory; `complete_merges`, `consolidate` and `fork` include the memtable.
- **Harness** (`q3-ivm-baseline/src/verification.rs`): `transcripts_do_not_depend_on_the_memtable`, in both modes, whole and split by an image (so a save and a restore with a non-empty memtable).

Not covered in phase 1: forced critical pressure with a memtable; the `trace/test.rs` spine proptests with the knob on.

**Risks, measured in the arm:**

- the seal's CPU: one serialization per item, the same work as any file build today, against a batch sort plus a build per transaction;
- the CPU of per-item inserts, and the vector memmove for wide rows;
- peak memory during a seal, since the flushing memtable counts in the budget until its run replaces it, while new inserts fill the next memtable;
- copies while a snapshot is held (counted as `memtable_node_copies`, with the copy time);
- accounting drift against RSS (report the delta).

**Run.** A-seek clustered, K = 10, prefix filters on, three arms:

- 512/512 with the memtable off;
- 448 cache with the memtable off;
- 448 cache + 64 MiB memtable.

Report, view first: IVM percentiles, runs per spine, per-run seeks per group, view writes by kind, flush count and size, the seal's CPU, and pressure per refresh.

## Open questions

- **Where the copy happens** (user, 2026-11-19). At seal, as chosen, or at the first compaction, which would need every sealed memtable forced into L1. See [Where the copy happens](#where-the-copy-happens).
