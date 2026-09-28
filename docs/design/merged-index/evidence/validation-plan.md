# Validation and beyond-memory experiment plan

**Current evidence:** the existing supplied Q3 checker passed 12 test methods; the operator checker passed 8.
They exercise seeded cases inside those methods. They do not execute a weighted merged-index backend, a
generated Feldera plan, Q5/Q10 reconstruction, crash recovery, or an I/O comparison. Revisions and commands
are recorded in [source-map](source-map.md#executed-semantic-checks). No runtime, diagnostics, APIs, or test
code were added by this documentation investigation. Everything below is proposed future work.

| Gate | Comparison and acceptance criterion | Failure meaning |
| --- | --- | --- |
| Exact selected semantics | Reconstructed old/new inputs and emitted deltas equal independent full evaluation as consolidated weighted tuples. | Incorrect reconstruction or invalid scope assumptions. |
| Hand-composed circuit boundary | Verify the explicit Rust DBSP operator graph, state ownership, endpoint schema, and batch deltas. | The circuit does not implement the selected expressions or retains replaced state elsewhere. |
| Storage/batch conformance | Stable old, sealed new, retained deletes/rekeys, atomic finalize/abort/restart under bounded memory. | Phase bit and scanner contract are insufficiently implemented. |
| Runtime integration | One complete batch produces exactly the oracle delta while replaced intermediates are not retained elsewhere. | Replay or double-maintained state defeats the intended replacement. |
| Physical benefit | Equal-budget, comparable-durability runs show reproducible total I/O/latency improvement against unmodified Feldera. | Semantic feasibility has not produced a useful physical design. |

## Exact semantic checks to add later

Use an independent evaluator over native tables for strict-key Q3 and explicit COL expressions for Q5/Q10.
Resolve extended Lineitem prefixes independently from endpoint Orders. At every batch compare: each selected
accumulated input in both states; each aggregate count/sum and group existence; `ΔT=T⁺−T⁻`; the accumulated
output after applying `ΔT`; and stored committed source weights after finalization. Compare complete tuples
and integer weights, not row counts, revenue-only checksums, or unordered XOR hashes. Use rational/fixed exact
decimal arithmetic so a mismatch cannot hide behind floating-point tolerances.

| Family | Directed cases and randomized generation | Required oracle property |
| --- | --- | --- |
| Q3 strict keys | Initial load; line insert/delete/price/discount/ship changes; order predicate and output-field changes; customer activation/deactivation; zero and last-row groups. | Native join/filter/group result equals selected aggregation-first result before sort/limit. |
| Q3 support and simultaneous changes | Same-key payload replacements; offsetting price changes; cancelling identical source deltas; changes on both join inputs and all three sources. | Support expansion retains changed groups; cross term counted once; equal output tuples cancel. |
| Parent changes | Order reassignment with unchanged lines; line moves between orders; parent identity change with valid referencing updates; simultaneous move and predicate change. | Both old/new physical prefixes and all induced descendants are covered; no orphan endpoint. |
| Q5 selected joins | Date endpoints; customer nation replacement; supplier payload preservation; fixed gate and explicitly changing external gate. | Exact `C5`, `O5`, `L5`, `B5`, and `J5` multiplicities; external gate changes expand all affected customers. |
| Q10 selected joins | Returnflag crossings; date endpoints; last returned line; zero revenue; customer payload-only replacement. | Exact `C10`, `O10`, `L10`, `B10`, and `J10`; output payload is preserved, customer aggregate remains outside. |
| Generalized bags | Repeated complete lines; multiplicity retractions; join products above one; simultaneous opposing multiplicity changes. | Endpoint weights stay nonnegative; use weighted expression oracle and label violations of strict source PK constraints. |
| Refresh sequences | RF1 fresh order groups, RF2 existing groups, multi-order batches, repeated batches, abort/retry. | One committed application per batch; full negative deletion payloads survive until consumed. |

Keep the strict-key and generalized-bag generators separate. Do not apply Q3's PK-based aggregation rewrite to
duplicate parent bags and call the result SQL equivalence. Generate valid endpoint pairs first, then derive
signed complete-tuple deltas; also generate long sequential workloads so state lifecycle bugs surface. Use the
[worked examples](weighted-examples.md) as directed expected results. Changing query parameters requires a
separate invalidation/recomputation contract; hold them fixed during the scoped incremental experiments.

## Storage and runtime conformance

Test the proposed direct weighted record format and separate phase bit with a fake small block/buffer budget
before large runs. Check exact seek, inclusive and exclusive ranges, type tags, empty ranges, duplicate tuple
consolidation, payload replacement, and old/new views of the same native identity. Parent lookups must be
persistent and remain correct when the index exceeds RAM. A changed customer or order with descendants larger
than the memory budget must complete with the declared bounded strategy, or fail explicitly before publishing
a result.

Drive multiple consumers through one range with different rates. Same-type accessors must have independent
positions; other-type rows must survive until all readers consume them. Independent sessions must not move
each other's cursors. Measure pinned buffers and any spill or re-read, and verify that a buffer limit never
silently drops required data. Old and new contributions must come from one stable/sealed boundary even when
readers interleave.

Inject failures after partial pending staging, after sealing, during evaluation, during fold, and at
commit/publication boundaries. Abort leaves the previous committed base intact. Recovery exposes either the
prior or the complete new batch according to the declared commit point, never a mixture; retry neither
duplicates weights nor skips a fold. Recovery tests must cover order lookup and both rekey prefixes as well as
the source records. This proposal adds no compaction-dependent correctness step.

The first runtime prototype should manually compose a Rust DBSP circuit with an explicit query-specific
old/new evaluator boundary, following [runtime reconstruction](../reports/runtime-reconstruction.md). Verify
that feeding a snapshot into an ordinary incremental operator has not recreated the very retained traces being
replaced. Later direct trace adapters would additionally need seek/snapshot/ownership/scheduling contract
tests, plus timestamp-history tests if generalized beyond the root circuit. Those general contracts are not an
algebraic prerequisite for this nonrecursive, complete-batch evaluator: its two stable batch views suffice.

## Circuit and boundary verification

Specify the manually composed Rust DBSP graph, its input/output weighted types, batch staging barrier,
evaluator ownership, and finalization order. Confirm that each reconstructed relation in the formula tables
has its intended consumer and that the circuit does not reintroduce retained intermediates. For Q3 verify
aggregation-first equivalence under the declared keys and the complete four-column relational result before
sorting/limit. For Q5 record where `G5` is supplied and whether it is fixed; for Q10 stop before Customer
aggregation and Nation attachment.

Keep the SQL compiler unmodified. Its generated graph, indexed streams, join orientation, root timestamp/batch
specialization, and output schema should be inspected to describe the baseline accurately; they do not need to
match the hand-composed prototype's graph. Compiler shape control is optional later work, not a prototype
gate. Compare equivalent endpoint schemas and consolidated batch deltas, using an independent full evaluator
as the correctness oracle. See [compiler control](../reports/compiler-control.md).

Q5's `+365` days and Q10's `+90` days in the inspected LeanStore code need explicit bound matching with the
SQL fixtures. Include leap-year and variable month-length cases if comparing calendar-interval SQL. A
predicate mismatch is an invalid comparison, not a storage-performance result.

## Physical experiment matrix

Use the pinned, unmodified Feldera runtime/compiler as the required baseline, with its ordinary disk-backed
batches, root-circuit indexed traces, and background work. Verify the root untimed file batch's actual
batched-fetch path and runtime setting; do not substitute the nested timed batch's behavior in the cost model.
Run the proposed evaluator separately with its weighted merged index, persistent parent lookup, pending
records, and all required access paths. Maintain identical input batches, selected outputs, worker counts,
hardware/storage, and durability guarantees. If a backend cannot match durability, report that cell separately
and make no like-for-like benefit claim. Inventory every stored structure; do not let one strategy inherit
unused indexes or warm data from another run.

| Axis | Required cases |
| --- | --- |
| Data/memory | State well beyond the full available memory budget; several state-to-memory ratios; a smaller resident control case. |
| Update locality | Clustered versus scattered orders; one-line changes; complete RF1/RF2 groups; Customer fan-out; order rekeys; skewed large groups. |
| Batch size | Single-order refresh matching the paper's workload, then small and large complete batches to expose range coalescing and amortization. |
| Cache and schedule | Declared cold and warm starts; fused shared traversal versus repeated per-integrator reconstruction; sufficient runs to report distributions. |
| Query boundary | Q3 full unordered relational result; Q5 and Q10 selected COL results with fixed declared external inputs. Report each query separately. |

Count the backend cache, operating-system cache allowance, operator state, pending tuples, parent lookup,
per-session buffers, key discovery, and temporary spills within equal total byte budgets. A bounded scanner
alone is not a bounded whole-query memory proof. Publish actual high-water memory and cache allocation,
including per-worker multiplication. State how cold cache conditions were established and how warm-up,
checkpointing, and background work enter timing.

For each complete batch and for the overall run, report:

- Weighted-input/output correctness, batch latency distribution, throughput,
  and time to the declared durable commit point.
- Actual physical reads and bytes, index/data block cache hits/misses, repeat
  reads, seeks, and the union of unique block identities across **all** consumers.
- Records visited/decoded, affected orders/customers, descendant fan-out, typed
  buffer high-water bytes, temporary spill/re-read traffic, and CPU time.
- Persistent bytes by structure and phase; staging, fold, parent-index,
  journal/WAL, checkpoint, and background write traffic; peak live storage while
  old and pending versions coexist.
- RF1/RF2 discovery costs and reuse: parent lookup, deleted payload acquisition,
  shared scans with reconstruction, and any second pass. Include fresh-key
  checks and rekey writes even when the output delta is zero.

The unique-block union estimates the available reuse; actual device read events determine realized I/O after
eviction and caching. Existing counters should be used where sufficient; any unavailable measurement must be
marked unavailable until a later instrumentation task supplies it. This plan is not an instruction to add
diagnostics in the current documentation change.

Present correctness first, then total batch I/O/latency and its read/write breakdown. Co-location's fewer
seeks can lose to large descendant scans, phase processing, or write amplification. A successful semantic test
or a faster resident query scan cannot establish the requested beyond-memory maintenance advantage. Preserve
all negative or mixed performance results with the same configuration and provenance detail as improvements.
