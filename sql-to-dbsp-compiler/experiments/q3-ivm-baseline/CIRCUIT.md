# Generated native circuit for `V`

This note describes the compiler-expanded logical graph and the actual runtime
graphs captured for the native Q3 baseline. Artifacts and capture logs are stored under the repository’s
`target/q3-circuit-exports/` directory. The graph files preserve the node
IDs, edges, ports, and clusters emitted by their source graph. The source code
and runtime operator behavior referenced below explain how to read those
graphs; they do not reconstruct the topology.

Run [generate.sh](generate.sh) before following local `src/lib.rs` and
`plan.json` links below. Those compiler outputs and `src/stubs.rs` are
Git-ignored; the [experiment instructions](README.md#generate-and-run)
describe their regeneration.

## Compiler-expanded logical graph

The compiler graph is the existing `DBSPCircuit` after
`DeltaExpandOperators` and monotonicity analysis, before `InsertLimiters`.
`MonotoneAnalyzer` exports it when the compiler receives
`--dump-expanded-circuit DIR`. The local capture is
[`expanded.svg`](../../../target/q3-circuit-exports/compiler/expanded.svg); its raw source is
[`expanded.dot`](../../../target/q3-circuit-exports/compiler/expanded.dot), with revision, command,
configuration, and capture stage in
[`expanded.metadata.txt`](../../../target/q3-circuit-exports/compiler/expanded.metadata.txt). This is the
incremental logical graph produced by the compiler; it excludes runtime
operator internals. In particular, compiler analysis omits Catalog's
materialization of `V`, which appears in the runtime graph.

The compiler graph visitor uses blue `[I]` for integrator state:
`DBSPIntegrateOperator` and `DBSPDelayedIntegralOperator`
(`accumulate_delay_trace`). Other compiler-declared `IStateful` operators
receive purple `[S]`. Tooltips and this
legend explain the categories:

| Marker | Meaning |
| --- | --- |
| `[I]` | Historical collection or its delayed prior-state representation. |
| `[S]` | Other compiler-declared state. |

This Q3 analysis graph has no explicit `integrate` node. Its
`DBSPDelayedIntegralOperator` nodes stand for prior accumulated collections
in the analysis, despite their `accumulate_delay_trace` operation name. They
are not a decomposition of the runtime integrator. In this capture, node 105
sums the Orders–Customer delta branches and feeds node 108, the second join's
prior-state placeholder for that intermediate result. `map_index` nodes only
rekey/project the incoming delta batches; they do not integrate history.
The expansion comes directly from
[`DeltaExpandOperators`](../../SQL-compiler/src/main/java/org/dbsp/sqlCompiler/compiler/visitors/outer/expansion/DeltaExpandOperators.java).

The expanded `stream_join` and `stream_join_index` products are neutral.
Their compiler classes inherit `IStateful` through `IJoin`, but here they
multiply the supplied operands rather than introduce another retained
historical relation. Runtime `JoinTrace` remains other state: it keeps a delta
snapshot, time-indexed pending outputs, and bookkeeping while reading an
input trace. Those are separate from historical integration.

Node IDs and original operator names remain intact.

![Compiler-expanded logical graph](../../../target/q3-circuit-exports/compiler/expanded.svg)

## Worker policy

All future IVM maintenance runs use one worker. Initial loading may use
multiple workers. The two-worker captures below are prior runtime inspection
artifacts. `CircuitConfig::with_workers(N)` creates local worker threads with
replicas of the circuit; input maps and join traces are partitioned by key.
The generated `join_*_balanced` wrappers select adaptive balancing only when
`adaptive_joins` is enabled, which is off in the default configuration used
for these captures. See the
[wrappers](../../../crates/dbsp/src/mono.rs) and
[default settings](../../../crates/feldera-types/src/config/dev_tweaks.rs).

## Per-worker runtime graphs

The runtime SVGs come from DBSP's `graph_profile()` after the final refresh
report, outside phase timers. Each run directory contains each worker's raw
profile DOT and JSON, unstyled SVG, styled DOT, and styled SVG. The styled
graph keeps the same topology and adds `[I]` in blue, `[S]` in purple, or
`[U]` in gray. Runtime names are classified by the explicit, source-referenced table in
[`src/main.rs`](src/main.rs); unknown names remain unclassified. The style is
presentation only.

| Marker | Meaning |
| --- | --- |
| `[I]` | Existing `accumulate_integrate_trace` or `shard_accumulate_integrate_trace` region, or `AccumulateDelayTrace`: historical integrator state. |
| `[S]` | Other state, including transaction delta collectors, trace append components, and other stateful components. |
| `[U]` | Runtime operator name is not classified by the table. |

Runtime `Accumulator` is a **transaction delta collector**: it collects the
current transaction's changes in a Spine. Historical trace accumulation retains
relation state across transactions. The
[naming distinction](../../../docs/design/merged-index/native-state-read-contracts.md#weird-feldera-naming-accumulation)
records these separate roles.
[Operator::flush](../../../crates/dbsp/src/circuit/operator_traits.rs#L349)
asks an operator to finish processing that transaction and emit its remaining
outputs. The collector's next evaluation returns its transaction Spine and
replaces it with an empty one. The returned Spine can still contain multiple
memory batches or files; `flush` does not consolidate it into a single batch
or request a storage write. Snapshots can retain memory-backed and file-backed
batches without requiring a spill; see the
[R2 snapshot contract](../../../docs/design/merged-index/native-state-read-contracts.md#existing-snapshot-fetch-and-cursor-behavior).
The [naming note and join example](../../../docs/design/merged-index/native-state-read-contracts.md#weird-feldera-naming-flush)
explain why `JoinTrace` couples this notification with its accumulated-state
snapshot reads: the current trace must include the transaction's complete
updates, while the other branch receives the delayed prior trace.
`AccumulateDelayTrace` snapshots an already-maintained trace; it does
not create or update the historical integral. The join's historical keyed
traces are built in `shard_accumulate_trace` regions, while Catalog's retained
`V` uses `shard_accumulate_integrate_trace`. The blue integrator highlight
applies to the named integration regions and `AccumulateDelayTrace`
snapshots. The snapshot represents historical integrator state, although it
does not itself append new deltas to that state. See the
[trace builders](../../../crates/dbsp/src/operator/dynamic/accumulate_trace.rs)
and [transaction delta collector](../../../crates/dbsp/src/operator/dynamic/accumulator.rs#L303).

The local one-worker capture is
[`worker-0-styled.svg`](../../../target/q3-circuit-exports/runtime-1/run-1/worker-0-styled.svg). The
two-worker capture is
[`worker-0-styled.svg`](../../../target/q3-circuit-exports/runtime-2/run-1/worker-0-styled.svg) and
[`worker-1-styled.svg`](../../../target/q3-circuit-exports/runtime-2/run-1/worker-1-styled.svg). Raw
graphs and the capture manifest are alongside each SVG. The manifest records
the source revision, command, configuration, run, and capture stage.

`JoinTrace` is a runtime join operator that consumes a supplied retained trace.
It performs keyed cursor and range access inside the operator; the graph does
not contain separate index-access nodes. The other-state category can include
buffered join work and does not mean the operator accumulates a trace. Read the
[join implementation](../../../crates/dbsp/src/operator/dynamic/join.rs#L651)
and its [range fetch path](../../../crates/dbsp/src/operator/dynamic/join.rs#L1661),
and see [generated Rust](src/lib.rs#L487) for the calls that instantiate the
joins.

## Reading the generated program

The optimized relational plan scans Orders and Customer, joins them on
customer key, projects their needed fields, and joins the result with Lineitem
on order key. The logical Lineitem input has `(l_orderkey, l_linenumber)`
identity and no customer key. It then projects the nine fields of `V`. These
are plan nodes 2, 3, 4, 5, 1, 6, 7, and 8 in [plan.json](plan.json). The SQL
starts with `Customer JOIN Orders`; the optimized plan's first join has Orders
on the left. The plan records the selected shape.

`B` below names the generated join output [s6](src/lib.rs#L487) for discussion;
it is not a SQL view. The compiler rekeys Orders by customer key in
[s3](src/lib.rs#L442), Customer by customer key in [s4](src/lib.rs#L456),
and Lineitem by order key in [s5](src/lib.rs#L465). It computes `s6` with
`s3.join_index_balanced(&s4)` and `s7` with `s5.join_balanced(&s6)`. The latter
produces the nine-column delta of `V`. A [materialized output
registration](src/lib.rs#L641) exposes both a signed delta handle and an
integrated snapshot.

With `O₀`, `C₀`, `L₀`, and `B₀` denoting state before the transaction, the
generated branch orientation is:

```text
ΔB = ΔO ⋈ (C₀ + ΔC) + O₀ ⋈ ΔC
ΔV = ΔL ⋈ (B₀ + ΔB) + L₀ ⋈ ΔB
V_after = V_before + ΔV
```

In [`dyn_join_generic`](../../../crates/dbsp/src/operator/dynamic/join.rs#L700),
each input's retained trace supplies the opposite branch. The left delta joins
the right trace after its current change; the right delta joins a delayed left
trace, giving the before state. Signed weights multiply in the joins and add
at the branch unions, so inserts, deletes, replacements, and concurrent
changes use the same circuit. In the default solo layout, Catalog's
[integration path](../../../crates/adapters/src/static_compile/catalog.rs#L130)
uses `shard_accumulate_integrate_trace_legacy`; the
[snapshot handle](../../../crates/adapters/src/static_compile/catalog.rs#L643)
reads that integral. Primary-key input maps retain base storage too. Native
trace sharing and extra indexes mean there is no universal one-index-per-
integrator allocation count.

The source program does not route trace reads through a merged index. The
initial load bootstraps the incremental circuit from empty before state; RF1
and RF2 use that same circuit and materialized output. The independent full
join in [main.rs](src/main.rs) is a checker outside the measured transaction
call.
