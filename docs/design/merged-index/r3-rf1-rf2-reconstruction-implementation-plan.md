# R3: generated Q3 reconstruction for RF1/RF2

Date: 2026-10-06. Status: implementation in progress.

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
temporary rows; it is not required for this implementation.

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
Maintenance uses one worker. Broader R4 verification and R5 beyond-memory
comparisons remain separate.
