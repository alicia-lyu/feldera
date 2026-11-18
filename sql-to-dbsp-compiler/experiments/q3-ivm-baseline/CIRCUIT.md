# Generated native circuit for `V`

## Two chained circuits

The experiment runs two generated circuits in one DBSP runtime (`Runtime::init_circuits`, one worker, one storage directory, one buffer cache; since 2026-11-05, measurement version 6):

- **Base circuit** (`q3-base-circuit/`, from [base.sql](base.sql)). It has the three
  tables with their primary keys and no view. Each table is an input map
  (`add_input_map_persistent`) that retains the base relation. The driver applies every
  transaction here first. Because the catalog registers inputs as outputs, each
  relation's delta stream is the net change of that transaction: an upsert that
  replaces a row is a `-1` of the old row and a `+1` of the new one, and a deletion
  carries the complete old row.
- **View circuit** (`q3-view-circuit/`, from [q3.sql](q3.sql)). It has the same tables
  without keys, so they are Z-set inputs (`add_input_zset`), and it has `V`. The
  driver feeds it the base circuit's net delta, with complete rows for deletions.
  This forwarding is harness overhead: a single DBSP circuit passes deltas between
  views as in-memory streams, so the JSON hop has no counterpart in a deployed
  pipeline. It is excluded from the base and IVM figures but inside the sequence totals.
  The reconstructed view circuit is hand-written (`q3-trie-view-circuit`). Everything below, from the
  compiler-expanded graph to the generated joins, describes the generated (native) view circuit.

The driver times the two circuits separately, so `q3_ms` is the view circuit's transaction
(`ivm_ms` adds `delta_ms`, the preparation of its input delta) and `base_ms` is the base circuit plus, in reconstructed mode, the merged index's upkeep;
[MEASUREMENT.md](MEASUREMENT.md) defines the phases.
The runtime graphs captured before the split (under `target/q3-circuit-exports/`) show
the earlier single-circuit program, which also held the primary-key input maps. They
remain valid for the join regions and `V`'s integral, and not for the base maps.

This note describes the compiler-expanded logical graph and the actual runtime
graphs captured for the native Q3 baseline. Artifacts and capture logs are stored under the repository’s
`target/q3-circuit-exports/` directory. They are local-only: `target/` is git-ignored, so the links to them below work only after a local capture. The graph files preserve the node
IDs, edges, ports, and clusters emitted by their source graph. The source code
and runtime operator behavior referenced below explain how to read those
graphs; they do not reconstruct the topology.

Run [generate.sh](generate.sh) before following local `q3-view-circuit/src/lib.rs` and
`plan.json` links below. Those compiler outputs, `q3-base-circuit/src/lib.rs`, and both
`src/stubs.rs` files are Git-ignored; the [harness CLAUDE.md](CLAUDE.md)
describes their regeneration.

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
historical relation. In this generated root circuit, runtime `JoinTrace`
keeps a delta snapshot and bookkeeping while reading an input trace. Records
have no per-record logical timestamps; old/new snapshots own references to
immutable batches that supply the before/after relation state. This join work
is separate from historical integration.

Node IDs and original operator names remain intact.

![Compiler-expanded logical graph](../../../target/q3-circuit-exports/compiler/expanded.svg)

## Worker policy

Maintenance and initial loading both use one worker (see
[decisions.md](../../../docs/design/merged-index/decisions.md)). Reconstructed
mode rejects any other count. The two-worker captures below are prior runtime
inspection artifacts of the native circuit. `CircuitConfig::with_workers(N)` creates local worker threads with
replicas of the circuit; input maps and join traces are partitioned by key.
The generated `join_*_balanced` wrappers select adaptive balancing only when
`adaptive_joins` is enabled, which is off in the default configuration used
for these captures. See the
wrappers in `crates/dbsp/src/mono.rs` and the default settings in
`crates/feldera-types/src/config/dev_tweaks.rs`.

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
[naming distinction](../../../docs/design/merged-index/dbsp-background.md#weird-feldera-naming-accumulation)
records these separate roles.
`Operator::flush` (`crates/dbsp/src/circuit/operator_traits.rs`)
asks an operator to finish processing that transaction and emit its remaining
outputs. The collector's next evaluation returns its transaction Spine and
replaces it with an empty one. The returned Spine can still contain multiple
memory batches or files; `flush` does not consolidate it into a single batch
or request a storage write. Snapshots can retain memory-backed and file-backed
batches without requiring a spill; see the
[R2 snapshot contract](../../../docs/design/merged-index/dbsp-background.md#existing-snapshot-fetch-and-cursor-behavior).
The [naming note and join example](../../../docs/design/merged-index/dbsp-background.md#weird-feldera-naming-flush)
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
and the transaction delta collector `Accumulator` in
`crates/dbsp/src/operator/dynamic/accumulator.rs`.

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
`JoinTrace` and `JointKeyCursor` in `crates/dbsp/src/operator/dynamic/join.rs`.
The generated `q3-view-circuit/src/lib.rs` instantiates the joins; it is git-ignored, so read the
generator (`ToRustVisitor.java`) or run [generate.sh](generate.sh).

## Reading the generated program

The optimized relational plan scans Orders and Customer, joins them on
customer key, projects their needed fields, and joins the result with Lineitem
on order key. The logical Lineitem input has `(l_orderkey, l_linenumber)`
identity (a primary key in the base circuit; the view circuit has no keys) and no
customer key. It then projects the nine fields of `V`. These
are plan nodes 2, 3, 4, 5, 1, 6, 7, and 8 in [plan.json](plan.json). The SQL
starts with `Customer JOIN Orders`; in the plan generated locally, the first
join has Orders on the left. Unverified in git: `plan.json` is git-ignored, so
rerun [generate.sh](generate.sh) and read the plan to confirm the orientation.

`B = Orders ⋈ Customer` names the first generated join's output for
discussion; it is not a SQL view. In the generated `q3-view-circuit/src/lib.rs`
(git-ignored; the generator is `ToRustVisitor.java`), the compiler rekeys Orders by
customer key (`s3`), Customer by customer key (`s4`), and Lineitem by order key (`s5`),
each straight from its Z-set input (`s1`, `s0`, `s2`).
It computes `s6` (`B`) with `s3.join_index_balanced(&s4)` and `s7` with
`s5.join_balanced(&s6)`. The latter produces the nine-column delta of `V`. The
materialized output registration exposes both a signed delta handle and an
integrated snapshot. Stream names such as `s6` may change between compiler
versions.

With `O₀`, `C₀`, `L₀`, and `B₀` denoting state before the transaction, the
generated branch orientation is:

```text
ΔB = ΔO ⋈ (C₀ + ΔC) + O₀ ⋈ ΔC
ΔV = ΔL ⋈ (B₀ + ΔB) + L₀ ⋈ ΔB
V_after = V_before + ΔV
```

In `dyn_join_generic` (`crates/dbsp/src/operator/dynamic/join.rs`),
each input's retained trace supplies the opposite branch. The left delta joins
the right trace after its current change; the right delta joins a delayed left
trace, giving the before state. Signed weights multiply in the joins and add
at the branch unions, so inserts, deletes, replacements, and concurrent
changes use the same circuit. In the default solo layout, Catalog's
integration path (`crates/adapters/src/static_compile/catalog.rs`) uses
`shard_accumulate_integrate_trace_legacy`, and the snapshot handle reads
that integral through `ro_snapshot()`. The primary-key input maps that retain the base
relations are in the base circuit. Native
trace sharing and extra indexes mean there is no universal one-index-per-
integrator allocation count.

The native source program does not route trace reads through a merged index;
the reconstructed program does, through the supplied-snapshot join (see
[reconstruction.md](../../../docs/design/merged-index/reconstruction.md)). The
initial load bootstraps the incremental circuit from empty before state; RF1
and RF2 use that same circuit and materialized output. The independent full
join in [main.rs](src/main.rs) is a checker outside the measured transaction
call.

## Generated native state-read contracts (R2)

Inspected on 2026-10-06 at Feldera revision `308950de73a28bb014c8ad002365d2aa64f1f152`; symbol names were rechecked on 2026-10-12. Code is cited by symbol and file path, not by line, because line anchors drift. Stream names such as `s3` to `s7` come from the generated `q3-view-circuit/src/lib.rs`, which is git-ignored and may change between compiler versions; run [generate.sh](generate.sh) and read the generator (`ToRustVisitor.java`) to confirm them. "Reading the generated program" above records the generated topology; this part binds its accumulated-state reads. The native behavior inspected here remains the comparison contract for reconstruction (see [reconstruction.md](../../../docs/design/merged-index/reconstruction.md)). The snapshot, fetch, and cursor behavior of the native joins, with the naming notes on "flush" and "accumulation", is in [dbsp-background.md](../../../docs/design/merged-index/dbsp-background.md).

### Relations and state notation

`O`, `C`, and `L` denote logical Orders, Customer, and Lineitem. Following the
[storage design notation](../../../docs/design/merged-index/storage-layout.md),
for any relation `X`:

- `X⁻` is accumulated state immediately before the current update set.
- `X⁺` is accumulated state after applying that update set.
- `ΔX = X⁺ − X⁻`, so `X⁺ = X⁻ + ΔX`.

The superscripts identify states, **not negative and positive tuple weights**.
Unchanged tuples belong to both states. Insertions, deletions, and replacements
contribute through the signed relation `ΔX`; addition consolidates weights of
identical complete tuples.

`B` names the actual generated Orders–Customer intermediate, stream
s6.
It is an order-keyed projection of `O ⋈ C`, retaining customer ID, segment,
order date, and ship priority. It is not a named SQL view. `V` is the
materialized nine-field Customer–Orders–Lineitem join generated as
s7.
Neither join applies Q3's residual filters or aggregation.

### Generated bindings and equations

The first generated call is `s3.join_index_balanced(&s4)`; the second is
`s5.join_balanced(&s6)`. Thus Orders is the first join's left input, and
Lineitem is the second join's left input. Each row below identifies the
accumulated operand read by a delta branch, rather than a separate input delta.

| Delta input | Accumulated state read | Lookup key | Generated stream and runtime wiring |
| --- | --- | --- | --- |
| `ΔO` (`s3`) | `C⁺` (`s4` integral) | Customer ID | s3/s4 join; left delta with current right trace (`dyn_join_generic`, `crates/dbsp/src/operator/dynamic/join.rs`). |
| `ΔC` (`s4`) | `O⁻` (`s3` integral) | Customer ID | s3/s4 join; right delta with delayed left trace (`dyn_join_generic`, `crates/dbsp/src/operator/dynamic/join.rs`). |
| `ΔL` (`s5`) | `B⁺` (`s6` integral) | Order ID | s5/s6 join; left delta with current right trace (`dyn_join_generic`, `crates/dbsp/src/operator/dynamic/join.rs`). |
| `ΔB` (`s6`) | `L⁻` (`s5` integral) | Order ID | s5/s6 join; right delta with delayed left trace (`dyn_join_generic`, `crates/dbsp/src/operator/dynamic/join.rs`). |

These bindings give the generated equations:

```text
ΔB = ΔO ⋈ C⁺ + O⁻ ⋈ ΔC
ΔV = ΔL ⋈ B⁺ + L⁻ ⋈ ΔB
V⁺ = V⁻ + ΔV
```

The first equality expands to
`ΔO ⋈ C⁻ + ΔO ⋈ ΔC + O⁻ ⋈ ΔC`; the simultaneous-change term occurs once.
The same reasoning applies to the second join. `dyn_join_generic` (`crates/dbsp/src/operator/dynamic/join.rs`)
creates each input trace, wires the two branches as cited above, and adds their outputs.
The delayed-trace accessor (`accumulate_delay_trace` in `crates/dbsp/src/operator/dynamic/accumulate_trace.rs`)
uses the cached prior-cycle trace; its
snapshot operator (`AccumulateDelayTrace`, same file)
captures that trace on initialization or on evaluation after the runtime's
notification that transaction inputs are complete (`Operator::flush`). A delayed snapshot is a
read of existing accumulation, not a second accumulation of the input.

The generated `*_balanced` wrappers use ordinary joins when adaptive joins are
disabled, as in the native baseline: see
join_balanced (`crates/dbsp/src/mono.rs`),
join_index_balanced (`crates/dbsp/src/mono.rs`), and the
default setting (`crates/feldera-types/src/config/dev_tweaks.rs`).
This contract describes that configuration, without extending it to adaptive
or multiworker maintenance.

### Concrete keyed values

The generated stream declarations and projection closures establish these
types and field orders. Each indexed collection associates a signed
i64 weight (`crates/dbsp/src/algebra/zset.rs`) with a complete
`(key, value)` pair; the superscript selects visibility, not the weight's sign.

| Read | Concrete key type | Concrete value type | Value fields in order | Generated source |
| --- | --- | --- | --- | --- |
| `C⁺` | `Tup1<i64>` | `Tup1<SqlString>` | Market segment | s4 |
| `O⁻` | `Tup1<i64>` | `Tup3<i64, Date, i32>` | Order ID, order date, ship priority | s3 |
| `B⁺` | `Tup1<i64>` | `Tup4<i64, SqlString, Date, i32>` | Customer ID, market segment, order date, ship priority | s6 |
| `L⁻` | `Tup1<i64>` | `Tup4<i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>` | Line number, extended price, discount, ship date | s5 |

The keys are customer ID for `C⁺` and `O⁻`, and order ID for `B⁺` and `L⁻`.
The logical Lineitem identity is `(order ID, line number)`; its generated
indexed value retains the line number. Its schema has no customer ID.

The output `s7` declaration and closure
use `WSet<Tup9<i64, SqlString, i64, Date, i32, i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>>`.
The fields are customer ID, segment, order ID, order date, ship priority,
line number, extended price, discount, and ship date, in that order. The
materialized output registration
exposes `V` through Catalog. Base input maps and materialized `V` are outside
the four accumulated join-read bindings above.


### Worked signed examples

**Validation status:** These are agent-authored algebraic illustrations, not
executable test cases. They have not been executed as written or reviewed or
validated by a human expert. R2 added no runtime code or executable tests.
The native baseline has tests for related scenarios (`native_deltas_match_full_join_across_updates` and `simultaneous_three_source_changes_match_reference` in `sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/main.rs`),
but those do not establish validation of these exact examples. The examples
illustrate the equations above; they are not correctness evidence for R3.

All examples use unit source weights and matching identities: customer 7,
order 10 owned by customer 7, and line 1 of order 10. Unmentioned payload
fields are fixed. `{+x}` means tuple `x` with weight `+1`; `{-x}` means weight
`−1`. Braces denote weighted relations, not before/after states. Each example
is a separate update set.

#### Empty bootstrap

Start with `O⁻ = C⁻ = L⁻ = B⁻ = V⁻ = ∅` and supply
`ΔO = {+o}`, `ΔC = {+c}`, and `ΔL = {+l}` together.
The resulting states have `C⁺ = {+c}` and `B⁺ = {+b}`, where `b` is the
generated order-keyed projection of the matching order/customer pair.

```text
ΔB = {+o} ⋈ {+c} + ∅ ⋈ {+c} = {+b}
ΔV = {+l} ⋈ {+b} + ∅ ⋈ {+b} = {+v}
V⁺ = ∅ + {+v} = {+v}
```

Every row enters through the native delta ports; no accumulated bootstrap
relation is supplied as a second delta.

#### Simultaneous customer and order deletion

Start with `O⁻ = {+o}`, `C⁻ = {+c}`, `L⁻ = {+l}`, `B⁻ = {+b}`,
and `V⁻ = {+v}`. Supply `ΔO = {-o}`, `ΔC = {-c}`, and `ΔL = ∅`.
Now `C⁺ = B⁺ = ∅` while the unchanged line belongs to both `L⁻` and `L⁺`.

```text
ΔB = {-o} ⋈ ∅ + {+o} ⋈ {-c} = {-b}
ΔV = ∅ ⋈ ∅ + {+l} ⋈ {-b} = {-v}
V⁺ = {+v} + {-v} = ∅
```

Reading `C⁻` on the first branch would instead count the deletion twice.
The correct expanded first join has weights `−1 − 1 + 1 = −1`.

#### Simultaneous payload replacement at unchanged keys

Start with customer `c_old` in BUILDING and line `l_old` priced at 100.00,
with the unchanged order `o`. Replace the customer segment with AUTOMOBILE
and the line price with 120.00 in the same update set:
`ΔC = {-c_old, +c_new}`, `ΔL = {-l_old, +l_new}`, and `ΔO = ∅`.
Thus `ΔB = {-b_old, +b_new}` and `B⁺ = {+b_new}`. Let `v(l,b)` denote
the generated nine-field output for a matching line/intermediate pair.

```text
ΔL ⋈ B⁺ = {-v(l_old,b_new), +v(l_new,b_new)}
L⁻ ⋈ ΔB = {-v(l_old,b_old), +v(l_old,b_new)}
ΔV       = {-v(l_old,b_old), +v(l_new,b_new)}
```

The mixed old-line/new-segment tuple cancels. `V⁺` contains the new segment
and price; no BUILDING filter runs in this circuit. The unchanged order
belongs to both `O⁻` and `O⁺`. Projecting either replacement delta to its
unchanged identity and summing would yield zero, although its payload changes.
