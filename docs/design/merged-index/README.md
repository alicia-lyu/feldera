# Range-aligned incremental view maintenance

This directory documents a prototype that maintains TPC-H Q3's join incrementally in Feldera.
The maintained view is the predicate-free join `V = Customer ⋈ Orders ⋈ Lineitem`.
The consuming query applies Q3's filters, revenue aggregation, ordering, and limit to `V`
(residual execution).

**Building a second merged index?** Start with [new-merged-index.md](new-merged-index.md),
"Building a second merged index": what exists generically, what is Q3- or COL-only and must be
redone, and a step checklist.

The prototype compares two modes:

- **Native.** The SQL compiler's ordinary incremental circuit. The two joins keep four
  integrator traces.
- **Reconstructed.** Those four traces are not built. Each join instead reads the key ranges it
  needs from a secondary Customer–Orders–Lineitem (COL) merged index, whose keys fold the
  customer, order, and line identifiers (11, 20, and 25 bytes). A harness-side owner,
  `ManualIndex`, appends to the index outside the circuit on an auxiliary thread of the runtime that
  hosts both circuits (measurement version 4 books this upkeep as base maintenance). Before each
  transaction it publishes the index's snapshot before the transaction (`Old`) and the transaction's run (`Dlt`); after reads take `Old ⊎ Dlt`. The join that consumes them is
  the supplied-snapshot join, `dyn_join_generic_with_snapshot_readers` in
  `crates/dbsp/src/operator/dynamic/join.rs`; it builds no integrate or delay traces.
  The view circuit of this mode is hand-written (`q3-trie-view-circuit`, since 2026-11-10); the `merged-index` Cargo feature selects it.

The workload is one DBSP worker, with initial loading followed by RF1 (insert complete
order and line groups) and RF2 (delete them). Reconstructed mode supports any delta of the base
tables since 2026-11-10 (line items with no order wait in a side store); the CLI drivers still
produce RF1 files and RF2 keys only.

Decisions are split by owner. Generic and process decisions, including the single-worker policy,
secondary-storage scope, and why folded keys need no uniqueness validator, are in
[decisions.md](decisions.md). Q3 harness and measurement decisions are in
[q3-ivm-baseline/DECISIONS.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/DECISIONS.md).

## Status of R1–R5

Statuses were checked against the code and tests at the commit that introduced this table.
The table makes no performance claim. Raw measurement artifacts, where they exist, live in
git-ignored `target/` directories and are local only.

| Stage | Meaning | Status | Evidence |
| --- | --- | --- | --- |
| R1 | Native baseline | Implemented. Correctness is checked against a full join; no measured result is recorded. | `q3-ivm-baseline` tests; [CIRCUIT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md) |
| R2 | COL library and state-read bindings | Implemented. The library and the native state-read contracts are documented. | `merged-index-col` tests; [merged_index/CLAUDE.md](../../../crates/dbsp/src/trace/merged_index/CLAUDE.md); [CIRCUIT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md); [dbsp-background.md](dbsp-background.md) |
| R3 | Reconstruction | Implemented behind the `merged-index` Cargo feature, with a hand-written view circuit, for general deltas. | [reconstruction.md](reconstruction.md); `q3-trie-view-circuit`; `q3-ivm-baseline/src/reconstruction.rs` |
| R4 | Verification | Complete. `verify.sh` compares native and reconstructed transcripts byte for byte, for RF1/RF2 and for general deltas. | [q3-ivm-baseline/CLAUDE.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CLAUDE.md); [reconstruction.md](reconstruction.md) |
| R5 | Maintenance measurement | Harness implemented (`measure.py`, `src/measurement.rs`). The qualifying Linux run is pending, so no comparison exists. | [MEASUREMENT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/MEASUREMENT.md); [LINUX-HANDOFF.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/LINUX-HANDOFF.md) |

### Acceptance criteria

R3 and R4 implement a single-worker prototype for loading, RF1/RF2, and general deltas of the
base tables. The criteria below say what would establish the design more broadly. The status
column covers only what the repository shows.

| Acceptance question | Required evidence | Status |
| --- | --- | --- |
| Are operator inputs and results identical? | Run the same join with retained and reconstructed state; compare requested tuples, weights, ordering, absence, and before/after reads; check retractions and insertions against an independent evaluation. | `verify.sh` in `q3-ivm-baseline` runs both modes over the same RF1/RF2 and general-delta transactions and compares emitted rows and materialized `V` with a full recomputed join. It covers one worker and the Q3 view only. |
| Does the adapter preserve encoding and weighted updates? | Cross-type byte ordering, exact range bounds, type decoding, replacements at unchanged keys, signed multiplicities, before/after reads. | Covered by the tests listed in [storage-layout.md](storage-layout.md#tests). |
| Does reading preserve lifecycle and memory bounds? | Overlapping request lifetimes, ranges larger than memory, abort and restart; borrowed-value validity; reference-only snapshots; each batch advanced exactly once. | Not shown. No hard memory bound is claimed, and abort and restart are not tested. |
| Does storage replacement actually occur? | Inventory retained state, including aggregate output and delays; confirm intermediate snapshots are not accumulated again. | In reconstructed mode `dyn_join_generic_with_snapshot_readers` builds no integrate or delay traces for the two joins (confirmed by code review of `join.rs`). Remaining state is not inventoried here. |
| Is total maintenance cheaper? | Datasets larger than memory, equal total memory, comparable durability, matched predicates and results. | Unmeasured. See [MEASUREMENT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/MEASUREMENT.md). Raw measurement artifacts are local-only, in git-ignored `target/` directories. |

When measuring, count every read and write: RF1/RF2, workload-side discovery, operator access, staging, merging, checkpointing, and spills. Report persistent and peak storage, CPU and decoding, memory high-water, and complete-transaction latency for clustered and scattered updates. Fewer unique blocks or a resident scan speedup does not by itself show lower total I/O.

## Open code follow-ups

These were found by the 2026-10-12 review and are tracked here, not yet scheduled:

- Turn the reader `expect` calls in `merged-index-col/src/q3.rs` into errors.
- Add a property test that compares the reconstructed join with the native join.
- Remove the dead `ordersCustomers` branch in `ToRustVisitor.processJoinBase`.
- Decide whether native runs should also reject `--workers` values other than 1.
- Review the changes visible upstream: the single-batch spill in `spine_async`, the default `is_background_merge_idle() = true`, and `SerBatch::location`.

## Code map

Each code directory has a `CLAUDE.md`, which Claude Code loads when it works there. Rules that
hold for any merged index or harness sit one level up; each tier links the tier above it.

| Directory | Holds |
| --- | --- |
| [`CLAUDE.md`](../../../CLAUDE.md) (root) | Fork rules, architecture, toolchain, research-wide rules, and how to trust the docs |
| [`crates/dbsp/`](../../../crates/dbsp/CLAUDE.md) | The fork's DBSP changes outside `merged_index/`: several circuits in one runtime, the supplied-snapshot join, `RunReads`, `BlockExpansions` |
| [`crates/dbsp/src/trace/merged_index/`](../../../crates/dbsp/src/trace/merged_index/CLAUDE.md) | The reusable machinery: key trie, encoders, folded keys, batches, `RunMerge`, `ScanSession`, generic readers |
| [`sql-to-dbsp-compiler/SQL-compiler/`](../../../sql-to-dbsp-compiler/SQL-compiler/CLAUDE.md) | The `.mi` parser and validator, the Rust emitter and its flags, `SqlCircuit::build`, `GeneratedRustNames` |
| [`sql-to-dbsp-compiler/experiments/`](../../../sql-to-dbsp-compiler/experiments/CLAUDE.md) | Rules for any index and harness: accounting, transaction order, general deltas, run placement |
| [`.../experiments/merged-index-col/`](../../../sql-to-dbsp-compiler/experiments/merged-index-col/CLAUDE.md) | COL's key and payload table, the Q3 readers and routing hook, regeneration of `generated.rs` |
| [`.../experiments/q3-ivm-baseline/`](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CLAUDE.md) | The Q3 harness: `reconstruction::prepare`, `OrphanLines`, `Pipeline`, `ManualIndex`, images, commands |

The Q3 harness directory has siblings: [MEASUREMENT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/MEASUREMENT.md) (R5 definitions and
the pending campaign), [LINUX-HANDOFF.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/LINUX-HANDOFF.md), [DECISIONS.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/DECISIONS.md)
(harness and measurement decisions), and [CIRCUIT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md) (the generated native
circuit and its state reads). Human landing pages:
[q3-ivm-baseline/README.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/README.md) and
[merged-index-col/README.md](../../../sql-to-dbsp-compiler/experiments/merged-index-col/README.md).

## Reading guide

- [new-merged-index.md](new-merged-index.md): building a second merged index.
- [decisions.md](decisions.md): generic and process decisions, withdrawn ideas, open questions.
  Q3 harness and measurement decisions are in [DECISIONS.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/DECISIONS.md).
- [WORKFLOW.md](WORKFLOW.md): cross-directory commands (prerequisites, DBSP and Java test filters,
  regenerating an index module, inspecting the compiler). Other commands live with their directory.
- [storage-layout.md](storage-layout.md): folded-key rules, batch and layer-file layout, prefix
  truncation, and why folded keys are unique.
- [general-definition.md](general-definition.md): the key-trie spec for declaring a merged index
  and the code the compiler generates from it.
- [shared-traversal.md](shared-traversal.md): the meta plan for shared partner reads (thesis
  Chapters 2 and 3): the algorithm, selective routing, the concepts in Feldera code, and the
  no-extra-decoding rules.
- [reconstruction.md](reconstruction.md): the generic access path, trace omission, and
  transaction contract, with Q3 as the example.
- [dbsp-background.md](dbsp-background.md): the trace and cursor interface, how native runs are
  built, leveled, and placed, memory pressure, and snapshot, fetch, and join-wait behavior.
- [deferred.md](deferred.md): designs not implemented, such as shared scan sessions and generic readers.
- [compaction.md](compaction.md): compaction options for small transactions, against the K = 10 measurements (documentation only; deferred on 2026-11-18).
- [memtable.md](memtable.md): a B+-tree memtable under its own budget, sealed by one ordered copy into the file builder (design only, 2026-11-19).
- [references.md](references.md): sibling-repository sources and their roles.
- [history.md](history.md): dated, non-authoritative records of superseded measurements, gaps, and plans.
- [slides-oct-8/](slides-oct-8/README.md): the 2026-10-08 advisor deck.

## Terms

- **Transaction delta.** The logical change set of one DBSP transaction (the signed change set of
  one transaction). Not a "batch".
- **Trace.** Feldera's name for an indexed collection of accumulated weighted updates. It implies
  no transaction log and no stored timepoints.
- **Run.** An immutable sorted collection of weighted records, in memory or in a layer file.
  Feldera's code calls it a batch.
- **Layer file.** Feldera's immutable file format for nested sorted groups. An indexed run has two
  levels: the *key level* holds each distinct key once, and the *value level* holds the values
  grouped under their key.
- **Spine.** The collection of runs that compacts them in the background.
- **`X⁻`, `X⁺`.** Accumulated state of `X` immediately before and after a transaction; `ΔX = X⁺ − X⁻`.
- **Retained state.** An integrator's maintained output. **Reconstructed state** is the same
  output computed from the merged index for the requested keys.
