# Phase 4 implementation note: native file storage and snapshots

Date: 2026-09-30.

The [Phase 4 plan](phase-4-file-backed-storage-implementation-plan.md) governs this work.
Storage tests use native batches and Spine insertion while preserving the existing
memory-only helpers. Base-relation storage remains in place; the merged index is secondary storage.

## Implementation

The separate `storage_tests` module constructs `OrdIndexedWSet` batches from existing
folding and payload codecs and inserts them once, with awaited `Trace::insert`, into a
native Spine using `TraceRole::Integral`. Production batching, Accumulator, integration,
and Spine implementations are unchanged.

The configuration-aware runtime test helper initializes storage and background merging.
It receives default `StorageOptions` and default merge settings. The simpler storage
helper uses `min_storage_bytes: Some(0)` and would bypass the intended size-based
transition, so this test supplies actual defaults instead. Runtime pressure is asserted
to be low; no destination or pressure override is used.

The fixture contains 16 disjoint batches of 10,000 Customers with 256-byte segments and
unit positive weights. Native snapshot cursors are checked against an independent
`BTreeMap<(folded key, payload), signed weight>` oracle before and after cancellation,
same-key replacement, and a key move. The updated snapshot is explicitly checked to
contain both memory and file batches. Exact/missing seeks, ordered traversal, prefix
stopping, and rewind use the same native cursor over that mixed snapshot.

The first snapshot still reads its original 10,000 records after compaction and updates;
the pre-update snapshot still reads all 160,000 original records after updates. A resulting
file batch is reopened through its native reader path and matching factories, then compared
with its original contents and round-tripped through the folding and payload codecs.
Snapshots retain references to immutable runs and files; their lifetime may retain storage
after live compaction. Reopening a referenced temporary file is not restart recovery.

## Verification

The focused storage test passed in 8.21 seconds. All 16 initial batches were memory
backed. The effective merge-storage threshold was 10,485,760 bytes, and the first eight
memory batches totalled 27,440,064 approximate bytes (3,430,008 each). At the observed
transition, a snapshot contained eight memory batches of 10,000 rows and one file batch
of 80,000 rows, with approximate size 3,581,952 bytes. Input run size selects native
merge placement; the resulting file's size need not exceed the threshold. Physical batch
boundaries and output sizes are observations, not fixed assertions of the test.

Native baseline checks passed: nine Spine merge-threshold tests, eight two-column file
tests, and the runtime memory-pressure threshold/storage-policy test.

The complete merged-index suite passed all 13 tests, including the 12 unchanged encoding,
memory-batch, cursor, and layer-file tests. Verification commands:

```sh
cargo +1.96.1 test -p dbsp --lib trace::merged_index --offline
cargo +1.96.1 test -p dbsp --lib trace::spine_async::merge_threshold_test --offline
cargo +1.96.1 test -p dbsp --lib storage::file::test::two_columns --offline
cargo +1.96.1 test -p dbsp --lib circuit::runtime::tests::memory_pressure_thresholds_and_spill_behavior --offline
cargo +1.96.1 fmt --all --check
```

Rust formatting, Markdown lint, all 201 active local/source link checks, three Mermaid
diagram parses, and `git diff --check` passed. Clippy completed with no diagnostics in the new storage test;
the warnings-as-errors run failed on existing warnings elsewhere in DBSP, including
unused prototype code and existing operator tests. No unrelated lint fixes were made.

## Limits and handoff

These tests demonstrate storage behavior. They do not establish transaction recovery,
a hard memory cap, or a performance improvement. Transaction integration and production
source/operator wiring remain deferred. The eventual substitution target is storage of
the persistent trace from `dyn_accumulate_integrate_trace` and its associated
`accumulate_delay_trace` reads. Leave `dyn_accumulate` and its delta-accumulation and
delayed-read path unchanged.
