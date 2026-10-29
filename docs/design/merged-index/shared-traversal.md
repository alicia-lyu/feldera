# Shared partner reads: one scan session per transaction

> [!NOTE]
> **Source:** Section 2.2 of `key-trie-laminar-ranges.tex` in the thesis repository ("Incremental view maintenance: shared partner reads"), with its appendices "One snapshot, overlaid with the deltas" and "Status against the prototype".
>
> **Notation:** `X⁻` and `X⁺` are the states before and after the transaction, `Δ` is a transaction delta, and a "run" is an immutable sorted collection.
>
> **Status:** plan only; nothing below is implemented. The algorithm work comes before all coding. Choices marked *unconfirmed* await the user; the confirmed ones are in [decisions.md](decisions.md) under 2026-10-29.

This document turns the thesis algorithm into an implementation plan. It covers:

- the algorithm in implementation terms;
- the code it touches;
- how today's prototype differs from it;
- the scan session that closes the gap;
- the algorithm work that must precede code;
- a coding roadmap.

## The algorithm, in implementation terms

The view `V = C ⋈ O ⋈ L` is maintained as `J = O ⋈ C` on `(custkey)`, then `V = L ⋈ J` on `(custkey, orderkey)`. The two delta rules need four partner reads.

| Read | Version | Driven by | Kind | Today's reader |
| --- | --- | --- | --- | --- |
| `C⁺` | after | `ΔO` | path | `customers_after` |
| `O⁻` | before | `ΔC` | range | `orders_before` |
| `J⁺` | after | `ΔL` | path | `joined_orders_after` |
| `L⁻` | before | `ΔJ` | range | `lineitems_before` |

A *path read* takes the entries on the root-to-node path of the instance tree. A *range read* takes the subtree under an instance node: every key sharing that node's byte prefix.

The thesis assumes four properties:

- **A1:** every join key is a trie key that is an ancestor-or-self of both inputs' keys.
- **A2:** every delta row carries its values on the join key; a `ΔL` row knows its `custkey`.
- **A3:** deltas are consumed in record order, one instance node at a time.
- **A4:** if reader `ρ` depends on reader `ρ'`, then `ρ'` takes its turn first.

Under them, all work of a transaction lies inside `Range(custkey = a)` for the touched customers `a`. `Visit(node)` does the following:

1. Decode the node's attached entries and push them on a path stack.
2. Let the readers with a request at the node take their turn: path readers take the path, and range readers open.
3. Visit the children in record order:
   - recurse where an open range or a request lies below;
   - otherwise seek to `next(prefix(child))`, skipping the subtree.

One pinned snapshot `Old` and the in-memory delta `Dlt` serve both versions. A before read takes `Old`; an after read takes `Old ⊎ Dlt`, unconsolidated.

The thesis proves five guarantees:

- the cursor moves only forward;
- each entry is decoded at most once;
- a skipped subtree costs one seek;
- the readers' outputs are exact;
- state is bounded by the path.

## Codebase analysis

### Storage: `crates/dbsp/src/trace/merged_index/`

- **Key layout.** `FoldedKey` is `[tag enc(value)]* 0 id`. The terminator `0` sorts below every domain tag, so a node's own entries come before its children.
- **Prefixes.** `KeyEncoder::prefix()` gives an instance node's prefix, and `then_tag` the prefix of its children of one domain.
- **Schema.** `KeyTrie` is the static schema. Its `Domain` has a name and columns but no value width.
- **Encoders.** `ordered.rs` encodes fixed-width, order-preserving values, so `next(prefix)` is computable.
- **Runs.** `MergedIndexBatch` is an `OrdIndexedWSet` of bytes. `build_batch` sorts and consolidates in memory, and `batch_cursor` returns a consolidating `CursorList`.
- **Missing:**
  - an abstraction for instance nodes;
  - a prefix seek or a subtree skip;
  - counters.

  Decoding is lazy and left to the client crate.

### Operators: `crates/dbsp/src/operator/`

- **Snapshot-reader join.** `dyn_join_generic_with_snapshot_readers` (`dynamic/join.rs`; typed `join_index_with_snapshot_readers` in `mono.rs`) wraps each side's `JoinTrace` in `StreamingBinaryWrapper`.
  - `JoinTrace` calls its reader once, at the commit flush, with the whole transaction delta.
  - The reader returns a materialized `SpineSnapshot`.
  - Output is chunked by `splitter_output_chunk_size`.
- **Flush order.** At commit, `circuit/schedule/dynamic_scheduler.rs` flushes a node only when all its predecessors have completed their flush (`FlushState::UnflushedDependencies(0)`). `Accumulator` holds a stream's chunks until it is flushed.
  - So two DBSP join operators cannot interleave their partner reads per instance node: the downstream join starts only after the upstream one has passed every touched customer.
  - This is why the scan session keeps the joins of a plan inside one operator.
- **Multi-input operators.** `async_stream_operators.rs` already offers `StreamingTernaryOperator` and `StreamingQuaternaryOperator`: one operator that consumes several inputs at its flush and streams its output in chunks. `StreamingNaryOperator` takes inputs of one type only.

### Q3 experiment: `sql-to-dbsp-compiler/experiments/`

- **Readers.** `merged-index-col/src/q3.rs`:
  - `Q3State` publishes a `before` and an `after` snapshot plus the order → customer placements.
  - `customers_after`, `orders_before`, `joined_orders_after`, and `lineitems_before` share `scan`. For each requested key, `scan` opens a fresh cursor, seeks, decodes every entry under the prefix, and filters afterwards.
- **Index upkeep.** `q3-ivm-baseline/src/reconstruction.rs`:
  - `ManualIndex::append` builds each transaction's run with `build_batch` on an auxiliary thread of the base runtime, and returns the snapshots before and after it.
  - `prepare` builds the placements from owner-closed deltas.
- **Scope.** `BaseState::validate_inner` (`workload.rs`) admits only RF1/RF2 changes.
- **Compiler.** `Q3MergedIndexBinding` pattern-matches the generated circuit, with `J` keyed by `o_orderkey` and Lineitem by `l_orderkey`. The spec language describes keys and payloads, not readers or joins.
- **Oracles:**
  - the reader tests in `merged-index-col/src/q3/tests.rs`;
  - `check_readers` and the 13-fixture `rf1_rf2_transcript` in `verification.rs`;
  - the transcript comparison in `verify.sh`;
  - `supplied_snapshot_join_matches_native_signed_updates` on the DBSP side.
- **Instrumentation:** backend bytes, write attribution, and buffer-cache hits. Nothing counts seeks or decodes.

## Current implementation against the algorithm

| Aspect | Prototype today | Section 2.2 |
| --- | --- | --- |
| `V`'s key | `orderkey` alone (A1 fails) | `(custkey, orderkey)` |
| Paths of `ΔL` and `ΔJ` | Placements from owner-closed deltas | Carried by the rows (A2) |
| Delta delivery | Whole delta at each join's flush (A3 fails) | Per instance node |
| Traversal | Four readers; a fresh cursor and seek per requested key | One forward cursor |
| Decoding | `O⁻` decodes whole customer subtrees, line items included; `J⁺` re-reads a customer per order | At most once; skipped subtrees never decoded |
| Versions | Two snapshots | `Old` plus `Dlt` |
| Owner-key change | Rejected | Subtree moved within `Dlt` |
| Counters | None | Needed for every cost claim |

## Remedies (decided 2026-10-29)

- **A1 and A4: a hand-written view circuit.** Every join is keyed by a trie key: `J` by `(custkey)`, `V` by `(custkey, orderkey)`. The circuit's author is responsible for A4.
- **A2: index deltas from the base transaction.** The merged index is base structure, not a view.
  - The base phase computes `Dlt`, the run `ManualIndex` already builds, with every entry at its full folded key.
  - The view circuit reads its inputs from `Dlt`, so every row carries its path and the placement map disappears.
  - An owner-key change becomes a moved subtree inside `Dlt`.
- **A3: a scan session.** It receives every base delta before the scan, computes each derived delta such as `ΔJ` at the node where it arises, and adds the requests that delta makes. The following were rejected:
  - a second traversal for `V`'s readers, as too costly;
  - a fused operator hard-coded to Q3, as not general;
  - changes to DBSP's scheduler, as too far from the algorithm.

## The scan session

The base deltas `ΔC`, `ΔO`, and `ΔL` are complete when the commit flush starts. Only the derived deltas, such as `ΔJ`, are not. One session operator receives every base delta plus `(Old, Dlt)`. It runs `Visit` over the touched root values and emits `ΔV`.

```text
Session(ΔC, ΔO, ΔL, Old, Dlt):
  static requests, from the base deltas:
    C⁺ at (c)    for each row of ΔO
    O⁻ at (c)    for each c with ΔC(c) ≠ 0
    J⁺ at (c,o)  for each row of ΔL
  for each touched c, ascending:  Visit((c))

Visit(n):
  decode the entries attached at n (Old ⊎ Dlt); push them on the path
  the readers at key(n) with a request at n take their turn
  if n = (c,o):
    ΔJ(o) = ΔO(o) ⋈ C⁺(c) + O⁻(o) ⋈ ΔC(c)    -- C⁺(c) on the path; O⁻(o) just decoded
    if ΔJ(o) ≠ 0: add the dynamic request L⁻ at (c,o)
  for each child m, in record order:
    if an open range reader or a request lies below m:  Visit(m)
    else:  seek to next(prefix(m))
  close the readers opened at n; pop
```

The thesis's worked transaction updates `c5` and inserts order `o5` with line items `l10` and `l11`. The session handles it as follows:

```text
(c5)      decode c5: C⁺ takes it, O⁻ opens
├ v2, v8  skipped by one seek
├ (c5,o3) decode o3: ΔJ(o3) ≠ 0, L⁻ opens, decode l7, l8
├ (c5,o4) decode o4: ΔJ(o4) ≠ 0, L⁻ opens, decode l9
└ (c5,o5) Dlt only: J⁺ takes c5, o5; ΔJ(o5) ≠ 0, L⁻ finds no old items
(c6)      untouched, never decoded
```

### Why Q3 needs no backward seek

A dynamic request at `(c,o)` depends only on:

- the base deltas;
- `C⁺(c)`, which lies on the path;
- `O⁻(o)`, the node's own entry, which the terminator byte places before the node's children.

So every request is known before the cursor needs it. A child-entering decision never waits on an unknown request:

- **If `O⁻` is open:** every order is entered.
- **If `O⁻` is not open:** `ΔJ(o) = ΔO(o) ⋈ C⁺` is known at `c` already.

### Where it would fail

The session would need a backward seek wherever a request depends on entries off the path. Two cases:

- a range reader at an ancestor that depends on a descendant;
- a dependency on a later sibling subtree, as in the thesis's Invoice example.

The thesis's remark on turns exempts a path reader that depends on a descendant, since its target stays on the path. Hand-written circuits must avoid both failing cases, which makes the condition part of the plan's contract.

### Generality

The session interprets a join plan: the hand-written circuit. It is not code written for Q3. A Q3-only fused operator would hard-code that plan.

## Algorithm work before coding

The thesis text and this document must settle these points first.

> [!NOTE]
> **Status (2026-10-29):** worked out in the thesis write-up (`docs/key-trie-laminar-ranges.tex`).
>
> - **Shared core:** Section 2.1.2 ("One scan session") now holds the core shared with query processing: `Advance`, `Skip`, `AtRangeEnd`, demands, and Theorem "One scan session".
> - **IVM side:** Section 2.2.3 adds the join plan, static and dynamic requests, and Lemma "Requests". Its condition is *forward requests*: when a request arises, every entry it targets is on the path or not yet passed. (A4) implies it.
> - **Logical operators:** the joins stay separate logical operators, which the session interleaves per instance node (Lemma "Interleaving is exact", with a worked coordination table).
> - **(A3):** it is split into (A3a) interleaved and (A3b) available. DBSP fails (A3a) for base deltas too.
> - **Notation:** a derived row is named by its instance node, `((custkey, orderkey) = (c5, o5))`, never `ΔJ(o)`. The pseudocode below predates this.

1. **The join plan.**
   - Define the session's input: a tree of joins whose keys are trie keys, each with its delta rules and the reader of each term (stream, version, path or range).
   - Define the turn order of the readers at one key.
   - Decide whether derived deltas such as `ΔJ` stay internal or are also emitted.
2. **The request lemma.**
   - Static requests come from the base deltas; dynamic requests come from derived rows.
   - The condition: every request at node `n` depends only on the base deltas and the entries on the path to `n`.
   - The claim to prove: under that condition, every request is known when the cursor enters `n` (or, for a path reader, before the cursor leaves `n`'s subtree). The cursor then never seeks backward, and the child-entering rule is complete.
   - Relate the condition to A3, A4, and the remark on turns.
3. **Derived deltas per node.**
   - Fix the loop order of each term (for `O⁻ ⋈ ΔC`, `O⁻` outside) so that derived rows come out in record order.
   - Show how overlay weights combine in path reads (the four-term expansion for `J⁺`) and in derived rows.
4. **Output.** `ΔV` is emitted node by node and consolidated by DBSP's batcher. Its order is record order, which the residual execution may exploit or ignore.
5. **The boundary.**
   - Which hand-written circuits meet the condition: chains along owner edges.
   - Which do not: joins across sibling subtrees, and an ancestor range reader depending on a descendant.
   - For those that do not, whether to fall back to held state, as in case 3 of the thesis's Theorem "state".

*Exit:* the lemma and the plan's definition are written up, and both the worked example and the Invoice counterexample are checked against them.

## Coding roadmap

The steps are ordered; each has an exit check and updates its docs. They are not R-numbered, since R1–R5 is the only numbering scheme.

1. **Counters on today's readers.**
   - Count cursor opens, logical seeks, per-run seeks, and decoded entries.
   - *Exit:* a test pins the counts on the worked example; they become the baseline.
2. **Instance-node cursor in `merged_index`.**
   - Add `next_prefix` and a cursor that can enter a node, list its attached entries, step through children by tag, and skip a subtree, counting as it goes.
   - `Domain` carries no value width, so add one to the trie and to `MergedIndexRustEmitter`.
   - *Exit:* property tests against brute-force prefix scans over random tries, data, and multi-run snapshots, including carry and an all-`0xFF` prefix.
3. **`Dlt` from the base transaction, and the overlay.**
   - Publish `(Old, Dlt)` instead of `(before, after)`, and rewrite the four readers to read `Old ⊎ Dlt` for the after state.
   - *Exit:* `check_readers` and the transcript comparison pass; the after snapshot is gone.
4. **Hand-written view circuit.**
   - The base deltas reach the view circuit as `Dlt`'s per-placement streams, keyed by their full trie paths.
   - Joins keyed `(custkey)` and `(custkey, orderkey)`, still on today's snapshot-reader joins.
   - The placement map is dropped.
   - *Exit:* the transcript comparison with native mode passes.
5. **The scan session.**
   - A join-plan type, plus a session operator over `StreamingTernaryOperator` that runs `Visit` and emits `ΔV`, replacing step 4's two joins.
   - *Exit:* on the worked example, the counters show forward-only movement and each entry decoded once.
   - A property test over random RF1/RF2 sequences matches native mode.
   - The transcript comparison passes.
6. **Measurement, new version.**
   - Add the counters to `measure.py`, and run native and session modes on Linux.
   - *Exit:* raw artifacts under `target/`; [measurement.md](measurement.md) reports base and IVM side by side.
7. **Beyond RF1/RF2 (optional).**
   - Customer updates, and owner-key changes moved within `Dlt` by the base phase.
   - *Exit:* new transcript fixtures pass in both modes.
8. **Deferred:**
   - plans outside the condition;
   - more than one worker, sharded by root value;
   - plans generated from the spec.

## Unconfirmed choices

- The session as one DBSP operator over the ternary streaming interface, or the quaternary one for wider plans.
- `ΔJ` internal to the session unless a plan exports it.
- Counters in the instance-node cursor, not in the generic `Cursor`.
- Step 4 as an intermediate milestone before the session.
- Owner-key changes out of scope until step 7.
