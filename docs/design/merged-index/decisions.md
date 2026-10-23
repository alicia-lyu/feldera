# Decisions

This file records the user's confirmed decisions for the merged-index prototype. Each entry is dated. A "decision" that appears elsewhere in these docs without an entry here is unconfirmed.

Entries come from two sources:

- the user's answers during the documentation review of 2026-10-12;
- dated user directives in the former `history/architectural-note-rewrite.md` and `slides-oct-8/advisor-working-style-2026-10-08.md`, both now archived under `TRASH/`.

## Current decisions

### 2026-10-12: One DBSP worker

Maintenance and initial loading both use one DBSP worker. No multiworker extension is planned. `main.rs` in `q3-ivm-baseline` rejects any other worker count in reconstructed mode and in R5 runs.

### 2026-10-12: Secondary storage only

The merged index is secondary storage, maintained beside Feldera's own base-table state. Clustered primary storage, in which the merged index would replace the base tables, is deferred.

### 2026-10-12: The compiler hook is the current approach

The `--q3-merged-index` compiler option is endorsed for now. It pattern-matches the exact Q3 circuit and emits the reconstructed joins. This supersedes the 2026-09-28 preference for hand-composed circuits (see [Superseded](#superseded-and-withdrawn)).

An open question remains; see [Open questions](#open-questions).

### 2026-10-12: No folded-key uniqueness validator

Folded keys are unique because the base tables have primary keys:

- **Keys.** `query.sql` declares the primary keys `c_custkey`, `o_orderkey`, and `(l_orderkey, l_linenumber)`. The folded keys are `C(c)`, `O(c, o)`, and `L(c, o, l)`. An order's customer never changes inside the workload.
- **Harness condition.** The merged index is fed the harness's own signed rows, not the base tables' net delta after upserts. Uniqueness therefore holds only because `BaseState::validate` in `q3-ivm-baseline/src/workload.rs` admits just the changes that the base tables apply as given: no duplicate, replacement, or key-moving deltas.
- **Future workloads.** A workload with upserts must derive the merged-index rows from the base tables' net delta.

This supersedes the 2026-09-28 requirement of a transaction-level uniqueness check (see [Superseded](#superseded-and-withdrawn)).

### 2026-10-12: Drop the R1 measurement table

The R1 table in `q3-ivm-baseline/RESULTS.md` has no raw artifact behind it, and its results are not actionable. It is removed.

### 2026-10-12: "Residual execution" is an acceptable term

An earlier slide plan banned the phrase. The ban is withdrawn and is not carried forward.

### 2026-10-12: Placement limitation accepted

The placement map (order → customer) comes only from Orders rows in the same transaction. Changes outside RF1 and RF2, such as a Lineitem added to an existing order, are therefore unsupported by design. This is an accepted limitation, not a defect. The harness's `prepare` rejects such changes.

### 2026-10-12: Report only what was measured

Never infer an unmeasured quantity from leftover arithmetic, such as assigning residual storage to the index; the user rejected this as circular. Make no performance or savings claim without a raw artifact.

### 2026-10-16: Typed merged-index values

The merged index stores each value as the typed `SourcePayload`, not as `rkyv` bytes wrapped in a byte string. Only the folded key is a byte string, because its byte order is its query order.

### 2026-10-16: Front-code byte-string keys, decoded on load

Folded keys are unique, so the key level of the merged index deduplicates nothing, and DBSP's layer files store every key in full. The fix is prefix truncation in DBSP: on disk, each key in a data block is stored as the length of the prefix it shares with the previous key plus the remaining suffix. Reading expands each block to its ordinary form before any other code sees it, so cursors, seeks, and the buffer cache are unchanged; only disk bytes and I/O shrink. The user chose this over a per-block prefix read in place (LeanStore style) and over front coding kept in the cache.

### 2026-10-16: Measure without block compression

Storage compression stays off in the R5 runs, so front coding's effect is measured on its own.

### 2026-10-16: "Key level" and "value level", not "column 0"

The docs call the two nesting levels of a layer file the key level and the value level. They do not use DBSP's "column 0" and "column 1".

### 2026-10-16: Merged indexes are declared as key-trie specs

A merged index is declared as a key-trie spec file, which the SQL compiler checks against the schema. The spec carries no derivable data: tags, ids, index names, and key columns come from a fixed traversal. Every placement declares its payload, and `*` marks a primary index that carries all non-key columns. A key value from another table is written `[domain via Relation]`. The design is in [general-definition.md](general-definition.md).

### 2026-10-16: The static trie is a nested literal

The parsed trie is a nested tree literal that mirrors the spec. It stores no indices, parent links, tags, ids, or paths; a pre-order walk recomputes them on each request. A domain's tag is its first appearance in the walk + 1, and a placement's id is its visit position + 1. Tries are small, so clean code wins over speed.

### 2026-10-16: Key fields are fixed-width and NOT NULL

Key types are fixed-width only, and every key column is NOT NULL, so there is no NULL key encoding for now.

### 2026-10-16: The first scope is the definition

The first scope is the definition and typed fold, unfold, and prefixes. Readers and joins stay hand-written. Roadmap Step 2 covers keys only and does not depend on typed payloads.

### 2026-10-16: Hand-written prefix names match the emitter's

The hand-written COL prefix functions are named as the emitter will name them: `prefix_custkey`, `prefix_custkey_orderkey`, and `prefix_custkey_orderkey_then_linenumber`.

### 2026-10-19: Drop the stored payload enum

The merged index no longer stores `SourcePayload`. The key's placement id already names the source, so the tag is redundant. The stored value is opaque bytes (`Vec<u8>`), decoded together with the key. `SourcePayload` remains the in-memory input and output type. This supersedes the 2026-10-16 typed-values entry for the stored value.

### 2026-10-19: Packed per-placement payload encoding

Each placement packs its payload fields back to back, little-endian, with no alignment padding. A string is a LEB128 length plus UTF-8 bytes. A decimal is narrowed by its declared precision `P`: `i16` up to 4, `i32` up to 9, `i64` up to 18, else `i128`. Decoding accepts only canonical bytes, range-checks every narrowed value, and returns an error instead of panicking.

### 2026-10-19: Elide the byte-string header on disk, as a separate transform

Every `Vec<u8>` value pays a 16 B `ArchivedVec` root, although its relative pointer and length follow from the item's position and bounds. A disk-only transform in `front_code.rs` drops the root for value-level `(Vec<u8>, weight)` items and rebuilds it on load, whatever the bytes encode. It has its own block magic, incompatible-feature bit, writer option, and `measure.py` flag, so it is measured apart from key front coding.

### 2026-10-19: Report three attributions, and frame packing as neither unique nor orthogonal

Report the enum, the byte-string header, and packing as three separate changes. Packing is neither a unique advantage of the merged index nor orthogonal to it. The merged index makes it cheap (unique keys need only byte equality, the format is private, and one decode point reads every value), and native could reach a similar gain at higher engineering cost. Decoding is not a differentiator, and nothing here claims native packing's bytes or decoding CPU cost.

### 2026-10-20: Views cannot be placed, for now

A spec may name a view, but the validator rejects every placement on one. Key uniqueness requires a primary key, and the SQL compiler records none for views. This is acceptable for now. The design leaves room for a later way to declare a view's key; see [deferred.md](deferred.md#general-merged-index-definition-later-work).

### 2026-10-20: Placements come before child nodes in a spec

Inside a node, placements are written before child nodes. A node's placements get their ids before its children's, so this keeps ids in written order. The parser rejects a placement that follows a child.

### 2026-10-20: Spec names are unquoted SQL identifiers

Relation and column names in a spec are unquoted SQL identifiers, matched in the compiler's canonical casing. Derived names keep the spec's spelling, and domain names are case-sensitive. Quoted identifiers are deferred.

### 2026-10-20: The spec flags arrive with the emitter

Roadmap Step 3 adds no compiler flag. `--merged-index` and `--merged-index-output` arrive in Step 4, together with reporting spec errors as compiler errors.

### 2026-10-20: Generated names come from the spec

Each placement gets a key struct `<Placement>Key`, with fields named by domain, and a payload struct `<Placement>Payload`, with fields named by SQL column. The `Key` and `Payload` enums have one variant per placement, named by the placement. The names do not imitate the hand-written `SourceKey` and `CustomerPayload`, so Step 5 renames their uses in `q3.rs` and `reconstruction.rs`.

### 2026-10-20: Index-id constants are named `INDEX_ID_`

A placement's index id is the constant `INDEX_ID_<PLACEMENT>`, for example `INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY`, after the design's term "index id". `IDX_` was rejected because it reads as an array position.

### 2026-10-20: Generated structs get plain derives

Generated key and payload structs derive `Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd`, and nothing else: no rkyv, `SizeOf`, `IsNone`, or `OrdRepr`. The stored value is packed bytes, so the typed payload is never archived. The typed-value measurement in `layout_tests.rs` keeps using the hand-written structs.

### 2026-10-20: Payloads must be packable, and a trie must place something

Every payload column is NOT NULL and has a packed encoding: an integer, BOOLEAN, DATE, TIME, TIMESTAMP, DECIMAL, or VARCHAR. A trie has at least one placement. Without these, the generated module would not compile. Nullable and other payload types are deferred.

### 2026-10-20: The emitter checks generated Rust names

The emitter rejects a spec whose derived Rust names collide (for example, domains `ab` and `aB` both give `TAG_AB`) or whose column names are not Rust identifiers. A Rust keyword used as a name becomes a raw identifier, such as `r#type`.

### 2026-10-20: The generated `unfold` reports an "unknown index id"

The hand-written `unfold` says "unknown placement id"; the generated one says "unknown index id", following the `INDEX_ID_` naming.

### 2026-10-21: Pin and retire the typed and rkyv value measurements

At roadmap Step 5, record the final typed (`SourcePayload`) and rkyv value-level bytes in [col-storage.md](col-storage.md), then retire those parts of `layout_tests.rs`. Only the packed measurement remains. Generated types keep plain derives. This replaces the last sentence of the 2026-10-20 plain-derives entry.

Summing spine run bytes instead (`layer_file_block_bytes` per spine file, as `q3-ivm-baseline/src/diagnostics.rs` does) was considered and does not help. It changes how bytes are counted, not what is stored, so the typed and rkyv encodings would still need their rkyv and DBData derives.

### 2026-10-21: COL re-exports its generated module and keeps only `build_batch`

At roadmap Step 5, `merged-index-col/src/lib.rs` re-exports the generated module at the crate root, so callers write `merged_index_col::Key`. The stateless wrapper `CustomerOrdersLineitemIndex` is dropped, because its methods only delegated. `build_batch` is a free function and the only hand-written storage code.

### 2026-10-21: Retire the generated-versus-hand-written tests

`generated_tests.rs` is retired with the hand-written definition it compared against. The golden-byte tests in `tests.rs`, whose expected bytes the switch left unchanged, are the regression guard for the byte format.

### 2026-10-22: The Linux measurement A/B tests key front coding

The Linux R5 measurement includes an A/B test of `--front-code-byte-keys`: runs with key front coding and runs without it, all else equal. Both the pilot and the campaign run both arms. The arms alternate per workload (clustered A, clustered B, scattered A, scattered B). Arm B keeps `--elide-value-headers` and `--setup-full-compaction`, so the arms differ in the key encoding alone. The arms and their cost are in [measurement.md](measurement.md#pending-linux-campaign).

### 2026-09-28: Maintained views exclude selection predicates

The maintained view is the unfiltered, unaggregated join `V = Customer ⋈ Orders ⋈ Lineitem`, carrying the fields that the consuming query needs. That query applies the segment and date filters, the revenue aggregation, ordering, and limit. Source records are stored before Q3's predicates, because unchanged Orders and Lineitems are still needed when a customer changes segment.

### 2026-09-28: Merged-index keys are folded byte strings in flat key-value storage

Merged-index keys are opaque byte strings, folded differently for each record type. Each key ends in the record's own identifier. Each value holds one payload; a payload list is never packed into one value. Feldera's LSM machinery (spines of immutable sorted runs) remains the backend: the merged index changes representation and comparison rules, not run management.

### 2026-09-28: Before/after read handles, not stored status bits

The user chose Feldera's existing operator wiring and removed any old/new or base/pending bit from storage. The index appends the transaction delta before view maintenance. Operators hold separate handles to the delta and to the accumulated state before and after the append. Completion releases those handles and never appends the delta a second time. Storage adds no parent-lookup records, relational-consistency checks, or automatic child moves.

### 2026-09-28: Reconstruction is interchangeable with retained state

Reconstructed state must answer the same requested-key reads that an integrator's retained state answers. The IVM algorithm and output path stay shared. The only reconstruction target is an integrator's accumulated output state, never its input delta stream. No reconstructed relation is assumed to fit in memory.

### 2026-09-28: No multi-versioning or expiration policies

The prototype adds no multi-versioning and no record or file expiration policy. Snapshots are temporary read handles over existing immutable batches.

### 2026-10-08: Slide-deck directions

These directions apply to the 2026-10-08 advisor deck; they do not set research scope.

- Introduce DBSP briefly, then derive physical behavior from code.
- Explain an already-loaded primary-key table before IVM mechanics; call its spine an LSM-like trace.
- Use Orders–Lineitem first for the explanatory program, but describe the experiment's Orders–Customer-first circuit as it actually is.
- Make no measured-savings, update-cost, or comparative performance claims.

### 2026-10-23: Linux campaign refresh selection

The user decided how `prepare_inputs.py` chooses refreshes:

- RF1 inserts and RF2 deletes are chosen independently and never deliberately correlated.
- Clustered RF1 draws each new Order's Customer from a Zipf distribution over a seeded random permutation of the Customer keys. The skew is in popularity, with no custkey locality. The exponent is `--zipf-exponent`, default 1.0; the default is the agent's choice and is unconfirmed.
- Clustered RF2 takes the Orders of the lowest-keyed Customers, as many as needed, with each pair taking the next block.
- Scattered RF1 is uniform over all Customer keys, including keys with no Orders. Scattered RF2 is a seeded uniform sample of existing Orders.

The `prepare_inputs.py` change lands in a parallel commit.

## Superseded and withdrawn

| Date | Earlier position | Replaced by |
| --- | --- | --- |
| 2026-10-14 | Leave the record-size gap open; schedule no encoding work. | 2026-10-16: typed values and front-coded byte-string keys. |
| 2026-10-16 | Store the typed `SourcePayload` enum as the value. | 2026-10-19: packed, untagged bytes per placement. |
| 2026-09-28 | Compose the Q3 circuit by hand; compiler shape control is an optional later route. | 2026-10-12: the compiler hook is the current approach. |
| 2026-09-28 | Require a transaction-level folded-key uniqueness check before a delta is incorporated. | 2026-10-12: no validator; uniqueness follows from the base tables' primary keys. |
| 2026-09-28 | A physical old/new bit, or base/pending bit, in each stored record. | 2026-09-28: before/after read handles. |
| 2026-09-28 | Reuse Feldera's grouped key/value batches unchanged for the merged index. | 2026-09-28: flat folded keys with an adapter over the same LSM machinery. |
| 2026-09-28 | A filtered, aggregate-first, five-state Q3 example. | 2026-09-28: the predicate-free join `V`. |
| 2026-09-28 | Q3 and operator semantic checkers as evidence. | Excluded by the user as unvalidated agent byproducts. |
| 2026-10-08 | A ban on the phrase "residual execution". | Withdrawn on 2026-10-12. |
| 2026-10-08 | "Five advances per join" in the slide walkthrough. | Not carried forward. |

## Open questions

- **Is the interesting-ordering shape required?** Reconstructing state from a merged index may require the circuit to read state in the merged index's order (customer > order > line). If so, the compiler cannot always produce that shape, and some queries will need hand-written circuits.
- **Clustered primary storage.** Deferred; see the secondary-storage decision.
