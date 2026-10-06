# R4: verify the implemented RF1/RF2 workload

Date: 2026-10-06. Status: implementation in progress.

## Scope and inspected components

Verify Customers-first initial loading, complete Order/Lineitem groups, RF1
insertion, and RF2 deletion with one maintenance worker. Keep the existing
transaction-local `BTreeMap` placement and `Q3State::publish`/`finish` flow.
Customer updates (including customer-only fan-out), replacements, key moves,
independent line updates, and multimap changes remain deferred. Residual Q3 and
performance measurements are outside this change.

`Reference::evaluate` in the experiment's `src/main.rs` independently enumerates
logical Lines and looks up Orders and Customers in primary-key maps. Evaluate
before and after each transaction: the difference supplies signed expected
`ΔV`, and the after relation supplies expected materialized `V`. Existing native,
reconstructed, storage, checkpoint, and unsupported-workload tests remain.

The four readers in `merged-index-col/src/q3.rs` consume requested delta keys
and immutable before/after COL snapshots. Their expected relations will be
projected directly from the logical source maps, without merged-index reads or
incremental join logic.

## Implementation and verification

1. Add a shared experiment test module for both generated modes. Use identical
   deterministic Customers-first load and RF1/RF2 fixtures with multiple customers
   and orders, identical-payload distinct lines, oversized groups, repeated
   refresh cycles, empty transactions, and multiple runtime steps. Verify every
   committed delta and snapshot with the full-join oracle. In reconstructed mode,
   compare all four reader outputs for the requested transaction keys with
   independent source-map projections.
2. Write canonical transcripts containing transaction identity, signed emitted
   rows, and materialized rows. Add `verify.sh` to regenerate both modes, run
   their suites with Rust 1.96.1 and offline locked dependencies, compare the
   transcripts exactly, and run the COL suite. Store logs/transcripts under
   ignored `target/`; restore default native generation on exit.
3. Run Rust formatting, Clippy with warnings denied for both modes and COL,
   shell syntax, and whitespace checks. Preserve existing storage/checkpoint and
   unsupported-workload regressions.
4. Update the roadmap, workflow, experiment results, and this plan with executed
   evidence and deferred scope. Document placement-map container selection as a
   future optimization beside copying/sorting. Stage specific files and fetch
   and merge the tracking branch before each commit.

## Outcome

Pending implementation and execution of the checks above.
