# Native Customer–Orders–Lineitem merged index

This standalone library supplies a concrete three-source definition for DBSP's
general merged-index implementation. Its folded keys place Customer first,
followed by Orders and Lineitem rows for each customer and order. Source
identities use `i64` customer and order IDs and an `i32` line ID. The complete
folded keys occupy 11, 20, and 25 bytes, respectively.

The payloads use SQL runtime values: `SqlString` for customer segment, `Date`
and `i32` for order date and ship priority, and `Date`,
`SqlDecimal<15, 2>`, and `SqlDecimal<4, 2>` for line ship date, extended price,
and discount. Payloads are serialized as checked `rkyv` bytes. The library
exposes `CustomerOrdersLineitemIndex`, its definition and source types, and
typed fold, prefix, batch, and payload codec operations. Batches remain
byte-backed and support signed weights.

From this directory, run the full library suite with:

```sh
cargo +1.96.1 test --offline
```

The tests cover encoding, payload validation, batch consolidation, cursor
seeks, layer files, native Spine compaction, snapshots, and reopened files.
Prototype files created with the prior primitive payload codec must be
regenerated; this experiment does not migrate persisted files. Q3 can consume
this library during a later join integration.
