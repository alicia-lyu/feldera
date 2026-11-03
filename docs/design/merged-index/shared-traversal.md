# Shared partner reads: one scan session per transaction

> [!NOTE]
> **Source:** Chapter 3 of `docs/key-trie-laminar-ranges.tex` in the thesis repository, "Interesting orderings for view maintenance: driving deltas". This doc cites the chapter by label, since its numbers drift: `sec:ivm`, `sec:ivm-assume`, `sec:ivm-reads`, `sec:ivm-buffered`, `sec:ivm-traversal`, `sec:ivm-interleave`, `sec:ivm-relax`, and the appendices `sec:overlay` and `sec:prototype`.
>
> **Notation:** `X⁻` and `X⁺` are the states before and after the transaction. `Δ` is a transaction delta, and a "run" is an immutable sorted collection. A derived row is named by its instance node, such as `((custkey, orderkey) = (c5, o5))`.
>
> **Status:** the algorithm is settled in the write-up; nothing below is implemented. This doc is the meta plan for the coding steps. Choices marked *unconfirmed* await the user. The confirmed ones are in [decisions.md](decisions.md) under 2026-10-29 and 2026-11-03.

This doc covers:

- the algorithm as Chapter 3 states it;
- an audit of the chapter against the code;
- how the chapter's concepts map onto Feldera code;
- how today's prototype differs from the chapter;
- the scan session that both executions share;
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

Buffered execution needs no order of requests. It needs only the delta grouped by touched customer; within a group, the buffers serve reads in any order.

### Selective execution

Selective execution visits only the instance nodes that requests need, and keeps only the entries that later requests can still name.

- **Requests.** A driving row makes a *request* to a reader, anchored at the instance node of the row's values cut to the reader's key.
  - A base delta row makes a *static* request, known before the scan.
  - A derived row makes a *dynamic* request when it is computed.
- **Order of requests.** Pending requests are kept in record order of their nodes (see [Ordered requests](#ordered-requests)).
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

#### Ordered requests

Only selective execution orders its requests (`fig:q3-requests`, drawn in `figures/deck/q3_ordered_requests.tex`). The session keeps its pending requests ordered by their nodes' folded-key prefixes, which is record order with ancestors first. The first request gives the first seek. Deciding whether a child is needed is then a prefix lookup among the pending requests.

A request's node anchors what it reads, but it is not always where the reads lie:

- a path read takes the node's entries and its ancestors', decoded on the way down and kept on the path;
- a range read takes the node's subtree, restricted to its own streams. So `O⁻`'s request at `(c5)` reads `o3` and `o4`, not `c5`.

The worked transaction's requests, in record order:

| Node | Reader | Reads | From |
| --- | --- | --- | --- |
| `(c5)` | `C⁺` | path: `c5` | `+o5` in `ΔO` |
| `(c5)` | `O⁻` | range: `o3`, `o4` | `−c5`, `+c5'` in `ΔC` |
| `(c5, o3)` | `L⁻` | range: `l7`, `l8` | `ΔJ` at `o3` (dynamic) |
| `(c5, o4)` | `L⁻` | range: `l9` | `ΔJ` at `o4` (dynamic) |
| `(c5, o5)` | `J⁺` | path: `c5`, `o5` | `+l10`, `+l11` in `ΔL` |
| `(c5, o5)` | `L⁻` | range: none | `ΔJ` at `o5` (dynamic) |

Each reader's static requests arrive sorted, since cutting sorted folded keys at one key keeps their order. Readers cut at different keys, though, so a later delta row can request an earlier node. A `ΔL` row at `(c5, o3, l7)` requests `J⁺` at `(c5, o3)`, while a later `ΔO` row at `(c5, o4)` requests `C⁺` at `(c5)`. The session therefore merges the readers' static requests.

Dynamic requests join the same order when their rows are derived. Under (A3), a dynamic request at or below the current node lies at or ahead of the cursor. A path request at an ancestor is served at once from the kept path.

The delta's own order is a convenience, not a requirement. A sorted `Dlt` gives each reader's static requests in order, so the session merges them instead of sorting. It also lets `Visit` merge `Old`'s children with `Dlt`'s in one forward pass. An unsorted delta would only need one sort up front. In Feldera, `build_batch` already sorts and consolidates the run, so `Dlt` arrives sorted at no extra cost.

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
4. **Buffered execution and the DBSP boundary.** *Applied on 2026-11-03,* in a revised form: `sec:ivm-buffered` now has the paragraph "Buffers shared across operators", after `prop:group-delta`'s sketch.
   - `J` and `V` can stay separate DBSP operators that hold references to the same group buffers, so each old entry is still read once.
   - DBSP flushes `V` only after `J` has passed every touched customer, so the buffers of all touched groups stay until `V` finishes.
   - Releasing each group before the next is collected needs the two joins to take turns per group, as the session does.

Noted here only, not yet in `BUFFER.md`:

1. **`alg:session` reads every entry of a group.** Q3's invoices are read but buffered nowhere. A storage engine that decodes on landing then decodes them. The collection could seek past domains no reader uses, as `Visit` already does. In this doc's [scan session](#the-scan-session), buffered execution makes one group request per touched root, which passes by seeking the domains the plan never reads, so the invoices are not decoded.

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
| `Advance` (seek) | `CursorList::seek_key` seeks every run, then re-merges. Zero-weight skipping reads each run's weight. | The session's `enter`, which counts logical seeks; the run cursors count per-run seeks. |
| pending requests | None. Each of today's readers loops over its own driving delta's keys. | The session's request queue, in record order. |
| `AtRangeEnd` | `key().starts_with(prefix)` after `step_key`, as in `scan` and the merged-index tests. | None. |
| decode | `unfold` and `decode_payload` in the client crate. `FileIndexedWSetCursor::move_key` also positions the value cursor, which deserializes the first value, every time the cursor lands on a key. A seek compares archived bytes while it searches, but it still lands on a key. | See [No extra decoding](#no-extra-decoding). |
| readers and terms | The four functions in `merged-index-col/src/q3.rs`. Each opens its own cursor for every requested key; no two readers share a cursor. | Readers driven by requests. |
| session operator | `add_ternary_operator` and `add_quaternary_operator` (`circuit_builder.rs`). Existing users include `GroupTransform` (ternary) and `AsofJoin` (quaternary). Accumulators in front supply each whole delta at the flush. Output is chunked by `splitter_output_chunk_size`. | A new operator. |
| snapshot input | Before each transaction the harness puts the snapshots in a shared slot (`Q3State::publish`). The join's read functions take them from that slot at the flush. | The same pattern, holding `(Old, Dlt)`. |
| joins taking turns | A DBSP operator flushes only after all its predecessors have finished flushing (`FlushState::UnflushedDependencies` in `dynamic_scheduler.rs`). `V`'s join therefore starts only after `J`'s has passed every touched customer. | Selective execution needs `J` and `V` in one session. Buffered execution can keep them as separate operators that share the group buffers, but then it holds every touched group until `V` finishes. |
| `Dlt` complete under (A2) | The base transaction runs first. `prepare` folds its net delta, and `ManualIndex::append` builds the run before the view transaction starts. For RF1/RF2 each line item finds its customer through its order's row in the same transaction. | The run is not published, and the view circuit reads the SQL net delta instead. Feed the view side from `Dlt`; no extra owner lookup is needed. |
| counters | Buffer-cache hits and misses, and Bloom and range filter statistics. No seek, step, or decode counters. | Run counters in DBSP's run cursors (step 1); logical counters in the scan session (step 2). |

The flush rule in the "joins taking turns" row also explains why DBSP's own operators cannot deliver deltas per instance node. The companion note `key-trie-execution-refinements.tex` develops that point as condition (P), for a circuit that would pipeline its own joins. Chapter 3 applies the rule twice:

- for selective execution, in "Boundary with DBSP";
- for buffered execution, in "Buffers shared across operators".

The two cases differ:

- **Buffered execution.** Separate operators still read each old entry once. What they lose is the memory bound of one group.
- **Selective execution.** Separate operators lose the traversal itself, since `V`'s requests arise only after `J` has passed the touched ranges.

## No extra decoding

`thm:shared` promises that each old entry reached is decoded once, and that a range no request needs is not decoded at all. Feldera's storage keeps that promise only if the session obeys the rules below. Otherwise the storage layer decodes entries that no reader takes.

### What counts as a decode

| Level | Where | Allowed for |
| --- | --- | --- |
| Key read | Storage deserializes a key when the cursor lands on it. | The keys the cursor must land on: taken entries, the first key at or past each seek target in each run, and one key past each range's end. |
| Value read | Storage deserializes a value, today on every landing (`move_key`). | Taken entries only, once each. |
| Payload decode | The client's `decode_payload`, plus `unfold` if the full key is parsed. | Taken entries only, once each, shared by every reader that takes the entry. |

An entry is *taken* when some reader with a request delivers it to its delta term. In the worked transaction, the taken old entries are `c5`, `o3`, `l7`, `l8`, `o4`, and `l9`. `v2`, `v8`, and `c6` get no value read and no payload decode. Only `c6`'s key is read, to end `c5`'s range.

### Rules

1. **Pass by seeking, never by stepping.** The session steps from one key to the next only when the next entry will be taken. It steps through a node's children one by one only while a reader takes every child, as `O⁻` does once it opens. Otherwise it computes the next position it needs and seeks straight to it:
   - a child domain's prefix (`then_tag`), which passes earlier domains: from `c5`, a seek to the orders passes `v2` and `v8`;
   - a pending request's node prefix;
   - the prefix successor past the current node, which passes a subtree no request needs and lands on the next sibling.

   `Visit` uses these seeks to pass children it does not enter; it never steps through them.
2. **Read values lazily in storage.** `FileIndexedWSetCursor::move_key` must stop positioning the value cursor on every landing. It should position the value cursor the first time `val`, `weight`, or `map_times` is called. Without this, every landing that is not a take costs a value read: each range-end key, and the run heads past each seek. This is a change in `crates/dbsp`. It must keep the `Cursor` contract and pass the existing trace tests.
3. **Consolidate only taken entries.** `CursorList` reads every run's weight at each landing in order to skip zero-weight keys, which positions values (`is_zero_weight`, `skip_zero_weight_keys_forward`). The session needs a merging cursor over `Old`'s runs that exposes keys at a landing and sums weights only when it takes the entry. An entry whose weights cancel across runs, such as a deletion not yet compacted, is read once and dropped. This is an inherent cost of reading an uncompacted run set, and it is counted, not hidden.
4. **Decode payloads once, and only for taken entries.** Route an entry to its readers from its key bytes: the prefix, the tag, and the placement id. Do this without `unfold` or `decode_payload`, and decode only when some reader takes the entry. Hand the one decoded entry to every reader that takes it: `C⁺` and `J⁺` share `c5`. Today's `scan` unfolds and decodes every entry under its prefix and then filters, which breaks this rule.
5. **Buffered collection seeks too.** `alg:session` reads every entry of a group, including streams no reader uses, such as Q3's invoices. In Feldera, that decodes them. Buffered execution reads through the [scan session](#the-scan-session), whose range requests name their streams. The session seeks past the subtrees of domains that no request names (rule 1).
6. **Read `Dlt` once.** `Dlt` is an in-memory batch, so its values are already deserialized. Its payloads are decoded once, at the node where the session reaches them.

### How the counters check the rules

The counters count each level separately, together with logical seeks and per-run seeks. DBSP's run cursors count per-run seeks, key reads, value reads, and weight reads (roadmap step 1). The scan session counts cursor opens, logical seeks, and payload decodes (step 2). The exit checks of steps 2, 5, and 6 then assert two things:

- value reads and payload decodes from `Old` equal the number of distinct old entries taken, plus the counted cancelled entries of rule 3;
- key reads are at most the taken entries, plus one per run per seek, plus one per range end.

On the worked transaction this means exactly six value reads and six payload decodes from `Old`.

## The scan session

Buffered and selective execution share one scan session, which each execution uses or extends (user decision of 2026-11-03). The session reads the index. The executions decide what it should read, through the requests they give it.

### Inputs

- `Old`, one pinned snapshot.
- `Dlt`, the transaction's sorted run.
- The requests, in record order of their nodes, supplied up front by the execution.

A selective request is a pair: a reader and an instance node, given by its key prefix. `O⁻`'s request at `c5` is `(O⁻, (custkey = c5))`. The plan fixes the rest for each reader:

| Fixed by the reader | `O⁻` | `C⁺` |
| --- | --- | --- |
| kind: path or range | range | path |
| version: before or after | before | after |
| streams: the index streams the read takes | Orders | Customer |

The kind alone does not suffice: readers of the same kind can differ in version and streams.

A request carries no rows. The driving rows of a static request stay in `Dlt`, which the session reads through its own forward cursor. The driving rows of a dynamic request stay with the driver that derived them. The requests tell the session what to read and what to pass. Each reader's streams are known, so a domain that no pending request's reader takes is passed by seeking. No separate list of participating domains is needed.

### Requests per execution

- **Selective execution** supplies the static requests, merged across readers, and adds a dynamic request whenever it derives a row.
- **Buffered execution** buffers each touched customer's whole group, so its readers do not matter to the session. It supplies one *group request* per touched root, such as `(custkey = c5)`, taken from `Dlt`'s roots. A group request reads the root's subtree in `Old` and its delta entries in `Dlt`. It passes by seeking only the domains whose streams the plan never reads, such as Q3's invoices, so that they are not decoded (rule 5).

No request of buffered execution can be late, since the group buffer keeps the whole group (compare `ex:coarse-request`).

### The logical overlay

The session never merges `Old` and `Dlt` into a run. It keeps three sorted forward streams: the cursor over `Old`, a cursor over `Dlt`, and the request queue. It compares them by key bytes. Nothing is consolidated between `Old` and `Dlt` either, since the tagged overlay `Old ⊎ Dlt` stays unconsolidated (`prop:overlay`). Each step costs one key comparison, on keys the cursors have already read.

In Q3, `Old`'s children never interleave with `Dlt`'s:

- The after reads, `C⁺` and `J⁺`, are path reads. Each takes the entries at one node from both cursors, so it positions both cursors and walks no children.
- The before reads, `O⁻` and `L⁻`, are range reads of `Old` only.
- A delta-only node, such as `(c5, o5)`, is reached through its request, never by walking `Old`'s children.

Only an after-version range read steps the two cursors together within a range. Q3 has none.

### Operations

```text
ScanSession(Old, Dlt, requests):         requests in record order of their nodes
  next():        the first pending request
  enter(n):      seek the cursors over Old and Dlt forward to prefix(n), if behind;
                 one logical seek
  entries(n):    n's attached entries from both cursors, as key bytes,
                 each tagged with its version and weight
  decode(e):     decode e's payload at its first take; later takes reuse it;
                 one payload decode
  range(n, r):   step through Range(n) for request r: in Old, and in Dlt if r reads
                 the after version or is a group request; seek past the domains whose
                 streams r's reader does not take (for a group request, that the plan never reads)
  skip(n):       seek past Range(n), to prefix(n)'s successor
  add(r):        insert a dynamic request in record order

Buffered(session):
  for each group request at a touched root a, in record order:
    enter(a); read a's subtree through entries and range, filling one buffer per stream
    add a's delta rows; evaluate the delta rules over the buffers; emit
    release a's buffers

Selective(session):
  for each touched root a, in record order:  Visit(a)
  Visit uses enter, entries, range, and skip,
  and calls add for each request that a derived row makes
```

### Ownership

| Owner | Holds |
| --- | --- |
| The session | The cursors over `Old` and `Dlt`, the request queue, the decoded entries of the current node, and the logical counters: cursor opens, logical seeks, and payload decodes |
| The buffered driver | The group buffers |
| The selective driver | The kept path, the ready pairs, and the derivation of dynamic requests |
| DBSP's run cursors | The run counters: per-run seeks, key reads, value reads, and weight reads |

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
- **The cost model.** A logical seek costs one seek per run of the snapshot. Decoding has three levels: key reads, value reads, and payload decodes. [No extra decoding](#no-extra-decoding) sets the rules and the counters that check them.
- **Delta-only nodes.** These are reached through their requests, by the [logical overlay](#the-logical-overlay): forward cursors over `Old` and `Dlt` compared by key bytes, with no merged run.
- **The join-plan type.** Both executions interpret it: the joins with their keys and delta terms, each term's reader (stream, version, path or range), and any selections and projections between them.
- **Output.** Emit chunks of the size DBSP expects, and leave consolidation to DBSP's batcher.
- **Workers.** One worker, as decided on 2026-10-12. Sharding by `custkey` is deferred.

## Coding roadmap

The steps are ordered. Each has an exit check and updates its docs. They are not R-numbered, since R1–R5 is the only numbering scheme.

1. **Run counters in DBSP.**
   - Per-run seeks in both run cursors, `VecIndexedWSetCursor` and `FileIndexedWSetCursor`. Key reads, value reads, and weight reads in the file cursor only, since only file runs deserialize.
   - A thread-local running total, read before and after a region on the same thread. The generic `Cursor` trait gains no counters.
   - *Exit:* DBSP unit tests pin the counts of single moves on an in-memory run and a file run, and one seek per run through `CursorList`. A COL test pins today's four readers on the worked transaction (without `v2` and `v8`, since COL has no invoices), over a file run plus an in-memory run. These storage-level counts are the baseline.
2. **Decode-free storage and the scan session.**
   - Lazy value positioning in `FileIndexedWSetCursor` (rule 2 of [No extra decoding](#no-extra-decoding)).
   - A merging cursor over `Old`'s runs that consolidates only taken entries (rule 3).
   - A prefix successor, and a width per domain.
   - The [scan session](#the-scan-session): requests in record order, the logical overlay of `Old` and `Dlt`, the operations `enter`, `entries`, `decode`, `range`, `skip`, and `add`, and the logical counters. It passes children and subtrees by seeking (rule 1).
   - *Exit:* property tests against brute-force prefix scans, over random tries, data, multi-run snapshots, and request sets, including carry and an all-`0xFF` prefix. Counter tests show no value read and no payload decode for any passed entry. The existing trace tests pass.
3. **Publish `(Old, Dlt)`, and read the after state as the overlay.**
   - Rewrite the four readers to read `Old ⊎ Dlt` for the after state.
   - *Exit:* `check_readers` and the transcript comparison pass, and the after snapshot is gone.
4. **Hand-written view circuit.**
   - The view side reads its inputs from `Dlt` (still RF1/RF2), each row keyed by its full trie path.
   - The joins are keyed by `(custkey)` and `(custkey, orderkey)`, still on today's snapshot-reader joins.
   - The placement map is dropped.
   - *Exit:* the transcript comparison with native mode passes.
5. **Buffered-execution session.**
   - The join-plan type, plus one operator that drives the scan session: it collects each touched customer once and evaluates the plan's delta terms over its buffers, the joins taking turns per group. Each group is released before the next is collected. It replaces step 4's two joins.
   - It supplies one group request per touched root ([Requests per execution](#requests-per-execution)).
   - It is exact for any plan with path or range reads, and it remains the fallback when (A3) has not been shown.
   - Collection seeks past domains no request names, and decodes only taken entries (rules 4 and 5).
   - *Exit:* the counters show each taken old entry read and decoded once, and none of Q3's invoices decoded. The transcript comparison passes.
6. **Selective-execution session.**
   - A second driver of the scan session: ready pairs, dynamic requests, the kept path, and `Visit`, over step 5's plan type, for plans that satisfy (A3).
   - *Exit:* on the worked transaction, the counters show two forward logical seeks, and exactly six value reads and six payload decodes from `Old`. A property test over random RF1/RF2 sequences matches native mode, and the transcript comparison passes.
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
- Step 4 as an intermediate milestone before the session.
- Lazy value positioning as a change to DBSP's file-batch cursor, not a merged-index-only cursor over the file format.
- Consolidating `Old` only at taken entries, through a merging cursor of the session's own, rather than reading `Old` through `CursorList`.
- Changes beyond RF1/RF2 kept out of scope until step 8.
- Step 5 as one operator with the joins taking turns per group, holding one group at a time. The alternative is separate operators sharing the group buffers: simpler, but holding every touched group until `V` finishes.
- A separate `weight_reads` run counter. The file format stores a value and its weight apart: landing on a value deserializes the value, and the weight is deserialized only when `weight`, `map_times`, or `map_values` asks for it. `CursorList`'s zero-weight skip reads weights at entries that no reader takes (rule 3), so a separate counter shows that cost. Folding weights into value reads would hide it.
