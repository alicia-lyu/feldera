# Phase 2 implementation plan: in-memory batches and raw cursors

Date: 2026-09-29. Status: implemented; see the
[Phase 2 implementation note](phase-2-implementation-note.md).

The [Step 2 plan](folded-key-layer-file-plan.md#phase-2-batches-and-cursors) governs this phase.
Use Feldera's `VecIndexedWSet<DynData, DynData, DynZWeight>` with `Vec<u8>` keys and values to
store the folded key, encoded payload, and signed weight. Retain the Phase 1 codecs and the
crate-internal boundary.

Accept small arbitrary-order input, sort it in memory, and combine identical `(K, payload)`
contributions within each batch. Add a raw cursor that seeks and advances across multiple
in-memory batches in key order. It returns stored contributions, including equal pairs in
different batches, without computing accumulated state.

Test unordered input, duplicate pairs within a batch, exact and missing-key seeks, prefix
stopping, and equal pairs across batches. Run the focused `dbsp` tests and `cargo fmt --check`.
Inputs below 100 MiB are a workload assumption, not an enforced cap. Phase 3 adds cross-batch
signed consolidation and K uniqueness. Phase 4 adds file-backed batches, bounded staging,
spine append, and snapshots.
