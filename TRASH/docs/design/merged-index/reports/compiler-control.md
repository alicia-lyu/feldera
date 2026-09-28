# Compiler audit and manual circuit boundary

**Conclusion.** Build the first Q3 experiment from the explicitly selected relational circuit and a
hand-composed runtime boundary. Stage weighted source changes, reconstruct affected old/new contributions from
one merged index, and feed their difference to the remaining circuit. The SQL compiler is useful for the
unmodified Feldera baseline and a full-query oracle; prototype correctness does not depend on it generating
the paper's aggregation-first shape. Its optimizer may reorder joins, its circuit passes move integrators, and
its Rust emitter targets existing DBSP operators. Automatic shape selection and custom merged-index code
generation are optional later work. The sibling Calcite fork is a planning artifact, not an executable Feldera
integration.

| Question | Finding | Status |
| --- | --- | --- |
| What fixes the prototype shape? | Manually compose the selected Q3 circuit and its replacement evaluator. Generated SQL shape is an optional comparison, not the correctness gate. | **Proposed** manual route; paper shape **semantically demonstrated**. |
| What happens to accumulated inputs? | The replacement evaluator reads weighted old/new source ranges and emits a result delta. Substituting existing join traces is a separate route requiring their full cursor, snapshot, and scheduling contracts. | **Proposed**; current trace contracts **implemented**. |
| Can a two-phase bit supply `z⁻¹`? | At one complete batch boundary, stable old and sealed new views supply the semantic delayed and current relations. The root Q3 circuit uses unit time; nested timestamps are a separate generality. | **Semantically demonstrated** access model; physical scheduling **unverified**. |
| Can SQL `CREATE INDEX` declare this structure? | Current DDL names columns of a view and lowers to [`DBSPMapIndexOperator`][map-index] plus a sink; a table index declaration has no effect. It cannot express interleaved typed bases or a phase bit. | **Implemented** DDL behavior; merged-index DDL **absent**. |
| What does the sibling Calcite fork prove? | Opt-in rules can replace selected sorted source boundaries with merged-index scan plan nodes, but those nodes emit empty enumerables and are not Feldera logical nodes. | **Implemented** planning artifact; execution and transfer **unverified**. |
| Is lower I/O established? | Compiler source and existing plans do not establish shared old/new reads or measured page savings for Q3, Q5, or Q10. | **Unverified**. |

The [source map](../evidence/source-map.md) owns inspected revisions and working-tree provenance. The pinned
links below identify compiler symbols; line locators refer to those revisions. See [paper
contracts](paper-contracts.md) for semantic assumptions and [index baseline](feldera-indexes.md) for current
runtime storage.

## Path from SQL to Rust

1. [`CompilerMain.run`][compiler-main] obtains a final circuit and either emits plan/dataflow
   JSON or writes Rust (`CompilerMain.java:173-177,197-215,259-270`). The
   documented sequence is `SqlNode` → optimized Calcite `RelNode` → ordinary
   `DBSPCircuit` → optional incremental circuit → Rust
   (`sql-to-dbsp-compiler/README.md:66-82`).
2. [`SqlToRelCompiler.optimize`][sql-to-rel] applies [`CalciteOptimizer`][calcite-opt] to each relational
   view (`SqlToRelCompiler.java:914-940,2401-2402`). Its join-order step can
   convert joins to a `MultiJoin` and apply bushy optimization; a later
   hypergraph step and another join-order step can alter the shape again
   (`CalciteOptimizer.java:291-334,384-398`). Filter/predicate pushdown and
   removal of unused operations also affect where a pipeline boundary sits
   (`CalciteOptimizer.java:426-451`). Hence the SQL join order alone is no
   guarantee of the selected Q3 subtree.
   The inspected step list does not register an aggregate-over-join
   preaggregation rule (`CalciteOptimizer.java:221-451`); whether it discovers
   the paper's aggregation-first shape for the exact Q3 SQL is unverified.
3. [`CalciteToDBSPCompiler.visit`][rel-to-dbsp] handles `LogicalAggregate`, `LogicalJoin`,
   and `LogicalSort`, among other logical nodes
   (`CalciteToDBSPCompiler.java:2833-2874`). A grouped aggregate first makes
   an indexed stream, then a stream aggregate
   (`:865-915`). The equijoin path creates separate keyed streams for both
   sides and a `DBSPStreamJoinOperator` (`:1510-1548,1587-1592`). These are
   ordinary logical collections at this stage; a shared physical COL record
   store is not represented by those map-index operators.
4. [`DBSPCompiler`][dbsp-compiler] assembles the circuit and runs [`CircuitOptimizer`][circuit-opt]
   (`DBSPCompiler.java:759-809,901-906`). When `-i` is set,
   [`IncrementalizeVisitor`][incrementalize] converts sources to delta streams and inserts
   source integrators (`IncrementalizeVisitor.java:33-66`).
   [`OptimizeIncrementalVisitor`][opt-incremental] pushes integrators through linear operators,
   turns a stream join of integrated inputs into an incremental join plus an
   output integrator, and similarly transforms stream aggregation
   (`OptimizeIncrementalVisitor.java:67-91,121-159,237-250`). Later passes
   share indexes and some output integrators, analyze state, adjust SQL
   indexes, balance joins, and implement unsupported join forms
   (`CircuitOptimizer.java:117-160,177-185`). These rewrites make the paper's
   five Q3 *logical* accumulations a reconstruction contract, not an asserted
   count or placement in a generated Feldera circuit.
5. [`ToRustVisitor`][rust-visitor] constructs the runtime circuit and emits operator method
   calls, including aggregate and join calls
   (`ToRustVisitor.java:346-390,1733-1755,1927-1963`).
   [`DBSPJoinOperator`][dbsp-join] is an incremental operator on two indexed inputs
   (`DBSPJoinOperator.java:42-52`). The inspected emitter path has no merged-index
   source accessor, old/new visibility argument, or range-reconstruction
   operator. Adding only a Calcite rule would leave
   these runtime calls and their trace ownership unresolved.

The compiler exposes `--plan` for optimized Calcite plan JSON and `--dataflow` for relational-plan/circuit
JSON ([`CompilerOptions.java:192-195`][compiler-options]; `DBSPCompiler.java:193-229`). Capturing those
artifacts would inventory the unmodified Feldera baseline or support later compiler integration. They are not
a prerequisite for the manual evaluator. This report did **not** compile Q3 or capture a concrete Feldera
plan; its operator mapping comes from code paths and the selected algebra.

## Selected shapes and the required ordinary and delta access

The complete Q3 example uses the feasibility note's aggregation-first ordinary relation circuit:

```text
F = filter qualifying extended Lineitem
A = group F by (customer, order), retaining count and revenue
B = A join eligible Orders on (customer, order)
W = B join eligible Customers on customer
V = project (order, revenue, order date, ship priority) from W
```

The note defines `F`, `A`, `B`, `W`, and `V` at `dbsp-merged-index-feasibility.tex:156-186`; its five logical
integrators are the Count/Revenue summary, `A`, eligible Orders, `B`, and eligible Customers (`:405-418`).
This is a selected circuit for reasoning, **not** proof that Feldera's Q3 SQL at
`crates/pipeline-manager/demos/sql/00-accelerating-batch-analytics.sql:298-322` optimizes to that plan. The
SQL groups after joining all three sources and also orders and limits the result. The selected COL boundary
ends at `V`; final sort/limit remains separate. Its customer-leading physical key requires an
order-to-customer mapping for Lineitem, which does not carry `custkey` in its native key (LeanStore
`frontend/tpch/tpch_family/col_pipeline.hpp:25-30`).

| Selected ordinary input | Incremental demand at one complete batch | Proposed index access |
| --- | --- | --- |
| Filtered Lineitem `F` and Count/Revenue | A changed line can alter a whole order group, including its existence. | Scan all qualifying weighted lines of an affected `(c,o)` in old and new views; count multiplicities and sum exact weighted revenue. |
| Group relation `A` | Emit the old group tuple's retraction and new tuple's insertion, even when only revenue changes. | Reconstruct `A⁻` and `A⁺` from the summaries, retaining zero-revenue live groups. |
| Eligible Orders | `ΔA` needs old matching Orders; `ΔOrders` needs current `A`. | Point/range read the relevant order tuple under both prefixes and apply the order-date filter in the requested view. |
| Joined relation `B` | `ΔB` needs old Customers; `ΔCustomer` needs current `B`. | Reconstruct requested old/new `A ⋈ Orders` rows; a changed customer can require all descendant orders. |
| Eligible Customers | The delayed branch uses old eligibility and the current branch includes pending changes. | Read full weighted customer tuples in old/new views, then apply the segment filter. |

This table applies `reconstruct(T, Mˢ, K) = T(DBˢ)|K` for `s ∈ {−,+}`. The note states the exact old/base and
new/base-plus-pending contract and the five access cases at `dbsp-merged-index-feasibility.tex:508-562`. It
also requires changed lines, orders, and changed-customer descendants from both states to enter the
affected-order set (`:564-579`). The **proposed** physical index stores signed weights directly and a
*separate* old/new phase bit. Phase 0 supplies old; the new view consolidates phase 0 with sealed phase-1
changes by complete tuple identity. A phase bit alone cannot replace a retraction or preserve a deleted
payload. Every consumer must finish before the phases fold, as detailed in [storage](merged-index-storage.md).
No compaction step is needed for that visibility rule.

For this one-step semantic circuit, `z⁻¹ I(x)[t] = X⁻` asks for the previous integrated relation and `I(x)[t]
= X⁺` asks for the current one. The two views suffice **at a complete batch boundary** when phase 0 remains
stable, phase 1 is sealed, and folding makes this batch's new view the next batch's old view
(`dbsp-merged-index-feasibility.tex:228-230,508-521`). That is a semantic sufficiency statement. Feldera's
top-level nonrecursive `RootCircuit` has time `()`, whose timed batch is the underlying untimed batch ([root
circuit][root-circuit]; [unit timestamp][unit-time]). A transaction can take multiple processing steps at one
logical timestamp ([transaction schedule][transaction-schedule]). The separate bit still does not implement a
generic trace cursor's seek, lifetime, replay, and flush contracts. Nested circuits can carry richer logical
times, but those are not a semantic requirement of this scoped root Q3 construction. The manual route bypasses
the selected join trace ownership ([runtime reconstruction](runtime-reconstruction.md)).

For Q5 and Q10, select only the corresponding COL order-sharing subtree. LeanStore's
[`col_group_walk`][col-walk] dispatches a customer, then its orders and lines in tagged order
(`col_pipeline.tpp:260-338`). The selected Q5 ordinary output is `(C ⋈ RN) ⋈ (Oe ⋈ L)`, where `RN` is the
filtered Region–Nation relation supplied as an **external side input** under the same snapshot. Supplier
matching, later aggregation, and ordering stay downstream. LeanStore's Q5 visitor filters customers and orders
and probes side tables during the walk; that implemented access pattern does not establish the selected
Feldera plan (`frontend/tpch/q5/query.tpp:603-639`; Feldera Q5 SQL
`00-accelerating-batch-analytics.sql:348-373`).

The selected Q10 ordinary output is `C ⋈ (Oe ⋈ returned L)`; per-customer aggregation, Nation lookup, and
ordering stay downstream. LeanStore's Q10 visitor performs some of that query work during the walk, without
storing it as COL index state (`frontend/tpch/q10/query.tpp:356-377`; Feldera Q10 SQL
`00-accelerating-batch-analytics.sql:504-537`). The paper's optional Q10 per-order partial aggregate is a
distinct storage design, not assumed here ([paper contracts](paper-contracts.md)). For both queries, `Oe` uses
the explicit SQL calendar window `lo ≤ orderdate < hi` (Q5 one calendar year, Q10 three calendar months).
LeanStore's visitor code uses fixed `+365` and `+90` day upper bounds
(`frontend/tpch/q5/query.tpp:183-188,306-311`; `frontend/tpch/q10/query.tpp:102-106`), so those prototype
bounds cannot silently stand in for the SQL predicates.

For incremental use, both scoped walks need old and new weighted COL ranges, complete predicate/output
payloads, changed-parent descendant discovery, and old/new source-contribution retractions. Q5 additionally
needs old/new `RN` inputs, and changes to `RN` must discover affected customers and descendants. The existing
LeanStore visitors demonstrate **ordinary reads**. These delta obligations and the selected Q5/Q10 Feldera
circuit shapes remain unverified.

## Manual boundary and optional compiler integration

The first prototype should hand-compose the selected Q3 computation: an affected-order evaluator reads the
index's complete old/new weighted views, computes `V⁺ − V⁻`, and publishes that relation-valued delta at the
boundary before downstream ordering/limit or output handling. It owns the source-side state that the selected
subcircuit would otherwise retain. The boundary must carry the exact output schema and signed multiplicities,
and all source, parent, and descendant reads must use one sealed batch ID. This is a **proposed** runtime
composition, not a currently generated Feldera plan. For the scoped Q5/Q10 experiments, compose the selected
outputs stated above; Q5's external `RN` input shares the old/new snapshot. Supplying reconstructed partners
to unchanged incremental joins would be a different route that retains their trace obligations.

If automatic SQL-to-merged-index compilation is pursued later, it would need to recognize or safely rewrite
the optimized `RelNode`, validate keys, predicates, output payloads and the customer-leading physical order,
preserve the decision through circuit passes, and lower a new access operation into Rust. Q5 would also
require explicit `RN` side-input scheduling. The inspected optimizer does not guarantee Q3's aggregation-first
shape, so that future compiler path needs its own correctness tests and fallback. None of these compiler
changes blocks the hand-composed experiment.

Current SQL `CREATE INDEX` is not that switch. `SqlToRelCompiler` parses it as a named column list
(`SqlToRelCompiler.java:1905-1940`), `DBSPCompiler` validates and records it
(`DBSPCompiler.java:537-571,788-795`). A declaration on a table explicitly has no effect
(`DBSPCompiler.java:537-545`). For a view, `CalciteToDBSPCompiler.compileCreateIndex` maps it into an indexed
output sink (`CalciteToDBSPCompiler.java:3265-3329`). It has no declaration for typed Customer/Orders/Lineitem
interleaving or transaction phase. The circuit's internal `DBSPMapIndexOperator`s similarly create keyed
logical streams; they do not signal shared physical records.

The sibling Calcite fork's [`PipelineToMergedIndexScanRule`][mi-rule] is opt-in, matches an `EnumerableSort`
boundary by source-node identity, and leaves parent joins and aggregates in place
(`core/src/main/java/org/apache/calcite/adapter/enumerable/PipelineToMergedIndexScanRule.java:37-74,88-115`).
Its [`EnumerableMergedIndexScan`][mi-scan] and [`EnumerableMergedIndexDeltaScan`][mi-delta-scan] return empty
enumerables from `implement()`
(`core/src/main/java/org/apache/calcite/adapter/enumerable/EnumerableMergedIndexScan.java:203-216`;
`core/src/main/java/org/apache/calcite/adapter/enumerable/EnumerableMergedIndexDeltaScan.java:185-199`).
Feldera's `CalciteToDBSPCompiler.visit` accepts logical nodes listed above, so these physical nodes cannot
simply be handed to it. The [sibling Q3 fixture][mi-q3-test] identifies native-key Customer–Orders and
Orders–Lineitem alternatives and explicitly cannot derive one three-table candidate from independent native
keys
(`plus/src/test/java/org/apache/calcite/adapter/tpch/MergedIndexSinglePipelineTpchPlanTest.java:180-259`).
LeanStore's `custkey` extension changes that physical premise; it does not establish a Feldera planning rule.

The next validation gate is the hand-composed boundary: compare its Q3 `V⁺ − V⁻` after initial load,
sequential batches, retractions, and rekeys against independent full-query evaluation. Check the selected
Q5/Q10 output shapes, calendar bounds, `RN` snapshot in Q5, and downstream aggregate boundaries within their
scoped experiments. Compile the unmodified Feldera SQL separately for the baseline; capture `--plan` and
`--dataflow` if the baseline operator inventory or a later compiler path needs them. Then compare
beyond-memory physical reads and writes at equal memory and comparable durability. Until that experiment,
co-location and fewer seeks remain hypotheses, not compiler or performance results.

[compiler-main]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/CompilerMain.java#L170-L177
[sql-to-rel]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/compiler/frontend/calciteCompiler/SqlToRelCompiler.java#L914-L940
[calcite-opt]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/compiler/frontend/calciteCompiler/optimizer/CalciteOptimizer.java#L221-L334
[rel-to-dbsp]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/compiler/frontend/CalciteToDBSPCompiler.java#L2833-L2874
[map-index]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/circuit/operator/DBSPMapIndexOperator.java#L45-L81
[dbsp-compiler]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/compiler/DBSPCompiler.java#L759-L809
[compiler-options]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/compiler/CompilerOptions.java#L192-L195
[circuit-opt]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/compiler/visitors/outer/CircuitOptimizer.java#L117-L160
[incrementalize]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/compiler/visitors/outer/IncrementalizeVisitor.java#L33-L66
[opt-incremental]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/compiler/visitors/outer/OptimizeIncrementalVisitor.java#L121-L159
[rust-visitor]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/compiler/backend/rust/ToRustVisitor.java#L1927-L1963
[dbsp-join]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/circuit/operator/DBSPJoinOperator.java#L42-L52
[root-circuit]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/circuit_builder.rs#L3109-L3125
[unit-time]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/time.rs#L214-L219
[transaction-schedule]: https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/schedule.rs#L200-L226
[col-walk]: https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/tpch_family/col_pipeline.tpp#L260-L338
[mi-rule]: https://github.com/alicia-lyu/calcite/blob/d87f6dac0ace51c434c58092016c9e26b6b7c302/core/src/main/java/org/apache/calcite/adapter/enumerable/PipelineToMergedIndexScanRule.java#L37-L115
[mi-scan]: https://github.com/alicia-lyu/calcite/blob/d87f6dac0ace51c434c58092016c9e26b6b7c302/core/src/main/java/org/apache/calcite/adapter/enumerable/EnumerableMergedIndexScan.java#L203-L216
[mi-delta-scan]: https://github.com/alicia-lyu/calcite/blob/d87f6dac0ace51c434c58092016c9e26b6b7c302/core/src/main/java/org/apache/calcite/adapter/enumerable/EnumerableMergedIndexDeltaScan.java#L185-L199
[mi-q3-test]: https://github.com/alicia-lyu/calcite/blob/d87f6dac0ace51c434c58092016c9e26b6b7c302/plus/src/test/java/org/apache/calcite/adapter/tpch/MergedIndexSinglePipelineTpchPlanTest.java#L180-L259
