# R3: generated Q3 reconstruction for RF1/RF2

Date: 2026-10-06. Status: implemented for the RF1/RF2 prototype.

## Contract

Initial loading prepares the state; measurements cover subsequent RF1/RF2 only.
Load Customers first, then complete Order/Lineitem groups. Refreshes insert or
delete complete groups under referential integrity. Customer updates, independent
line changes, orphan rows, replacements, and key moves are outside this prototype.

Maintain the COL secondary index manually with base changes before running Q3.
Capture native before/after snapshots around one insertion of the physical delta.
Resolve order/customer placement from complete Orders records in the same logical
delta batch; no historical placement map or Q3-owned index accumulation is added.

## Implementation

1. Add a supplied-state native join path preserving transaction delta collectors
   and `JoinTrace`, while omitting the replaced historical join traces.
2. Extend the reusable COL library with four typed, query-specific reconstruction
   readers and an externally published per-transaction snapshot pair.
3. Add guarded `--q3-merged-index` code generation for the supported incremental
   Catalog circuit, retaining the default native generator behavior.
4. Add grouped loading, manual index maintenance, refresh verification, and separate
   index/Q3/combined refresh timing to the reconstructed experiment.
5. Exercise live snapshot/file behavior and native checkpoint restoration at a
   completed prototype transaction boundary. Document circuit stacking and its
   cross-circuit checkpoint coordination as deferred work.

The reconstructed native batches copy only requested accumulated key ranges.
This copying, decoding, and possible sorting is suboptimal and charged to Q3
maintenance. A future direct reconstructed cursor could avoid materializing these
temporary rows; it is not required for this implementation. Selecting the
transaction-local placement map's container is also a future optimization; the
current implementation uses `BTreeMap` and `BTreeSet` following the
[separate placement correction](placement-lookup-multimap-plan.md). Supported
RF1/RF2 preparation supplies one candidate customer for each order.

## Validation

- Compare all four typed readers with independent expected weighted relations.
- Compare generated native/reconstructed deltas and materialized `V` with full
  joins for loading, RF1, RF2, empty and multi-step transactions.
- Cover multiple customers/orders, identical-payload distinct lines, batch/group
  boundaries, unsupported changes, and missing placement rejection.
- Check mixed memory/file snapshots, compaction, and native save/restore.
- Inspect graphs for removal of replaced historical traces and preservation of
  transaction delta collectors; Q3 must perform no secondary-index insertion.
- Run focused compiler/Rust tests, formatting, lint, and documentation checks.

Preserve the native baseline's broader tests and the other agent's COL changes.
Maintenance uses one worker. The [R4 verification plan](r4-rf1-rf2-verification-implementation-plan.md)
adds shared tests for supported loading and RF1/RF2 and checks that the native
and reconstructed JSON result files are identical byte for byte. Broader update
cases and R5 beyond-memory comparisons remain deferred.

## Implementation outcome

The compiler's `--q3-merged-index` option validates the supported Q3 circuit and
emits the four COL state readers through two supplied-snapshot joins. The default
generator retains native joins. The manual index owner uses the native `Spine`
on a DBSP auxiliary thread; Q3 keeps its transaction delta collectors and
materialized output while omitting the four replaced historical traces.

Native checkpoint restoration is verified at a completed transaction boundary.
The harness saves the manual trace using the prepared checkpoint UUID before
committing Q3, so native checkpoint dependency collection includes its batch
files. This does not implement independently stacked circuits or their
checkpoint protocol.

Completed checks:

- The supplied-snapshot join regression matches native signed updates across
  simultaneous and multi-step transactions.
- The COL library passes all 19 tests, including weighted reconstruction,
  consolidation, mixed memory/file snapshots, and compaction.
- Compiler binding tests pass all four cases; opt-in generation emits both
  reconstructed joins and rejects unsupported circuits.
- The reconstructed harness passes all six tests, including independent full
  join verification, empty and multi-step transactions, workload rejection,
  grouped loading, and checkpoint restoration followed by RF2.
- Default regeneration passes all nine native harness tests.
- Both harness modes and the COL library pass Clippy with warnings denied;
  Rust formatting and shell syntax checks pass.
- A small file-driven RF1/RF2 smoke run verifies deltas and materialized output.
  It supplies no performance comparison.
