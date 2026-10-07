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
cargo +1.96.1 test --offline --locked
```

The tests cover encoding, payload validation, batch consolidation, cursor
seeks, layer files, native Spine compaction, snapshots, and reopened files.
Prototype files created with the prior primitive payload codec must be
regenerated; this experiment does not migrate persisted files.

## Q3 RF1/RF2 readers

The `q3` module reconstructs the four native accumulated reads from externally
maintained COL snapshots. `Q3State::publish` supplies one transaction's before/after
views and its temporary placement map of order keys to customer sets. The harness supplies
singleton sets obtained from complete Orders delta records. The
[separate placement correction](../../../docs/design/merged-index/placement-lookup-multimap-plan.md)
documents multiple-candidate reader checks; it does not extend the harness workload.
Finish Q3 maintenance before calling `Q3State::finish` or publishing
another transaction. Q3 never inserts into the secondary index.

The supported harness loads Customers first and then complete order/line groups.
Subsequent RF1/RF2 insert or delete complete groups under referential integrity.
General replacements, key moves, orphan lines, and Customer updates are deferred.
The [Q3 experiment](../q3-ivm-baseline/README.md) documents generation and execution.

`joined_orders_after` reconstructs `B⁺` on the fly for distinct order keys in
`ΔL`: it seeks the Customer and Order at each supplied candidate in the after
snapshot, combines their payloads, and multiplies their signed weights. Placement comes from
the current Orders delta. No accumulated `B` relation is stored.

Readers copy only requested accumulated rows into transient native indexed batches.
`SpineSnapshot` shares references to the original source batches, and native
sorting/consolidated cursors preserve the generated key/value order and signed
weights. This temporary row copying and possible sorting is suboptimal and included
in Q3 maintenance time. Direct reconstructed cursors are a future optimization;
neither a new cursor framework nor a hard memory bound is claimed here. The
transaction-local placement map uses `BTreeMap` and `BTreeSet`; container selection
is another future optimization, preserving the placement and publication contract.

The Q3 experiment's `verify.sh` runs both generated circuit modes with identical
loading and RF1/RF2 fixtures. It compares requested results from these four readers
with independently projected logical source relations, then compares emitted
deltas and materialized `V` with full joins and canonical transcripts across modes.
The COL weighted-reader, storage, and compaction tests remain in this library's
suite. Broader Customer updates and independent line changes remain deferred.

An upstream maintenance circuit and coordination of its checkpoint with Q3 remain
future work. The prototype manually maintains its index before logical Q3 deltas.
