# General merged-index definition

This document is a design and a roadmap. All five roadmap steps are implemented: the key encoders, the static trie with the reshaped `MergedIndexDefinition`, the spec parser and validator in the SQL compiler, the emitter, and the switch of COL to its generated module.

It proposes a key-trie spec, checked by the SQL compiler, as the single definition of a merged index. From the spec the compiler would emit typed Rust: key and payload types, folding, and prefixes, with key field types taken from SQL column types at codegen time.

The trie spec supersedes the flat `key_domain` / `source_index` / `merged_index` notation in the archived `TRASH/docs/design/merged-index/folded-key-layer-file-plan.md`.

## Why

Until Step 5, COL was defined by hand. `merged-index-col/src/lib.rs` spelled out its trie, a key enum, payload structs, and straight-line `fold` and `unfold`. A second merged index would have needed all of it written again.

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
  - Lineitem is secondary too. Its list order also matches the former hand-written `LineitemPayload`.

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

For COL, both tags and ids come out as 1, 2, 3, which are the values the former hand-written `CustomerOrdersLineitemDefinition` hard-coded. Reordering the spec renumbers tags and ids, and so changes the stored bytes. That is acceptable, because the definition is compiled with the index it describes. The static trie records which numbering was used.

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
   - There is at least one placement.
7. **Payloads are valid.**
   - `*` appears only where the key is exactly the primary key.
   - A list names existing non-key columns, without duplicates.
   - Every payload column, including those that `*` brings in, has a packed encoding: it is NOT NULL, and its type is an integer, BOOLEAN, DATE, TIME, TIMESTAMP, DECIMAL, or VARCHAR (see "Packed payload fields").

The SQL compiler implements the parser and these checks in package `org.dbsp.sqlCompiler.compiler.frontend.mergedIndex`:

- `MergedIndexSpecParser` turns spec text into a `KeyTrie`, a Java mirror of the Rust trie below.
- `MergedIndexValidator` checks the trie against the compiled tables and views and returns a `ValidatedMergedIndex`, which holds each placement's key and payload columns with their types.
- Errors are `SpecError`s with a spec line and column. The parser stops at the first error; the validator reports all of them.

Three details of that implementation are confirmed decisions (see [decisions.md](decisions.md)):

- **Placements come first.** Inside a node, placements are written before child nodes, because a node's placements get their ids first.
- **Names are unquoted SQL identifiers.** Relation and column names are matched in the compiler's canonical casing. Derived names keep the spec's spelling. Domain names are case-sensitive.
- **Views are rejected.** A spec may name a view, but views have no primary key, so rule 5 rejects every placement on one (see [decisions.md](decisions.md)).

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

The compiler's Java model has the same shape, and the emitter prints it as this Rust value. COL's `TRIE` in `merged-index-col/src/generated.rs` reads like its spec: `custkey { Customer; orderkey { Orders; linenumber { Lineitem [custkey via Orders] } } }`.

## Generated code

For one spec, `MergedIndexRustEmitter` (in the compiler's `backend/rust` package) emits one Rust module. The emitter derives every item from the trie at generation time and writes the results as constants and straight-line code, so `fold` never walks the trie at run time. For a placement `OrdersByCustkeyOrderkey` and a domain `custkey`, the module contains:

- **Constants:**
  - `TAG_CUSTKEY`, one per domain in tag order;
  - `INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY`, one per placement.
- **`TRIE`,** the trie as a `KeyTrie` value. Generic code and tests query it at run time.
- **A key struct per placement,** `OrdersByCustkeyOrderkeyKey`, with one field per path domain, named by the domain. Field types come from the SQL column types, rendered by the existing `ToRustInnerVisitor`. For example, the Lineitem key has `custkey: i64` (from `o_custkey`, through the `via`), `orderkey: i64`, and `linenumber: i32`.
- **A payload struct per placement,** `OrdersByCustkeyOrderkeyPayload`, with fields named by SQL column, in payload order. It has `encode` (to packed bytes, with `PayloadEncoder`) and `decode` (with `PayloadDecoder`). Both structs derive `Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd`.
- **`Key` and `Payload` enums,** with one variant per placement, named by the placement. `Payload` is the in-memory input and output type only: the stored value is opaque bytes, readable only with its key.
  - `Payload::matches(&Key)` checks that a payload belongs to its key's placement.
  - `encode_payload` and `decode_payload(&Key, &[u8])` dispatch on the placement.
- **`<IndexName>Definition`,** an implementation of `MergedIndexDefinition`. Its `fold` and `unfold` are straight-line code: constant tags, and one key encoder call per domain in path order. `unfold` selects the placement from the last byte, then checks length and tags.
- **Typed prefix functions,** with one parameter per path domain:
  - `prefix_<path>` per node: the bytes shared by every key in that node's sort range, the note's β(v, a);
  - `prefix_<path>_then_<domain>` per child edge.

The emitter rejects a spec whose derived Rust names collide or are not Rust identifiers. A Rust keyword such as `type` becomes a raw identifier, `r#type`.

The compiler flags `--merged-index <spec>` and `--merged-index-output <file.rs>` select the output; each requires the other. Spec errors are reported as compiler errors that name `spec:line:column`. The circuit and `--q3-merged-index` are untouched.

For COL, `merged-index-col/generate.sh` runs the compiler on `q3-ivm-baseline/query.sql` and `merged-index-col/col.mi`, formats the result with rustfmt 1.96.1, and writes the committed `src/generated.rs`, which COL's `lib.rs` re-exports. The compiler wrapper always exits 0, so the script treats a missing output as the failure.

## DBSP prerequisites

The generated module needs two things from `crates/dbsp/src/trace/merged_index/`.

### A reshaped `MergedIndexDefinition`

Implemented, for keys only. The trait now has these items:

- `type Key`;
- `const TRIE: KeyTrie`;
- typed `fold` and `unfold`. `fold` cannot fail, because the key's fields are typed.

Two helpers own the folded-key byte layout, `[tag value]* 0 id`. `KeyEncoder` writes `(tag, value)` pairs in path order and ends a key with `finish(id)`, `prefix()`, or `then_tag(tag)`. `KeyDecoder` checks the terminator, splits off the id, and reads one tagged field at a time. A generated definition is straight-line code over them.

This replaced the four flat tables (`KEY_DOMAINS`, `SOURCE_INDEX_SPECS`, `DOMAIN_BYTE_TAGS`, `SOURCE_BYTE_TAGS`), the `i64` `project` and `construct`, and the run-time tag scans in `MergedIndex<D>`.

Payloads are not part of the trait. `build_batch`, `batch_cursor`, and `write_parent` are generic over a stored value type `P: DBData`, which for COL is `Vec<u8>`: the packed encoding of one payload, readable only together with its key. COL's `build_batch` rejects a payload of the wrong variant with the generated `Payload::matches` before it encodes. Adding `type Payload` and placement functions to the trait remains open.

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
3. **Parser and validator** in the SQL compiler, with no emission yet. Implemented; see the end of the Validation section.
   - Exit check: JUnit tests of the derivation (`MergedIndexDerivationTest`). COL comes out as 1, 2, 3; sibling order follows the spec; `custkey via Orders` resolves to `o_custkey`; `*` expands in table order.
   - Exit check: one failing case per validation rule (`MergedIndexValidatorTest`); syntax errors are in `MergedIndexSpecParserTest`.
4. **Emitter, and COL regenerated.** Implemented: `merged-index-col/col.mi`, `generate.sh`, and the committed `src/generated.rs`, which sat beside the hand-written definition until Step 5. `verify.sh` regenerates the module and fails if the committed file differs. The flags report `SpecError`s as compiler errors; the compiler's own source positions refer only to the SQL file.
   - Exit check: the generated and hand-written tries are equal (`generated_tests.rs`, retired with the hand-written definition in Step 5).
   - Exit check: `fold` is byte-identical over property-based keys.
   - Exit check: the prefixes match, and sample payloads encode to identical packed bytes. Since 2026-10-19 the stored value is packed bytes rather than an archive.
5. **Switch.** Implemented: COL uses the generated module, re-exported at the crate root, and the hand-written definition is in `TRASH/`. `lib.rs` keeps only `build_batch`.
   - The typed and rkyv value-level bytes that `layout_tests.rs` pinned are recorded in [col-storage.md](col-storage.md), and those measurements are retired. Only the packed measurement remains, so generated types keep plain derives (see [decisions.md](decisions.md), 2026-10-21).
   - Exit check: `verify.sh` transcripts are unchanged.

Later work, outside this roadmap, is listed in [deferred.md](deferred.md).
