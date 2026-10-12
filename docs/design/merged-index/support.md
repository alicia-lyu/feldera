# Trace and cursor interface

This file records the Feldera interfaces that the Customer–Orders–Lineitem (COL) merged index reads through: traces, batches, snapshots, and cursors. It ends with the acceptance criteria for the reconstructed-state design. For the stored format see [col-storage.md](col-storage.md). For how reads feed the join see [reconstruction.md](reconstruction.md) and [native-state-read-contracts.md](native-state-read-contracts.md). Designs that are not implemented are in [deferred.md](deferred.md). Sources are listed in [references.md](references.md).

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

In R3 the supplied-snapshot join (`dyn_join_generic_with_snapshot_readers` in `crates/dbsp/src/operator/dynamic/join.rs`) takes this read side from caller-supplied readers. Each reader (for example `Q3State::customers_after` in `sql-to-dbsp-compiler/experiments/merged-index-col/src/q3.rs`) seeks into a COL snapshot, stops at the end of the requested prefix, and returns a `SpineSnapshot` of the rows it decoded. The join consumes that snapshot through the cursor interface above. Only the harness-side `ManualIndex` writes source updates; reconstructed outputs are never inserted into a trace.

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

## Acceptance criteria

R3 and R4 implement a restricted single-worker RF1/RF2 prototype. The criteria below say what would establish the design more broadly. The status column covers only what the repository shows.

| Acceptance question | Required evidence | Status |
| --- | --- | --- |
| Are operator inputs and results identical? | Run the same join with retained and reconstructed state; compare requested tuples, weights, ordering, absence, and before/after reads; check retractions and insertions against an independent evaluation. | `verify.sh` in `q3-ivm-baseline` runs both modes over the same RF1/RF2 transactions and compares emitted rows and materialized `V` with a full recomputed join. It covers the restricted workload only. |
| Does the adapter preserve encoding and weighted updates? | Cross-type byte ordering, exact range bounds, type decoding, replacements at unchanged keys, signed multiplicities, before/after reads. | Covered by the tests listed in [col-storage.md](col-storage.md#tests). |
| Does reading preserve lifecycle and memory bounds? | Overlapping request lifetimes, ranges larger than memory, abort and restart; borrowed-value validity; reference-only snapshots; each batch advanced exactly once. | Not shown. No hard memory bound is claimed, and abort and restart are not tested. |
| Does storage replacement actually occur? | Inventory retained state, including aggregate output and delays; confirm intermediate snapshots are not accumulated again. | In reconstructed mode `dyn_join_generic_with_snapshot_readers` builds no integrate or delay traces for the two joins (confirmed by code review of `join.rs`). Remaining state is not inventoried here. |
| Is total maintenance cheaper? | Datasets larger than memory, equal total memory, comparable durability, matched predicates and results. | Unmeasured. See [measurement.md](measurement.md). Raw measurement artifacts are local-only, in git-ignored `target/` directories. |

When measuring, count every read and write: RF1/RF2, workload-side discovery, operator access, staging, merging, checkpointing, and spills. Report persistent and peak storage, CPU and decoding, memory high-water, and complete-transaction latency for clustered and scattered updates. Fewer unique blocks or a resident scan speedup does not by itself show lower total I/O.
