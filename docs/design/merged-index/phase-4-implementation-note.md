# Phase 4 implementation note: native file storage and snapshots

Date: 2026-09-30.

The [Phase 4 plan](phase-4-file-backed-storage-implementation-plan.md) governs this work.
Storage tests use native batches and Spine insertion while preserving the existing
memory-test behavior. Base-relation storage remains in place; the merged index is secondary storage.

## Implementation

The separate `storage_tests` module constructs standard `OrdIndexedWSet` batches
exclusively through `index.build_batch` and inserts them once, with awaited
`Trace::insert`, into a native Spine using `TraceRole::Integral`. Initial L0 runs request
`Some(BatchLocation::Memory)` unconditionally, even in a storage-enabled runtime whose
generic construction policy otherwise selects files. Feldera's generic construction
policy is “weird” relative to this required L0 contract because it may select files while
building. Later Spine persistence and compaction retain native placement behavior.

The configuration-aware runtime test helper initializes storage and background merging.
It receives default `StorageOptions` and default merge settings. The simpler storage
helper uses `min_storage_bytes: Some(0)` and would bypass the intended size-based
transition, so this test supplies actual defaults instead. Runtime pressure is asserted
to be low; later persistence and compaction receive no destination or pressure override.

The fixture contains 16 batches of 10,000 distinct customers, each with a 256-byte
segment, two Orders, and two Lineitems per order: 1,120,000 records total. Each batch has
70,000 records. Orders use IDs `1_000_000 + 2*c + j` for `j = 0, 1`; their dates and
priorities and all Lineitem fields are populated deterministically. All records are built
through `index.build_batch` with unit positive weights. Native snapshot cursors are checked
against an independent
`BTreeMap<(folded key, payload), signed weight>` oracle before and after cancellation,
same-key replacement, and a complete group move. Customer 10's group is canceled,
customer 20's payloads are replaced at unchanged keys, and customer 30's group moves to
customer 160,000 with newly assigned order IDs. The result contains 1,119,993 records.
The updated snapshot is explicitly checked to contain both memory and file batches.
Customer, Order, and Lineitem exact/missing lookups, ordered traversal, prefix scans, and
rewind use the same native cursor over that mixed snapshot.

The first-insertion snapshot still reads its original 70,000 records after compaction and
updates; the pre-update snapshot still reads all 1,120,000 original records after updates.
A resulting file batch containing Customer, Order, and Lineitem records is reopened through its native
reader path and matching factories, then compared by typed keys, payloads, and weights
with its original contents.
Snapshots retain references to immutable runs and files; their lifetime may retain storage
after live compaction. Reopening a referenced temporary file is not restart recovery.

## Verification

All 14 merged-index tests passed, including the 12 encoding, memory-batch, cursor, and
layer-file tests and both storage tests. Every initial COL run was memory backed, including
under zero storage thresholds where a generic nonempty builder selected a file.

In one verified default-policy run, the effective merge-storage threshold was 10,485,760
bytes and the first eight memory runs occupied 74,239,964 approximate bytes. The observed
snapshot contained eight memory runs of 70,000 rows and one file run of 560,000 rows
(17,624,576 approximate bytes). Physical batch boundaries and output sizes are observations,
not fixed assertions. The test awaits insertion and polls for background file compaction
for at most 30 seconds, reporting locations, sizes, and thresholds on timeout.

Native baseline checks passed: nine Spine merge-threshold tests, eight two-column file
tests, the runtime memory-pressure threshold/storage-policy test, and the empty-builder
zero-threshold test. Verification commands:

```sh
cargo +1.96.1 test -p dbsp --lib trace::merged_index --offline
cargo +1.96.1 test -p dbsp --lib trace::spine_async::merge_threshold_test --offline
cargo +1.96.1 test -p dbsp --lib storage::file::test::two_columns --offline
cargo +1.96.1 test -p dbsp --lib circuit::runtime::tests::memory_pressure_thresholds_and_spill_behavior --offline
cargo +1.96.1 test -p dbsp --lib trace::test::an_empty_builder_stays_in_memory_under_a_zero_threshold --offline
cargo +1.96.1 fmt --all --check
cargo +1.96.1 clippy -p dbsp --lib --tests --offline
```

Rust formatting, Markdown lint, 131 local links across the nine current design documents,
135 active links/source line ranges in the index and support pages, three Mermaid diagrams,
and `git diff --check` passed. The source-link check excludes external documentation URLs
from local-file validation.

Clippy completed successfully with no Clippy-specific diagnostics in the changed merged-index
code. It still reports existing unused prototype imports/dead code because production wiring
is deferred, and unrelated warnings in operator, storage, cursor, and utility tests. No
unrelated lint fixes were made.

## Limits and handoff

These tests demonstrate storage behavior. They do not establish transaction recovery,
a hard memory cap, or a performance improvement. Transaction integration and production
source/operator wiring remain deferred. The eventual substitution target is storage of
the persistent trace from `dyn_accumulate_integrate_trace` and its associated
`accumulate_delay_trace` reads. Leave `dyn_accumulate` and its delta-accumulation and
delayed-read path unchanged.
