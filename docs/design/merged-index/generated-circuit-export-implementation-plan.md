# Export DBSP-generated circuit graphs

Implement the supplied plan using existing compiler and runtime graphs.

1. Reuse compiler DOT writing in `ToDot.exportArtifacts`; expose the expanded
   `MonotoneAnalyzer` circuit through `--dump-expanded-circuit` and style actual
   operator classes without changing graph structure.
2. Extend the monitor renderer with optional state styles using the same topology
   traversal, keeping profiling output unchanged.
3. Capture per-worker runtime DOT/JSON and styled SVG after the experiment's final
   report through `--dump-runtime-circuit`, with configuration and provenance.
4. Replace hand-authored diagrams with generated artifacts, document commands,
   classification evidence, Catalog materialization, and `JoinTrace` reads.
5. Test CLI parsing, failures, classification, topology preservation, and existing
   correctness with capture enabled; render and inspect SVG; check docs and commit.

Load, RF1, and RF2 use the same incremental circuit. No execution semantics change.

## Integrator styling refinement

Use blue `[I]` for historical integrator state, including compiler
`DBSPIntegrateOperator` and `DBSPDelayedIntegralOperator`. The Q3 analysis expansion uses delayed prior-integral placeholders
and contains no explicit runtime integration region. Its `map_index` operators
rekey/project delta batches without integrating history.

At runtime, mark existing `accumulate_integrate_trace` and
`shard_accumulate_integrate_trace` regions as integrators. `AccumulateDelayTrace` snapshot/delay operators also use integrator-state
styling. Delta accumulators and trace append components use other-state styling. The renderer must style existing cluster IDs as well
as existing simple node IDs, preserving all topology and profiling output.

## Artifact destination

Keep generated diagrams, raw graphs, and logs in the repository’s
`target/q3-circuit-exports/`. Documentation links point to these local ignored
artifacts; graph outputs are not committed.

## Execution policy

All future IVM maintenance runs use one DBSP worker. Initial loading may use
multiple workers. Previously captured two-worker graphs remain runtime inspection
artifacts. The driver's one-worker default already implements this policy.

Partitioning and distributed execution are outside the current IVM maintenance
scope. No multiworker maintenance extension is planned.
