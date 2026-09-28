# Runtime reconstruction and state ownership

**Conclusion.** Manually compose a Rust DBSP circuit with a query-specific, affected-order evaluator over the
merged index's sealed before/after views. For Q3 it can emit the complete unordered relational result delta
without retaining the five logical intermediates between transactions. Use reconstructed batches and existing
non-incremental operators as a correctness bridge where useful. Defer generic trace substitution because it
changes existing state ownership and snapshot interfaces. Old/new views suffice for this nonrecursive,
complete-batch algebra; retained timestamp histories are not a merged-index requirement. This recommendation
is **proposed**; Q3 reconstruction is **semantically demonstrated**, and runtime integration and I/O savings
are **unverified**.

| Route | What persists between transactions? | Evidence and assessment |
| --- | --- | --- |
| Replay/backfill into the ordinary circuit | Rebuilt join/aggregate traces and output state | **Implemented** replay machinery; a merged-index replay source is **proposed**. Useful for initialization or migration, not a retention-elimination result. |
| Query-specific before/after evaluation | Weighted source index, parent/reverse paths, recovery metadata; no retained Q3 intermediate required | **Semantically demonstrated** for Q3; **proposed** runtime route. Best first experiment for the intended state/I/O tradeoff. |
| Reconstructed batches consumed by existing operators | Temporary batches; ordinary incremental consumers additionally retain their normal traces | Non-incremental join/aggregate kernels are **implemented**; reconstruction wiring is **proposed**. Scope batches to complete groups and account for sorting/spill. |
| Reconstructed trace/cursor substitution | Source index plus transient reconstruction/cache state, if every selected trace owner is replaced | Cursor mechanics are **implemented**; the substitute is **proposed** and snapshot compatibility **unverified**. Timed compatibility applies only when the chosen circuit uses nontrivial logical time. |

This report uses unmodified Feldera source pinned to `f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c`; the inspected
runtime files match that revision. It covers one Customer–Orders–Lineitem pipeline, Q3 before ordering and
limit, and the corresponding LeanStore Q5/Q10 portions. It excludes Q9, multi-pipeline sharing, and
compaction-driven maintenance. The five logical accumulations in the [paper contract](paper-contracts.md) are
a semantic inventory, not a count of traces emitted by the SQL compiler. The prototype directly composes that
selected algebra in Rust; controlling a generated SQL plan is optional later work, not a prerequisite. See
[compiler control](compiler-control.md) for the existing compiler's behavior. The comparison baseline remains
normal SQL execution in unmodified Feldera.

## What the existing runtime actually owns

`dyn_join_generic` constructs two `dyn_shard_accumulate_trace` streams, uses current and delayed traces in
separate `JoinTrace` branches, then adds their outputs. Its particular orientation is `Δleft ⋈ right⁺ + left⁻
⋈ Δright`. The paper note's `Δleft ⋈ right⁻ + left⁺ ⋈ Δright` is equally valid, but the delayed side changes;
mixing their branch assignments either omits or doubles the simultaneous-change cross term. [Join
construction, `join.rs:698–749`][join-build]

`dyn_shard_accumulate_trace` owns a feedback trace, appends the accumulated transaction input, registers
replay, and caches its delayed counterpart. `JoinTrace::eval` obtains a read-only snapshot at flush, can await
`fetch`, then uses a joint cursor; the timed path multiplies weights and joins timestamps. Consequently, a
replacement must survive asynchronous/chunked consumption and match more than `seek(key) → rows`. [Trace
construction, `accumulate_trace.rs:67–145`][trace-build]; [join evaluation, `join.rs:1635–1748`][join-eval]

Aggregation has two state owners to audit: `dyn_aggregate_generic` supplies an input trace to
`AggregateIncremental`, then passes its new values through `upsert`, which retracts previous output values.
The implementation explicitly chooses retaining old output over recomputing it. Replacing only the input trace
does not remove that output-state obligation. Even the linear API weighs its input and calls this generic
incremental path. [Aggregate construction, `aggregate.rs:452–553`][aggregate-build]; [retained-output
rationale, `aggregate.rs:766–796`][aggregate-state]

The [index report](feldera-indexes.md) details the physical baseline. A snapshot can merge cursors across
multiple file batches. The concrete batch type depends on the circuit's clock; the generic name `TimedSpine`
alone does not imply stored time/weight histories for Q3.

### Old/new state versus logical timestamps

For a complete input batch, `z⁻¹` supplies the previous integrated relation; the selected Q3 algebra needs
that old relation and the new relation. It does not need arbitrary past snapshots. The two phase views
reconstruct exactly those values, and multiple execution steps do not create extra semantic versions: the
scheduler defines a transaction as steps for **one logical timestamp** and flushes operators to completion.
[Scheduler contract, `schedule.rs:186–226`][schedule]

This distinction is also present in the implementation. `RootCircuit` is `ChildCircuit<(), ()>`;
`ChildCircuit<P,T>::Time` is `T`. `Timestamp for ()` defines `TimedBatch<B> = B`, so
`TimedSpine<B,RootCircuit>` resolves to `Spine<B>` over the original untimed batch. The source explicitly says
nonrecursive queries need only the root circuit. `JoinTrace` branches on `size_of::<T::Time>()`: the zero-size
branch multiplies the delta weight by `weight_checked()` without timestamp-valued output scheduling. Thus even
ordinary root-circuit Q3 joins need not have the timed `FileValBatch` layout. [Clock type,
`circuit_builder.rs:1684–1692`][clock-type]; [root definition, `:3109–3131`][root-clock]; [unit timestamp
mapping, `time.rs:214–228`][unit-time]; [trace alias, `accumulate_trace.rs:43`][trace-alias]; [untimed join,
`join.rs:1796–1824`][untimed-join]

This is the multithreaded runtime's actual entry point too: `Runtime::init_circuit` accepts a constructor over
`&mut RootCircuit` and calls `RootCircuit::build` on each worker. There is no automatic iterative wrapper in
that construction. For an untimed indexed input, the fallback batch delegates `fetch` to its file variant, and
`FileIndexedWSet::fetch` implements selective reads. Therefore the baseline's `fetch_join` option must not be
dismissed based on the separate timed `FileValBatch` implementation. [Runtime construction,
`dbsp_handle.rs:714–753,788–803`][runtime-root]; [fallback fetch, `indexed_wset.rs:325–337`][fallback-fetch];
[file fetch, `indexed_wset_batch.rs:447–478`][file-fetch]

The general implementation also handles nested/iterative circuits. Their product timestamps distinguish inner
iterations across outer epochs; joins combine times by their least upper bound and may schedule output at a
later logical time. That is why the generic trace/cursor interfaces expose time/diff pairs. These are
computation timestamps, not event-time columns, wall-clock times, or a user-facing MVCC history. They are
relevant if that general operator path is reused, not a requirement to add history to the scoped merged index.
[Logical-time model, `time.rs:1–42`][logical-time]; [nested join formula, `join.rs:663–696`][nested-join];
[future outputs, `join.rs:1755–1795`][future-outputs]

## Replay rebuilds retained state

The [concurrent bootstrapping design](../../../../../docs/design/concurrent_bootstrapping.md) is useful precedent for defining a
consistent cut, recording changes after that cut, and waiting for synchronization before state transfer. It is
not an implementation of on-demand intermediate reconstruction.

`CircuitHandle::restore` locates missing checkpoint state, walks backward to registered replay sources, and
restricts execution to the backfill region. `compute_replay_nodes_step` substitutes replay edges where a
checkpointed source exists. `ReplayState::next_chunk` reads an owned trace, sums weights across its
timestamps, skips zeros, and emits bounded batches; a chunk may end inside a key. Completion requires both
exhausted replay sources and a completed transaction. [Restore contract,
`circuit_builder.rs:8023–8055`][restore]; [replay substitution, `:8675–8718`][replay-edges]; [chunk semantics,
`replay.rs:16–79`][replay-chunks]; [completion, `circuit_builder.rs:8724–8761`][replay-complete]

Concurrent bootstrap records boundary deltas, feeds them through synchronization replay, and swaps the rebuilt
operators' state into the live circuit. Those are **implemented** mechanisms at the pinned revision. A
proposed merged-index replay source could provide equivalent initial rows, but ordinary downstream joins and
aggregates would still materialize their state. Replaying a complete snapshot repeatedly into a live
incremental input would also double-count it unless that region is reset or supplied the correct delta.
[Boundary recorder, `circuit_builder.rs:8203–8264`][recorders]; [synchronization and state transfer,
`:8402–8470`][cutover]

## Three reconstruction choices

### 1. Query-specific before/after evaluation

Construct the nonrecursive circuit directly in Rust using `Runtime::init_circuit` (or `RootCircuit::build` for
a local circuit): one custom evaluator consumes staged source changes, requests sealed old/new ranges, and
emits the selected relation's weighted delta. Its internal algebra can be handwritten or assembled from the
existing batch operators below. The evaluator and storage adapter are **proposed**, while direct Rust circuit
construction is **implemented**. This route controls the algebra and state owners without depending on SQL
optimizer choices. [Root construction contract, `circuit_builder.rs:3109–3123`][root-clock]

For each affected order, reconstruct the following in both snapshots. Let `s ∈ {−,+}`, `Fˢ = σ(ship >
cutoff)Lˢ`, and `g(l) = price(l)(1−discount(l))`. All sums use the stored signed tuple weights; valid
integrated states satisfy the [paper contract](paper-contracts.md).

| Logical accumulated input | Reconstruction from weighted sources | Lifetime in this route |
| --- | --- | --- |
| Count/Revenue per `(c,o)` | `Countˢ = Σ Fˢ(l)`; `Revenueˢ = Σ Fˢ(l)g(l)` over all matching lines | Two temporary summaries per order |
| `Aˢ` | One tuple `(c,o,Revenueˢ)` of weight 1 exactly when `Countˢ > 0` | Temporary group relation |
| Eligible Orders `Oeˢ` | `σ(day < cutoff)Oˢ` with original tuple weights and payload | Point/range reconstruction |
| `Bˢ` | `Aˢ ⋈(c,o) Oeˢ`, multiplying weights | Temporary per-order contribution |
| Eligible Customers `Ceˢ` | `σ(segment = parameter)Cˢ`, preserving weights and payload | Parent lookup or customer-range context |

Project `Bˢ ⋈c Ceˢ` to `(orderkey,revenue,orderdate,shippriority)` and emit the new contribution minus the old
contribution. A live zero-revenue group remains a row; an empty group does not. A revenue replacement retracts
the entire old result tuple and inserts the new one. These are the complete Q3 relational outputs before
order/limit. The [query report](query-reconstruction.md) provides the detailed weighted examples and proof.

The affected-order support set includes changed lines and orders, every order under a changed customer, and
both old/new placements of rekeys. Derive support from tuple changes before projecting and summing signed
weights: a price replacement can have zero net weight at the order key and still change revenue. Each order is
evaluated once after deduplication. Initial load uses the empty old view. Only the source index, required
access paths, and commit/recovery metadata persist; temporary output batches and affected-key sets may need
external storage. Downstream output delivery can still own state, so this does not claim a state-free entire
application.

### 2. Reconstructed batches consumed by existing operators

Existing `dyn_stream_join_generic` and `dyn_stream_aggregate_generic` evaluate their supplied batches without
constructing the incremental trace feedback loops above. They provide a practical reuse boundary: construct
complete, properly sorted old/new group batches, evaluate each snapshot, and subtract their outputs.
Alternatively, supply reconstructed accumulated partners to explicit bilinear delta branches. Both are
**proposed manual Rust circuit wiring**; no SQL compiler change is needed to try them. [Batch join,
`join.rs:359–377`][stream-join]; [batch aggregate, `aggregate.rs:369–390`][stream-aggregate]

Batch completeness matters. If half an order's lines arrive in one chunk and half in another, independently
aggregating each produces partial revenue rows; pairwise joining arbitrary chunks likewise misses cross-chunk
pairs. Keep a whole group together, carry an explicitly bounded summary across chunks, or spill/replay the
necessary partner batches. A very large group cannot be assumed to fit RAM. Preserve Count alongside Revenue:
using only a numeric sum loses the empty-versus-zero distinction. Feeding these batches into ordinary `join`
or incremental `aggregate` instead recreates retained traces; it is a valid hybrid only when that retention is
intentional.

### 3. Trace/cursor substitution

A reconstructed provider would expose intermediate tuples in each consumer's logical key/value order, despite
the physical `(c,o,l)` order, and support seeks, value rewind, weight enumeration, and snapshots that remain
stable for the consumer's lifetime. The current `Cursor` contract allows multiple time/diff pairs;
`SpineSnapshot` pins immutable batches through `Arc`s. Those guarantees need an equivalent owner for
merged-index pages and reconstruction buffers. [Cursor contract, `cursor.rs:42–90,160–166`][cursor]; [snapshot
ownership, `snapshot.rs:110–153`][snapshot]

There is no demonstrated drop-in implementation. Snapshot fetch, statistics, iteration order and delayed
feedback have concrete expectations even when `TimedSpine` resolves to an untimed root trace. Reconstructing
`B` also executes joins inside the provider; it is not merely decoding another record type. A two-phase bit
supplies the old/new boundary required here. If the provider is later generalized to nested-time consumers,
their time/diff and output-scheduling contracts need a separate implementation and proof. That broader
compatibility is unverified and is not required for the manually composed root circuit.

## Snapshot and scheduling contract for the recommended route

These are **proposed** runtime obligations, shared with the [storage contract](merged-index-storage.md),
rather than APIs supplied by `mi_db`:

1. Assign one batch/transaction identity. Phase 0 stores stable weighted bases;
   phase 1 stores signed pending complete tuples. `old = base` and
   `new = consolidate(base + pending)`. The bit is independent of weight and
   payload identity. Seal all source changes and parent-derived rekeys before
   evaluating either view.
2. Pin both views for all workers and consumers. Preserve old payloads, deleted
   rows, and both paths of a moved order and its unchanged descendants. Bind
   every range/parent lookup to the same batch; separate scan sessions do not
   themselves establish a common snapshot.
3. Discover and deduplicate affected keys, then schedule bounded reconstruction
   tasks. For a shared transaction-level walk, demultiplex typed rows and reuse
   temporary group summaries explicitly; independent consumer scans otherwise
   repeat I/O. Define ownership or exchange for each group so concurrent workers
   neither omit nor duplicate its contribution.
4. Wait for complete groups and complete result deltas. Feldera transactions span
   multiple steps, and commit forces all operators to finish their inputs.
   One `step()` or an exhausted storage iterator is not the completion barrier.
   Include asynchronous reads, exchanges, output chunks, and delayed consumers.
   [Transaction contract, `circuit_builder.rs:7777–7803`][transactions]
5. After every consumer releases the old view, atomically fold pending weights
   and retire pending records through a backend transaction or journal. Couple
   source progress, output progress, and restart metadata to one recoverable
   boundary; otherwise a crash can lose or duplicate deltas. The physical fold
   and the cross-system recovery protocol are unverified.

RF1/RF2 fit the same contract only after their inserts/deletes become staged weighted changes; direct physical
erasure before readers finish is invalid. `mi_db` scan sessions are a proposed bounded access design, not a
runtime snapshot, scheduler, or transaction implementation.

## Q5/Q10 reuse and I/O decision

LeanStore's Q5 `query_by_merged` builds side tables and calls `col_group_walk` with a visitor that gates
customers/orders and probes supplier-related data. Q10 uses the same walk with its own visitor and
customer-group finalization. The selected Q5 output is `(C ⋈ RN) ⋈ (Oe ⋈ L)`, with external Region/Nation
input `RN` bound to the same snapshot; Supplier and final aggregation remain outside. Q10 selects `C ⋈ (Oe ⋈
returned_L)`; customer aggregation, Nation attachment, and ordering remain outside. These are selected
semantic boundaries, not verified generated Feldera plans; [query reconstruction](query-reconstruction.md)
defines their exact payloads and side-input obligations. These are **implemented batch scan patterns**, not
weighted incremental reconstruction. Reuse the COL access/session boundary and query-specific predicates; give
side-table reads the same snapshot boundary where they participate. This report makes no claim to maintain
full Q5/Q10 results or to derive affected COL ranges for arbitrary side-table changes. [Q5 visitor entry,
`query.tpp:603–631`][q5]; [Q10 visitor entry, `query.tpp:356–376`][q10]

The performance question is whether avoided persistent intermediate writes and trace probes outweigh repeated
source reconstruction. One changed line can rescan an entire order twice; a customer update can scan all
descendant orders. Separate reconstructions for each logical integrator can multiply that traffic; a fused
old/new order walk may reduce it but is not measured. Charge temporary batch sorting/spill, parent/reverse
lookups, phase consolidation, affected-key deduplication, and output writes to the design. Scanner memory
bounds alone do not bound total operator memory.

Compare the recommended route first against **unmodified Feldera** with equal total memory and comparable
durability on data larger than RAM. Record cold and warm block reads, bytes written, scan amplification, peak
temporary memory, transaction latency, and restart behavior. Keep the baseline's ordinary storage maintenance
enabled; the proposed visibility protocol does not depend on it. Use
[validation-plan](../evidence/validation-plan.md) to gate correctness and physical measurements before
pursuing cursor substitution. No runtime code was added or benchmarked in this investigation.

[join-build]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/join.rs#L698-L749
[trace-build]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/accumulate_trace.rs#L67-L145
[join-eval]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/join.rs#L1635-L1748
[aggregate-build]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/aggregate.rs#L452-L553
[aggregate-state]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/aggregate.rs#L766-L796
[restore]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/circuit_builder.rs#L8023-L8055
[replay-edges]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/circuit_builder.rs#L8675-L8718
[replay-chunks]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/replay.rs#L16-L79
[replay-complete]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/circuit_builder.rs#L8724-L8761
[recorders]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/circuit_builder.rs#L8203-L8264
[cutover]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/circuit_builder.rs#L8402-L8470
[stream-join]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/join.rs#L359-L377
[stream-aggregate]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/aggregate.rs#L369-L390
[cursor]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/cursor.rs#L42-L166
[snapshot]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/spine_async/snapshot.rs#L110-L153
[transactions]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/circuit_builder.rs#L7777-L7803
[schedule]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/schedule.rs#L186-L226
[clock-type]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/circuit_builder.rs#L1684-L1692
[root-clock]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/circuit_builder.rs#L3109-L3131
[unit-time]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/time.rs#L214-L228
[trace-alias]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/accumulate_trace.rs#L43
[untimed-join]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/join.rs#L1796-L1824
[logical-time]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/time.rs#L1-L42
[nested-join]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/join.rs#L663-L696
[future-outputs]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/join.rs#L1755-L1795
[runtime-root]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/dbsp_handle.rs#L714-L803
[fallback-fetch]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/fallback/indexed_wset.rs#L325-L337
[file-fetch]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/file/indexed_wset_batch.rs#L447-L478
[q5]: ../../../../../leanstore/frontend/tpch/q5/query.tpp#L603
[q10]: ../../../../../leanstore/frontend/tpch/q10/query.tpp#L356
