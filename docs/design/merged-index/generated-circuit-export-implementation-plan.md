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
