# Evidence for reconstructed accumulated state

## Storage differences from RocksDB

A **trace** is the runtime interface for reading and updating retained state. `Spine` is its concrete
implementation: a **spine** (LSM run collection and background merger) holds **batches** (immutable sorted
runs of weighted updates, in memory or files) and merges them in the background
([spine
description](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/spine_async.rs#L1-L7)).
A **layer file** (Feldera's immutable file format for nested sorted groups) calls each nesting level a
**column** (storage level, not a SQL attribute). Each level has a tree whose leaves are data blocks and whose
interior nodes are index blocks ([file
layout](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/storage/file.rs#L3-L54)).
For an indexed weighted relation, the outer level contains search keys `K`; the inner level contains sorted
values `V` grouped under each key, with associated weights. One `K` can have many values, while each distinct
`(K,V)` has one consolidated weight. A whole SQL row can be one `V`. This is a nested representation of the
composite mapping `(K,V) → weight`, not secondary indexing of every SQL field.

The file-backed batch builder writes keys to level zero and values plus weights to level one, then finishes
one file-backed batch. Thus these trees belong to each immutable run, not to the entire LSM collection
([batch
construction](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/file/indexed_wset_batch.rs#L982-L1016)).
The outer level compares `K`. After selecting a fixed `K`, the inner level compares `V`; it does not
compare `K` again or order records by weight. Inner seeks use a target `V` within that group's sorted values.
For example, `(K=7,V=order_a)` precedes `(K=7,V=order_b)` according to the order-tuple comparator. When runs
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

A **Z-set** (relation with signed tuple multiplicities) adds weights for identical complete tuples and omits zero totals
([read
consolidation](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/cursor/cursor_list.rs#L150-L174)).
RocksDB's `Put`/`Delete` semantics differ, but its application-defined
[merge operator](https://github.com/facebook/rocksdb/wiki/Merge-Operator) can combine updates too.
The distinction is the collection contract, not an inability to express addition in RocksDB.

`VecIndexedWSet` (in-memory indexed weighted relation) contains keys, offsets, values, and signed weights
([memory
layout](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/vec/indexed_wset_batch.rs#L158-L199));
`FallbackIndexedWSet` (batch type choosing memory or file storage) selects memory or file
representation
([variants](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/fallback/indexed_wset.rs#L34-L58)).
The **root circuit** (top-level operator graph) has only one timestamp value, `()` in Rust, selecting a batch
without varying logical time ([timestamp
mapping](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/time.rs#L214-L219)).
File indexed batches implement asynchronous key fetching
([fetch](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/file/indexed_wset_batch.rs#L447-L478)),
which joins can use
([join fetch
path](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/join.rs#L1653-L1687)).
A comparison must record the actual fetch setting
and storage/cache configuration.

## Folded keys require flat KV storage

The chosen merged-index representation is `byte_key → encoded_value`, with one value per complete physical
key in each visible state. **Folding** (record-type-specific encoding of key fields into bytes) produces the
key. Customer, Orders, and extended Lineitem use different folding rules. Their logical positions `(c)`,
`(c,o)`, and `(c,o,l)` describe the intended order, not storage-visible columns. The storage layer compares
opaque byte strings; the access layer knows the encodings, constructs range bounds, and decodes record types.
Encoding must distinguish records and preserve the required cross-type order. Merely concatenating fields
or putting a type tag first is not a specified encoding.

This is a user-specified architectural requirement. Feldera's existing `K → {V → weight}` representation
remains the baseline for operator state; it is not the physical layout chosen for the merged index. An
operator requests a key or range; the adapter computes encoded bounds, seeks the KV cursor, and decodes
the required fields from the returned entries. The backend compares byte keys without maintaining a
separate index on each folded field.

The proposed Q3 scan keeps the current order identifier, required old/new parent payloads, and four running
scalars: old/new qualifying-line count and revenue. It advances through the order's encoded range and adds
each qualifying line's weighted contribution to those scalars. Once the range ends, it produces the old/new
state tuples required by the selected access, including parent fields when reconstructing a joined state.
The scan does not retain a vector or hash map of
all line tuples. Its summary state does not grow with line count; storage pages, decoding buffers, variable
payload sizes, and shared-consumer buffers still count toward the memory budget.

A consumer that requires individual line tuples receives them through the cursor as the scan advances.
Sharing uses a byte-limited record buffer with a position for each consumer. Borrowed fields remain valid
only while their backing buffer is retained; longer-lived values require budgeted copies. An oversized
range or lagging consumer must trigger backpressure, spill, or a charged reread. If a consumer needs another
ordering, an external sort or maintained access path must supply it; decoding alone does not change order.
For Q3, a returned aggregate tuple such as `(order, revenue)` is a computed cursor result. A bounded buffer
may hold several such tuples until consumers advance. Neither requires constructing a new immutable storage
batch for the entire reconstructed relation. If an existing operator interface requires a batch object,
the adapter must explicitly handle that requirement with budgeted storage, including spills where necessary;
it cannot silently collect the full result in an unbounded in-memory batch. Buffers are released after their
consumers finish; intermediate delta streams remain part of normal operator execution.

These are physical requirements for the adapter. Exact buffer ownership, limits, and fallback selection
remain implementation work, and must be tested with a range larger than the memory budget.

Reuse the same LSM machinery: the spine, immutable-run management, compaction scheduling, cache, and
snapshot ownership. Flat KV changes the records and their comparison/merge rules, not the need for that
machinery. `Spine` is generic over its batch type
([generic trace
implementation](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/spine_async.rs#L2085-L2090));
the adapter must satisfy its batch contracts. This does not mean reusing `FileIndexedWSet` unchanged. The file layer
separates key data from auxiliary data, and documents typed comparisons
([file representation and
ordering](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/storage/file.rs#L3-L65)).
That provides a place to investigate a flat byte-key representation; it does not establish an existing
flat weighted-KV adapter. Select a byte-key type/comparator with the required lexicographic order and define
how batches, merging, reads, and snapshots preserve the encoded values.

Flat KV shape does not select update semantics. Values must preserve payloads, weights, and enough type
information for decoding. A payload replacement can retract and insert different complete tuples at the
same logical identity. The pending-update representation must retain those changes and the old snapshot;
it cannot sum their weights by folded identity alone and discard the payload difference. Exact version and
pending-record encoding remains implementation work. Complete-tuple weighted reconstruction, one visible
value per physical key, and consistent old/new views are required regardless of that choice.

## Aggregation also retains output state

The incremental aggregate reads its accumulated input and passes aggregate results through `upsert`
([construction](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/aggregate.rs#L452-L499)).
The current implementation retains previous output to retract an old aggregate value
([output-state
rationale](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/aggregate.rs#L766-L796)).

The provider boundary is a request for an accumulated relation, keys, and old/new view. Its result must match
ordinary retained-state access, including tuple values, multiplicities, group absence, ordering, and cursor
behavior. Reconstruction can compute a summary or a prior output tuple to satisfy that request; it does not
introduce another aggregate delta-emission algorithm. Both providers feed the same operator path.

For example, if an existing additive-summary algorithm expects a value difference, a revenue change from
100 to 110 produces a difference of 10 in that summary's value. If the algorithm replaces a relation tuple,
it emits `-[[order,100]] + [[order,110]]`, where `[[t]]` means one copy of tuple `t`. These are different
output contracts, not interchangeable wire formats: 10 is a revenue difference, whereas -1 and +1 are tuple
multiplicities. Existing operator code chooses and computes the required form using its deltas and state
accesses. The merged-index provider supplies the same requested state for either form. It must also preserve
group existence when count changes, even if revenue does not.

For this architecture, reconstruct the previous Q3 aggregate tuple from old `A` and supply it where the
output-update path requests the prior value. Preserve the upsert/retraction algorithm. Simply reconstructing
Count/Revenue while leaving its old output **trace** (retained state accessed through the runtime interface) populated
would not remove all
replaced accumulated state.
The same inventory must cover delayed views and both join-side traces. This is a design obligation, not an
existing configurable adapter. The runtime's join wiring uses left delta/current right and right delta/delayed
left ([join
construction](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/join.rs#L698-L749));
accessors must preserve that orientation.

## Weighted reconstruction preserves group existence

For each requested relation `T` and key set `K`, require
`reconstruct(T, source_s, K) = T(database_s) restricted to K`, including payloads, weights, and group existence.
The supplied note identifies the five logical **integrators** (operators accumulating changes into current state)
([five accumulations](../../../../DBSP_w_merged_index/dbsp-merged-index-feasibility.tex#L405)) and their
weighted reconstruction ([definitions](../../../../DBSP_w_merged_index/dbsp-merged-index-feasibility.tex#L508)).

```text
N_s(k) = sum_l w_s(l) * [qualifying(l)]
R_s(k) = sum_l w_s(l) * [qualifying(l)] * rho(l)
A_s    = { (k, R_s(k)) -> 1 | N_s(k) > 0 }
deltaA = A_new - A_old
delta(L join R) = deltaL join R_new + L_old join deltaR
```

A live line with revenue zero gives `(N,R)=(1,0)` and a present aggregate row. Deleting that last line gives
`(0,0)` and retracts the row. A generalized line bag of weight three contributes three times its revenue;
retracting one copy changes its weight to two. This illustrates weighted reconstruction, not SQL equivalence
of the aggregation-first rewrite with duplicate parent rows. That equivalence assumes valid primary/foreign
keys and unit-weight parents ([query
assumptions](../../../../DBSP_w_merged_index/dbsp-merged-index-feasibility.tex#L177)).
The count/existence requirement also appears in the
[operator-state guide](../../../../DBSP_w_merged_index/operator-state.tex#L39).

The main note's replacement has two distinct complete line tuples at weights `-1` and `+1`. Their key weights
sum to zero, but their **support** (tuples with nonzero weight) still marks the order as affected. Identical
complete-tuple changes that cancel
can be discarded. Changed customers expand to descendant orders in either endpoint; **rekeys** (moves to changed
physical key prefixes) include both
prefixes ([affected-key derivation](../../../../DBSP_w_merged_index/dbsp-merged-index-feasibility.tex#L564)).

## Immutable batches preserve old reads

`SpineSnapshot` (read view retaining a fixed set of immutable runs) owns reference-counted batches and supports
constructing a view with additional batches
([snapshot ownership and
composition](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/spine_async/snapshot.rs#L56-L153)).
Combined cursors add matching
weights and suppress zero totals
([consolidation](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/cursor/cursor_list.rs#L150-L174)).
These establish existing weighted-batch primitives. The flat KV adapter must implement equivalent old/new
visibility for encoded payloads; concatenating batches or overwriting equal byte keys is not by itself proof
of correct weighted reconstruction. Reads must resolve the pending changes without waiting for compaction.

The integration must keep the old source snapshot and parent lookup alive, stage complete payload retractions
and descendant rekeys, then **seal** the pending updates
(make the complete set immutable and readable) for a consistent new view. Retire old ownership only when every consumer
has
finished. Runtime transactions can span multiple steps, so cursor exhaustion or one step is not the barrier
([transaction
scheduling](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/schedule.rs#L186-L226),
[commit
flushing](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/circuit_builder.rs#L7777-L7805)).

These primitives support the design but do not establish atomic publication or restart of the new shared
index. Implementation must coordinate source batches, native-order lookup, and output progress so recovery
exposes a complete endpoint and retry does not apply weights twice. Snapshot retention also has a memory and
storage cost. A per-record phase bit is not an architectural requirement; the flat KV adapter must specify its version
retention and pending-update representation.

## Consumers determine the required payload

Q5's selected Customer–Orders–Lineitem expression consumes an external Nation/Region **gate** (eligibility filter) and
preserves
supplier identifiers for later matching. A changing gate needs consistent old/new multiplicities and an
expansion to affected customers; a boolean gate suffices only under the key assumptions
([Q5 consumer and
gate](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/q5/query.tpp#L219-L335)).

Q10's selected expression produces returned-line join rows and full required customer payloads. Customer
aggregation and Nation-name attachment remain downstream; a per-order revenue-only record cannot replace
those join rows ([Q10 logical
plan](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/q10/plans/family_logical.dot#L34-L69),
[customer output
consumption](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/q10_family/visitor.hpp#L126-L200)).
These are analytical boundary checks, not
executed Feldera reconstruction experiments. Any comparison must match actual date endpoints: the inspected
implementations use day offsets ([Q5
bounds](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/q5/query.tpp#L307-L315),
[Q10
bounds](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/q10/query.tpp#L100-L107)),
which need not equal calendar intervals.

## Refresh reads can serve reconstruction

Existing **RF2** (TPC-H order-and-lines deletion refresh) discovery looks up the order's customer and scans native
Lineitem for line numbers
([discovery](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/tpch_family/refresh.hpp#L143-L178));
**COL** (Customer–Orders–Lineitem index) helpers perform direct insertions and erasures
([maintenance](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/tpch_family/col_pipeline.tpp#L150-L220)).
These demonstrate the access paths, not old-payload
retention or shared incremental reads. The proposed scanner must retain full required payloads, distribute
them to consumers, and charge any repeated pass. **RF1** (TPC-H order-and-lines insertion refresh) requires verified
freshness for its new-key shortcut.

The supplied paper's refresh experiment describes order-group insertion/deletion
([workload](../../../../merged_index_interesting_orderings/sections/experiments_revised.tex#L173)). Its LSM
variant matches **Base-Merge** (the paper's baseline merge-join plan) rather than leading in that experiment
([backend comparison](../../../../merged_index_interesting_orderings/sections/experiments_revised.tex#L229)).
That result does not predict a benefit for Feldera; storage backend and total maintenance work matter.

## Evidence and measurements bound the claim

The source links pin Feldera at `f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c` and LeanStore at
`305ad0a98b147d048a37a1eba3787b35b1181b85`. Local manuscript/checker links assume sibling checkouts under the
same parent as Feldera; their `#L` fragments are source-viewer line locators. The DBSP note and checkers are at
`ac8380511fe4463b81651a3f6d6991b849577ff7`; the cited files are clean. The paper's cited experiment file is clean
at `6c4c3a5d851c9044352da2aead6b419c45c77237`. Unrelated working-tree manuscript changes are not evidence here.

Run the supplied models from the repository root:

```sh
python3 ../DBSP_w_merged_index/validation/check_q3.py
python3 ../DBSP_w_merged_index/validation/check_operators.py
```

The Q3 suite has 12 test methods and the operator suite 8, including weighted changes, simultaneous changes,
group existence, rekeys, and sequential updates
([Q3 tests](../../../../DBSP_w_merged_index/validation/check_q3.py#L391)). Passing them demonstrates model
semantics. They do not execute this access-layer design, recovery, Q5/Q10 reconstruction, or physical I/O.

| Acceptance question | Required evidence |
| --- | --- |
| Are operator inputs and results identical? | Run the same operator path with retained and reconstructed state providers; compare requested tuples, weights, ordering, absence, and old/new views, then check value-difference or retraction/insertion outputs against independent evaluation. |
| Does the flat KV adapter preserve encoding and weighted updates? | Verify cross-type byte ordering, exact range bounds, type decoding, payload replacements at unchanged keys, signed multiplicities, and old/new visibility. |
| Does shared access preserve lifecycle and memory bounds? | Interleave consumers, exceed the buffer budget, and exercise abort/restart; verify old payload retention, consistent lookup publication, and exactly-once batch advancement. |
| Does storage replacement actually occur? | Inventory retained state, including aggregate output and delays; confirm intermediate snapshots are not accumulated again. |
| Is total maintenance cheaper? | Compare beyond-memory runs under equal total memory and comparable durability, with matched predicates/results and baseline fetch enabled where configured. |

For equal-sized blocks, `unique footprint = block_bytes * size(union of all consumers' blocks)`.
Actual read traffic is the sum of physical read events; eviction can cause repeat reads. Include RF1/RF2,
parent lookup, all operator access, staging, merging, checkpointing, and spills in measured reads/writes.
Report persistent and peak storage, CPU/decoding, memory high-water, fan-out, and complete-batch latency for
clustered and scattered updates. Neither fewer unique blocks nor a resident scan speedup proves lower total
maintenance I/O.
