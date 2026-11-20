# Native Part–Partsupp–Lineitem merged index

A standalone library with the concrete Part–Partsupp–Lineitem (MI_P) definition for DBSP's general merged-index module, `dbsp::trace::merged_index`. It serves Q14 first; Q2, Q9, Q16, Q17, Q19, and Q20 follow.

- Definition, key and payload table, and tests: [CLAUDE.md](CLAUDE.md).
- How the work proceeds: [new-merged-index.md](../../../docs/design/merged-index/new-merged-index.md).
- The sibling index it is modeled on: [merged-index-col](../merged-index-col/README.md).
- The library's suite runs with `cargo +1.96.1 test --offline --locked` from this directory.
