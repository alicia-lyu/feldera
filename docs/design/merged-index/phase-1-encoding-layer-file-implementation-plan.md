# Phase 1 implementation plan: folded codecs and one layer-file batch

Date: 2026-09-29. Status: ready for implementation; no Phase 1 code is claimed.

## Boundary and source of truth

This plan covers [Phase 1](folded-key-layer-file-plan.md#phase-1-encoding-and-one-readable-layer-file-batch)
of the Step 2 storage plan. Its exit condition is a checked codec for each folded key and payload,
plus a small two-column layer file whose parent-key seek and child-row reads return the expected
signed contributions. The Step 2 contract governs over historical proposals in `history/`.
The [architecture guide](README.md#what-changes-in-our-design) and
[storage evidence](support.md#folded-keys-in-feldera-layer-files) explain why the file has one
folded key per outer row and weighted payloads in its inner row group.

Phase 1 does not publish a `Batch`/`Trace` implementation. Memory-backed batches, bounded
staging and spill, multi-batch cursors, signed compaction, accumulated-K validation, snapshots,
and runtime integration have their own later phases. A fixture's write/read success is evidence
for the file mapping, not for those behaviors.

## Confirmed representation

1. Define a crate-internal `FoldedKey` whose ordering compares the raw bytes lexicographically.
   Implement checked folding and unfolding for `Customer(c)`, `Orders(c, o)`, and
   `Lineitem(c, o, l)`. Use domain tags `0x00` through `0x03` and identifiers `0x01` through
   `0x03` as specified in the [Step 2 encoding](folded-key-layer-file-plan.md#k-the-papers-folded-key).
   Encode each signed `i32` as big-endian `((x as u32) ^ 0x8000_0000)`. The only valid complete
   lengths are 7, 12, and 17 bytes. Keep explicit prefix constructors for customer, order,
   and line-only scans; a prefix is not necessarily a complete K.
2. Define separate typed Customer, Orders, and Lineitem payload structures with precisely the
   fields and units in the [payload contract](folded-key-layer-file-plan.md#payload-signed-weight-and-replacement).
   Encode each with Feldera's existing `rkyv` serialization into `PayloadBytes`; select the
   decoder from the *validated* K identifier. Do not add a kind, format field, weight, or payload
   list to those bytes. Document the codec's version assumption before any persisted-format
   compatibility claim. Use checked decoding, including any required archive alignment, so
   malformed bytes return an error rather than panic or unchecked archive access. Because V
   has no discriminator, the codec cannot promise to detect every cross-schema payload mix-up;
   callers must use the schema selected by K.
3. Map `Writer2` as column 0 `(FoldedKey, ())` and column 1 `(PayloadBytes, ZWeight)`. The
   [writer contract](../../../crates/dbsp/src/storage/file/writer.rs#L1564) requires at least
   one child per parent, strictly increasing parent keys, and strictly increasing payload keys
   within each parent. Write every child with `write1` before its parent with `write0`. Keep
   weight in child auxiliary data. Byte ordering of the payload is only the file's internal
   equality and merge order; it has no query ordering meaning.

The implementation should first verify that `FoldedKey` and `PayloadBytes` wrappers satisfy
Feldera's `DBData`/factory requirements. If a wrapper adds trait machinery without protecting an
invariant, use `Vec<u8>` inside the file fixture and keep checked constructors at its boundary.
Do not alter the generic layer-file format for this phase.

## Implementation steps

1. Add the small codec module under `crates/dbsp/src/trace/merged_index/` and expose it only
   within the crate. Register the module in `trace.rs`. Keep domain tags, lengths, identifiers,
   payload schemas, and decode errors together so later batch code has one source of truth.
2. Add focused codec tests beside that module. Assert exact bytes for all three record types,
   including negative, zero, and positive key components; round-trip the `i32::MIN` and `i32::MAX`
   boundaries; and assert parent-before-child and signed numeric order by byte comparison.
   Reject truncated/extra keys, wrong tag positions, and wrong terminal identifiers. Verify
   malformed payload bytes are rejected. Assert repeat encoding of the same payload yields
   identical bytes.
3. Add one storage-backed fixture test, following the existing
   [two-column test](../../../crates/dbsp/src/storage/file/test.rs#L956) and
   [`FileIndexedWSetBuilder`](../../../crates/dbsp/src/trace/ord/file/indexed_wset_batch.rs#L880).
   Use a temporary backend and the existing `Writer2`/`Reader` factories. Include a Customer,
   at least two Orders under it, and Lineitems under one Order; include a different Customer to
   prove prefix stopping. Give one K two distinct payload children with opposite signed weights,
   sorted by `PayloadBytes`, to prove that K is written once and weights stay with their payloads.
   Use fixture data already ordered by `(K, PayloadBytes)`; avoid introducing a general batch
   sorter or consolidation path.
4. Finish the writer into a reader, seek the outer row group with
   `advance_to_value_or_larger`, then traverse the selected parent's `next_column()` group.
   Decode K and each payload, inspect its signed auxiliary weight, and assert exact expected
   rows. Test a present key, a missing key between present keys, a prefix scan that stops at
   the next Customer, and a seek past the last key. The reader API is unsafe around `rkyv`
   deserialization; keep any unsafe call in a small helper with its file-origin invariant
   stated locally. Do not add a public raw cursor in this phase.
5. Record the final codec choice, layer-file mapping, test command/results, and any API
   surprises in a short Phase 1 implementation note. Update the main Step 2 plan's phase status
   only after its exit evidence passes; carry unresolved batch/cursor choices into Phase 2.

## Verification gate

Run `cargo fmt --check`, targeted `dbsp` codec and fixture tests, and the relevant existing
two-column storage tests. A successful gate demonstrates exact folded bytes, checked key and
payload round trips, deterministic payload equality, and indexed retrieval of the fixture's
weighted child rows. It does not claim signed consolidation or accumulated-state correctness;
those require the later phase's independent signed-weight oracle.

Before committing implementation, inspect affected parent and child documentation, update any
changed contracts, and follow the repository's specific-file staging and remote-sync rules.
