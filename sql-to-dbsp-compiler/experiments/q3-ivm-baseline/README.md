# Native Q3 join IVM baseline

This standalone experiment maintains `V = Customer ⋈ Orders ⋈ Lineitem` with the SQL compiler's
incremental circuit, and optionally with the reconstructed circuit that reads the
Customer–Orders–Lineitem (COL) merged index instead of four integrator traces. `V` carries the
identities and payloads Q3 needs before its residual filters, revenue aggregation, ranking, limit,
and projection. [query.sql](query.sql) has one predicate-free materialized view and no intermediate
view or join hint.

## Two chained circuits

Base-table maintenance and view maintenance are separate generated circuits (user decision of
2026-10-23), which run in one DBSP runtime with one worker, one storage directory, and one buffer
cache (`Runtime::init_circuits`, measurement version 6):

- **Base circuit** ([base.sql](base.sql), crate `q3-base-circuit`). The three tables with primary
  keys and no view. The driver applies each transaction here first. Its net delta, read from the
  delta stream that the catalog registers for each input map, is exactly the change to the base
  relations, with complete rows for deletions.
- **View circuit** ([q3.sql](q3.sql), crate `q3-view-circuit`). The same tables without keys (Z-set
  inputs) and `V`. The driver feeds it the net delta, so it maintains `V` and nothing else.
  This is the native view circuit. The reconstructed build replaces it with the hand-written
  `q3-trie-view-circuit`, whose inputs are keyed by their full trie paths and come from `Dlt`.

[query.sql](query.sql) remains the logical definition: keyed tables plus `V`. The merged-index
module is generated from its schema.

In reconstructed mode the harness also maintains the merged index (`ManualIndex`) on an auxiliary
thread of the same runtime. Its upkeep is base maintenance, so base and IVM are always reported side
by side. The phase fields, the transaction order, and the contracts that the code enforces are in
[CLAUDE.md](CLAUDE.md). The prototype makes no performance-gain or hard-memory-cap claim.

## Where to read next

| Topic | File |
| --- | --- |
| Harness map, contracts, commands, verification | [CLAUDE.md](CLAUDE.md) |
| What R5 measures, report fields, the pending Linux campaign | [MEASUREMENT.md](MEASUREMENT.md) |
| Running the pilot and campaign on a fresh Linux machine | [LINUX-HANDOFF.md](LINUX-HANDOFF.md) |
| The generated native circuit and its state reads | [CIRCUIT.md](CIRCUIT.md) |
| Harness and measurement decisions, dated | [DECISIONS.md](DECISIONS.md) |
| Rules for any merged index and harness | [../CLAUDE.md](../CLAUDE.md) |
| Design, R1–R5 status, dated smoke results | [design README](../../../docs/design/merged-index/README.md), [history.md](../../../docs/design/merged-index/history.md) |
| The COL library | [merged-index-col](../merged-index-col/README.md) |

The native path is the default: `./generate.sh` and Cargo without the `merged-index` feature. The
reconstructed path needs the `merged-index` Cargo feature.
