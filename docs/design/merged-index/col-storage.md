# COL storage specification

This file specifies how the Customer–Orders–Lineitem (COL) merged index stores records: the folded-key encoding, the definition and source types, the batch and file layout, and why folded keys are unique. It describes the code as it stands. The generic machinery is the public module `trace::merged_index` in `crates/dbsp/src/trace/merged_index/`. The concrete COL definition and its payload codecs are in `sql-to-dbsp-compiler/experiments/merged-index-col/src/`.

The merged index is secondary storage, kept beside Feldera's own base-table state ([decisions.md](decisions.md)). Reading it for the maintained view `V` is described in [reconstruction.md](reconstruction.md). The trace and cursor interfaces it reuses are in [support.md](support.md).

## Vocabulary

- **Trace.** Feldera's name for an indexed collection of accumulated weighted updates. It implies no transaction log and no stored timepoints.
- **Transaction delta.** The signed change set of one transaction.
- **Run.** An immutable sorted collection of weighted records (a batch). A run is in memory or in a layer file.
- **Layer file.** Feldera's immutable file format for nested sorted groups. Each nesting level is a *column*; it is not a SQL attribute.
- **Spine.** The collection of runs that merges them in the background.

## Folded keys

A folded key (K) is a byte string that sorts a customer before its orders, and an order before its lines. The storage layer compares K as opaque lexicographic bytes.

```text
domain tags:        INDEX = 0x00, customer = 0x01, order = 0x02, line = 0x03
source-index tags:  CustomerPrimary = 0x01, OrdersByCustomer = 0x02, LineitemByCustomer = 0x03

fold_iN(x) = big_endian_N_bits((x as unsigned_N_bits) XOR sign_bit_N)

C(c):     0x01 fold_i64(c) | INDEX | 0x01                                  (11 bytes)
O(c,o):   0x01 fold_i64(c) | 0x02 fold_i64(o) | INDEX | 0x02               (20 bytes)
L(c,o,l): 0x01 fold_i64(c) | 0x02 fold_i64(o) | 0x03 fold_i32(l) | INDEX | 0x03   (25 bytes)
```

Rules:

- **Field widths.** Customer and order IDs are `i64`; line numbers are `i32`. The sign-bit flip preserves signed order within a domain.
- **Terminal tag.** The key ends with `INDEX` and the source-index tag. `INDEX` is not an extra end marker: its value sorts below every child-domain tag, so a customer precedes its orders.
- **Nothing else in K.** Payload, weight, and occurrence counters never extend K.
- **Checked conversion.** Generic field values are `i64`. A value for an `I32` domain is converted with a range check; `fold` and `prefix` return an error outside the range.
- **Validation.** `unfold` rejects wrong lengths, wrong domain tags, a missing `INDEX` tag, and unknown or ambiguous source tags. Tag bindings that reuse `0x00` or collide are rejected.
- **Prefixes.** `MergedIndex::prefix` returns the leading fields without `INDEX` or the source tag. A customer prefix ends after `c`; an order prefix ends after `o`; a line prefix also includes the line domain tag.
- **Scans.** A prefix scan seeks the prefix and steps until the key no longer starts with it. No successor function is needed.

Fixture tests: `mixed_width_folding_uses_definition_metadata`, `signed_boundaries_round_trip_and_sort_in_both_domains`, `i32_fields_reject_out_of_range_projection_and_prefix_values`, `malformed_keys_are_rejected_without_panics`, and `ambiguous_domain_tags_are_rejected` in `crates/dbsp/src/trace/merged_index/tests.rs`.

## Definition and source types

The definition separates three things:

- **Key domains** name a logical identity (`customer`, `order`, `line`) and its primitive type (`KeyPrimitive::I64` or `I32`). Shared domain identity, not field-name similarity, establishes shared ordering.
- **Source indexes** (`SourceIndexSpec`) select ordered key fields and payload fields from one base relation. The base-relation ID is symbolic.
- **Byte tags** (`DomainByteTag`, `SourceByteTag`) belong to one merged-index definition. Another merged index can assign different tags to the same source index.

In Rust, `MergedIndexDefinition` supplies these constants plus `project` (typed key to source index and field values) and `construct` (the inverse). `MergedIndex<D>` folds, unfolds, and builds prefixes from any such definition. Source row types have no global folding operation.

COL fills in the definition as `CustomerOrdersLineitemDefinition`, with `SourceKey` variants `Customer(i64)`, `Orders(i64, i64)`, and `Lineitem(i64, i64, i32)`:

| Source index | Key fields | Payload |
| --- | --- | --- |
| `CustomerPrimary` | customer | `CustomerPayload { segment: SqlString }` |
| `OrdersByCustomer` | customer, order | `OrdersPayload { order_date: Date, ship_priority: i32 }` |
| `LineitemByCustomer` | customer, order, line | `LineitemPayload { ship_date: Date, extended_price: SqlDecimal<15, 2>, discount: SqlDecimal<4, 2> }` |

`CustomerOrdersLineitemIndex` wraps `MergedIndex<CustomerOrdersLineitemDefinition>` with typed `fold`, `unfold`, prefix helpers (`customer_prefix`, `order_prefix`, `line_prefix`), `encode_payload`, `decode_payload`, and `build_batch`.

Payload rules:

- Payloads are serialized as checked `rkyv` bytes (`PayloadBytes`). Decoding validates the folded key first to choose the payload type, then checks the archived bytes before deserializing.
- Only K needs an order-preserving encoding. The file merger orders payload bytes for equality and merging, not as a query-visible order.
- Prototype files written with an earlier payload codec must be regenerated; there is no migration.

A future SQL compiler extension could emit these static definitions. That is not implemented. Any generator must reproduce the exact folded bytes and payload serialization before it replaces the handwritten definition.

## Batch layout

`batch::build_batch` in DBSP turns `(FoldedKey, PayloadBytes, ZWeight)` rows into one immutable `MergedIndexBatch`, an `OrdIndexedWSet` over byte keys and byte values:

- It sorts by `(K, payload)`, adds the weights of equal pairs, reports overflow as an error, and omits zero totals.
- It requests `BatchLocation::Memory` for the initial run. Feldera's generic builders can choose a file during construction; this explicit request keeps the initial run in memory (test `initial_col_run_stays_in_memory_when_runtime_selects_files`).
- The typed `CustomerOrdersLineitemIndex::build_batch` also checks that each payload matches its key's source index.

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

A file-backed run has two columns, written by `Writer2` (`crates/dbsp/src/storage/file/writer.rs`):

```text
column 0: search key = FoldedKey;    data = ()
column 1: search key = PayloadBytes; data = weight (ZWeight)
```

`MergedIndex::write_parent` writes all children of one K (`write1`), then the parent (`write0`). The caller supplies strictly increasing keys, because the file format requires unique keys within each group. Test `native_spine_compacts_full_col_runs_to_file_and_preserves_snapshots` in `merged-index-col/src/storage_tests.rs` reopens a compacted file run and compares its keys, payloads, and weights with the original run.

The second column exists even for a key with a single payload. Its file-size, cache, and read costs are unmeasured; make no performance claim about it.

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
