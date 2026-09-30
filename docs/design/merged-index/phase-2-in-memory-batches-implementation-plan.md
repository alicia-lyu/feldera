# Phase 2 implementation plan: in-memory batches and consolidated cursors

Date: 2026-09-30. Status: implemented; see the
[Phase 2 implementation note](phase-2-implementation-note.md).

The [Step 2 plan](folded-key-layer-file-plan.md#phase-2-batches-and-cursors) governs this phase.
Refactor the existing `index.build_batch` while retaining the Phase 1 codecs. Return standard
`OrdIndexedWSet` batches, storing folded keys, encoded payloads, and signed weights in the
crate-internal representation. Construct each initial L0 run with
`Some(BatchLocation::Memory)` unconditionally, independent of runtime configuration or
memory pressure. Generic Feldera builders may choose files during construction; for this
required L0 contract, explicitly request memory. Keep folding, payload validation, sorting,
signed consolidation, zero removal, and overflow checks in the existing path.

Replace the custom raw cursor with a small constructor returning Feldera's `CursorList` over
existing batch cursors. Support zero, one, or multiple batches without copying or separating
source records. Reads combine matching `(K, payload)` weights and omit zero totals while
leaving the batches intact. Use the native cursor interface for traversal, exact and lower-bound
seeks, prefix scans, and rewind followed by seeking.

Verify unordered input, within-batch consolidation, multiple batches, interleaved source
records, addition, cancellation, replacement, empty input, exact/prefix scans, and rewind
followed by seeking. Run focused merged-index tests, formatting, and documentation checks.
Inputs below 100 MiB remain a workload assumption, not an enforced cap.

Retain Feldera's base-relation storage; the merged index is secondary storage. Reuse native
signed merging. The separate Phase 3 plan remains withdrawn. Clustered primary storage and
Phase 4 storage verification follows the
[native storage plan](phase-4-file-backed-storage-implementation-plan.md). No new merger,
cursor framework, or runtime uniqueness validator belongs to this revision.
