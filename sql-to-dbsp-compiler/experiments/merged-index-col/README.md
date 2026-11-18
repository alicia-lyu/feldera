# Native Customer–Orders–Lineitem merged index

A standalone library with the concrete Customer–Orders–Lineitem (COL) definition for DBSP's general merged-index module, `dbsp::trace::merged_index`, and the Q3 readers of the reconstructed circuit.

- Definition, key and payload table, readers, contracts, and tests: [CLAUDE.md](CLAUDE.md).
- Storage layout: [storage-layout.md](../../../docs/design/merged-index/storage-layout.md).
- Commands: [WORKFLOW.md](../../../docs/design/merged-index/WORKFLOW.md). The library's suite runs with `cargo +1.96.1 test --offline --locked` from this directory.
- The harness that uses it: [q3-ivm-baseline](../q3-ivm-baseline/README.md).
