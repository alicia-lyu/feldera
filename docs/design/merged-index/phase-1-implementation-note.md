# Phase 1 implementation note: folded keys and layer-file rows

Date: 2026-09-29.

## Encoding and ownership

`crates/dbsp/src/trace/merged_index/` contains the crate-internal
`MergedIndex<D>` base. Static source-index descriptors supply logical base-relation
IDs, ordered key fields and key-domain IDs, and payload field IDs. A
`MergedIndexDefinition` selects those source indexes, assigns domain and source
byte tags, and supplies typed key projection and reconstruction.
`CustomerOrdersLineitemIndex` wraps the base for the Customer, Orders, and extended
Lineitem constitution. Source row types do not own a global folding operation.
`CustomerPrimary`, `OrdersByCustomer`, and `LineitemByCustomer` name the three
source-index descriptors. Their base-relation IDs are symbolic; generated row
types and field-provenance checks belong to later code generation. The same
source-index descriptor can receive different byte tags in another merged index.

Each signed `i32` key field is encoded as big-endian bytes after flipping its sign
bit. The ordered domain tags are `0x01` for customer, `0x02` for order, and `0x03`
for line; `0x00` marks the terminal index identifier. Complete keys have lengths
7, 12, or 17 bytes. Unfolding validates the length, each domain tag, and the
terminal source identifier before reconstructing a typed key. Prefixes omit the
terminal index identifier and can end before a complete key.
The code-generation example's `i64` keys and index tag `255` are illustrative.
This implementation retains its `i32` keys and terminal index tag `0x00`.

The three payload types match the [Step 2 payload contract](folded-key-layer-file-plan.md#payload-signed-weight-and-replacement).
They use Feldera's `rkyv` serializer. Decoding validates the folded key before
selecting its payload type and checks the archived payload bytes before
deserialization. Serialized payload bytes are compared for file equality and
merging; their byte order is not a query-visible value order.

## File mapping

The generic base writes one parent folded key per outer row with `()` auxiliary
data. It writes each `(payload bytes, signed ZWeight)` child to the inner row
group before writing the parent. The fixture supplies already sorted, distinct
payload bytes within each parent, as required by `Writer2`. It does not add a
sorter, consolidation, `Batch`, or `Trace` implementation.

## Verification

On 2026-09-29, `cargo fmt --check` passed. After the source-index descriptor
refactor, the focused merged-index tests passed (6/6), covering exact key bytes,
malformed keys, signed ordering and boundaries, payload round trips and
corruption, source-index metadata, the generic base with a second definition,
and seeks and child-row reads in a temporary layer file. The existing two-column
storage tests also passed (8/8). The fixture checks a present key, a missing key
between records, a customer prefix that stops at the next customer, and a seek
past the last key. One parent has two distinct payload children with opposite
signed weights.

The test commands used the installed Rust 1.96.1 binaries directly because the
default toolchain was Rust 1.78.0:

```sh
RUSTC=/Users/alicialyu/.rustup/toolchains/1.96.1-aarch64-apple-darwin/bin/rustc \
  /Users/alicialyu/.rustup/toolchains/1.96.1-aarch64-apple-darwin/bin/cargo test \
  -p dbsp trace::merged_index --lib --offline
RUSTC=/Users/alicialyu/.rustup/toolchains/1.96.1-aarch64-apple-darwin/bin/rustc \
  /Users/alicialyu/.rustup/toolchains/1.96.1-aarch64-apple-darwin/bin/cargo test \
  -p dbsp storage::file::test::two_columns --lib --offline
RUSTC=/Users/alicialyu/.rustup/toolchains/1.96.1-aarch64-apple-darwin/bin/rustc \
  RUSTFMT=/Users/alicialyu/.rustup/toolchains/1.96.1-aarch64-apple-darwin/bin/rustfmt \
  /Users/alicialyu/.rustup/toolchains/1.96.1-aarch64-apple-darwin/bin/cargo fmt --check
```

The fixture uses `Vec<u8>` as the file key type at both levels. Its lexicographic
ordering matches `FoldedKey` and `PayloadBytes` without adding dynamic-data
trait machinery to the wrappers. The reader's unsafe `rkyv` calls are grouped
in a helper whose input comes from the file written by the fixture.
The `rkyv` serializer's error needs debug formatting rather than `Display`.

## Phase 2 handoff

The subsequent [Phase 2 plan](phase-2-in-memory-batches-implementation-plan.md) narrows the next
implementation increment to in-memory batches and raw reads across batches. File-backed batch
integration, bounded staging, and spill move to Phase 4. Phase 2 must preserve the crate-internal
key and payload representation. Signed consolidation, accumulated-key uniqueness, snapshots,
and runtime wiring remain separate later phases.
