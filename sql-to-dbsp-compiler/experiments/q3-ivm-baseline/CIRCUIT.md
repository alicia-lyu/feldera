# Generated native circuit for `V`

This note describes the compiler-expanded logical graph and the actual runtime
graphs captured for the native Q3 baseline. The graph files preserve the node
IDs, edges, ports, and clusters emitted by their source graph. The source code
and runtime operator behavior referenced below explain how to read those
graphs; they do not reconstruct the topology.

## Compiler-expanded logical graph

The compiler graph is the existing `DBSPCircuit` after
`DeltaExpandOperators` and monotonicity analysis, before `InsertLimiters`.
`MonotoneAnalyzer` exports it when the compiler receives
`--dump-expanded-circuit DIR`. The checked-in capture is
[`expanded.svg`](graphs/compiler/expanded.svg); its raw source is
[`expanded.dot`](graphs/compiler/expanded.dot), with revision, command,
configuration, and capture stage in
[`expanded.metadata.txt`](graphs/compiler/expanded.metadata.txt). This is the
incremental logical graph produced by the compiler; it excludes runtime
operator internals. In particular, compiler analysis omits Catalog's
materialization of `V`, which appears in the runtime graph.

The compiler graph visitor marks explicit accumulation operators, including
integrate and delayed integral, with `[A]` in blue. Other compiler-declared
`IStateful` operators receive `[S]` in purple. Tooltips explain these
categories; the accompanying legend is:

| Marker | Meaning |
| --- | --- |
| `[A]` | Explicit accumulated collection or delayed trace. |
| `[S]` | Compiler-declared `IStateful` operator other than explicit accumulation. |

Node IDs and original operator names remain intact.

## Per-worker runtime graphs

The runtime SVGs come from DBSP's `graph_profile()` after the final refresh
report, outside phase timers. Each run directory contains each worker's raw
profile DOT and JSON, unstyled SVG, styled DOT, and styled SVG. The styled
graph keeps the same topology and adds `[A]` in blue, `[S]` in purple, or
`[U]` in gray. Runtime names are classified by the explicit, source-referenced table in
[`src/main.rs`](src/main.rs); unknown names remain unclassified. The style is
presentation only.

| Marker | Meaning |
| --- | --- |
| `[A]` | Runtime operator name is in the accumulation table. |
| `[S]` | Runtime operator name is in the other-state table. |
| `[U]` | Runtime operator name is not classified by the table. |

The checked-in one-worker capture is
[`worker-0-styled.svg`](graphs/runtime-1/run-1/worker-0-styled.svg). The
two-worker capture is
[`worker-0-styled.svg`](graphs/runtime-2/run-1/worker-0-styled.svg) and
[`worker-1-styled.svg`](graphs/runtime-2/run-1/worker-1-styled.svg). Raw
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
