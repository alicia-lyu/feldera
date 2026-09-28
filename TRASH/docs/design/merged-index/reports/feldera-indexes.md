# Feldera index and trace feasibility for one pipeline

**Conclusion.** Feldera already stores weighted Z-sets in physical sorted memory/disk batches, with seeks,
scans, caching, and batch merging. For the scoped nonrecursive **root circuit**, `Time = ()`: the generic
`TimedSpine` alias reduces to a spine of the ordinary untimed batch type. Historical timestamps are not a
barrier to the proposed old/new protocol. The work is reconstructing intermediates and satisfying cursor,
ownership, and transaction contracts; no merged-index mode exists in the inspected paths. Beyond-memory
benefit remains **unverified**. [Root
circuit](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/circuit_builder.rs#L3109-L3125),
[unit timestamp
mapping](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/time.rs#L214-L219)

| Finding | Status | Consequence for a merged index |
| --- | --- | --- |
| Indexed Z-sets carry sorted keys, values, and signed weights; joins consume their traces through the `BatchReader`/`Cursor` boundary. | Implemented | Reuse the logical operator contract. [Index conversion](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/index.rs#L24-L65), [cursor contract](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/cursor.rs#L42-L53) |
| A spine reads multiple immutable memory/file batches. Root joins use untimed batches; nontrivial clocks select timed batches. | Implemented | Baseline root joins can use `FileIndexedWSet` batched fetch; do not infer physical layout from the `TimedSpine` name. [Alias](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/accumulate_trace.rs#L43), [unit mapping](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/time.rs#L214-L219) |
| Existing trace reuse is keyed to a stream; a mapped key order creates another indexed stream. | Implemented | A physical record shared across different key orders requires new ownership and update routing. [Trace cache](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/trace.rs#L550-L565), [mapped index](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/filter_map.rs#L85-L106) |
| A separate old/new phase bit and the proposed shared source layout are absent from these paths. | Proposed | Specify update ordering, snapshots, recovery, and beyond-memory cost before claiming a win. [Current file record write](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/file/indexed_wset_batch.rs#L982-L1016) |

## Scope and baseline

This report pins the unmodified Feldera source at `f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c`. It considers
**one pipeline** and Q3's complete relational result before ordering/limit: filter customer, orders, and
lineitem; join them; and sum revenue by order/date/priority. The supplied aggregation-first reconstruction
circuit is equivalent under its key assumptions; it is not a captured Feldera plan. For LeanStore Q5 and Q10,
it considers only the corresponding filtered, indexed join input and lookup/scan portions, not full query
execution. The repository's Q3, Q5, and Q10 SQL establishes those relational shapes, but the physical operator
inventory below is an architectural mapping from DBSP APIs, **not** a captured compiler plan for those SQL
statements. Q9, cross-pipeline sharing, and maintenance whose correctness depends on compaction are outside
this design. [Q3
SQL](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/pipeline-manager/demos/sql/00-accelerating-batch-analytics.sql#L298-L322),
[Q5
SQL](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/pipeline-manager/demos/sql/00-accelerating-batch-analytics.sql#L348-L373),
[Q10
SQL](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/pipeline-manager/demos/sql/00-accelerating-batch-analytics.sql#L504-L535)

Z-sets are the logical weighted collections; traces retain changes to those collections, and sorted
memory/disk batches implement that state physically. “Timed” denotes logical computation time, not necessarily
a SQL event-time column. The generic trace model uses `(key, val, time, diff)`, but root `time = ()` stores no
changing timestamp history. `BatchReader` reads sorted keys and values, `Batch` builds and merges batches, and
`Trace` appends batches. The weight is signed multiplicity, so positive and negative weights must still add
and cancel in the proposed format. A visibility bit is an additional physical field indicating whether a
particular record version participates in a snapshot; it cannot stand in for `diff`. [Trace
model](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace.rs#L1-L27),
[weight
contract](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace.rs#L159-L177),
[zero-weight
suppression](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/cursor/cursor_list.rs#L150-L174)

## Operator to state to representation to access

The table maps the operators relevant to the scoped relational work to their likely state and access pattern.
Each row is an API-level mapping; which rows the SQL compiler actually instantiates for a particular query
remains to be checked with a generated plan.

| Scoped operation | State held or produced | Current representation | Access and merged-index implication |
| --- | --- | --- | --- |
| Q3 customer segment, order date, and lineitem ship-date filters; Q5/Q10 selected predicates | Filtered delta stream; an indexed stream if a downstream join needs a key | `filter` retains records, then `map_index` creates `OrdIndexedWSet` batches keyed by the join column. | A scan/filter followed by ordered batch construction. The proposed design needs either an index on the predicate/order key or a scan that can feed the join key without duplicating payload. [Filter/map APIs](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/filter_map.rs#L74-L106) |
| Q3 customer–orders and orders–lineitem equijoins; matching Q5/Q10 join portions | Accumulated trace for each join side, plus current deltas | `dyn_join_generic` constructs `TimedSpine<B,C>`; at `RootCircuit`, this is `Spine<B>` with untimed indexed batches. | Key seek or ordered delta/trace merge, scan matching values, multiply weights. Share reconstructed source access without assuming separate timestamps. [Join construction](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/join.rs#L698-L749), [root weight product](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/join.rs#L1796-L1824) |
| Q3 revenue grouped by order/date/priority (Q5/Q10 final aggregates are downstream) | Indexed group input and aggregate state/output | `aggregate_linear` accepts an indexed Z-set and multiplies each value expression by its input weight; generic `aggregate` emits an indexed result. | Group-key seek/scan and weighted accumulation remain required even if the join inputs share a merged record store. Do not assume the join index also indexes the final revenue sort. [Linear aggregate](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/aggregate.rs#L205-L245), [generic aggregate](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/aggregate.rs#L30-L56) |
| Q3 order by revenue/date and top ten (outside replacement boundary) | Separate downstream state if the full SQL is benchmarked | Not established by this trace investigation | Hold downstream work consistent; a join-key index alone cannot answer it. [Q3 SQL](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/pipeline-manager/demos/sql/00-accelerating-batch-analytics.sql#L315-L322) |

`index` only reshapes an existing `(key, value)` tuple stream, and `index_generic` caches that conversion by
input stream ID. `index_with_generic` adds a new unary operator to compute a mapping; `map_index` creates
indexed batches directly. These are different logical streams when their key projections differ. Both the
integral and the generic accumulated join-trace caches are keyed by stream ID, so they avoid duplicate trace
construction for the *same* stream but do not provide one shared physical tuple store across join-key
permutations. [Index cache and
mapping](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/index.rs#L42-L94),
[untimed trace
cache](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/trace.rs#L550-L565),
[accumulated join trace
cache](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/accumulate_trace.rs#L87-L105)

## Physical layout and beyond-memory I/O

`OrdIndexedWSet` aliases `FallbackIndexedWSet`, with `VecIndexedWSet` and `FileIndexedWSet` variants. The
vector form has key offsets into weighted value leaves; the file form writes keys and weighted values through
`Writer2<K, DynUnit, V, R>`. This is real indexed physical storage for Z-sets.
[Alias](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord.rs#L5-L9),
[fallback
variants](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/fallback/indexed_wset.rs#L34-L58),
[vector
layout](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/vec/indexed_wset_batch.rs#L158-L201),
[file
writes](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/file/indexed_wset_batch.rs#L982-L1016)

`TimedSpine<B,C>` selects its batch type through the circuit timestamp. `RootCircuit = ChildCircuit<(),()>`,
whose `WithClock::Time` is `()`, and `Timestamp for ()` defines `TimedBatch<B> = B`. Thus root joins use the
ordinary untimed batch. Only a nontrivial clock selects `B::Timed<T>`; the corresponding `FileValBatch` stores
time/weight lists. The join explicitly selects the untimed weight-only branch for a zero-sized timestamp. This
is a specialization of the same weighted algebra, not a contradiction of it or evidence of SQL MVCC. [Clock
type](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/circuit_builder.rs#L1684-L1692),
[root
alias](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/circuit_builder.rs#L3109-L3127),
[timestamp
mappings](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/time.rs#L214-L240),
[branch
selection](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/join.rs#L1701),
[untimed
branch](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/join.rs#L1796-L1824)

`FileIndexedWSet` supports ordered seeks, membership checks, and asynchronous batched `fetch` of requested
keys into an in-memory indexed Z-set. Therefore the root join baseline can benefit from batched fetching when
`fetch_join` is enabled. A snapshot invokes member-batch fetch and falls back to cursors when unavailable.
`FileValBatch` has no fetch override, but that fact concerns the timed variant; it must **not** be used to
claim root Q3 joins lack batched fetch. Probe cost still depends on overlapping batches, filters, blocks
touched, and cache residency. These are source-derived access possibilities, not measured I/O counts. [Indexed
fetch](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/file/indexed_wset_batch.rs#L447-L478),
[default
fetch](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace.rs#L730-L753),
[snapshot
fallback](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/spine_async/snapshot.rs#L274-L305),
[join fetch
branch](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/join.rs#L1653-L1687)

The file reader caches data and index blocks by file/block location, recording hits and misses; the storage
writer documents the data-block tradeoff between fewer I/O operations and cache occupancy. The cache builder
can allocate foreground/background slots per worker pair or share S3-FIFO more broadly. A proposed merged
index should be evaluated at the same byte budget and report block reads, bytes read, cache misses, and write
traffic, not just CPU time. [Data-block
cache](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/storage/file/reader.rs#L605-L625),
[index-block
cache](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/storage/file/reader.rs#L1075-L1101),
[block
tradeoff](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/storage/file/writer.rs#L88-L114),
[cache
allocation](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/buffer-cache/src/builder.rs#L83-L122)

The fallback builder selects memory, storage, or a size threshold. At the spine boundary, memory batches can
be flushed to storage; memory pressure changes insertion and merge thresholds. Background spine merges remain
part of the **unmodified baseline** and can coalesce signed weights and reduce read fan-out. This proposal
does not depend on compaction to maintain visibility or secondary-index correctness: every committed update
must be queryable before any later merge. [Builder
destination](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/fallback/utils.rs#L32-L82),
[merge
destination](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/fallback/utils.rs#L104-L134),
[runtime
thresholds](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/runtime.rs#L1222-L1282),
[spine
flush](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/spine_async.rs#L2450-L2483)

## Integration implications

The [storage report](merged-index-storage.md) owns the proposed record and visibility protocol: directly
stored weighted typed source tuples, stable phase-0 base, and phase-1 signed pending changes. This is a
two-view batch contract, sufficient for the scoped old/new algebra. It need not implement nested clock
histories. A query-specific evaluator can emit an ordinary weighted output batch after reconstructing affected
contributions. A generic `BatchReader`/`Cursor` substitution remains a separate, higher-risk option.

Physical sharing does not mean that all intermediates alias one record. Q3's source rows, aggregate tuples,
and joined tuples are different relations; the proposal reconstructs the latter from the former. Existing
stream-ID caches cannot establish this derivation or coordinate shared scans across integrators. A persistent
parent lookup and bounded scan sessions are additional costs; multiple full secondary copies are not assumed
to be free.

Trace retention is separate from physical visibility. Feldera exposes monotone key/value retention controls,
including for accumulated traces, and passes filters into spine state and merges. A merged index must apply
the same logical bound to every key order and only reclaim a record after no active snapshot or access path
needs it. This is a proposed mapping; it does not assume a visibility bit can replace retention or that merges
are needed for correctness. [Retention API
contract](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/trace.rs#L157-L205),
[accumulated
retention](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/accumulate_trace.rs#L245-L273),
[spine
filters](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/spine_async.rs#L2232-L2247)

## Feasibility gate

The implementation seam is promising, but beyond-memory I/O benefit remains **unverified**. A useful Q3
prototype must preserve positive/negative updates, legal multiplicities, complete old/new snapshots, and
restart; a generic trace adapter additionally needs the selected circuit's clock and retention conformance
(root time is unit). It must demonstrate both point joins and ordered scans; and compare against the pinned,
unmodified Feldera run at the same storage and cache budget. For Q5/Q10, measure only the agreed join-input
portions. The decisive metrics are per-operator index/data block reads, bytes read and written, cache misses,
seek fan-out, scan throughput, and end-to-end Q3 latency or throughput. Visibility metadata and pending-row
maintenance that add random reads or writes could erase gains from fewer seeks. Record the `fetch_join` switch
and confirm the actual root batch type and fetch path; do not disable or overlook baseline batched fetch when
comparing selective reads. [Join fetch
switch](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/join.rs#L1653-L1687),
[default
fetch](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace.rs#L730-L753),
[timed file
batch](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/file/val_batch.rs#L291-L345)
