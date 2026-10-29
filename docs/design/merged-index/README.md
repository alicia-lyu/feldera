# Range-aligned incremental view maintenance

This directory documents a prototype that maintains TPC-H Q3's join incrementally in Feldera.
The maintained view is the predicate-free join `V = Customer ⋈ Orders ⋈ Lineitem`.
The consuming query applies Q3's filters, revenue aggregation, ordering, and limit to `V`
(residual execution).

The prototype compares two modes:

- **Native.** The SQL compiler's ordinary incremental circuit. The two joins keep four
  integrator traces.
- **Reconstructed.** Those four traces are not built. Each join instead reads the key ranges it
  needs from a secondary Customer–Orders–Lineitem (COL) merged index, whose keys fold the
  customer, order, and line identifiers (11, 20, and 25 bytes). A harness-side owner,
  `ManualIndex`, appends to the index outside the circuit on an auxiliary thread of the base
  runtime (measurement version 4 books this upkeep as base maintenance). Before each
  transaction it publishes before and after snapshots of the index. The join that consumes them is
  the supplied-snapshot join, `dyn_join_generic_with_snapshot_readers` in
  `crates/dbsp/src/operator/dynamic/join.rs`; it builds no integrate or delay traces.
  The compiler option `--q3-merged-index` selects this mode.

The workload is one DBSP worker, with initial loading followed by RF1 (insert complete
order and line groups) and RF2 (delete them). The placement map (order → customer) comes
only from Orders rows in the same transaction, so other changes are unsupported by design.
See [decisions.md](decisions.md) for the user's confirmed decisions, including the
single-worker policy, secondary-storage scope, and why folded keys need no uniqueness validator.

## Status of R1–R5

Statuses were checked against the code and tests at the commit that introduced this table.
The table makes no performance claim. Raw measurement artifacts, where they exist, live in
git-ignored `target/` directories and are local only.

| Stage | Meaning | Status | Evidence |
| --- | --- | --- | --- |
| R1 | Native baseline | Implemented. Correctness is checked against a full join; no measured result is recorded. | `q3-ivm-baseline` tests; [CIRCUIT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md) |
| R2 | COL library and state-read bindings | Implemented. The library and the generated-read contracts are documented. | `merged-index-col` tests; `crates/dbsp/src/trace/merged_index/`; [native-state-read-contracts.md](native-state-read-contracts.md) |
| R3 | Reconstruction | Implemented behind `--q3-merged-index` and the `merged-index` Cargo feature, for RF1/RF2 only. | [reconstruction.md](reconstruction.md); `Q3MergedIndexBinding.java`; `q3-ivm-baseline/src/reconstruction.rs` |
| R4 | RF1/RF2 verification | Complete. `verify.sh` compares native and reconstructed transcripts byte for byte. Replacements, key moves, independent line updates, and customer updates are deferred. | [reconstruction.md](reconstruction.md); [WORKFLOW.md](WORKFLOW.md) |
| R5 | Maintenance measurement | Harness implemented (`measure.py`, `src/measurement.rs`). The qualifying Linux run is pending, so no comparison exists. | [measurement.md](measurement.md) |

## Open code follow-ups

These were found by the 2026-10-12 review and are tracked here, not yet scheduled:

- Turn the reader `expect` calls in `merged-index-col/src/q3.rs` into errors.
- Add a property test that compares the reconstructed join with the native join.
- Remove the dead `ordersCustomers` branch in `ToRustVisitor.processJoinBase`.
- Decide whether native runs should also reject `--workers` values other than 1.
- Review the changes visible upstream: the single-batch spill in `spine_async`, the default `is_background_merge_idle() = true`, and `SerBatch::location`.

## Reading guide

- [decisions.md](decisions.md): confirmed decisions, withdrawn ideas, open questions.
- [WORKFLOW.md](WORKFLOW.md): every command (compile, test, verify, measure).
- [col-storage.md](col-storage.md): folded-key encoding and batch and file layout.
- [general-definition.md](general-definition.md): the key-trie spec for declaring a merged index,
  and the roadmap for generating its definition.
- [reconstruction.md](reconstruction.md): the transaction contract, the placement map, R3 and R4.
- [measurement.md](measurement.md): R5 definitions and the pending Linux campaign.
- [native-state-read-contracts.md](native-state-read-contracts.md): the four generated reads.
- [native-run-placement.md](native-run-placement.md): how native runs are built,
  leveled, placed in memory or files, and steered by memory pressure.
- [support.md](support.md): the trace and cursor interface and acceptance criteria.
- [shared-traversal.md](shared-traversal.md): the plan for one forward scan session per
  transaction (thesis Section 2.2): gaps, algorithm work, and coding roadmap.
- [deferred.md](deferred.md): designs not implemented, such as shared scan sessions.
- [references.md](references.md): sibling-repository sources and their roles.
- [history.md](history.md): dated, non-authoritative records of superseded measurements and gaps.
- [slides-oct-8/](slides-oct-8/README.md): the 2026-10-08 advisor deck.
- Experiments:
  - [q3-ivm-baseline](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/README.md)
    (harness; [CIRCUIT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md)
    and [RESULTS.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/RESULTS.md))
  - [merged-index-col](../../../sql-to-dbsp-compiler/experiments/merged-index-col/README.md)
    (COL keys, payloads, readers)

## Terms

- **Transaction delta.** The logical change set of one DBSP transaction.
- **Run.** An immutable sorted collection of weighted records, in memory or in a file.
  Feldera's code calls it a batch.
- **`X⁻`, `X⁺`.** Accumulated state of `X` immediately before and after a transaction; `ΔX = X⁺ − X⁻`.
- **Retained state.** An integrator's maintained output. **Reconstructed state** is the same
  output computed from the merged index for the requested keys.
