# Phase 1 implementation plan: index-owned folding and one layer-file batch

Date: 2026-09-29. Status: ready for implementation; no Phase 1 code is claimed.

## Boundary and source of truth

This plan covers [Phase 1](folded-key-layer-file-plan.md#phase-1-encoding-and-one-readable-layer-file-batch)
of the Step 2 storage plan. Its exit condition is an index-owned definition of sources, key
fields, and domains; checked folding, unfolding, and payload serialization; and a small
two-column layer file whose parent-key seek and child-row reads return the expected
signed contributions. The Step 2 contract governs over historical proposals in `history/`.
The [architecture guide](README.md#what-changes-in-our-design) and
[storage evidence](support.md#folded-keys-in-feldera-layer-files) explain why the file has one
folded key per outer row and weighted payloads in its inner row group.

Phase 1 does not publish a `Batch`/`Trace` implementation. Memory-backed batches, bounded
staging and spill, multi-batch cursors, signed compaction, accumulated-K validation, snapshots,
and runtime integration have their own later phases. A fixture's write/read success is evidence
for the file mapping, not for those behaviors.

## Merged-index constitution

The [constitution](folded-key-layer-file-plan.md#merged-index-constitution) answers three
questions for this particular merged index:

1. **Sources:** Customer, Orders, and extended Lineitem.
2. **Key fields:** Customer uses `(customer_id)`; Orders uses `(customer_id, order_id)`;
   extended Lineitem uses `(customer_id, order_id, line_id)`, in that order.
3. **Shared domains:** All three `customer_id` fields use the `customer` tag. The two
   `order_id` fields use the `order` tag. The line field uses the `line` tag.

The notation `source S id N key (field: domain, ...)` expresses those choices in one place.
For Phase 1, represent it as a small static Rust definition, with a source roster, ordered
field/domain entries, and domain tags. Implement the shared behavior in a generic base now;
no parser or macro is needed.

### Folding from the constitution

> **Ownership:** Rust uses composition and generic types instead of class inheritance. Make
> `MergedIndex<D>` the reusable base. `D: MergedIndexDefinition` supplies the constitution and
> typed source-key projection. `CustomerOrdersLineitemIndex` wraps
> `MergedIndex<CustomerOrdersLineitemDefinition>`. Source row types have no global fold method.

```text
CustomerOrdersLineitemDefinition.project(source_key):
    match source_key:
        Customer(c)       -> Customer, [c]
        Orders(c, o)      -> Orders, [c, o]
        Lineitem(c, o, l) -> ExtendedLineitem, [c, o, l]

MergedIndex<D>.fold(source_key):
    source, values = D.project(source_key)
    spec = D.constitution.sources[source]
    require len(values) == len(spec.key_fields)
    K = []
    for (field, value) in zip(spec.key_fields, values):
        K.append(D.constitution.domains[field.domain])
        K.extend(fold_i32(value))
    K.append(INDEX_TAG)  # record-layout marker, not a source-field domain
    K.append(spec.id)
    return K

MergedIndex<D>.unfold(K):
    require K has an INDEX tag and a recognized terminal source identifier
    select spec from D.constitution using that source identifier
    require exact length and each domain tag in spec.key_fields' order
    decode each i32; call D.construct_key(source, values)

CustomerOrdersLineitemIndex.fold(source_key):
    return self.base.fold(source_key)
```

The definition extracts and reconstructs typed keys. The generic base uses its ordered fields,
domain tags, and source identifier to fold, validate, unfold, and construct prefixes. A second
merged index can reuse that base with a different `D`, including different folding for a source
it shares with this index. Prefixes use leading field/domain entries without appending `INDEX`
and the source identifier.

> **Visibility:** “Crate-internal” means visible to code inside the `dbsp` Rust crate, using
> `pub(crate)` where needed, but absent from the public API used by other crates.

### Feldera's schema precedent

Feldera's SQL compiler represents a source row as a named-column struct and emits the Rust
struct with serialization support for its Rust backend
([source operator](../../../sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/circuit/operator/DBSPSourceTableOperator.java#L22),
[Rust emitter](../../../sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/compiler/backend/rust/ToRustVisitor.java#L675)).
The pipeline manager requests schema JSON and, for the non-Gen-2 runtime path, generated Rust;
Gen-2 requests circuit IR instead of Rust source
([compiler invocation](../../../crates/pipeline-manager/src/compiler/sql_compiler.rs#L814)).
That generated table schema does not define shared domains or a merged index's source-key
paths. Handwrite this index's static definition in Phase 1, in a form a later generator could
emit, without changing SQL schema generation.

## Stored representation

```text
one stored contribution = (folded K, payload, signed weight)
layer-file column 0: folded K
layer-file column 1: payload -> signed weight
```

1. Define a `FoldedKey` whose ordering compares the raw bytes lexicographically.
   Implement checked folding and unfolding in `MergedIndex<D>`, exercised through
   `CustomerOrdersLineitemIndex` for `Customer(c)`, `Orders(c, o)`, and `Lineitem(c, o, l)`.
   Use domain tags `0x00` through `0x03`
   and identifiers `0x01` through `0x03` as specified in the
   [Step 2 encoding](folded-key-layer-file-plan.md#k-the-papers-folded-key).
   Encode each signed `i32` as big-endian `((x as u32) ^ 0x8000_0000)`. The only valid complete
   lengths are 7, 12, and 17 bytes. Keep explicit prefix constructors for customer, order,
   and line-only scans; a prefix is not necessarily a complete K.
2. Define separate typed Customer, Orders, and Lineitem payload structures with precisely the
   fields and units in the [payload contract](folded-key-layer-file-plan.md#payload-signed-weight-and-replacement).
   Encode their fields with Feldera's existing serializer. Before reading a payload, validate
   K's length, domain tags, and final source identifier. An identifier of `0x01`, `0x02`, or
   `0x03` then selects the Customer, Orders, or Lineitem payload structure. No second source-kind
   field or file-format field belongs in each record: K identifies the source, and the layer
   file handles its format. Report corrupt payload bytes as an error.
3. Map `Writer2` as column 0 `(FoldedKey, ())` and column 1 `(PayloadBytes, ZWeight)`. The
   [writer contract](../../../crates/dbsp/src/storage/file/writer.rs#L1564) requires at least
   one child per parent, strictly increasing parent keys, and strictly increasing payload keys
   within each parent. Write every child with `write1` before its parent with `write0`. Each
   child row stores one payload and its signed weight; there is no packed payload list. Payload
   byte order serves only file merging and equality, not query ordering.

The implementation should first verify that `FoldedKey` and `PayloadBytes` wrappers satisfy
Feldera's `DBData`/factory requirements. If a wrapper adds trait machinery without protecting an
invariant, use `Vec<u8>` inside the file fixture and keep checked constructors at its boundary.
Do not alter the generic layer-file format for this phase.

## Implementation steps

1. Add `MergedIndex<D>` and a small `MergedIndexDefinition` trait under
   `crates/dbsp/src/trace/merged_index/`, exposed only within the crate. Register the module
   in `trace.rs`. The base implements checked fold, unfold, and prefix construction using D's
   source roster, ordered key fields, and domain tags. Add
   `CustomerOrdersLineitemDefinition` for this constitution, its typed source-key projection,
   and payload schemas; make `CustomerOrdersLineitemIndex` wrap the generic base. Put the
   two-column weighted-row mapping used by the Phase 1 fixture in the generic base, with Q3
   supplying only its folded keys and payloads. Leave full `Batch` integration for Phase 2.
2. Add focused tests beside that module. Check that each declared source emits tags in its
   stated field order, that shared customer and order domains use identical tags across sources,
   and that the source identifier terminates K. Assert exact bytes for all three record types,
   including negative, zero, and positive key components; round-trip the `i32::MIN` and `i32::MAX`
   boundaries; and assert parent-before-child and signed numeric order by byte comparison.
   Reject truncated/extra keys, wrong tag positions, and wrong terminal identifiers. Verify
   malformed payload bytes are rejected. Assert repeat encoding of the same payload yields
   identical bytes. Call fold, unfold, and prefix methods through
   `CustomerOrdersLineitemIndex`. Add a tiny second definition in tests to verify the generic
   base accepts a different domain mapping for a shared source without changing base code.
3. Add one storage-backed fixture test, following the existing
   [two-column test](../../../crates/dbsp/src/storage/file/test.rs#L956) and
   [`FileIndexedWSetBuilder`](../../../crates/dbsp/src/trace/ord/file/indexed_wset_batch.rs#L880).
   Exercise the base's two-column mapping with a temporary backend and the existing
   `Writer2`/`Reader` factories. Include a Customer,
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
5. Record the final encoding and serialization choices, layer-file mapping, test commands and
   results, and any API surprises in a short Phase 1 implementation note. Update the main
   Step 2 plan's phase status only after its exit evidence passes; carry unresolved batch/cursor
   choices into Phase 2.

## Verification gate

Run `cargo fmt --check`, targeted `dbsp` key/payload and fixture tests, and the relevant existing
two-column storage tests. A successful gate demonstrates exact folded bytes, checked key and
payload round trips, deterministic payload equality, and indexed retrieval of the fixture's
weighted child rows. It does not claim signed consolidation or accumulated-state correctness;
those require the later phase's independent signed-weight oracle.

Before committing implementation, inspect affected parent and child documentation, update any
changed contracts, and follow the repository's specific-file staging and remote-sync rules.
