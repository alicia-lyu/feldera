# Phase 2 implementation plan: in-memory batches and consolidated cursors

Date: 2026-09-30. Status: implemented; see the
[Phase 2 implementation note](phase-2-implementation-note.md).

The [Step 2 plan](folded-key-layer-file-plan.md#phase-2-batches-and-cursors) governs this phase.
Keep the existing batch builder and Phase 1 codecs. Store folded keys, encoded payloads, and
signed weights in Feldera's `VecIndexedWSet<DynData, DynData, DynZWeight>` with `Vec<u8>` keys
and values, within the crate-internal boundary.

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
Phase 4 implementation are deferred. No new merger, cursor framework, or runtime uniqueness
validator belongs to this revision.
