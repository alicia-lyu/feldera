# COL storage specification

This file specifies how the Customer–Orders–Lineitem (COL) merged index stores records: the folded-key encoding, the definition and source types, the batch and file layout, and why folded keys are unique. It describes the code as it stands. The generic machinery is the public module `trace::merged_index` in `crates/dbsp/src/trace/merged_index/`. The concrete COL definition and its payload codecs are in `sql-to-dbsp-compiler/experiments/merged-index-col/src/`.

The merged index is secondary storage, kept beside Feldera's own base-table state ([decisions.md](decisions.md)). Reading it for the maintained view `V` is described in [reconstruction.md](reconstruction.md). The trace and cursor interfaces it reuses are in [support.md](support.md).

## Vocabulary

- **Trace.** Feldera's name for an indexed collection of accumulated weighted updates. It implies no transaction log and no stored timepoints.
- **Transaction delta.** The signed change set of one transaction.
- **Run.** An immutable sorted collection of weighted records (a batch). A run is in memory or in a layer file.
- **Layer file.** Feldera's immutable file format for nested sorted groups. An indexed batch has two levels: the *key level* holds each distinct key once, and the *value level* holds the values grouped under their key.
- **Spine.** The collection of runs that merges them in the background.

## Folded keys

A folded key (K) is a byte string that sorts a customer before its orders, and an order before its lines. The storage layer compares K as opaque lexicographic bytes.

```text
domain tags:    INDEX = 0x00, custkey = 0x01, orderkey = 0x02, linenumber = 0x03
placement ids:  CustomerByCustkey = 0x01, OrdersByCustkeyOrderkey = 0x02, LineitemByCustkeyOrderkeyLinenumber = 0x03

fold_iN(x) = big_endian_N_bits((x as unsigned_N_bits) XOR sign_bit_N)

C(c):     0x01 fold_i64(c) | INDEX | 0x01                                  (11 bytes)
O(c,o):   0x01 fold_i64(c) | 0x02 fold_i64(o) | INDEX | 0x02               (20 bytes)
L(c,o,l): 0x01 fold_i64(c) | 0x02 fold_i64(o) | 0x03 fold_i32(l) | INDEX | 0x03   (25 bytes)
```

Rules:

- **Field widths.** Customer and order IDs are `i64`; line numbers are `i32`. The sign-bit flip preserves signed order within a domain.
- **Terminal tag.** The key ends with `INDEX` and the placement id. `INDEX` is not an extra end marker: its value sorts below every child-domain tag, so a customer precedes its orders.
- **Nothing else in K.** Payload, weight, and occurrence counters never extend K.
- **Typed fields.** Key fields keep their Rust types. A line number is an `i32`, so no range check is needed and `fold` cannot fail.
- **Validation.** `unfold` rejects wrong lengths, wrong domain tags, a missing `INDEX` tag, trailing bytes, and unknown placement ids. `KeyDecoder` implements the checks that do not depend on the definition.
- **Prefixes.** `KeyEncoder::prefix` returns the leading fields without `INDEX` or the placement id. A customer prefix ends after `c`; an order prefix ends after `o`; a line prefix also includes the line domain tag, through `KeyEncoder::then_tag`.
- **Scans.** A prefix scan seeks the prefix and steps until the key no longer starts with it. No successor function is needed.

Fixture tests: `mixed_width_folding_writes_tags_values_terminator_and_id`, `signed_boundaries_round_trip_and_sort`, `malformed_keys_are_rejected_without_panics`, `decoder_reports_each_kind_of_malformed_key`, and the `validate_rejects_*` tests in `crates/dbsp/src/trace/merged_index/tests.rs`.

## Definition and source types

The definition is a static key trie plus typed folding. It separates three things:

- **Domains** name a logical identity (`custkey`, `orderkey`, `linenumber`) and list the columns that join on equal values. Shared domain identity, not field-name similarity, establishes shared ordering. The key field types are Rust types (`i64`, `i32`) with an `OrderedKey` encoding.
- **Placements** put an index on one relation at a trie node. The path from the root to the node lists the key's domains. A placement may take a key value from another relation (`custkey via Orders`).
- **Byte tags and placement ids** are derived from the trie by a pre-order walk: tags are the first appearance of a domain, and ids are the visit order of placements. Another merged index can assign different tags to the same relation.

In Rust, `MergedIndexDefinition` supplies `type Key`, `const TRIE: KeyTrie`, and typed `fold` and `unfold`. The design of the trie is in [general-definition.md](general-definition.md). Source row types have no global folding operation.

COL fills in the definition as `CustomerOrdersLineitemDefinition`, with `SourceKey` variants `Customer(i64)`, `Orders(i64, i64)`, and `Lineitem(i64, i64, i32)`:

| Placement (id) | Key fields | Payload |
| --- | --- | --- |
| `CustomerByCustkey` (1) | custkey | `CustomerPayload { segment: SqlString }` |
| `OrdersByCustkeyOrderkey` (2) | custkey, orderkey | `OrdersPayload { order_date: Date, ship_priority: i32 }` |
| `LineitemByCustkeyOrderkeyLinenumber` (3) | custkey, orderkey, linenumber | `LineitemPayload { ship_date: Date, extended_price: SqlDecimal<15, 2>, discount: SqlDecimal<4, 2> }` |

`CustomerOrdersLineitemIndex` delegates `fold` and `unfold` to the definition. It adds prefix helpers (`prefix_custkey`, `prefix_custkey_orderkey`, `prefix_custkey_orderkey_then_linenumber`) and `build_batch`. The definition's tags and ids are constants. The tests tie them to the trie with `check_against_trie`.

Payload rules:

- The stored value is opaque bytes (`Vec<u8>`): the packed encoding of one payload, with no tag. The key's placement id names the source, so a value is readable only together with its key. `decode_payload(&SourceKey, &[u8])` dispatches on the key's variant. Decoding accepts only canonical bytes and returns an error for a truncated, over-long, or out-of-range value. A value decoded with another source's key usually fails the same way (the test `packed_payload_decoding_needs_the_matching_key` checks every pair of its fixtures), but no check guarantees it, because the bytes carry no tag. The Q3 readers always decode with the key they read.
- `SourcePayload` is the in-memory input and output type. Its variant must match the source index that the folded key names (`SourcePayload::matches`). `build_batch` rejects a mismatch before it encodes.
- Only K needs an order-preserving encoding. Values are compared as byte strings. The comparison serves equality and merging, not queries, and is sound because each K is unique.
- The packed encoding writes fields back to back, little-endian, with no padding. A string is a LEB128 length and UTF-8 bytes. A decimal takes the narrowest integer that holds its declared precision (`i16` for `P` up to 4, `i32` up to 9, `i64` up to 18, else `i128`).
- Values were byte strings (`PayloadBytes`) until 2026-10-16, then the typed enum `SourcePayload` until 2026-10-19. Files from those times must be regenerated; there is no migration.

A future SQL compiler extension could emit these static definitions; see [general-definition.md](general-definition.md). That is not implemented. Any generator must reproduce the exact folded bytes and payload types before it replaces the handwritten definition.

## Batch layout

`batch::build_batch::<P>` in DBSP turns `(FoldedKey, P, ZWeight)` rows into one immutable `MergedIndexBatch`, an `OrdIndexedWSet` over byte keys and values of a stored value type `P` (`Vec<u8>` for COL, the packed payload):

- It sorts by `(K, payload)`, adds the weights of equal pairs, reports overflow as an error, and omits zero totals.
- It requests `BatchLocation::Memory` for the initial run. Feldera's generic builders can choose a file during construction; this explicit request keeps the initial run in memory (test `initial_col_run_stays_in_memory_when_runtime_selects_files`).
- The typed `CustomerOrdersLineitemIndex::build_batch` also checks that each payload matches its key's placement.

The batch is inserted once into a native `Spine` created with `TraceRole::Integral`. Persistence and compaction then follow Feldera's native placement policy. There is no custom merger, cursor framework, or staging area.

## Reads

`batch::batch_cursor` returns Feldera's `CursorList` over the runs' own cursors. A snapshot (`Spine::ro_snapshot` in `crates/dbsp/src/trace/spine_async.rs`) does the same over a spine. Matching `(K, payload)` contributions are summed and zero totals omitted, without copying or changing the stored runs. Callers rewind before seeking backwards or switching from an exact lookup to an ordered scan.

Signed replacement works because cancellation matches complete `(K, payload)` pairs:

```text
Before append:       (K, P100, +1)
Transaction delta:   (K, P100, -1), (K, P120, +1)
After consolidation: (K, P120, +1)
```

Summing by K alone would hide the replacement, whose net weight at K is zero.

## Layer-file layout

A file-backed run has two levels, written by `Writer2` (`crates/dbsp/src/storage/file/writer.rs`):

```text
key level:   FoldedKey (bytes), nothing else
value level: packed payload bytes (Vec<u8>), then weight (ZWeight)
```

`MergedIndex::write_parent` writes all values of one K, then K itself. The caller supplies strictly increasing keys, because the file format requires unique keys within each group. Test `native_spine_compacts_full_col_runs_to_file_and_preserves_snapshots` in `merged-index-col/src/storage_tests.rs` reopens a compacted file run and compares its keys, payloads, and weights with the original run.

Because every K is unique, the key level stores one entry per row and shares nothing between rows. A native join trace instead stores one key per join-key value; its Lineitem trace, for example, stores one order key for all of that order's lines. Neighbouring folded keys share long prefixes, so with `--front-code-byte-keys` the layer file stores each key-level data block front-coded: each key is the length of the prefix it shares with the previous key in the block, then the rest of its bytes, and reading expands the block back to its original bytes. On 2026-10-19, with typed values and setup full compaction, the key level took 33,280 bytes of data blocks for 2,624 keys with front coding and 132,096 bytes without; the value level took 212,992 bytes. The four native join traces' key levels took 13,312 bytes together (`target/r5-typed-values/` and `target/r5-front-coded/`, local; see [measurement.md](measurement.md)).

## Uniqueness of folded keys

At a completed transaction boundary each folded K has at most one active payload. The argument ([decisions.md](decisions.md#2026-10-12-no-folded-key-uniqueness-validator)):

- The base tables have primary keys `c_custkey`, `o_orderkey`, and `(l_orderkey, l_linenumber)`. The folded keys `C(c)`, `O(c, o)`, and `L(c, o, l)` extend them, and an order's customer never changes inside the workload. Source-index tags keep the three relations' keys apart.
- The merged index is fed the harness's own signed rows, not the base tables' net delta. The argument therefore holds only while `BaseState::validate` in `sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/workload.rs` admits just the changes the base tables apply as given: no duplicate, replacement, or key-moving deltas.
- A workload with upserts must derive the merged-index rows from the base tables' net delta.

No runtime validator exists. `Writer2` checks K and payload uniqueness within one file group; it cannot see across runs. If a transaction inserts `(K, P120, +1)` without retracting `(K, P100, +1)`, a read returns both payloads. Storage never picks a winner or repairs invalid source state.

## Tests

As of this commit, run from the repository root:

```sh
cargo +1.96.1 test --offline -p dbsp --lib merged_index
cargo +1.96.1 test --offline --locked --manifest-path sql-to-dbsp-compiler/experiments/merged-index-col/Cargo.toml
```

The COL storage tests build 16 runs of 10,000 customers each (two orders per customer, two lines per order) through `build_batch`. They insert them into a native spine and wait for background compaction to produce a file run. They then check cancellation, same-key replacement, and a group move against an independent `BTreeMap` oracle. They also check that earlier snapshots still read their original rows. Exact byte sizes, thresholds, and run boundaries in earlier prototype runs were observations, not assertions, and are not recorded here. These tests show storage behavior only. They establish no recovery guarantee, memory cap, or speedup.
