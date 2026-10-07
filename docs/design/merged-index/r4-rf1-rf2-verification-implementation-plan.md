# R4: verify the implemented RF1/RF2 workload

Date: 2026-10-06. Status: implemented and verified for loading and RF1/RF2.

## Scope and inspected components

Verify Customers-first initial loading, complete Order/Lineitem groups, RF1
insertion, and RF2 deletion with one maintenance worker. Keep the existing
transaction-local placement and `Q3State::publish`/`finish` flow. R4 makes no
placement-map changes. The separately implemented
[placement correction](placement-lookup-multimap-plan.md) uses candidate customer
sets; supported RF1/RF2 preparation supplies singleton sets. Broader multimap
verification is outside R4. Customer updates (including customer-only fan-out),
replacements, key moves, and independent line updates remain deferred. Residual
Q3 and performance measurements are outside this change.

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

The shared `verification::rf1_rf2_transcript` test executes 13 deterministic
transactions in both generated modes, with three customers, nine inserted
orders, 44 inserted lines, and 38 RF2 line deletions. Groups of 13, 11, and nine
lines cross the configured eight-record splitter boundary. Three explicit
multi-step transactions include RF1, RF2, and an empty transaction. Every
committed delta and snapshot agrees with full evaluation; all four requested
reader relations agree with independent source-map projections.

The complete `verify.sh` run passed on 2026-10-06 with Rust 1.96.1 and offline
locked dependencies:

- Native: all 10 tests passed, retaining the broader native regressions.
- Reconstructed: all seven tests passed, retaining workload rejection, grouped
  loading, multi-step publication, and checkpoint restoration followed by RF2.
- COL: all 21 tests passed, including storage and compaction regressions and the
  separately implemented candidate-location checks.
- Native and reconstructed canonical transcripts matched exactly across all
  13 transactions. Each contains 97,718 bytes and ends with six materialized rows.
- Formatting, Clippy with warnings denied, shell syntax, and whitespace passed.
- Script lifecycle probes verified native restoration after success, a test
  failure, and a transcript mismatch. The full run also restored native generation.
- Markdown heading/list spacing, fences, and local links were checked.

Logs and transcripts are in the experiment's ignored
`target/r4-verification/20261006T222308Z-60261/`. The roadmap, workflow, and
experiment results document the completed workload and deferred broader cases.
The concurrent placement correction was preserved in its own commit; R4 changes
only verification and documentation and uses singleton placements.
