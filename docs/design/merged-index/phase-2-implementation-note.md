# Phase 2 implementation note: in-memory batches and raw cursors

Date: 2026-09-29.

## Implementation

`CustomerOrdersLineitemIndex::build_batch` folds keys and serializes payloads using the
Phase 1 codecs. It checks that each payload belongs to its source key, sorts arbitrary-order
input by `(K, payload)`, and adds signed weights for equal pairs within one batch. Zero-weight
rows disappear; weight overflow returns an error. The resulting batch is Feldera's existing
`VecIndexedWSet<DynData, DynData, DynZWeight>` with `Vec<u8>` keys and values. It implements
Feldera's full `Batch` interface without a new storage format or trait implementation.

`RawBatchCursor` holds one cursor per in-memory batch. `seek_ge` positions each at the first
eligible folded key; `next` chooses the next stored `(K, payload)` row in key order, breaking
ties by batch position. `row` lends the folded key and payload bytes until cursor advancement
and returns the signed weight. Equal pairs in different batches remain separate raw
contributions. The cursor does not compute accumulated state or validate K uniqueness.

## Verification

Commit `a2fa68aea` passed `cargo check -p dbsp --lib`, focused
`cargo test -p dbsp --lib trace::merged_index` (11/11 tests), and `cargo fmt --check`.
Tests cover unordered input, within-batch addition and cancellation, empty batches,
overflow and source/payload mismatch, multi-batch signed contributions, exact and missing-key
seeks, and prefix stopping. Existing Phase 1 layer-file tests remain in the focused suite.

## Handoff

This phase assumes small input batches below 100 MiB. It does not enforce a memory cap or
bound peak sorting memory. Phase 3 adds cross-batch signed consolidation and accumulated-K
uniqueness. Phase 4 adds file-backed batches, bounded staging, spine append, snapshots, and
spill/reopen evidence. The raw cursor currently selects among batch positions by scanning
their current rows; a later spine integration can use its own merge cursor if batch count
makes this selection cost significant.
