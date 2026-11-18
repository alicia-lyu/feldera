# DBSP background: traces, cursors, and native runs

This file records the Feldera interfaces that a merged index reads through (traces, batches, snapshots, and cursors), how native DBSP builds, levels, and places runs, and how its snapshot, fetch, and join-wait behavior works. The examples come from the Customer–Orders–Lineitem (COL) index and the Q3 joins. For the stored format see [storage-layout.md](storage-layout.md). For how reads feed the join see [reconstruction.md](reconstruction.md); the Q3 bindings of the native joins are in [CIRCUIT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md). Designs that are not implemented are in [deferred.md](deferred.md). Sources are listed in [references.md](references.md).

Source links point at Feldera revision `f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c`, so their line fragments stay valid; the current code may differ.

## Storage differences from RocksDB

> [!NOTE]
> **Glossary**
>
> **Retained state** — An integrator's accumulated output stored and maintained across input updates.
> Reads obtain its tuples from that maintained collection, which may reside in memory or files.
>
> **Reconstructed state** — The same integrator output computed for requested keys and a before/after view
> from stored weighted source records, instead of maintaining that output as a separate collection.
>
> Both supply the same tuples, weights, group presence/absence, and cursor ordering to the existing IVM
> path. Neither term refers to reconstructing input deltas. Reconstructed state still depends on retained
> source data and may use bounded temporary buffers; the distinction is how the integrator output is supplied.
>
> **spine** — collection of immutable batches and background merger.
>
> **batches** — immutable sorted collections of weighted updates, in memory or files.
>
> **layer file** — Feldera's immutable file format for nested sorted groups.
>
> **column** — storage level, not a SQL attribute.
>
> **Trace** — The runtime interface for reading and updating retained state.

The **trace** interface exposes retained state. `Spine` is its concrete
implementation: a **spine** holds **batches** and merges them in the background
([spine
description](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/spine_async.rs#L1-L7)).
A **layer file** calls each nesting level a
**column**. Each level has a tree whose leaves are data blocks and whose
interior nodes are index blocks ([file
layout](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/storage/file.rs#L3-L54)).
For an indexed weighted relation, the outer level contains search keys `K`; the inner level contains sorted
values `V` grouped under each key, with associated weights. One `K` can have many values, while each distinct
`(K,V)` has one consolidated weight. A whole SQL row can be one `V`. This is a nested representation of the
composite mapping `(K,V) → weight`, not secondary indexing of every SQL field.

The file-backed batch builder writes keys to level zero and values plus weights to level one, then finishes
one file-backed batch. Thus these trees belong to each immutable batch, not to the entire LSM collection
([batch
construction](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/file/indexed_wset_batch.rs#L982-L1016)).
The outer level compares `K`. After selecting a fixed `K`, the inner level compares `V`; it does not
compare `K` again or order records by weight. Inner seeks use a target `V` within that group's sorted values.
For example, `(K=7,V=order_a)` precedes `(K=7,V=order_b)` according to the order-tuple comparator. When batches
are combined, equal `K` groups are matched and equal `V` values within them have their weights added.
Ordering by `V` does not create a global lookup by `V` across different `K` groups. A scan-only
consumer need not use that seek capability. The file-format goals explicitly distinguish seeking from
sequential access and discuss disabling value indexing when unnecessary; that goal alone does not establish
an implemented switch or a measured benefit
([access
goals](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/storage/file.rs#L30-L36)).

This differs from RocksDB's key-to-value interface: an indexed Feldera relation exposes sorted keys and
sorted weighted values within each key. RocksDB's ordinary writes enter a mutable memtable; Feldera's spine
accepts already built immutable batches. See the official
[RocksDB overview](https://github.com/facebook/rocksdb/wiki/RocksDB-Overview).

> [!NOTE]
> **Glossary**
>
> **Z-set** — relation with signed tuple multiplicities.

A **Z-set** adds weights for identical complete tuples and omits zero totals
([read
consolidation](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/cursor/cursor_list.rs#L150-L174)).
RocksDB's `Put`/`Delete` semantics differ, but its application-defined
[merge operator](https://github.com/facebook/rocksdb/wiki/Merge-Operator) can combine updates too.
The distinction is the collection contract, not an inability to express addition in RocksDB.

> [!NOTE]
> **Glossary**
>
> **root circuit** — top-level operator graph.
>
> **`VecIndexedWSet`** — An in-memory indexed weighted relation.
>
> **`FallbackIndexedWSet`** — A batch type choosing memory or file storage.

`VecIndexedWSet` contains keys, offsets, values, and signed weights
([memory
layout](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/vec/indexed_wset_batch.rs#L158-L199));
`FallbackIndexedWSet` selects memory or file
representation
([variants](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/fallback/indexed_wset.rs#L34-L58)).
The generated **root circuit** uses ordinary weighted batches without per-record
logical timestamps ([batch
specialization](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/time.rs#L214-L219)).
Old/new snapshots own references to immutable batches; those references determine
the before/after state read by an operator.
File indexed batches implement asynchronous key fetching
([fetch](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/file/indexed_wset_batch.rs#L447-L478)),
which joins can use
([join fetch
path](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/join.rs#L1653-L1687)).
A comparison must record the actual fetch setting
and storage/cache configuration.

## Trace access traverses keys then weighted values

> [!NOTE]
> **Glossary**
>
> **`BatchReader`** — Feldera's read interface for an ordered weighted collection.
>
> **`Cursor`** — A movable read position with separate key and value navigation and seek operations.
>
> **`WithSnapshot`** — The interface for obtaining a stable, read-only view of current trace contents.

`Trace` extends `BatchReader` with insertion of immutable update batches. Snapshot creation is a separate
interface implemented by the spine. The following is a deliberately reduced pseudocode view of these APIs
for root-circuit state (`Time = ()`); it omits factories, persistence, reverse navigation, and other methods.
`insert` appends signed updates, not replacement snapshots. Sources:
[`Trace` and insertion](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace.rs#L231-L308),
[`BatchReader`](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace.rs#L468-L496),
[`WithSnapshot`](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/spine_async/snapshot.rs#L23-L43).

```text
interface BatchReader<K, V, R>:
    cursor() -> Cursor<K, V, R>                 # starts at first key and its first value

interface Trace<K, V, R> extends BatchReader<K, V, R>:
    async insert(batch: ImmutableWeightedBatch<K, V, R>)

interface WithSnapshot<K, V, R>:
    ro_snapshot() -> Snapshot<K, V, R>          # snapshot implements BatchReader

interface Cursor<K, V, R>:
    key_valid() -> bool
    val_valid() -> bool
    key() -> borrowed K                        # requires valid key
    val() -> borrowed V                        # requires valid key and value
    weight() -> borrowed R                     # consolidated weight of current (K,V)
    step_key()                                # next K; reset V to first value of that K
    step_val()                                # next V within current K only
    seek_key(target: K)                        # forward lower-bound seek by K
    seek_val(target: V)                        # forward lower-bound seek within current K
    rewind_keys()                             # restart at first K and first V
```

`K → {V → weight}` describes the contents exposed by those methods, not a required nested map allocation.
For example, one memory batch can represent this content with sorted arrays and offsets into the values:

```text
Exposed content:                  One memory-batch layout:
7 -> { order_a -> 1,              keys    = [7, 9]
       order_b -> 2 }            offsets = [0, 2, 3]
9 -> { order_c -> 1 }             values  = [order_a, order_b, order_c]
                                 weights = [      1,       2,       1]

For keys[i], its values occupy [offsets[i], offsets[i+1]).
At key index i and value index j:
    key()    = keys[i]
    val()    = values[j]
    weight() = weights[j]
```

This is the structure of the existing
[memory batch](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/vec/indexed_wset_batch.rs#L158-L199).
A file batch instead locates the key and its value group through file indexes and block reads. Both expose
cursor navigation; the consumer does not unpack the entire group into another container.

A snapshot can contain several batches. Its cursor combines their ordered contents: for matching `(K,V)` pairs,
sum the weights and omit zero totals; omit a key if all its values cancel. For example, appending updates
`((7,order_a),-1)`, `((7,order_d),+1)`, and `((9,order_c),-1)` to the batch above leaves
`7 → {order_b → 2, order_d → 1}`. The old snapshot still exposes the original contents. The running merge
keeps cursor positions in its member batches; it does not need to materialize the entire accumulated relation.
See [snapshot cursor construction](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/spine_async/snapshot.rs#L196-L215)
and [weight consolidation and zero suppression](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/cursor/cursor_list.rs#L150-L174).

The following traversal unpacks the exposed groups into weighted tuples one at a time. A weight of two stays
one weighted tuple; the consumer does not have to expand it into two copies.

```text
with trace.ro_snapshot() as snapshot:
    cursor = snapshot.cursor()
    while cursor.key_valid():
        while cursor.val_valid():
            consume(cursor.key(), cursor.val(), cursor.weight())
            cursor.step_val()
        cursor.step_key()

# Lookup instead of a full traversal:
with trace.ro_snapshot() as snapshot:
    cursor = snapshot.cursor()
    cursor.seek_key(requested_key)
    if cursor.key_valid() and cursor.key() == requested_key:
        while cursor.val_valid():
            consume(cursor.key(), cursor.val(), cursor.weight())
            cursor.step_val()
    # A lower-bound seek may land on a larger key: that means the requested key is absent.
```

`step_val()` never advances into the next key. `seek_val(v)` compares values inside the selected group;
it cannot locate `v` globally across keys. Forward seeks do not rewind a cursor already beyond their target;
use a fresh or rewound cursor for an earlier key. Borrowed fields must be consumed before movement, or copied
into explicitly budgeted storage. The snapshot remains alive until its readers finish. These semantics follow
the [cursor navigation contract](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/cursor.rs#L42-L109)
and [seek methods](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/cursor.rs#L197-L245).

A read exposes each complete `(K, V)` pair with its consolidated signed weight. Before/after visibility comes from the immutable runs that the selected snapshot references, not from filtering timestamped records.

The supplied-snapshot join (`dyn_join_generic_with_snapshot_readers` in `crates/dbsp/src/operator/dynamic/join.rs`) takes this read side from caller-supplied readers instead of a trace. The Q3 readers (`CustomersAfter`, `OrdersBefore`, `JoinedAfter`, `LineitemsBefore` in `sql-to-dbsp-compiler/experiments/merged-index-col/src/q3.rs`) are filled by the scan session: `Q3State::collect` reads `Dlt` once and scans the touched groups of `Old` once with a `ScanSession`, and each reader keeps only the entries it requests. `collect` then seals each reader (`SealedReader`: its requests and its finished partner batch), and the join reads the partner batch through the cursor interface above, with no lock. The scan session and the generic readers are described in [shared-traversal.md](shared-traversal.md) and [merged_index/CLAUDE.md](../../../crates/dbsp/src/trace/merged_index/CLAUDE.md); the Q3 readers and routing hook in [merged-index-col/CLAUDE.md](../../../sql-to-dbsp-compiler/experiments/merged-index-col/CLAUDE.md). Only the harness-side `ManualIndex` writes source updates; reconstructed outputs are never inserted into a trace.

## Weighted reads

For a key K and payload P, the before and after snapshots give:

```text
weight_before(K, P) = sum of weights for (K, P) in the before snapshot
weight_after(K, P)  = weight_before(K, P) + weight_delta(K, P)
```

A reader must return every nonzero `(K, P)` and never sum by K alone. A replacement has `(K, old, -1)` and `(K, new, +1)`, whose net weight at K is zero. Joined rows multiply weights: `weight(B row) = weight(O row) * weight(C row)`, and `weight(V row) = weight(B row) * weight(L row)`.

The **support** of a Z-set is the set of tuples with nonzero weight. Wherever this documentation says "support of a transaction delta" it means that: the changed complete tuples, taken before projecting to folded keys. Discovering affected keys from the support, not from key-only sums, keeps replacements visible. Customer changes affect the matching orders and lines in both states. Physical key moves appear only when the transaction supplies both the old and the new extended keys.

Group existence (a group with revenue zero versus no group) belongs to the consuming Q3 query's aggregate operator, not to the merged index.

## Snapshots

`Spine::ro_snapshot` clones `Arc` references to the existing immutable runs. It copies no tuples, memory runs, or files, writes nothing, creates no persistent version, and sets no expiration. Dropping the handle releases the references. A snapshot keeps its runs readable while the live spine compacts them, and compaction does not wait for snapshots. A file-backed run keeps a reader and metadata in memory; cursors load blocks through the cache as they advance.

Combined cursors add the weights of matching pairs and suppress zero totals
([consolidation](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/cursor/cursor_list.rs#L150-L174)).
Concatenating runs or overwriting equal byte keys would not give correct weighted reads.

A snapshot is a read handle, not a rollback point. A DBSP transaction is `start_transaction`, any number of `step()` calls, then `commit_transaction` (`crates/dbsp/src/circuit/dbsp_handle.rs`). Outputs appear at commit, and the logical clock advances once. Cursor exhaustion or the end of one step is therefore not a completion barrier: the reconstructed join calls its readers once, at the commit flush, and returns early on other steps. Keep each snapshot valid until the join's flush finishes.

## Native run building, levels, placement, and memory pressure

This part records how native DBSP creates runs, assigns them to levels, and decides whether each run lives in memory or in a file. It also records what memory pressure changes. The claims were checked against the code on 2026-10-12 and cite symbols, not line numbers. Paths are relative to `crates/dbsp/src/` unless stated otherwise. The slide deck in [slides-oct-8/](slides-oct-8/README.md) depends on these facts. The R5 harness's own placement override is described in [q3-ivm-baseline/CLAUDE.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CLAUDE.md); decisions are in [decisions.md](decisions.md).

### Building a run

An operator builds each output run by appending already sorted
(key, value, weight) records to a builder. Sorting happens earlier, in
memory, on bounded chunks:

- Joins, aggregates, `distinct`, and similar operators collect output in a
  batcher. They seal a run every `splitter_output_chunk_size()` records
  (`circuit/dbsp_handle.rs`). The size comes from `splitter_chunk_size_records`
  and defaults to 10,000 (`feldera-types/src/config/dev_tweaks.rs`).
- One transaction can therefore produce one or more runs. A run never
  contains more than one transaction's output until a merge combines runs.
- Operators do not aim at any particular run size; merges build larger runs.

A builder writes to memory, to a file, or to memory up to a byte threshold
(`BuildTo` and `BuilderInner` in `trace/ord/fallback/indexed_wset.rs`). In the
threshold mode, the builder appends to memory arrays. Once the bytes reach the
threshold, `spill` copies the records so far into a file builder and keeps
appending there. This works without sorting again because records arrive in
order.

A file builder (`FileIndexedWSetBuilder` over `Writer2` in
`storage/file/writer.rs`) is a streaming, bottom-up B-tree writer:

- It keeps a key tree and a value tree. `push_val` appends a (value, weight)
  row to the value tree (`write1`). The key's `push_key` follows its values
  and appends (key, ()) with the range of value rows just written (`write0`).
- Each tree keeps one open data block (`DataBlockBuilder`; `min_data_block`
  defaults to 8 KiB before compression and must be at least 4 KiB). A full
  block is compressed (Snappy by default), padded to a multiple of 512 bytes,
  and written, and its
  summary goes into the open index block one level up (`index_blocks`). Index
  blocks fill and propagate upward the same way. Nothing is revisited, so
  memory stays at about one block per tree level, whatever the run size.
- Index blocks record each child's file offset and on-disk (compressed)
  size in 512-byte units (`get_child_location` in `storage/file/reader.rs`).
- `close` flushes partial blocks, writes the index roots, the key filter
  (Bloom or roaring bitmap, the one structure that grows with the run), and
  the trailer. Each data block also enters the buffer cache as it is written
  (`from_raw_with_cache`), so a new file run starts warm.

The threshold is `min_step_storage_bytes`, read through
`Runtime::min_step_storage_bytes` (`circuit/runtime.rs`). It defaults to
unlimited, so operators build runs in memory. Only at Critical memory pressure
does it drop to zero; nonempty runs are then written to a file from their
first record.

The merged index's delta follows the same threshold (user decision of
2026-11-10). `build_batch` in `trace/merged_index/batch.rs` fixes no location,
so the delta run of a transaction is placed by `BuildTo::for_capacity` like an
operator's output: in memory unless the runtime is at Critical memory
pressure. The index's `Spine` places the inserted run by
`min_insert_storage_bytes`, as it places a native trace's. The worked-transaction
tests (`native_worked_transaction_tests`, `session_worked_transaction_tests`)
put both sides under the same pressure and report the layout each got; see
[shared-traversal.md](shared-traversal.md).

### Levels

A run's level depends only on its record count (`Spine::size_to_level` in
`trace/spine_async.rs`):

| Level | Records |
| --- | --- |
| L0 | up to 14,999 (`MAX_LEVEL0_BATCH_SIZE_RECORDS`, configurable) |
| L1 | up to 99,999 |
| L2–L8 | each level holds runs ten times larger than the one before |

- An inserted run goes straight to its level (`AsyncMerger::add_batch` with
  `merge=false`). It does not climb through L0.
- A level merges once it holds enough loose runs (`MERGE_COUNTS`,
  `min_batches_to_merge`): at least 8 runs at L0 and L1, and 3 (or 2 at
  L6–L8) deeper down.
- A merge output's level is derived again from its size. It can land back in
  the level it came from and be merged again. The code comment on
  `min_batches_to_merge` states the usual case: merging three runs stays in
  the level, and merging ten or more moves the result down one level.
- For merge outputs only, each negative-weight record counts as several
  records. Runs with many retractions therefore move down sooner and meet the
  insertions they cancel.

### Merging

Each level has its own background merger task, started on demand
(`MergeWorkers` in `trace/spine_async.rs`). A merger wakes when a run is added
to its level or when memory pressure changes (`memory_pressure_notify`).

- **Regular merge.** A level starts a merge when it has no merge in flight
  and holds at least its minimum number of loose runs (`Slot::try_start_merge`,
  `MERGE_COUNTS`): 8 at L0 and L1, 3 at L2–L5, 2 at L6–L8. A merge takes the
  oldest loose runs, at most 128 at L0 and 64 elsewhere.
- **Early merge.** From High pressure up, `must_relieve_memory_pressure` lets
  a level start as soon as it holds any memory run, even one. Merge outputs
  go to files at that pressure, so an early merge mainly moves memory runs to
  disk.
- **Requested compaction.** A level asked to compact
  (`CompactionStatus::Requested`) merges all its runs regardless of count. The
  request then passes to the next level down. This file does not trace who
  issues these requests.

An L0 merge runs to completion at once. Deeper merges run in slices sized
like an average L0 merge and yield between slices. Inputs stay readable until
the merge completes. `merge_complete` derives the output's level again and
adds it as a loose run there, which can start the next merge.

The 128-loose-run backpressure does not start merges; it pauses foreground
insertion until the mergers catch up.

### Location: memory or file

Location is a separate, per-run decision made when a run is created. Level
does not determine it, so no level boundary divides memory from disk. One
level can hold memory and file runs together. The thresholds below assume
storage is configured; without storage, every run stays in memory.

- **Inserted runs.** `Spine::insert` calls `Spine::maybe_flush_batch`, which
  asks `pick_insert_destination` (`trace/ord/fallback/utils.rs`). Below
  Moderate pressure, a run stays in memory at any size. The merger decides
  later.
- **Merge outputs.** `pick_merge_destination` writes a file once the inputs
  total `min_storage_bytes` or more, defaulting to 10 MiB
  (`Runtime::min_merge_storage_bytes`). Smaller outputs stay in memory.

A memory run moves to disk only by being rewritten, through a merge or an
eager spill on insertion. Spilling writes a copy, so readers can keep the
memory original alive.

Recording location per run is deliberate. A run's location is its
representation: memory arrays or a file, the two variants of one batch type
(`FallbackIndexedWSet` in `trace/ord/fallback/indexed_wset.rs`). Runs are
immutable, so a new location means a rewrite. Merges rewrite runs constantly
as they move down the levels, and each rewrite applies the current policy.
Early merges actively rewrite memory runs under High pressure. Placement is
therefore lazy but converges.

At a fixed moment and pressure, location does split the levels, but by bytes
rather than by level. With records of about 100 bytes, 10 MiB is about 100k
records, near the L1/L2 boundary, so L0–L1 runs sit in memory and deeper runs
in files. Two kinds of run cross the line: a large inserted run stays in
memory at Low pressure until a merge rewrites it, and snapshots can pin
obsolete memory runs.

### Memory pressure

A runtime thread samples the process resident set size (RSS) about once a
second and compares it with `max_rss_bytes` (`Runtime::raw_memory_pressure`
and the `rss-monitor` thread in `circuit/runtime.rs`). Hysteresis is applied
to the result before use.

| RSS / `max_rss_bytes` | Pressure | Inserted run | Merge output | Early merge | New runs built |
| --- | --- | --- | --- | --- | --- |
| below 85% | Low | memory | inputs below 10 MiB: memory; 10 MiB or more: file | no | in memory |
| 85% or more | Moderate | below 10 MiB: memory; 10 MiB or more: file | file | no | in memory |
| 90% or more | High | file | file | yes | in memory |
| 95% or more | Critical | file | file | yes | straight to file |

**Early merge.** From High up, `Slot::must_relieve_memory_pressure` lets any
level holding a memory-resident loose run start a merge at once, even with one
run. The merge output goes to a file, so this moves memory runs to disk.

Memory pressure does not:

- Resize the block cache.
- Pause connectors. The adapter controller only reports it in statistics.
- Control the 128-loose-run backpressure (`LooseBatchCount`), which applies
  independently of pressure.

### Memory components

The memory that runs use is the same kind any LSM tree uses:

- **Memory runs**, held whole. This includes obsolete runs that snapshots
  still reference after a merge.
- **Merge output under construction.**
- **Block cache** for file runs (`storage/buffer_cache`). Readers fetch file
  blocks through it, so a snapshot that pins a file run pins the file, not its
  bytes in memory.
- **Key filters and index metadata** for file runs (`storage/file/reader.rs`).
  Modular Bloom filters can be partly resident.

The difference from a standard LSM is the absence of a fixed memtable budget,
not a different set of components. RocksDB, for example, bounds its memory
part with `write_buffer_size`. DBSP instead lets sealed runs stay in memory
under the thresholds above and uses process RSS to tighten placement.

## Existing snapshot, fetch, and cursor behavior

The native Q3 joins ([CIRCUIT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md)) use one `JoinTrace` implementation, with their respective types and current or delayed trace.

### Weird Feldera naming: flush

> [!NOTE]
> **Glossary — weird Feldera naming**
>
> **`Operator::flush`** — The scheduler's notification to finish processing a
> transaction's inputs and emit remaining outputs. For `JoinTrace`, this
> coordinates delta processing with snapshots of the required before/after
> accumulated states. The name does not mean a memory-to-storage flush.

`Operator::flush` means **finish processing the current transaction's inputs
and emit the remaining outputs**. The
scheduler (`crates/dbsp/src/circuit/schedule.rs`)
invokes it on an operator after all predecessors have finished producing their
outputs for that transaction, then waits for the operator to finish before
flushing downstream operators. It flushes pending computation downstream;
it does not request a storage write or consolidation into one batch or file.

For example, the
transaction delta collector (`crates/dbsp/src/operator/dynamic/accumulator.rs`)
inserts incoming batches into its transaction Spine. After its `flush`
notification, the next evaluation returns that Spine and replaces it with an
empty one. The returned delta is one logical collection, which may still
contain multiple memory-backed or file-backed batches. The retained historical
trace is a separate collection and can likewise span multiple batches and files.

### Weird Feldera naming: accumulation

> [!NOTE]
> **Glossary — weird Feldera naming**
>
> **`Accumulator`** — Feldera's name for a **transaction delta collector**.
> It collects only the current transaction's changes and resets after handing
> them downstream. It does not maintain the relation's historical integral.
>
> **Historical trace accumulation** — Applying successive transaction deltas
> to retained relation state across transactions, yielding `X⁺ = X⁻ + ΔX`.

**Documentation convention:** avoid “delta accumulator” in explanatory prose;
use **transaction delta collector** for the native `Accumulator`. Use
**historical trace accumulation** for the distinct operation that maintains
accumulated relation state. Keep actual source identifiers unchanged in code
references.

| Role | Native source | Logical output | Lifetime and reset |
| --- | --- | --- | --- |
| Transaction delta collector | Accumulator (`crates/dbsp/src/operator/dynamic/accumulator.rs`), used by `dyn_accumulate` and `dyn_shard_accumulate` | The current transaction's `ΔX`, collected from its input batches | Hands off its transaction Spine after `Operator::flush`, then starts with an empty Spine. |
| Historical trace accumulation | Trace construction and feedback (`crates/dbsp/src/operator/dynamic/accumulate_trace.rs`) with AccumulateTraceAppend (`crates/dbsp/src/operator/dynamic/accumulate_trace.rs`) | Accumulated `X⁺`, including `X⁻` and the current `ΔX` | Retains relation state across transactions; transaction completion does not reset it. |

The trace construction uses a transaction delta collector internally before
appending its output to historical state. Both roles can use a Spine, so the
collection's representation does not distinguish their temporal scope.

### Why this join waits for complete inputs

The scheduler (`crates/dbsp/src/circuit/schedule.rs`) permits
operators to process inputs before transaction completion; some operators
accumulate them for later processing. The wait described here belongs to the
existing accumulated-trace join, not a requirement that every downstream
operator defer every tuple.

The generated first join computes `ΔB = ΔO ⋈ C⁺ + O⁻ ⋈ ΔC`. The `ΔO` branch
requires the complete current `C⁺`, including all Customer changes in the
transaction. Tuple delivery alone does not establish that this state is ready.
For example, start with empty Orders and Customer and insert a matching order
and customer in the same transaction. If the order delta were processed against
the Customer trace before the customer insertion, it would produce no pair.
The other branch uses empty `O⁻`, so it would not recover that missing pair.
The existing branch wiring therefore waits for input completion before reading
its required accumulated state.

In this implementation, **transaction completion notifications and snapshot
reads are coupled**: after predecessor outputs are complete, `JoinTrace`
snapshots the trace supplied to its branch and uses it with the transaction
delta. The
current branch (`crates/dbsp/src/operator/dynamic/join.rs`)
receives the accumulated trace including current updates; the
before-state branch (`crates/dbsp/src/operator/dynamic/join.rs`)
receives the delayed prior trace. `flush()` does not itself take a sequential
pair of snapshots before and after appending the delta. The
delayed-trace wiring (`crates/dbsp/src/operator/dynamic/accumulate_trace.rs`)
provides the before state. This coordination controls logical visibility,
not batch placement, storage writes, or compaction.

### Snapshot, fetch, and cursor operations

`Cursor<K, V, T, R>` is the interface for navigating one weighted collection.
`JointKeyCursor` is a private join helper that contains two such cursors, one
for the delta and one for the accumulated trace, and positions them at matching
keys. It does not define another storage cursor interface. The join loop uses
the two underlying cursors to enumerate value pairs and multiply their weights.

The following operations are confirmed in the inspected source:

1. JoinTrace::eval (`crates/dbsp/src/operator/dynamic/join.rs`)
   takes a read-only snapshot of an arriving delta and retains it until
   Operator::flush (`crates/dbsp/src/circuit/operator_traits.rs`)
   notifies the operator that all predecessors have produced their outputs
   for the current transaction. The
   join's notification handler (`crates/dbsp/src/operator/dynamic/join.rs`)
   sets a flag; the following evaluation snapshots the supplied accumulated
   trace and processes the retained delta. That trace's wiring determines
   whether it represents the before or after state. This scheduling ensures
   that the join consumes complete transaction inputs. Snapshot acquisition
   itself neither requires spilling nor consolidating the referenced batches.
2. WithSnapshot (`crates/dbsp/src/trace/spine_async/snapshot.rs`)
   defines read-only, non-merging snapshots. A
   SpineSnapshot (`crates/dbsp/src/trace/spine_async/snapshot.rs`)
   owns `Vec<Arc<B>>`; acquisition from a
   Spine (`crates/dbsp/src/trace/spine_async/snapshot.rs`)
   obtains batch references. Its
   cursor (`crates/dbsp/src/trace/spine_async/snapshot.rs`)
   retains cloned batch references. Both memory-backed and file-backed batches
   can be retained; snapshot acquisition does not require spilling in-memory
   batches. The existing
   [storage test](../../../sql-to-dbsp-compiler/experiments/merged-index-col/src/storage_tests.rs)
   preserves a memory-backed snapshot across compaction and later checks a
   snapshot containing both batch locations.
   Reading that view does not append updates or merge its batches. These
   snapshot and notification operations do not define a memory-limit policy.
   For a consolidated read, the
   CursorList (`crates/dbsp/src/trace/cursor/cursor_list.rs`)
   sums contributions for identical key/value pairs across batches and skips
   zero totals.
3. When `fetch_join == Some(true)`, the join invokes
   trace.fetch(&delta) (`crates/dbsp/src/operator/dynamic/join.rs`).
   SpineSnapshot::fetch (`crates/dbsp/src/trace/spine_async/snapshot.rs`)
   delegates to `FetchList`, which concurrently fetches each batch and keeps
   batch cursors for batches without a specialized fetch result. The join
   uses the fetched cursor when present, otherwise the snapshot's full cursor.
4. **Join traversal and output.** Each input cursor implements the
   Cursor interface (`crates/dbsp/src/trace/cursor.rs`): ordered
   key/value navigation and borrowed key/value references.
   JointKeyCursor (`crates/dbsp/src/operator/dynamic/join.rs`)
   finds keys shared by the delta and accumulated-state snapshot: it visits
   keys on one side and seeks the same key on the other. For
   these inner joins, fetched state makes the delta drive; otherwise
   estimated key counts (`crates/dbsp/src/operator/dynamic/join.rs`)
   choose which side drives. Missing keys produce no joined rows. At each
   matching key, the
   JoinTrace loop for this circuit (`crates/dbsp/src/operator/dynamic/join.rs`)
   takes each delta value in turn and pairs it with every accumulated-state
   value under that key. After each delta value, it resets the accumulated-state
   cursor to the first value under the same key, so the next delta value sees
   all of them again. After all value pairs, it advances to the next shared key.
   Each pair supplies the shared key, delta payload, and accumulated-state
   payload to the generated join function. Each resulting output tuple receives
   the delta record's signed weight multiplied by the accumulated-state record's
   consolidated signed weight. The native batcher consolidates identical output
   tuples. Value pairing, weight multiplication, and output construction belong
   to this loop; `JointKeyCursor` only coordinates matching keys.

**The generated Q3 circuit has no per-record logical timestamps.** Old/new
snapshots own references to immutable batches. Reading and consolidating the
weighted records in those referenced batches yields the before/after relation
state; records are not selected by a timestamp or a time-travel request. A
snapshot can reference memory-backed or file-backed batches.

In the source, the generic time parameter is Rust's unit type `()`, and its
batch specialization (`crates/dbsp/src/time.rs`) uses the ordinary
batch directly. This is a type-level representation of the absence of
per-record logical time, not a timestamp stored with each record.

Fetch is conditional; a binding
does not imply that its delta always drives a selective lookup.
