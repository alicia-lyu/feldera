# Merged-index library (`crates/dbsp/src/trace/merged_index/`)

The schema-independent part of the merged index: everything a second index (MI_P) reuses unchanged.
A merged index stores several relations in one sorted run, under folded keys; an index-specific
crate (Q3's is `sql-to-dbsp-compiler/experiments/merged-index-col/`) supplies the trie, the payload
types, and any join hook. Design: [general-definition.md](../../../../../docs/design/merged-index/general-definition.md).
Batch layout and reads: [storage-layout.md](../../../../../docs/design/merged-index/storage-layout.md).
Algorithm of the scan and the readers: [shared-traversal.md](../../../../../docs/design/merged-index/shared-traversal.md).
Rules for any index and harness: the root `CLAUDE.md` and `sql-to-dbsp-compiler/experiments/CLAUDE.md`.

## Map

| Piece | File and symbols |
| --- | --- |
| Key trie | `trie.rs`: `KeyTrie`, `Node`, `Placement`, `Domain`, `PlacedIndex` |
| Order-preserving key encoders | `ordered.rs` (`OrderedKey`), `mod.rs` (`KeyEncoder`, `KeyDecoder`) |
| Packed payload encoders | `packed.rs`: `PackedField`, `PayloadEncoder`, `PayloadDecoder` |
| Folded keys, definition trait | `mod.rs`: `FoldedKey`, `MergedIndexDefinition`, `prefix_successor` |
| Merged-index batches | `batch.rs` (schema-independent; layout in storage-layout.md) |
| Merging cursor | `run_merge.rs`: `RunMerge` |
| Scan session | `session.rs` (tests in `session/`): `ScanSession`, `Pass`, `SessionCounts` |
| Generic readers | `reader.rs` (tests in `reader/`) |
| A query's transaction state | `transaction.rs`: `TransactionViews`, `Published`, `SessionReads`, `measure_collection`, `consolidate`, `multiply_weights` |
| Test helpers | `testing.rs` (`check_against_trie` ties the generated fold constants to the trie) |

## The key trie

`KeyTrie` is a static Rust value that mirrors the spec and stores nothing derivable: no indices,
parent links, tags, ids, or paths. Each query walks it in pre-order.

```rust
pub struct KeyTrie {
    pub name: &'static str,
    pub domains: &'static [Domain],   // column membership and the encoded byte width
    pub children: &'static [Node],    // the root's children; the root is implicit
}

pub struct Domain {
    pub name: &'static str,
    pub members: &'static [ColumnRef], // the (relation, column) members
    pub width: usize,                  // byte width of the encoded value (OrderedKey::WIDTH)
}

pub struct Node {
    pub domain: &'static str,         // the edge into this node, by domain name
    pub placements: &'static [Placement],
    pub children: &'static [Node],
}

pub struct Placement {
    pub relation: &'static str,
    pub via: &'static [Via],          // Via { domain, relation }
    pub payload: Payload,             // All, or Columns(names); metadata only
}
```

- `placements()` returns each placement with its id (visit position + 1) and path of domain names;
  `domain_tag(name)` is the position of the domain's first appearance, + 1.
- `root_width()` is the byte width of the one root domain. A key's group is its first
  `1 + root_width()` bytes (tag and value). `prefix_width(stream_id, depth)` gives a request
  prefix width: depth 1 is the group.
- A trie has exactly one root domain. Two root domains mean two key tries, each stored as its own
  merged index. Several streams may reside at the root node.
- The Java `MergedIndexValidator` enforces this and is the only trie check (decision of 2026-11-10).
  `root_width`, `placements`, `key_columns`, and `capture` panic on a hand-built trie that breaks it.

## Keys, payloads, folding

- A folded key is `[tag value]* 0 id` in one `Vec<u8>` (`FoldedKey`); its byte order is the query
  order. Each domain's value is encoded by an order-preserving `OrderedKey` encoder of fixed width.
- A payload is packed bytes (`PackedField`, `PayloadEncoder`, `PayloadDecoder`); the bytes do not
  order like the typed values (little-endian integers, length-prefixed strings). The stored value is
  opaque and readable only with its key: the id (last key byte) selects the payload type.
- `MergedIndexDefinition` (implemented by the generated module) supplies `TRIE`, `fold`, `unfold`,
  and `decode_payload(stream_id, bytes)`. Folded keys are unique because of the base tables'
  primary keys.
- `prefix_successor(prefix)` is the smallest byte string above every key with that prefix (none for
  an all-`0xff` prefix); a group read seeks to it.

## `RunMerge`

A merging cursor over the runs of one index snapshot. `step` and `seek` read no value or weight;
`key` is the merged key, already read. `take` sums weights per value and counts the values that
cancelled. Untouched positions therefore cost a key decode and nothing else.

`seek_group(prefix)` (with `RunMerge::with_group_prefix`) asks each run behind the group whether it
can hold it (`Batch::maybe_contains_prefix`) and parks those that cannot; `key` then ends at the
group's end. An index's runs get the prefix filter through `batch::index_factories(&TRIE)`, whose
spec is `KeyTrie::root_prefix_spec()`; use the same factories for the spine and for
`batch::build_batch_in`. `ScanSession` seeks every group with `seek_group`, probing the prefix of
the snapshot's `filter_spec`; a spine without one is sought run by run, as before.

## `ScanSession<D: MergedIndexDefinition>`

One session per transaction: a cursor over the touched groups of `Old`, schema-independent and bound
to its definition (`D::TRIE`, `D::decode_payload`). It buffers nothing and calls nothing back. It
never sees `Dlt`: `ScanSession::new(old, touched, pass)` takes the sorted touched group prefixes
that `reader::capture` built.

| Operation | Effect |
| --- | --- |
| `next_group` | Positions at the next touched group, once per group |
| `key` | The merged key if it starts with the group, else nothing; no value or weight read |
| `take_and_decode` | Sums weights and decodes the payload; the caller calls it only for a kept entry |
| `step` | Next entry; the range is scanned contiguously, with no skips |

- `Pass::{Seek, Step}` is the one knob: pass the untouched groups between two touched ones by one
  logical seek, or by stepping through their entries. A group read has no exact-key probe.
- `SessionCounts`: `groups`, `cursor_opens`, `logical_seeks`, `logical_steps`, `positions`,
  `payload_decodes_old` (kept entries), `payload_decodes_dlt` (filled in by `capture`), `cancelled`.

## Generic readers (`reader.rs`)

A reader is one partner read of one join: its `Requests` (sorted folded-key prefixes of its driving
delta's keys, with a forward pointer) and a partner batch holding the requested slice of the index.

| Symbol | Role |
| --- | --- |
| `ReaderDefinition` | Static plan: `PARTNER_STREAM` (or none), `REQUEST_DEPTH`, `DRIVING_DELTA`, `unfold`, `unfold_delta` |
| `DrivingDelta::{Stream(id), Join}` | `Dlt` rows of one stream, or the output of a join evaluated at routing |
| `Delta<R>` | The driving delta; owned by the caller of `capture`; `into_batch` moves it out, `rows_at` borrows it |
| `capture` | Reads `Dlt` once: touched groups, driving deltas, requests, one decode per entry |
| `route` / `Position` | Offers each `Old` position to the stored readers by folded key alone; reads, decodes, and unfolds a kept entry once |
| `RowBuilder` | Pushes rows straight into DBSP's builder an entry at a time (at most three rows of one folded key; reorders and consolidates within an entry, errors on entries out of order; no general sort; user decision of 2026-11-12) |
| `Reader::into_sealed`, `SealedReader` | The finished requests and partner batch, `Sync`, so joins read with no lock |

A partner read errors on a delta key outside its reader's requests.

Reused as is: `ReaderDefinition`, `Requests`, `capture`, `route`, `RowBuilder`, sealing. An
index-specific hook is needed wherever rows are combined rather than copied: the generic `route`
copies entries and has no notion of a join or of the state before or after the transaction. Q3's
hook (`Routing::hook` in `merged-index-col/src/q3.rs`) computes `J` (the customer after times the
order after, and `ΔJ`) and steps the orders only `ΔO` mentions. MI_P needs its own hook until the
planned replacement: [deferred.md](../../../../../docs/design/merged-index/deferred.md#generic-readers-before-and-after-state-and-declared-join-readers)
("Generic readers") moves the partner's state into `ReaderDefinition` so join readers are declared.

## Test filters

From the repository root, with `cargo +1.96.1 test --offline -p dbsp --lib <filter>`:

- `merged_index` (everything here), `merged_index::run_merge`, `merged_index::session`,
  `merged_index::reader`.
- Index-specific tests (readers on the worked transaction, A/B of the passes) live in
  `merged-index-col/src/q3/tests.rs`.
