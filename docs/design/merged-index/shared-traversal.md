# Shared partner reads: one scan session per transaction

> [!NOTE]
> **Source:** Chapter 3 of `docs/key-trie-laminar-ranges.tex` in the thesis repository, "Interesting orderings for view maintenance: driving deltas". This doc cites the chapter by label, since its numbers drift: `sec:ivm`, `sec:ivm-assume`, `sec:ivm-reads`, `sec:ivm-buffered`, `sec:ivm-traversal`, `sec:ivm-interleave`, `sec:ivm-relax`, and the appendices `sec:overlay` and `sec:prototype`.
>
> **Notation:** `X⁻` and `X⁺` are the states before and after the transaction. `Δ` is a transaction delta, and a "run" is an immutable sorted collection. A derived row is named by its instance node, such as `((custkey, orderkey) = (c5, o5))`.
>
> **Status:** the algorithm is settled in the write-up; nothing below is implemented. This doc is the meta plan for the coding steps. Choices marked *unconfirmed* await the user. The confirmed ones are in [decisions.md](decisions.md) under 2026-10-29.

This doc covers:

- the algorithm as Chapter 3 states it;
- an audit of the chapter against the code;
- how the chapter's concepts map onto Feldera code;
- how today's prototype differs from the chapter;
- design work the chapter leaves to the implementation;
- the coding roadmap.

The implementation follows all three of the chapter's assumptions, (A1)–(A3).

## The algorithm

### Inputs and the running example

The view `V = C ⋈ O ⋈ L` is maintained as two joins:

- `J = O ⋈ C` on `(custkey)`;
- `V = L ⋈ J` on `(custkey, orderkey)`.

Each join applies DBSP's bilinear rule, `Δ(R ⋈ S) = ΔR ⋈ S⁺ + R⁻ ⋈ ΔS`. In each term, the delta factor is the *driving delta*: its rows select which partner rows to read. The two joins need four partner reads.

| Read | Version | Driving delta | Kind | Entries taken | Today's reader |
| --- | --- | --- | --- | --- | --- |
| `C⁺` | after | `ΔO` | path | Customer at `(custkey = a)` | `customers_after` |
| `O⁻` | before | `ΔC` | range | Orders below `(custkey = a)` | `orders_before` |
| `J⁺` | after | `ΔL` | path | Customer and Orders on the path to `(a, b)` | `joined_orders_after` |
| `L⁻` | before | `ΔJ` | range | Lineitem below `((custkey, orderkey) = (a, b))` | `lineitems_before` |

A *path read* takes the entries on the root-to-node path of the instance tree. A *range read* takes the node's subtree, restricted to the read's own streams (Lemma `lem:partner`). `J⁺` is an intermediate join result, but it is a path read of stored entries, so `J` is never stored.

Index maintenance runs before view maintenance and supplies two inputs:

- `Old`, a snapshot of the index before the transaction;
- `Dlt`, the transaction's signed changes to the index entries, sorted by folded key, so that the index after the transaction is `Old + Dlt`.

`Dlt` holds every changed entry at its full folded key. A replaced payload appears as its old entry with weight −1 and its new one with weight +1. When an order changes customer, every entry in its subtree gets a new folded key, because those keys begin with `custkey`. `Dlt` then deletes each line item under the old customer and inserts it under the new one, although SQL's Lineitem rows did not change (`rem:rekey`, `ex:move-delta`). The base deltas `ΔC`, `ΔO`, and `ΔL` are the parts of `Dlt` for each stream. Derived deltas, such as `ΔJ`, are computed by the view's joins.

### Assumptions

- **(A1) Join keys on the trie.** Every join's key is a key of the trie and a prefix of the keys its inputs are sorted on. A join column outside this key is a *residual join column*, which only filters within the key's group. Q3 meets (A1) by extending `V`'s key from `orderkey` to `(custkey, orderkey)` through the functional dependency.
- **(A2) Complete deltas.** The inputs are `Old` and `Dlt`, so every base delta row has its full folded key and all base deltas are known before the scan. Every derived row keeps the key values at which its consumers read it.
- **(A3) Available requests.** (i) Every request names entries that are still ahead of the cursor or kept for its consumer. (ii) Before the session passes a child's range, it either knows every request that can name an entry there, or it enters the child to compute the rows from which such requests can arise.

(A3) must be shown for each circuit; the pre-order of the readers' keys does not imply it. Only selective execution needs it.

### Locality

Call a `custkey` value `a` *touched* if some delta row has it. All work of a transaction lies inside `Range(custkey = a)` for the touched `a`, and each touched customer's work is independent (Lemma `lem:locality`).

One snapshot serves both versions (Proposition `prop:overlay`):

- a before read takes `Old`;
- an after read takes `Old ⊎ Dlt`, unconsolidated, because a join is bilinear;
- a path read over two nodes, such as `J⁺`, multiplies the weights of its two entries, giving four terms that are equal as a Z-set.

Only `Old` is read from storage, so it must be one pinned logical snapshot.

### Buffered execution

For each touched customer, buffered execution:

1. collects the old participating entries of the customer's range once, one buffer per stream;
2. adds that customer's base deltas;
3. evaluates the delta rules over the buffers, in the circuit's order;
4. releases the buffers when every consumer has finished.

The pseudocode is `alg:ivm-buffered`:

```text
MaintainGroup(a):
  collect C⁻, O⁻, L⁻ of customer a, with the rows of ΔC, ΔO, ΔL for a
  C⁺, O⁺, L⁺ ← the buffers plus their deltas
  ΔJ ← ΔO ⋈ C⁺ + O⁻ ⋈ ΔC
  read the needed rows of J⁺ = O⁺ ⋈ C⁺ as paths in the buffers
  emit ΔL ⋈ J⁺ + L⁻ ⋈ ΔJ, with signed weights
  release the group's buffers
```

Under (A1) and (A2), this computes the view change exactly, and it reads each collected old entry once (Proposition `prop:group-delta`). The same holds for any circuit of joins whose partner reads are path or range reads. Buffered execution therefore needs no (A3), and it is the fallback wherever (A3) has not been shown.

### Selective execution

Selective execution visits only the instance nodes that requests need, and keeps only the entries that later requests can still name.

- **Requests.** A driving row makes a *request* to a reader, anchored at the instance node of the row's values cut to the reader's key.
  - A base delta row makes a *static* request, known before the scan.
  - A derived row makes a *dynamic* request when it is computed.
- **Order of requests.** The session keeps pending requests ordered by their nodes' folded-key prefixes, which is record order. Each reader's static requests arrive sorted, because `Dlt` is sorted; the readers' requests are then merged. Deciding whether a child is needed is a prefix lookup among the pending requests.
- **Ready pairs.** A pair of a delta term is *ready* when all its rows have been read. At each node, the session computes every ready pair and makes the requests its rows produce, before choosing a child. It repeats while new rows make further pairs ready.
- **Delivery.** A derived row is delivered at the node where its next consumer reads it, and it waits there until the cursor arrives. That node need not be at the producing join's key. `J` joins on `(custkey)`, but its rows carry the `orderkey` at which `L⁻` reads them.
- **Steering.** Reader `ρ'` *steers* reader `ρ` when entries `ρ'` returns are combined into a derived row that requests `ρ`. In Q3, `C⁺` and `O⁻` steer `L⁻` through `ΔJ`. A request is *late* if an entry it needs was already passed and not kept; otherwise it is *in time*.
- **Loop order.** In `O⁻ ⋈ ΔC`, the term loops over the orders outside and the customer's delta rows inside. The other order would read the orders once per delta row, which a forward cursor cannot do.

The index reads and the visit are `alg:session-requests` and `alg:visit`:

```text
Advance(n):
  if the cursor precedes prefix(n): seek forward to it
  read the old entries attached at n once, with n's delta entries
  keep the entries that the current path and pending consumers need

AtRangeEnd(n):
  return whether the next folded key lies outside Range(n)

Visit(n):
  Advance(n)
  deliver n's entries to the readers that requested them, in their versions
  compute the ready pairs and path reads; deliver derived rows; make their requests
  for each child m, in the old range or among the delta and request keys, in record order:
    if a request needs m, or m must be visited to compute requests:  Visit(m)
    else:  skip m's range in Old, one seek for adjacent skipped children when known
    compute any newly ready pairs
  finish the readers whose ranges end here; release entries whose consumers finished
```

A node that exists only in `Dlt`, such as a new order, needs no read of `Old`.

The chapter's results:

- **Lemma `lem:requests`.** Under (A1)–(A3) and an acyclic circuit, a traversal that computes all ready pairs before it advances serves every request without seeking back.
- **Lemma `lem:interleave`.** The rows emitted in traversal order sum to the same Z-set as the joins' delta rules evaluated in the circuit's order.
- **Theorem `thm:shared`.**
  - The cursor over `Old` moves only forward.
  - Each old entry reached is decoded once and shared by every reader that takes it.
  - A range that no request needs is passed by one forward seek.
  - The output is exact.
  - For Q3, the kept old entries are those on the current path. Deltas, waiting rows, and output are kept besides.

### Why Q3 satisfies (A3)

All static requests are known before the scan. When `O⁻` reads an old order, the order's `ΔJ` rows are computed before the visit enters its line items, and their `L⁻` requests name that order's range only. A `ΔO` row paired with `C⁺` makes its request when the customer is read. `J⁺` reads only the customer and order on the current path. So the session passes an order unvisited only if no request names it and no reader of Orders must visit it to compute a request.

### Two circuits that violate (A3)

- **A coarse request (`ex:coarse-request`).** Join `J` with `L` on `(custkey)` alone. A customer update derives one `ΔJ` row per order, and each requests all of the customer's line items. The second order's request then names line items already passed: late unless kept.
- **Sibling tag order (`fig:turns`).** `W = Invoice ⋈ (L ⋈ O)` on `(custkey)`, with `invoicekey` as a residual join column. Under COL's tags, invoices precede orders, so requests from changed orders name invoices already passed. Swapping the tags makes them in time, but tag order alone does not prove (A3).

### Worked transaction

The transaction replaces `c5` by `c5'` and inserts order `o5` with line items `l10` and `l11`. This is a condensed form of `tab:interleave`.

| Cursor at | Readers | `ΔJ` | `ΔV` |
| --- | --- | --- | --- |
| `(custkey = c5)`, decodes `c5` | `C⁺` takes `c5` with `−c5`, `+c5'`; `O⁻` opens | 3 rows at `(c5, o5)`, waiting | — |
| `v2`, `v8` | passed by one seek | — | — |
| `(c5, o3)`, decodes `o3` | `O⁻` takes `o3`; `L⁻` opens | `−(c5, o3)`, `+(c5', o3)` | — |
| `l7`, `l8` | `L⁻` takes them | — | 4 rows |
| `(c5, o4)`, decodes `o4`, `l9` | `O⁻`, then `L⁻` | 2 rows | 2 rows |
| `(c5, o5)`, `Dlt` only | `L⁻` finds no old items; `J⁺` takes the path | — | 6 rows |
| end of `c5` | `O⁻` closes | — | complete |

Two forward seeks serve all four reads. Each of `c5`, `o3`, `l7`, `l8`, `o4`, and `l9` is decoded once from `Old`. `v2`, `v8`, and `c6` are never decoded. `ΔV` leaves the session unconsolidated, and DBSP's consolidation of the output batch removes the pairs of opposite weight.

### The boundary with DBSP

In DBSP, each join is an operator of its own, scheduled separately. Each receives its whole driving delta at the commit flush. The session therefore runs together all joins whose partner reads share its traversal, with any selections and projections between them. It receives the base deltas and the overlay, and it emits ordinary weighted deltas. DBSP's scheduler is unchanged. Operators downstream of the session's last join run as usual.

## Audit of the write-up

These points were checked against the code on 2026-11-03. Each is proposed as an edit in the thesis repository's `BUFFER.md` (entry of 2026-11-03), not applied to the `.tex`.

From the code:

1. **`tab:prototype`, row "Moved line items".** The row says "Supplied by the transaction", which is false. The prototype rejects owner-key changes and replacement Orders deltas (`BaseState::validate_inner`). Merge the row into "Owner-key change: Rejected".
2. **"Q3 as compiled today".** The sentence "Sorting each reader's driving delta keeps that reader moving forward" is wrong:
   - `scan` opens a fresh cursor and seeks for every requested key;
   - `J⁺` and `L⁻` request by `orderkey`, which is not the index's order.
3. **`tab:prototype`, row "Seeks".** Add that `J⁺` scans the customer once per pair of order and placement customer.

Wording only:

1. **Cost wording.** Applies to `thm:shared`(iii), the worked traversal, and its "Two seeks". Say that a seek is logical: on a snapshot of k runs it costs k physical seeks. Say also that ending a range reads the next key without decoding it; `c6`'s key is read. A storage-agnostic footnote suffices.
2. **The algorithm note of `alg:session-requests`.** "Even after the cursor over `Old` has passed its key" is ambiguous. A delta-only node has no old entries, so the cursor over `Old` may already stand past its prefix.
3. **(A2)'s last clause.** "Every derived row keeps the key values at which its consumers read it" is a requirement on the circuit, not on the inputs. Consider stating it beside (A1).
4. **Buffered execution and the DBSP boundary.** "Boundary with DBSP" appears only in `sec:ivm-interleave`. Buffered execution also needs `J` and `V` in one operator. Otherwise `V` collects each touched customer a second time, against `prop:group-delta`'s "each collected old entry is read once".

None of these points changes the algorithm.

## The write-up's concepts in Feldera code

The facts were checked against the code on 2026-11-03.

| Concept | Feldera today | Gap |
| --- | --- | --- |
| `Old` | `ManualIndex::append` takes `before` from `trace.ro_snapshot()`. A `SpineSnapshot` holds shared references to its runs, so a later compaction does not change it (`prop:overlay`(iii)). | None. |
| `Dlt` | `ManualIndex`'s `Request::Append` handler builds the run with `build_batch`, moves it into `trace.insert`, and keeps no copy. | Keep the run and publish it. |
| a node's prefix; a child domain's prefix | `KeyEncoder::prefix` and `then_tag` in `crates/dbsp/src/trace/merged_index/mod.rs`. The folded key is `[tag enc(value)]* 0 id`. | None. |
| seek past a prefix | Only `Cursor::seek_key`, which is forward-only. No prefix successor exists. | A byte-string successor, with carry and the all-`0xFF` case. |
| a child's prefix length | Value widths exist only as `OrderedKey::WIDTH` on Rust types. `Domain` has a name and columns only. | A width per domain in `KeyTrie` and `MergedIndexRustEmitter`. |
| `Advance` (seek) | `CursorList::seek_key` seeks every run, then re-merges. Zero-weight skipping reads each run's weight. | Count logical seeks and per-run seeks. |
| `AtRangeEnd` | `key().starts_with(prefix)` after `step_key`, as in `scan` and the merged-index tests. | None. |
| decode | `unfold` and `decode_payload` in the client crate. A file batch's `move_key` also deserializes the key and its first value on every key move. Only a seek compares archived bytes without deserializing. | "Never decoded" holds only for entries passed by a seek. Count key visits, payload decodes, and storage deserializations separately. |
| readers and terms | The four functions in `merged-index-col/src/q3.rs`. Each opens its own cursor for every requested key; no two readers share a cursor. | Readers driven by requests. |
| session operator | `add_ternary_operator` and `add_quaternary_operator` (`circuit_builder.rs`). Existing users include `GroupTransform` (ternary) and `AsofJoin` (quaternary). Accumulators in front supply each whole delta at the flush. Output is chunked by `splitter_output_chunk_size`. | A new operator. |
| snapshot input | Before each transaction the harness puts the snapshots in a shared slot (`Q3State::publish`). The join's read functions take them from that slot at the flush. | The same pattern, holding `(Old, Dlt)`. |
| joins in one operator | A DBSP operator flushes only after all its predecessors have finished flushing (`FlushState::UnflushedDependencies` in `dynamic_scheduler.rs`). `V`'s join therefore starts only after `J`'s has passed every touched customer. | Both executions need `J` and `V` in one operator. |
| `Dlt` complete under (A2) | The base transaction runs first. `prepare` folds its net delta, and `ManualIndex::append` builds the run before the view transaction starts. For RF1/RF2 each line item finds its customer through its order's row in the same transaction. | The run is not published, and the view circuit reads the SQL net delta instead. Feed the view side from `Dlt`; no extra owner lookup is needed. |
| counters | Buffer-cache hits and misses, and Bloom and range filter statistics. No seek, step, or decode counters. | Add them. |

The "joins in one operator" row also explains why DBSP's own operators cannot deliver deltas per instance node. The companion note `key-trie-execution-refinements.tex` develops that point as condition (P), for a circuit that would pipeline its own joins. Chapter 3 applies the same flush rule to the session. Buffered execution needs it too: `prop:group-delta` reads each old entry once only if both joins process a group while its buffers are held.

## Current implementation against Chapter 3

| Aspect | Prototype today | Chapter 3 |
| --- | --- | --- |
| `V`'s key | `orderkey` alone, so (A1) fails | `(custkey, orderkey)` |
| View-side inputs | SQL net deltas, plus the order → customer placements from the same transaction's Orders rows | Base deltas from `Dlt`, each row at its full folded key (A2) |
| Versions | Two snapshots, `before` and `after` | `Old`, overlaid with `Dlt` |
| Delta delivery | Each join receives its whole driving delta at its own flush | Per instance node, inside one session |
| Cursors | A fresh cursor and seek per requested key; no cursor shared between readers | One forward cursor |
| `O⁻` | Scans the customer's whole range and decodes its line items, keeping only Orders | Visits only the orders |
| `J⁺` | Scans the customer once per pair of order and placement customer | Reads the kept path |
| Request order | `J⁺` and `L⁻` request by `orderkey`, which is not index order | Record order of the instance nodes |
| Changes admitted | RF1/RF2 only. Customers change only at the initial load; changing an order's customer is rejected | Any change, given a complete `Dlt` |
| Counters | None | Needed for every cost claim |

## Design work the write-up leaves to the implementation

These are implementation questions, not algorithm questions:

- **Publishing `Dlt`.** Keep the run that the base phase already builds, publish it with `Old`, and feed the view side from it.
- **The cost model.** A logical seek costs one seek per run of the snapshot, and a key move on a file batch deserializes a value. The counters must separate the chapter's units from storage costs.
- **Delta-only nodes.** Merge `Old`'s children with `Dlt`'s and the requests' keys in record order.
- **The join-plan type.** Both executions interpret it: the joins with their keys and delta terms, each term's reader (stream, version, path or range), and any selections and projections between them.
- **Output.** Emit chunks of the size DBSP expects, and leave consolidation to DBSP's batcher.
- **Workers.** One worker, as decided on 2026-10-12. Sharding by `custkey` is deferred.

## Coding roadmap

The steps are ordered. Each has an exit check and updates its docs. They are not R-numbered, since R1–R5 is the only numbering scheme.

1. **Counters on today's readers.**
   - Count cursor opens, logical seeks, per-run seeks, key visits, and payload decodes.
   - *Exit:* a test pins the counts on the worked transaction. They become the baseline.
2. **Instance-node cursor in `merged_index`.**
   - Add a prefix successor, a width per domain, and a cursor that enters a node, lists its attached entries, steps through its children, and skips a subtree, counting as it goes.
   - *Exit:* property tests against brute-force prefix scans over random tries, data, and multi-run snapshots, including carry and an all-`0xFF` prefix.
3. **Publish `(Old, Dlt)`, and read the after state as the overlay.**
   - Rewrite the four readers to read `Old ⊎ Dlt` for the after state.
   - *Exit:* `check_readers` and the transcript comparison pass, and the after snapshot is gone.
4. **Hand-written view circuit.**
   - The view side reads its inputs from `Dlt` (still RF1/RF2), each row keyed by its full trie path.
   - The joins are keyed by `(custkey)` and `(custkey, orderkey)`, still on today's snapshot-reader joins.
   - The placement map is dropped.
   - *Exit:* the transcript comparison with native mode passes.
5. **Buffered-execution session.**
   - The join-plan type, plus one operator that collects each touched customer once and evaluates the plan's delta terms over its buffers. It replaces step 4's two joins.
   - It is exact for any plan with path or range reads, and it remains the fallback when (A3) has not been shown.
   - *Exit:* the counters show each old entry read once; the transcript comparison passes.
6. **Selective-execution session.**
   - Requests, ready pairs, and `Visit`, over step 5's plan type, for plans that satisfy (A3).
   - *Exit:* on the worked transaction, the counters show two forward seeks and the decodes of `thm:shared`. A property test over random RF1/RF2 sequences matches native mode, and the transcript comparison passes.
7. **Measurement, new version.**
   - Add the counters to `measure.py`, and run the native, buffered, and selective modes on Linux.
   - *Exit:* raw artifacts under `target/`; [measurement.md](measurement.md) reports base and IVM side by side.
8. **Beyond RF1/RF2 (optional).**
   - Customer updates, line items added to existing orders, and orders that change customer.
   - Each needs index maintenance to produce a complete `Dlt`. That is base-phase work; view maintenance is unchanged.
   - *Exit:* new transcript fixtures pass in every mode.
9. **Deferred:**
   - plans derived from the spec;
   - more than one worker, sharded by `custkey`;
   - groupings, whose state the chapter leaves for later work.

## Unconfirmed choices

- The session as one DBSP operator over the ternary streaming interface, or the quaternary one for wider plans.
- `ΔJ` internal to the session unless a plan exports it.
- Counters in the instance-node cursor, not in the generic `Cursor`.
- Step 4 as an intermediate milestone before the session.
- Buffered execution (step 5) before selective execution.
- Counting per-run seeks and storage deserializations, besides the chapter's logical seeks and decodes.
- Proposing the write-up fixes in the thesis `BUFFER.md`, rather than editing the `.tex`.
- [decisions.md](decisions.md) (2026-10-29) still names A3 and A4 in the write-up's earlier sense. Rewording those entries to (A1)–(A3) is proposed, not done.
- Changes beyond RF1/RF2 kept out of scope until step 8.
