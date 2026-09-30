# Phase 4 implementation plan: native file storage and snapshots

Date: 2026-09-30. Status: implementation plan. Completed test evidence belongs in the
[Phase 4 implementation note](phase-4-implementation-note.md).

The [Step 2 storage plan](folded-key-layer-file-plan.md#phase-4-native-file-storage-and-snapshots)
governs this phase. Retain Feldera's base-relation storage; the merged index is secondary
storage. Preserve the existing memory-only helpers and tests. Add separate storage tests
within the merged-index module. Do not change native batching, Accumulator, integration,
or Spine implementations.

## Native storage boundary

Use existing folding and payload codecs to create signed Customer, Order, and Lineitem
records. Construct standard `OrdIndexedWSet` batches exclusively through
`index.build_batch` before inserting them into a native Spine. Initial L0 construction must
always request `Some(BatchLocation::Memory)`, including inside a storage-enabled runtime
whose normal construction policy selects files. This explicit choice is required because
Feldera's generic batch builders may choose file storage while constructing a batch. This
does not alter later Spine persistence or compaction. This alias
already selects `FallbackIndexedWSet`, which supports memory and file storage. Spine has
no point-insertion interface. Insert each batch once with awaited `Trace::insert`; use
`TraceRole::Integral`, the native role for persistent integrator state. Initialize storage
and background merging with the existing runtime test helper. That helper supplies test
infrastructure, not transaction semantics.

Use default shared runtime storage and merge configuration under low memory pressure for
the default-policy storage test. **Creating initial L0 sorted runs is run generation, not
spilling.** Initial construction explicitly requests memory to satisfy the required L0
contract. Moving existing in-memory runs to disk is handled through native compaction,
including pressure-triggered single-run compaction. Ordinary storage tests inherit shared
defaults; targeted policy tests may override shared configuration.

Use 16 batches, each covering 10,000 distinct customers. For each customer `c`, create a
Customer with a 256-byte segment, two Orders with IDs `1_000_000 + 2*c + j` for `j = 0`
or `1`, and two Lineitems per order with line IDs 1 and 2. Set `order_day = c % 365 - 180`,
`ship_priority = j`, `ship_day = order_day + line_id`,
`extended_price_cents = 10_000 + order_id + line_id`, and
`discount_hundredths = c % 101`. Build all 1,120,000 records through `index.build_batch`
with positive unit weights. Assert initial batches are memory backed, including in a
storage-enabled runtime. Separately confirm that the generic native builder can select a
file when its threshold is configured to zero; this documents why merged-index L0
construction explicitly requests memory.

Preserve default-policy coverage: compare batch sizes across eight insertions, await each
insertion, and poll snapshot metadata for up to 30 seconds for a file-backed output. On
timeout, report locations, sizes, and thresholds. Do not force later storage destinations
or invoke explicit full compaction to satisfy this test.

## Reads and lifecycle

Retain a snapshot after the first insertion and verify it remains unchanged through
later insertion and compaction. Verify consolidated reads of the current snapshot contain
all 1,120,000 expected records, independent of physical batch boundaries. Reopen a resulting
file batch through native path and reader access with matching factories, and compare its
contents with the batch visible before reopen. Snapshots retain references to immutable
runs and files; a retained snapshot may keep storage alive after live compaction.

Retain first-insertion and pre-update snapshots and verify their contents remain unchanged.
Add signed updates that cancel customer 10's complete group, replace every payload at
unchanged keys in customer 20's group, and move customer 30's complete group to customer
160,000 with newly assigned order IDs.
Compare consolidated reads with an independent `(folded key, payload)` signed-weight
oracle using representable weights. Verify Customer, Order, and Lineitem exact/missing
lookup, ordered traversal, prefix scans, and rewind across memory and file runs. Require a
resulting file batch to contain all three
record types; reopen it through native path and reader access with matching factories and
compare typed keys, payloads, and weights. Keep the test's physical
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
