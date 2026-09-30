# Phase 4 implementation plan: native file storage and snapshots

Date: 2026-09-30. Status: implementation plan. Completed test evidence belongs in the
[Phase 4 implementation note](phase-4-implementation-note.md).

The [Step 2 storage plan](folded-key-layer-file-plan.md#phase-4-native-file-storage-and-snapshots)
governs this phase. Retain Feldera's base-relation storage; the merged index is secondary
storage. Preserve the existing memory-only helpers and tests. Add separate storage tests
within the merged-index module. Do not change native batching, Accumulator, integration,
or Spine implementations.

## Native storage boundary

Use existing folding and payload codecs to create signed Customer records. Construct
standard `OrdIndexedWSet` batches before inserting them into a native Spine. This alias
already selects `FallbackIndexedWSet`, which supports memory and file storage. Spine has
no point-insertion interface. Insert each batch once with awaited `Trace::insert`; use
`TraceRole::Integral`, the native role for persistent integrator state. Initialize storage
and background merging with the existing runtime test helper. That helper supplies test
infrastructure, not transaction semantics.

Use default shared runtime storage and merge configuration under low memory pressure.
Native policy chooses placement and compaction. **Creating initial L0 sorted runs is run
generation, not spilling.** It corresponds to the sorted-run generation phase of external
merge sort. Moving existing in-memory runs to disk is handled through compaction,
including pressure-triggered single-run compaction. Do not describe these as one
“buffer-and-spill” operation. **Under normal circumstances, the merged-index adapter
should never force a storage destination.** Let Feldera's native compaction and
memory-pressure policy determine when runs move to disk. Ordinary tests inherit shared
defaults; targeted policy tests may override shared configuration.

Use 16 batches of 10,000 distinct Customer records with disjoint key ranges. Each record
has a 256-byte segment payload and weight `+1`. Assert initial batches are memory backed,
and that eight batches together exceed the effective merge-storage threshold. Batch count
triggers compaction; combined run size determines disk placement. Poll snapshot batch
metadata for up to 30 seconds for a file-backed output. On timeout, report locations,
sizes, and thresholds. Do not force storage destinations or invoke explicit full
compaction to satisfy this test.

## Reads and lifecycle

Retain a snapshot after the first insertion and verify it remains unchanged through
later insertion and compaction. Verify consolidated reads of the current snapshot contain
all 160,000 expected records, independent of physical batch boundaries. Reopen a resulting
file batch through native path and reader access with matching factories, and compare its
contents with the batch visible before reopen. Snapshots retain references to immutable
runs and files; a retained snapshot may keep storage alive after live compaction.

Add signed updates for cancellation, unchanged-key payload replacement, and key moves.
Compare consolidated reads with an independent `(folded key, payload)` signed-weight
oracle using representable weights. Verify exact lookup, missing keys, ordered traversal,
prefix stopping, and rewind across memory and file runs. Keep the test's physical
placement observations separate from its logical read assertions.

Run unchanged memory-only tests, the new storage tests, relevant native tests, Rust
formatting, Markdown lint, and link checks. The implementation note should record actual
locations, thresholds, test outcomes, and any environmental limitation. These tests do
not establish transaction recovery, a hard memory cap, or a performance improvement.

## Later integration

For subsequent production wiring, substitute only storage of the persistent trace
produced by `dyn_accumulate_integrate_trace` and its associated
`accumulate_delay_trace` reads. Leave `dyn_accumulate` and its delta-accumulation and
delayed-read path unchanged. Transaction integration and production source/operator
wiring are deferred. Phase 4 needs no custom run format, merger, staging spine, chunk
limits, storage owner, transaction operators, or point-insertion facade.
