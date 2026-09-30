# Phase 2 implementation note: in-memory batches and consolidated cursors

Date: 2026-09-30.

## Implementation

`CustomerOrdersLineitemIndex::build_batch` retains the Phase 1 codecs and refactors the
existing builder to return standard `OrdIndexedWSet` batches. It checks source/payload
correspondence, sorts arbitrary-order input by `(K, payload)`, and combines identical pairs
within each batch. Zero totals disappear; local weight overflow returns an error. Initial
L0 runs are always built with `Some(BatchLocation::Memory)`, regardless of runtime
configuration or pressure. Feldera's generic builders have a “weird” policy for this
contract because they can choose files during construction. Later Spine persistence and
compaction retain native placement behavior.

`batch_cursor` returns Feldera's `CursorList` over the existing batch cursors, including for
zero batches. Native reads combine matching `(K, payload)` signed weights and omit zero
totals. They borrow the batches without copying, partitioning by source, or mutating stored
rows. Callers use Feldera's `Cursor` interface; rewind keys before seeking backwards or
switching from an exact lookup to an ordered scan. Prefix scans stop at the caller's boundary.
The custom `RawBatchCursor` traversal has been removed.

## Verification

Passed `cargo +1.96.1 test -p dbsp --lib trace::merged_index --offline` (14/14 tests, including the Phase 4 storage tests),
`cargo +1.96.1 fmt --all --check`, Markdown lint, local link/source checks, Mermaid parsing,
and `git diff --check`.

The focused merged-index suite covers unordered input, within-batch addition and cancellation,
empty input, local overflow and source/payload mismatch, cross-batch addition, cancellation,
replacement, negative totals, and interleaved Customer/Orders/Lineitem records. It checks
exact and missing-key seeks, prefix stopping, rewind followed by seeking, and preservation
of the original batch rows. Memory-test batches assert memory-backed construction. The
Phase 4 storage tests cover this contract inside a runtime configured to select files.
Existing Phase 1 layer-file tests remain in the suite.

## Storage scope

Retain Feldera's existing base-relation storage and treat the merged index as secondary
storage. Reuse native signed merging; no separate Phase 3 plan or runtime uniqueness validator
is introduced. Clustered primary storage remains deferred. Phase 4 follows the
[native storage plan](phase-4-file-backed-storage-implementation-plan.md) under the
[settled scope](folded-key-layer-file-plan.md#settled-scope-before-phase-4).
This phase assumes small inputs below 100 MiB without enforcing a memory cap.
