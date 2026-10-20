# General merged-index definition

This document is a design and a roadmap. Roadmap Steps 1 and 2 are implemented: the key encoders, and the static trie with the reshaped `MergedIndexDefinition`. The spec parser, the emitter, and the switch to generated code are not.

It proposes a key-trie spec, checked by the SQL compiler, as the single definition of a merged index. From the spec the compiler would emit typed Rust: key and payload types, folding, and prefixes, with key field types taken from SQL column types at codegen time.

The trie spec supersedes the flat `key_domain` / `source_index` / `merged_index` notation in the archived `TRASH/docs/design/merged-index/folded-key-layer-file-plan.md`.

## Why

COL is defined by hand. `merged-index-col/src/lib.rs` spells out its trie, a key enum, payload structs, and straight-line `fold` and `unfold`. A second merged index would need all of it written again.

Before Steps 1 and 2, the generic layer in `crates/dbsp/src/trace/merged_index/` had three limits:

- **Every key value crossed its API as an `i64`.** `MergedIndexDefinition::project` and `construct` took and returned them.
- **Only `KeyPrimitive::I32` and `I64` could be encoded.** DATE, DECIMAL, and the other key types had no encoding.
- **Tags were looked up at run time,** by linear scans of four flat constant tables.

The companion note `merged_index_vldb2026_slides/docs/key-trie-laminar-ranges.tex` frames a merged index as a **key trie**:

- each node is a key, a sequence of domains;
- each participating index is placed at the node of its full key;
- folding walks the path from the root to that node and appends one tag and one value per domain.

Each node also defines a byte prefix whose matches form one contiguous sort range. The spec below writes that trie down directly.

## Spec format

The spec for COL:

```text
domains {
  custkey    = Customer.c_custkey, Orders.o_custkey;
  orderkey   = Orders.o_orderkey, Lineitem.l_orderkey;
  linenumber = Lineitem.l_linenumber;
}

merged_index CustomerOrdersLineitem {
  custkey {
    Customer: *;
    orderkey {
      Orders: o_orderdate, o_shippriority;
      linenumber {
        Lineitem [custkey via Orders]: l_shipdate, l_extendedprice, l_discount;
      }
    }
  }
}
```

- **`domains`** groups columns that join on equal values. Sharing a domain, not having similar names, is what puts fields in one order.
- **A nesting level is a domain,** that is, an edge of the trie.
- **`Relation [vias]: payload;`** places an index on that relation at the enclosing node.
- Relations may be tables or views of the SQL program, and comments use `--`.

The spec contains only what cannot be derived. Tags, index ids, index names, and key columns are all derived (see "What the compiler derives").

## Payload

Every placement declares its payload.

- **`*` marks a primary index.** It carries all of the relation's non-key columns, in table order. It is allowed only where the index key is exactly the relation's primary key. In COL that is Customer.
- **A column list marks a secondary index.** It carries exactly the listed columns, in the listed order.
  - Orders is secondary: its key, `(custkey, orderkey)`, puts a foreign key first.
  - Lineitem is secondary too. Its list order also matches the hand-written `LineitemPayload`.

## Key values from another table: `via`

Lineitem is placed below `custkey`, so every Lineitem key starts with a customer key. Lineitem has no customer column, though. A line's customer is the one who placed its order: `o_custkey` of the Orders row whose `o_orderkey` equals the line's `l_orderkey`.

`[custkey via Orders]` states exactly that: take `custkey` from the Orders row that this Lineitem row refers to. The compiler checks two things:

1. **Orders' primary key, `o_orderkey`, lies in a domain for which Lineitem has its own column:** `orderkey`, through `l_orderkey`. So each line refers to exactly one order.
2. **Orders has a column in the requested domain:** `o_custkey` in `custkey`.

The generated Lineitem key still has a `custkey` field, typed from `o_custkey`. The caller supplies its value when it folds a key, as COL's placement map does today (see [reconstruction.md](reconstruction.md)). The compiler emits no lookup.

## What the compiler derives

The compiler makes one pre-order traversal of the trie, visiting children in the order they are written.

- **Domain tags** are 1, 2, 3, … in order of first appearance. Sibling subtrees are stored in tag order, so the order in which siblings are written decides which comes first in the index. The note treats that order as a design choice.
- **Index ids** are 1, 2, 3, … in the order placements are visited. They follow the reserved terminator tag 0.
- **Index names** follow `<Relation>By<Path>`, with the path's domains in CamelCase, e.g. `OrdersByCustkeyOrderkey`.
- **Key columns** are, for each domain on the path from the root to the node, the relation's column in that domain, or the column named by its `via`.

For COL, both tags and ids come out as 1, 2, 3, which are the values `CustomerOrdersLineitemDefinition` hard-codes today. Reordering the spec renumbers tags and ids, and so changes the stored bytes. That is acceptable, because the definition is compiled with the index it describes. The static trie records which numbering was used.

## Validation

Each failure is a compiler error that names the spec line.

1. **Domains.**
   - Every `Relation.column` exists.
   - A column belongs to at most one domain.
   - A relation has at most one column per domain.
2. **Types agree, and keys are NOT NULL.** All columns of a domain have the same SQL type, and every key column is NOT NULL. Primary keys already are; a nullable foreign-key column must be declared NOT NULL to serve as a key.
3. **Fixed width.** Domain types are integers, BOOLEAN, DATE, TIME, TIMESTAMP, or DECIMAL. Other types are rejected as variable-width.
4. **Every path domain resolves.**
   - Each domain on a placement's path has either the relation's own column or a `via` that passes both checks above.
   - A `via` is an error if the relation already has a column in that domain, or if the domain is not on the path.
5. **Keys are unique.** The resolved key columns contain the relation's primary key, and a relation without one is rejected. This is the condition that makes folded keys unique (see [col-storage.md](col-storage.md)).
6. **The trie is well formed.**
   - A node's child domains are distinct.
   - No domain repeats on one path.
   - A relation is placed at a node at most once.
   - There are at most 255 domains and 255 placements, because tags and ids are one byte.
7. **Payloads are valid.**
   - `*` appears only where the key is exactly the primary key.
   - A list names existing non-key columns, without duplicates.

## The static trie

The parsed trie becomes a static Rust value, `KeyTrie` in `merged_index/trie.rs`. It is a nested tree that mirrors the spec and stores nothing that can be derived:

```rust
pub struct KeyTrie {
    pub name: &'static str,
    pub domains: &'static [Domain],   // the spec's `domains` block: column membership only
    pub children: &'static [Node],    // the root's children; the root is implicit
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

`Domain` is a name with its `(relation, column)` members, so a domain's columns are looked up by name.

The tree stores no indices, parent links, tags, ids, or paths. Each query walks the tree in pre-order and recomputes what it needs:

- `placements()` returns each placement with its id (visit position + 1) and its path of domain names;
- `domain_tag(name)` returns the position of the domain's first appearance in the walk, + 1;
- `PlacedIndex` derives `name()`, `is_primary()`, and `key_columns(&trie)`;
- `validate()` checks the schema-free rules of the Validation section: declared and distinct domains, no repeat on a path, one placement per relation per node, at most 255 domains and 255 placements, and that every path domain resolves through an own column or a valid `via`. Type, NOT NULL, and primary-key checks need the schema, so they stay in the compiler.

Tries are three or four levels deep, so this favors clean code over speed. Folding does not walk the tree: it uses constants, which `testing::check_against_trie` ties to the trie.

The compiler's Java model has the same shape, and the emitter prints it as this Rust value. COL's `TRIE` in `merged-index-col/src/lib.rs` reads like its spec: `custkey { Customer; orderkey { Orders; linenumber { Lineitem [custkey via Orders] } } }`.

## Generated code

For one spec, the compiler emits one Rust module containing the following items.

- **`const TRIE: KeyTrie`.**
- **A key struct per placement.** Its field types come from the SQL column types, rendered by the existing `ToRustInnerVisitor`. For example, the Lineitem key has `custkey: i64`, `orderkey: i64`, and `linenumber: i32`.
- **A payload struct per placement,** with fields in payload order.
- **Packed codecs.** Per placement, an `encode` that writes the fields back to back with `PayloadEncoder`, and a `decode` over `PayloadDecoder`. A `decode` dispatching on the placement id calls the right one. No `Payload` enum is stored: the value is opaque bytes, readable only with its key.
- **A `Key` enum** over the key structs, and a `Payload` enum as the in-memory input and output type only.
- **An implementation of `MergedIndexDefinition`.** Its `fold` and `unfold` are straight-line code: constant tags, and one key encoder call per domain in path order. `unfold` selects the placement from the last byte, then checks length and tags.
- **Typed prefix functions:**
  - `prefix_<path>` per node: the bytes shared by every key in that node's sort range, the note's β(v, a);
  - `prefix_<path>_then_<domain>` per child edge, which replaces today's `prefix_with_next_tag`.

The output is selected by two new compiler flags, `--merged-index <spec>` and `--merged-index-output <file.rs>`. The circuit and `--q3-merged-index` are untouched.

## DBSP prerequisites

The generated module needs two things from `crates/dbsp/src/trace/merged_index/`.

### A reshaped `MergedIndexDefinition`

Implemented, for keys only. The trait now has these items:

- `type Key`;
- `const TRIE: KeyTrie`;
- typed `fold` and `unfold`. `fold` cannot fail, because the key's fields are typed.

Two helpers own the folded-key byte layout, `[tag value]* 0 id`. `KeyEncoder` writes `(tag, value)` pairs in path order and ends a key with `finish(id)`, `prefix()`, or `then_tag(tag)`. `KeyDecoder` checks the terminator, splits off the id, and reads one tagged field at a time. A definition, hand-written now and generated in Step 4, is straight-line code over them.

This replaced the four flat tables (`KEY_DOMAINS`, `SOURCE_INDEX_SPECS`, `DOMAIN_BYTE_TAGS`, `SOURCE_BYTE_TAGS`), the `i64` `project` and `construct`, and the run-time tag scans in `MergedIndex<D>`.

Payloads are not part of the trait. `build_batch`, `batch_cursor`, and `write_parent` are generic over a stored value type `P: DBData`, which for COL is `Vec<u8>`: the packed encoding of one payload, readable only together with its key. COL rejects a payload of the wrong variant with `SourcePayload::matches` before it encodes. Generating that check, or adding `type Payload` and placement functions to the trait, is left to Step 4.

### Packed payload fields

Payload fields are packed by the trait `PackedField` in `merged_index/packed.rs`. It is the payload counterpart of `OrderedKey` and differs in three ways: it needs no byte order, so values are little-endian; it has no fixed width, so a string is a LEB128 length and UTF-8 bytes; and a decimal is narrowed by its declared precision (`P` up to 4 as `i16`, up to 9 as `i32`, up to 18 as `i64`, else `i128`). Decoding accepts only canonical bytes and range-checks every narrowed value. `PayloadEncoder` and `PayloadDecoder` are straight-line helpers in the style of `KeyEncoder` and `KeyDecoder`; `PayloadDecoder::finish` rejects trailing bytes. The implementations for dates, times, timestamps, and strings are in `crates/sqllib/src/packed_field.rs`, and for `Fixed` in `crates/fxp/src/dbsp_impl.rs`.

### Order-preserving key encoders

Folding needs, for each key type, a fixed-width byte encoding whose `memcmp` order equals the type's `Ord`. Decoding accepts only canonical bytes, so a folded key parses into exactly one sequence of values, as the note's unique-parsing lemma requires. The trait `OrderedKey` in `merged_index/ordered.rs` provides this, with a `WIDTH` constant, `encode_ordered`, and `decode_ordered`. It is implemented for these types:

- **Signed integers:** big-endian with the sign bit flipped. This is byte-identical to today's `I32` and `I64` encoding.
- **Unsigned integers:** big-endian.
- **`bool`:** one byte, 0 or 1.
- **`Date`, `Time`, and `Timestamp`,** in `crates/sqllib/src/ordered_key.rs`. Each delegates to its inner integer: days, nanoseconds, or microseconds.
- **`SqlDecimal`.** This is fxp's `Fixed`, so its implementation is in `crates/fxp/src/dbsp_impl.rs`, behind that crate's existing `dbsp` feature, as the orphan rule requires. It delegates to the i128 mantissa, which is correct because all columns of a domain share one precision and scale. Decoding rejects a mantissa outside the type's range.

There is no encoding for NULL, because key columns are NOT NULL (see Validation). Floats are not key types.

## Decisions

The user's decisions for this design, dated 2026-10-16, are recorded in [decisions.md](decisions.md).

## Roadmap

These steps are ordered. Each ends with an exit check and lands with its own doc updates. The steps are not numbered stages, because R1–R5 is the only numbering scheme in these docs.

1. **Key encoders.** Implemented: `OrderedKey`, with its implementations in dbsp, sqllib, and fxp.
   - Exit check: proptests that byte order equals `Ord`, that decoding round-trips, and that widths are exact, including boundary values.
   - Exit check: the i32 and i64 bytes are unchanged.
2. **Static trie and reshaped trait.** Implemented, for keys only: `KeyTrie`, the new `MergedIndexDefinition`, `KeyEncoder` and `KeyDecoder`, and the hand-written COL ported to them with a hand-written `TRIE`.
   - Exit check: the COL tests and `verify.sh` pass with unchanged transcripts. The golden-byte tests are unchanged.
   - Exit check: `check_against_trie` and a proptest tie COL's hard-coded tags and ids to its trie.
3. **Parser and validator** in the SQL compiler, with no emission yet.
   - Exit check: JUnit tests of the derivation. COL comes out as 1, 2, 3; sibling order follows the spec; `custkey via Orders` resolves to `o_custkey`; `*` expands in table order.
   - Exit check: one failing case per validation rule.
4. **Emitter, and COL regenerated.** Write `col.mi` and generate the COL module with the two new flags. Commit the generated file, and make `verify.sh` fail if it is stale.
   - Exit check: the generated and hand-written tries are equal.
   - Exit check: `fold` is byte-identical over property-based keys.
   - Exit check: the prefixes match, and sample payloads archive to identical bytes.
5. **Switch.** COL uses the generated module, and the hand-written definition moves to `TRASH/`.
   - Exit check: `verify.sh` transcripts are unchanged.

Later work, outside this roadmap, is listed in [deferred.md](deferred.md).
