# Shared partner reads: one buffered scan session per transaction

> [!NOTE]
> **Source:** Chapters 2 and 3 of `docs/key-trie-merged-index-order.tex` in the thesis repository (renamed on 2026-11-09 from `key-trie-laminar-ranges.tex`). Chapter 2, "Interesting orderings for queries: one group at a time", gives the scan session (`sec:session`, `alg:session`, `thm:session`), the plan run over one buffered group (`sec:demand`, `alg:buffered-pipeline`), and skip scans (`sec:skip`, `alg:skip`, `lem:skip`). Chapter 3, "Interesting orderings for view maintenance: driving deltas", gives `sec:ivm`, `sec:ivm-assume`, `sec:ivm-reads` (`lem:partner`, `lem:locality`), `sec:ivm-buffered` (`alg:ivm-buffered`, `prop:group-delta`), `sec:ivm-relax`, and the appendices `sec:overlay` (`prop:overlay`) and `sec:prototype` (`tab:prototype`). This doc cites them by label, since numbers drift.
>
> **Scope:** the write-up now has buffered execution only. Selective execution (requests, `Visit`, assumption (A3)) moved to the companion note `key-trie-execution-refinements` (a PDF linked from the `.tex`). This doc follows the write-up; the removed material is summarized in [deferred.md](deferred.md#selective-execution).
>
> **Notation:** `X⁻` and `X⁺` are the states before and after the transaction. `Δ` is a transaction delta, and a "run" is an immutable sorted collection. A derived row is named by its instance node, such as `((custkey, orderkey) = (c5, o5))`.
>
> **Status:** the algorithm is settled in the write-up. Roadmap step 1, the run counters, is implemented and reported by the R5 harness in both modes. Step 2, decode-free storage and the buffered scan session, is implemented (2026-11-09), and in-memory runs count their accesses in separate fields (2026-11-10); nothing later is. This doc is the meta plan for the coding steps. Choices marked *unconfirmed* await the user. The confirmed ones are in [decisions.md](decisions.md) under 2026-10-29, 2026-11-03, and 2026-11-09.

This doc covers:

- the algorithm as Chapters 2 and 3 state it;
- an audit of the write-up against the code;
- how the write-up's concepts map onto Feldera code;
- why Feldera's storage needs two changes before the write-up's decode claims hold;
- the scan session that executes the algorithm;
- how today's prototype differs from the write-up;
- design work the write-up leaves to the implementation;
- the coding roadmap.

The implementation follows both of the write-up's assumptions, (A1) and (A2).

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

`Dlt` holds every changed entry at its full folded key. A replaced payload appears as its old entry with weight −1 and its new one with weight +1. When an order changes customer, every entry in its subtree gets a new folded key, because those keys begin with `custkey`. `Dlt` then deletes each line item under the old customer and inserts it under the new one, although SQL's Lineitem rows did not change (`ex:move-delta`). The base deltas `ΔC`, `ΔO`, and `ΔL` are the parts of `Dlt` for each stream. Derived deltas, such as `ΔJ`, are computed by the view's joins.

### Assumptions

- **(A1) Join keys on the trie.** Every join's key is a key of the trie and a prefix of the keys its inputs are sorted on. A join column outside this key is a *residual join column*, which only filters within the key's group. Q3 meets (A1) by extending `V`'s key from `orderkey` to `(custkey, orderkey)` through the functional dependency. The circuit must also keep, in every derived row, the key values at which its consumers read it: `J` joins on `(custkey)`, but `J⁺` is read as a path of a customer and an order.
- **(A2) Complete deltas.** The inputs are `Old` and `Dlt`, so every base delta row has its full folded key, moved entries included, and all base deltas are known before the scan.

### Locality

The *root domain* is the domain of the trie's top node, `custkey` in COL. Call a root-domain value `a` *touched* if some delta row has it, even when the row weights cancel (`−c5`, `+c5'` touches `c5`'s range). All work of a transaction lies inside `Range(custkey = a)` for the touched `a`, and each touched customer's work is independent (Lemma `lem:locality`).

One snapshot serves both versions (Proposition `prop:overlay`):

- a before read takes `Old`;
- an after read takes `Old ⊎ Dlt`, unconsolidated, because a join is bilinear;
- a path read over two nodes, such as `J⁺`, multiplies the weights of its two entries, giving four terms that are equal as a Z-set.

Only `Old` is read from storage, so it must be one pinned logical snapshot. The price of not consolidating is one pair of output rows of opposite weights per cancelled entry, which DBSP's consolidation of the output batch removes.

### Scanning and buffering one group

A *processing group* is one instance node of the pipeline's coarsest requirement, with the pipeline's participating entries in its range (`def:group`). For Q3 it is one customer: the customer, its orders, and their line items. Each participating stream has a buffer of its entries in record order. The scan session (`alg:session`) holds one cursor over the index, moving forward, and decodes each position once:

- `Peek` decodes the next position once and keeps it;
- `Step` takes it and advances;
- `Collect(group, inputs)` steps through the group's range and appends each entry of a stream in `inputs` to that stream's buffer.

`Skip(rejected)` (`alg:skip`) passes a rejected subtree by one forward `SkipPrefix` seek, when no remaining consumer needs the subtree (`lem:skip`). The rejected entry itself stays buffered until its consumers finish.

Theorem `thm:session`: the cursor only moves forward, each position it reaches is decoded once, each participating entry is read once, and every reader of a group is supplied without a backward seek. The session keeps the buffers until the plan's output for the group ends, then releases them.

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

Under (A1) and (A2), this computes the view change exactly, and it reads each collected old entry once (Proposition `prop:group-delta`). The same holds for any circuit of joins whose partner reads are path or range reads. Buffered execution needs no order of requests: it needs the delta grouped by touched customer, and within a group the buffers serve reads in any order. Groups that no delta row touches are passed, not read.

#### Buffers shared across operators

`J` and `V` can stay separate DBSP operators that hold references to the same group buffers, so each old entry is still read once. DBSP flushes an operator only after its predecessors have flushed, so `V` starts only after `J` has passed every touched customer. The buffers of all touched groups then stay until `V` finishes. Releasing each group before the next is collected needs the two joins to take turns per group, as the session's callback does ([The scan session](#the-scan-session)).

### Worked transaction

The transaction replaces `c5` by `c5'` and inserts order `o5` with line items `l10` and `l11`. The group of `c5` holds the old entries `c5`, `o3`, `o4`, `l7`, `l8`, `l9`, and the `Dlt` entries `−c5`, `+c5'`, `+o5`, `+l10`, `+l11`. The next group, `c6`, ends the range.

| Step | Session | Buffers | Evaluation |
| --- | --- | --- | --- |
| 1 | Pass to `(custkey = c5)`: one logical seek, landing on `c5` | — | — |
| 2 | Collect: step through `c5`, `o3`, `l7`, `l8`, `o4`, `l9`; read `c6`'s key, which ends the range | `C`: `c5`; `O`: `o3`, `o4`; `L`: `l7`, `l8`, `l9`; plus the `Dlt` entries of each stream, kept apart | — |
| 3 | Callback `on_group(c5, buffers)` | unchanged | `ΔJ` = 5 rows; `ΔV` = 8 rows (6 from `L⁻ ⋈ ΔJ`, 2 from `ΔL ⋈ J⁺`) |
| 4 | Release the buffers; pass to the next touched group, if any | released | — |

Each of `c5`, `o3`, `l7`, `l8`, `o4`, and `l9` is read from `Old` once, and `c6` only has its key read. `ΔV` leaves the callback unconsolidated, and DBSP's consolidation of the output batch removes the pairs of opposite weight. The write-up's Q3 also has invoices `v2` and `v8` under `c5`; they are positions the scan reaches and buffers nowhere. COL's tests have no invoices.

### The boundary with DBSP

In DBSP, each join is an operator of its own, scheduled separately. Each receives its whole driving delta at the commit flush. The session therefore runs inside one operator that holds the joins whose partner reads share its groups, with any selections and projections between them. The operator receives `Old` and `Dlt`, and emits ordinary weighted deltas. DBSP's scheduler is unchanged. Operators downstream of the last join run as usual.

## Audit of the write-up

These points were checked against the code on 2026-11-03 and against the renamed write-up on 2026-11-09. Each was proposed as an edit in the thesis repository's `BUFFER.md`; this repository does not edit the `.tex`.

Applied in the renamed write-up (checked 2026-11-09):

1. **`tab:prototype`, "Moved line items".** Merged into "Owner-key change: Rejected".
2. **"Q3 as compiled today".** The wrong sentence about sorting each reader's driving delta is gone. The text now says each read opens a fresh cursor and seeks for every key, and that `J⁺` and `L⁻` read by `orderkey`.
3. **`tab:prototype`, "Seeks".** It now says `J⁺` reads the customer again for each pair of order and placement customer.
4. **Buffers shared across operators.** In `sec:ivm-buffered`.
5. **(A2)'s last clause.** It now stands beside (A1) as a requirement on the circuit.

Still open (wording only):

1. **Cost wording.** `thm:session`, `lem:skip` ("`g` seeks"), and `tab:prototype` count logical seeks. On a snapshot of `k` runs a logical seek costs `k` physical seeks. Say so in a footnote. Say also that ending a range decodes the next group's first position (`c6`), which `Peek` keeps for the next group.
2. **"Every position decoded".** `thm:session`(i) and `alg:session` decode every position the loop reaches, "whatever its stream", because naming an entry's stream and key takes its decode. In Feldera a folded key names both, so a position needs only a key decode. [No extra decoding](#no-extra-decoding) states what Feldera adds.

None of these points changes the algorithm.

## The write-up's concepts in Feldera code

The facts were checked against the code on 2026-11-09.

| Concept | Feldera today | Gap |
| --- | --- | --- |
| `Old` | `ManualIndex::append` takes `before` from `trace.ro_snapshot()`. A `SpineSnapshot` holds shared references to its runs, so a later compaction does not change it (`prop:overlay`(iii)). | None. |
| `Dlt` | `ManualIndex`'s `Request::Append` handler builds the run with `build_batch`, moves it into `trace.insert`, and keeps no copy. | Keep the run and publish it (roadmap step 3). |
| a node's prefix; a child domain's prefix | `KeyEncoder::prefix` and `then_tag` in `crates/dbsp/src/trace/merged_index/mod.rs`. The folded key is `[tag enc(value)]* 0 id`. | None. |
| `SkipPrefix`: seek past a prefix | Only `Cursor::seek_key`, which is forward-only. No prefix successor exists. | A byte-string successor, with carry and the all-`0xFF` case. |
| the root prefix of a `Dlt` key | Value widths exist only as `OrderedKey::WIDTH` on Rust types. `Domain` has a name and columns only. | A width per domain in `KeyTrie` and `MergedIndexRustEmitter`. Only the root's is used: a group is the first `1 + root_width` bytes of a key. |
| `Peek` and `Step` | `CursorList` merges the runs and reads every run's weight at each landing (`is_zero_weight`, `skip_zero_weight_keys_forward`). `FileIndexedWSetCursor::move_key` also positions the value cursor, which deserializes the first value. | `RunMerge`, and lazy value positioning ([No extra decoding](#no-extra-decoding)). |
| seek to a group | `CursorList::seek_key` seeks every run, then re-merges. | The session's `PassTo`, which counts logical seeks; the run cursors count per-run seeks. |
| `Collect`'s range test | `key().starts_with(prefix)` after a step, as in `scan` and the merged-index tests. | None. |
| decode | `unfold` and `decode_payload` in the client crate. | Decode only buffered entries, from key bytes ([No extra decoding](#no-extra-decoding)). |
| the four readers | The four functions in `merged-index-col/src/q3.rs`. Each opens its own cursor for every requested key; no two readers share a cursor. | Replaced by the session and the operator's callback (roadmap step 5). |
| session operator | `add_ternary_operator` and `add_quaternary_operator` (`circuit_builder.rs`). Existing users include `GroupTransform` (ternary) and `AsofJoin` (quaternary). Accumulators in front supply each whole delta at the flush. Output is chunked by `splitter_output_chunk_size`. | A new operator. |
| snapshot input | Before each transaction the harness puts the snapshots in a shared slot (`Q3State::publish`). The join's read functions take them from that slot at the flush. | The same pattern, holding `(Old, Dlt)`. |
| joins taking turns | A DBSP operator flushes only after all its predecessors have finished flushing (`FlushState::UnflushedDependencies` in `dynamic_scheduler.rs`). `V`'s join therefore starts only after `J`'s has passed every touched customer. | One operator whose callback evaluates both joins per group. Separate operators sharing the buffers would hold every touched group until `V` finishes. |
| `Dlt` complete under (A2) | The base transaction runs first. `prepare` folds its net delta, and `ManualIndex::append` builds the run before the view transaction starts. For RF1/RF2 each line item finds its customer through its order's row in the same transaction. | The run is not published, and the view circuit reads the SQL net delta instead. Feed the view side from `Dlt`; no extra owner lookup is needed. |
| counters | Buffer-cache hits and misses, and Bloom and range filter statistics. `RunReads` (`trace/cursor/run_reads.rs`) counts per-run seeks in both run cursors, and filter rejections and key, value, and weight decodes in the file cursor, as per-thread running totals split by read owner (`with_read_owner`). `DBSPHandle::run_reads` reports the workers' totals. `SessionCounts` (`merged_index/session.rs`) counts the session's logical moves and payload decodes. | None. |

## No extra decoding

`thm:session` promises that the cursor decodes each position it reaches once. The write-up decodes a whole record per position, of any stream, because naming a position's stream and key takes its decode. Feldera differs in three ways, and each decides what a position costs.

1. **Key and value are separate columns.** A file run stores them apart, and the cursor deserializes each on demand. Until 2026-11-09 it deserialized the first value on every landing (`FileIndexedWSetCursor::move_key`); now a landing reads the key only.
2. **The key names the entry.** The folded key `[tag value]* 0 id` gives an entry's node and stream from its bytes alone. A position that nothing buffers needs a key decode and nothing else.
3. **A snapshot is several runs.** One logical cursor is `k` run cursors, and entries may cancel across runs, such as a deletion not yet compacted. `CursorList` reads every run's weight at each landing in order to skip zero-weight keys.

### What counts as a decode

| Level | Where | Allowed for |
| --- | --- | --- |
| Key decode | Storage deserializes a key when a run cursor lands on it. | Every position the session reaches, plus the first key at or past each seek target in each run. |
| Value decode | Storage deserializes a value. | Buffered old entries only, once each, plus the counted cancelled entries. |
| Weight decode | Storage deserializes a weight. | Buffered old entries only, once each, plus the counted cancelled entries. |
| Payload decode | The client's `decode_payload`, plus `unfold` if the full key is parsed. | Buffered old entries only, once each. |

An entry is *buffered* when its stream is in `inputs` and the session appends it. In the worked transaction the buffered old entries are `c5`, `o3`, `l7`, `l8`, `o4`, and `l9`. `c6` costs a key decode, to end `c5`'s range, and nothing else.

### Remedies

Both belong to roadmap step 2.

1. **Lazy value positioning in DBSP's file cursor.** `move_key` positions only the key cursor. The value cursor is filled the first time `val`, `weight`, or `map_times` asks. Laziness lives in DBSP's own cursor, not in a cursor only the session uses, so native joins get the same saving and the comparison stays fair.
2. **A merging cursor of the session's own, `RunMerge`.** It holds one cursor per run of `Old`, finds the minimum key by linear scan, and steps and seeks reading no value or weight. It sums weights only when the session takes an entry. An entry whose weights cancel across runs is read once and dropped. This is an inherent cost of reading an uncompacted run set, and it is counted, not hidden.

The session also routes an entry from its key bytes (the stream id, the last byte), decodes its payload once if the stream is in `inputs`, and reads `Dlt` once with one forward cursor. `Dlt` is an in-memory run, so its values are already deserialized.

### How the counters check the rules

The counters count each level separately, together with logical seeks and per-run seeks. DBSP's run cursors count per-run seeks, key decodes, value decodes, and weight decodes of file runs, and the key, value, and weight reads of in-memory runs apart (roadmap step 1). The scan session counts groups, cursor opens, logical seeks and steps, positions reached, payload decodes, and cancelled entries (`SessionCounts`, step 2). The exit checks of steps 2 and 5 assert two things:

- value, weight, and payload decodes from `Old` equal the number of buffered old entries, plus the counted cancelled entries;
- key decodes are at most the positions reached, plus one per run per seek.

On the worked transaction, with one group under `Seek` (the session's knob, below), this means seven key decodes (`c5` at open, then `o3`, `l7`, `l8`, `o4`, `l9`, and `c6`), and six value, six weight, and six payload decodes from `Old`. It means no seek: `Old` opens on `c5`, so the pass to `c5`'s prefix finds every run at or past it and moves nothing. The plan expected one logical seek and two per-run seeks, but a seek that moves no run costs nothing and counts nothing. The A/B below pins seeks that do move runs.

## The scan session

One scan session executes buffered execution. It is schema-independent, and it drives the traversal: it passes the untouched groups, collects each touched group, and hands the group's buffers to the execution through a callback. The callback is the permanent interface (decision of 2026-11-09). In roadmap step 2 the callbacks are tests. In step 5 the view operator supplies the callback that evaluates `ΔJ` and `ΔV` over the buffers and emits rows.

### Inputs

- `Old`, one pinned snapshot, read through `RunMerge`.
- `Dlt`, the transaction's sorted run, read once by one forward cursor.
- The trie, for the root domain's width.
- The index definition `D` (`MergedIndexDefinition`), which supplies the trie (`D::TRIE`) and the payload decoder (`D::decode_payload(stream_id, bytes)`). The stream id selects the payload type, so decoding needs no `unfold`. The session is bound to `D`: `ScanSession<D>`, and nothing else is passed.
- One knob, below.

A trie has exactly one root domain (decision of 2026-11-09). Two root domains mean two key tries, each stored as its own merged index, and several streams may still reside at the root domain's node. The Java `MergedIndexValidator` rejects any other trie, so `root_width()` is a plain `usize`; it panics on a hand-built trie with no single root, and so does the session (decision of 2026-11-10: the compiler's validator is the only trie check).

The groups are inferred from `Dlt`: the group of a `Dlt` key is its first `1 + root_width` bytes, the tag and value of the root domain. The session needs no list of requests, since a touched customer is exactly a customer with a `Dlt` entry.

### Operations

```text
ScanSession<D>(Old: snapshot → RunMerge, Dlt run, pass: Seek|Step)      trie = D::TRIE

  Maintain(inputs: set of index ids, reject?, on_group: (group, buffers) -> ()):
    for each group g in Dlt, in order:     g = first 1 + D::TRIE.root_width() bytes of a Dlt key
      PassTo(g)
      buffers ← Collect(g, inputs, reject), plus Dlt's entries under g (version Dlt)
      on_group(g, buffers); release buffers

  PassTo(g):                               untouched groups in between are never buffered
    Seek: Old.seek(g)                      one logical seek if any run moves
    Step: Old.step() while Old.key() < g

  Collect(g, inputs, reject):
    while Old.key() starts with g:         Peek is the merged key, already read
      id ← last byte of the key
      if id ∉ inputs: Old.step()
      else: Old.take → D::decode_payload(id, bytes) for each → buffer[id] (version Old)
            if reject(any entry of the key): Skip(key without its trailing [0, id])
            else: Old.step()

  Skip(n):                                 n is the rejected entry's own node prefix
    if Old.key() does not start with n: return
    Seek: Old.seek(prefix_successor(n)), or exhaust Old on None
    Step: Old.step() while Old.key() starts with n
```

The code differs from the write-up's pseudocode in two details:

- `Skip` ends on the first key outside the node, so `Collect` steps only when no skip happened. A step after a `Seek` skip would pass an unread key.
- A key's entries are judged together: `Skip` runs when any entry of the key is rejected. `reject` sees only old entries; `Dlt` is read whole.

The traversal mirrors the write-up: `Maintain` is `alg:ivm-buffered`'s loop over touched groups, `Collect` is `alg:session`'s, and `Skip` is `alg:skip`'s. `Old.key()` is the merged key `RunMerge` has already read, which plays the role of `Peek`'s kept `next_entry`.

### Rules the session follows

- The buffers keep `Old` and `Dlt` entries apart, unconsolidated (`prop:overlay`).
- `Dlt` is read once, by one forward cursor, so its entries under a group are the next ones in its order.
- A rejected entry stays in its buffer; `Skip` passes only the rest of its subtree.
- The session does not seek past domains whose streams are not in `inputs`, such as Q3's invoices. Each such position costs a key decode and nothing else (decision of 2026-11-09).
- `Old` and `Dlt` are merged only logically: the session compares forward cursors by key bytes and builds no merged run. A delta-only node, such as `(c5, o5)`, is part of `Dlt`'s entries under the group and needs no read of `Old`.

### Knob

`Pass` is the one knob. It is an A/B test with equal outputs.

| Knob | Values | Compares |
| --- | --- | --- |
| `Pass` | `Seek`, `Step` | Passing an untouched group, or a rejected subtree, by one logical seek against stepping through its entries. One knob governs both. |

A group read has no exact-key probe (decision of 2026-11-09). A probe of the group's root entry with `seek_key_exact` moves a run whose filter admits the key onto it, but a run whose filter rejects the key stays behind and must be sought anyway, since it can still hold entries below the root. The per-run seeks are then those of a prefix seek, plus the filter checks. The saving of 2026-11-04 holds for path reads only, which take a single record.

### Counters

`SessionCounts` holds:

- `groups`;
- `cursor_opens` (two: one logical cursor over `Old`, one over `Dlt`);
- `logical_seeks`: seeks over `Old` that moved some run. A seek that finds `Old` already at or past its target costs nothing and counts nothing, so a first group that `Old` starts on counts no seek;
- `logical_steps`;
- `positions`: the first key of `Old`, and each key that a step or seek landed on. Each costs one key decode in a file run;
- `payload_decodes_old` and `payload_decodes_dlt`: one per buffered entry;
- `cancelled`: values whose weights cancelled across runs.

DBSP's run cursors count the storage costs: per-run seeks, filter rejections, and key, value, and weight decodes. In-memory runs count their key, value, and weight accesses in separate fields, `memory_key_reads`, `memory_value_reads`, and `memory_weight_reads` (names unconfirmed; see [decisions.md](decisions.md)), by the same rules as the file decodes: a landing reads a key, a value counts once per position, and each weight returned reads a weight. A memory access deserializes nothing, so the two kinds are never added.

### Ownership

| Owner | Holds |
| --- | --- |
| The session | `RunMerge` over `Old`, the cursor over `Dlt`, the knob, the current group's buffers, and `SessionCounts` |
| The execution (the callback) | The evaluation of the delta rules over a group's buffers, and any derived deltas such as `ΔJ` |
| DBSP's run cursors | The run counters: per-run seeks, key decodes, value decodes, and weight decodes |

## Current implementation against the write-up

This table refreshes `tab:prototype`.

| Aspect | Prototype today | Write-up |
| --- | --- | --- |
| Reads | `C⁺`, `O⁻`, `J⁺`, `L⁻`, versions as in the table above | Same |
| `V`'s key | `orderkey` alone, so (A1) fails | `(custkey, orderkey)` |
| Delta delivery | Each reader receives its whole driving delta once, at the transaction's flush | Per touched group, from its buffers |
| Base deltas | SQL net deltas, plus the order → customer placements from the same transaction's Orders rows; RF1/RF2 only | The complete changes of the stored entries, `Dlt`, from index maintenance (A2) |
| Versions | Two snapshots, `before` and `after` | `Old`, overlaid with `Dlt` |
| Owner-key change | Rejected, as are replacement Orders deltas; customers change only at the initial load, so no line item moves in the validated workloads | Both folded keys of each moved entry, in `Dlt` |
| Cursors and seeks | A fresh cursor and seek per requested key; `O⁻` decodes the customer's whole subtree and discards all but Orders; `J⁺` reads the customer again for each pair of order and placement customer; `J⁺` and `L⁻` request by `orderkey`, which is not index order; a seek visits every run, and a file-backed run decodes the first value on every landing | One forward seek per touched group, each old entry read once; untouched groups not read |
| Key order | Folded byte keys compared as bytes; signed integers in value order, tested | Folded order |
| Counters | Run counters only (`RunReads`); no logical seeks or payload decodes | `SessionCounts` and the run counters, needed for every cost claim |

## Design work the write-up leaves to the implementation

These are implementation questions, not algorithm questions:

- **Publishing `Dlt`.** Keep the run that the base phase already builds, publish it with `Old`, and feed the view side from it.
- **The cost model.** A logical seek costs one seek per run of the snapshot. Decoding has four levels: key, value, weight, and payload. [No extra decoding](#no-extra-decoding) sets the rules and the counters that check them.
- **The join-plan type.** The operator interprets it: the joins with their keys and delta terms, each term's version and kind (path or range), and any selections and projections between them. The callback evaluates it over a group's buffers.
- **Output.** Emit chunks of the size DBSP expects, and leave consolidation to DBSP's batcher.
- **Workers.** One worker, as decided on 2026-10-12. Sharding by `custkey` is deferred.

## Coding roadmap

The steps are ordered. Each has an exit check and updates its docs. They are not R-numbered, since R1–R5 is the only numbering scheme.

1. **Run counters in DBSP.** *Done on 2026-11-03; extended and measured in both modes on 2026-11-04.*
   - Per-run seeks in both run cursors, `VecIndexedWSetCursor` and `FileIndexedWSetCursor`. Key decodes, value decodes, and weight decodes in the file cursor only, since only file runs deserialize.
   - A thread-local running total, read before and after a region on the same thread. The generic `Cursor` trait gains no counters.
   - Each read also counts for the thread's read owner, set by `with_read_owner` and given by `RunReads::current_for`. COL's `scan` reads the index under `WriteOwner::MergedIndex` and runs its callback under the caller's owner, so index reads stand apart from reads of driving deltas and other batches. *Added on 2026-11-04.*
   - The R5 harness reports each refresh's index run reads per reader, beside the index snapshots' layout (measurement version 5; fields in [measurement.md](measurement.md#report-fields)). *Added on 2026-11-04.*
   - For a native comparison, `DBSPHandle::run_reads` asks the workers for their totals per owner, and the harness reports the view worker's reads per refresh in both modes. Native join traces count under `query`; the index readers count under `merged_index`. Only indexed Z-set runs count, so a non-join trace such as `V`'s output is uncounted in both modes. A `seek_key_exact` that a file filter rejects counts as a filter rejection, not a seek: native joins probe this way, while the index's `scan` uses plain seeks. The fields were renamed from `*_reads` to `*_decodes` on 2026-11-04. *Added on 2026-11-04.*
   - *Measured on 2026-11-04* ([measurement.md](measurement.md#run-reads-version-5-smoke-2026-11-04)): one macOS smoke run per mode at SF 0.01, with RF2 deleting the oldest Orders and native joins reading through run cursors (`fetch_join` off; with it on, the native join batch-fetches into in-memory copies that the counters do not see). Over the sequence, reconstructed mode seeks about 2.4 times as often as native, decodes about 5.9 times the keys and 2.5 times the values, and reads more backend blocks. The counts follow the rules:
     - `J⁺` scans twice per Order where native probes its stored `J` once.
     - The index holds more runs, and every scan seeks each of them.
     - Every index entry has its own key, where a native trace groups values under one.
     - Each scan opens a cursor and ends past its range.

     *Rerun on 2026-11-05* with exact path reads ([measurement.md](measurement.md#exact-path-reads-2026-11-05)): reconstructed mode seeks about 2.3 times as often as native and decodes about 2.9 times the keys and 1.2 to 1.3 times the values. `J⁺` skips one seek per new Order through the file filter. Backend reads were not comparable in that run: the index's blocks were cached in the base runtime, which wrote them, not the view runtime, which read them.

     *Rerun on 2026-11-05* in one runtime with one buffer cache ([measurement.md](measurement.md#one-runtime-version-6-2026-11-05)): every run-read count equals the exact-path run's, in both workloads and modes. Reconstructed backend reads fell from 169 to 72 (clustered) and from 192 to 83 (scattered); native read 7 and 11. At a 256 MiB cache (clustered only), both modes made no backend reads, so the remaining reads at 32 MiB depend on cache capacity, not on placement.

     Steps 2 and 5 address the per-scan cursors, the repeated customer reads, and the landings that take nothing. The per-run multiplier of a logical seek depends on compaction, which no step changes.
   - *Exit:* DBSP unit tests (filter `run_reads`) pin the counts of single moves on an in-memory run and a file run, and one seek per run through `CursorList`. A COL test pins today's four readers on the worked transaction (without `v2` and `v8`, since COL has no invoices), over a file run plus an in-memory run.
   - *Baseline.* `Old` is a file run holding `c5`, `o3`, `l7`, `l8`, `o4`, `l9`, and `c6`, plus an in-memory run holding `c7`; the after state adds `Dlt`. Each comment in `todays_readers_run_reads_on_the_worked_transaction` derives its count. The counts are the index's own. The driving deltas are in memory and read under the caller's owner, so the test pins their memory reads apart: one key, value, and weight for each key of a delta (`requested_keys` steps through them, and `CursorList` reads the first value and weight under each key).

     | Reader | Per-run seeks | Filter rejections | Key decodes | Value decodes | Weight decodes | Memory key reads | Memory value reads | Memory weight reads |
     | --- | --- | --- | --- | --- | --- | --- | --- | --- |
     | `C⁺` | 3 | 0 | 1 | 2 | 1 | 2 | 3 | 3 |
     | `O⁻` | 2 | 0 | 7 | 8 | 8 | 1 | 0 | 0 |
     | `J⁺` | 5 | 1 | 2 | 3 | 1 | 5 | 5 | 5 |
     | `L⁻` | 6 | 0 | 9 | 9 | 9 | 3 | 0 | 0 |
     | Total | 16 | 1 | 19 | 22 | 19 | 11 | 8 | 8 |

     *Added 2026-11-10:* the memory columns. The in-memory runs are `M` (`c7` alone) and, for the reads of the new state, `Dlt`. Opening reads each run's first key, so every scan reads `M`'s key, and `Dlt`'s too in `C⁺` and `J⁺`. `Dlt` holds two values under `c5` (`new` and `old`), so `C⁺` reads three values and three weights: the first value and its weight at the opening and again after the seek empties the value, then the second value, whose weight is summed with the file run's. `J⁺`'s second probe reads `Dlt`'s `c5` value and weight again, and one more of each at `o5`. `M` is past every range, so `CursorList` never reads its value. The file columns are those of the table above, unchanged.

     *Updated 2026-11-09 for lazy values* (sub-step 2 below): every count is unchanged. `CursorList` asks for the first value of each run at its smallest key, and the file run is at the smallest key at every landing here, so the lazy cursor decodes the same values on demand that it decoded eagerly. Laziness saves a decode only for a run that is not at the smallest key, such as `second` in `cursor_list_reads_values_only_of_the_runs_at_its_smallest_key`; `RunMerge` (roadmap step 2) removes the rest.

     Six old entries are taken (`c5`, `o3`, `l7`, `l8`, `o4`, `l9`), against 22 value decodes. `O⁻` alone reads all seven file keys, and `c6` is decoded three times, as the end of `O⁻`'s range and as `L⁻`'s landing for `o4` and `o5`.
   - The path reads, `C⁺` and `J⁺`, probe their one exact key with `seek_key_exact` (`scan_entry`), as a native join probes its trace: a file run whose filter excludes the key is neither sought nor decoded, and nothing past the entry is read. `J⁺`'s probe of `(c5, o5)`, which only `Dlt` holds, is the one rejection. The range reads, `O⁻` and `L⁻`, keep `scan`, since the filters answer whole keys, not prefixes. Each probe still opens a cursor, which decodes the file run's first key, and its first value through `CursorList`. *Changed on 2026-11-04;* before, the path reads also used `scan`, and the totals were 17 seeks and 22, 25 and 21 decodes.
2. **Decode-free storage and the buffered scan session.** *Done 2026-11-09.* The sub-steps committed one by one:
   - **Prefix successor and per-domain width.**
     - `prefix_successor(prefix) -> Option<Vec<u8>>` in `merged_index/mod.rs`. It drops trailing `0xFF` bytes and increments the last remaining byte. It returns `None` when every byte is `0xFF`, meaning the range runs to the end of the index.
     - `width` on `Domain` in `trie.rs`, `KeyTrie::root_width()`, and a `root_width()` that panics on any trie without exactly one top-level node (*changed on 2026-11-09*; it returned an `Option` before, and a Rust `validate()` rejected such tries until 2026-11-10, when the compiler's validator became the only check). `MergedIndexRustEmitter` derives the width from the domain's Rust type (`OrderedKey::WIDTH`); Java's `KeyTrie` stores none. DECIMAL key domains keep failing to compile.
     - Regenerate `merged-index-col/src/generated.rs`.
     - *Exit:* unit tests for the empty prefix, carry, and all-`0xFF`; a property test that `k.starts_with(p)` holds exactly when `p <= k < succ(p)`; COL's widths are 8, 8, and 4.
   - **Lazy value positioning** in `FileIndexedWSetCursor` ([Remedies](#remedies)). *Done 2026-11-09.* The value cursor becomes a once-filled slot (`OnceCell`), and `count_value_decode` moves to where it is filled. A landing reads the key only; the first `val`, `val_valid`, `weight`, `map_times`, or value move fills the slot. A key move keeps the emptied cursor as the hint for the next first value. The `run_reads` tests pin the lazy counts; the COL baseline is unchanged (see the note under its table).
     - *Exit:* a landing alone decodes no value; the first `val()` counts one value decode and a second counts none; `step_val`, `seek_val`, and `weight` before `val` work; a clone works before and after the fill. The pinned counts of `todays_readers_run_reads_on_the_worked_transaction` are recomputed with each derivation in its comment, and the baseline table above is updated. The existing trace tests pass unchanged.
   - **`RunMerge`** in `merged_index/run_merge.rs`. *Done 2026-11-09.* `key`, `step`, `seek`, and `take`. `step` and `seek` read no value or weight; `take` sums weights per value and counts cancelled values. `seek` moves only the runs below its target and reports whether any moved. `exhaust` moves past every key without a read, for a skipped prefix that has no successor.
     - *Exit:* a property test against a consolidated brute force over several runs, file runs among them, with cross-run cancellations and replaced payloads; a counter test that stepping past entries counts no value or weight decode.
   - **`ScanSession`** in `merged_index/session.rs`, with `Maintain`, `PassTo`, `Collect`, `Skip`, the `Pass` knob, and `SessionCounts` ([The scan session](#the-scan-session)). *Done 2026-11-09.* `RunMerge` gained `exhaust` for a skipped prefix with no successor.
     - *Exit:* a property test against a brute-force reference that consolidates `Old`, groups `Dlt` by root, filters by prefix and `inputs`, and drops the rest of a rejected node's subtree. It ranges over three small tries (one with a two-byte root and two placements at the root node, one with no root placement), values near `0x00`, `0x01`, `0xFE`, and `0xFF` to force carry, multi-run snapshots with file runs, random `Dlt`, `inputs`, and rejection tests, and both passes. Counter tests show that positions outside `inputs`, and the position that ends a group, cost no value, weight, or payload decode; and pin the seek and step counts of the two passes on a six-group layout and on a rejected subtree. *Changed on 2026-11-09:* the `GroupStart` knob and the root probe were removed, and the proptest ran four knob combinations before. The session became `ScanSession<D: MergedIndexDefinition>`, and the proptest's three fixture tries became small definition types.
   - **COL counter tests, the A/B, and docs.** *Done 2026-11-09.* On the worked transaction in `merged-index-col/src/q3/tests.rs`, `Maintain({C, O, L})` over `CustomerOrdersLineitemDefinition`, which decodes through its `decode_payload`, under `Seek` pins the counts of [How the counters check the rules](#how-the-counters-check-the-rules) (`session_run_reads_on_the_worked_transaction`). An A/B test on the same kind of data, extended with untouched customers between touched ones and a synthetic segment filter, pins the run and session counters of both passes, derives each in a comment, and checks equal outputs (`the_passes_pass_untouched_groups_and_rejected_subtrees_in_different_ways`).
     - *Exit:* `CLAUDE.md` and `WORKFLOW.md` gain the new files, the knob, and test filters. The version-6 measurements predate lazy values; R5 timing of the passes belongs to step 6.

     **Pinned: the worked transaction**, one group (`c5`), `Seek`, with `Old` as in the step 1 baseline (a file run holding `c5` through `c6`, a memory run holding `c7`) and `Dlt` as in the worked transaction:

     | Count | Session reading | Today's four readers (step 1) |
     | --- | --- | --- |
     | Per-run seeks | 0 | 16 |
     | Key decodes | 7 | 19 |
     | Value decodes | 6 | 22 |
     | Weight decodes | 6 | 19 |
     | Payload decodes from `Old` | 6 | |
     | Payload decodes from `Dlt` | 5 | |
     | Logical seeks, steps, positions | 0, 6, 7 | |

     **Pinned: the A/B.** `Old` has 18 keys (17 in a file run, `c7` in a memory run) over customers `c3`, `c5`, `c6`, `c7`, `c8`, `c9`. `Dlt` touches `c5` (old entries `c5`, `o3`, `l7`, `l8`, `o4`, `l9`; delta entries `-c5`, `+c5'`, `o5`, `l10`, `l11`), `c8` (a delta line item; its old customer is rejected by the segment filter, so its order and line item are passed unread), and `c10` (past the end of `Old`). Both passes buffer the same entries, and decode seven old payloads, eight delta payloads, seven values, and seven weights:

     | Pass | Logical seeks | Logical steps | Positions | Run seeks | Key decodes |
     | --- | --- | --- | --- | --- | --- |
     | `Seek` | 4 | 6 | 10 | 5 | 10 |
     | `Step` | 0 | 18 | 18 | 0 | 17 |

     What the table shows:

     - Seeking and stepping buffer the same entries. Stepping decodes seven more keys here, one for each key that seeking jumps over (`o1` and `l1`; `o6` and `l12`; `o8` and `l13`; `o9`), and takes 18 steps against 6.
     - Memory runs add seeks but no decodes, so the key decodes of a pass equal its file-run positions.
     - *Before 2026-11-09* a second knob, `GroupStart::ProbeThenRange`, probed the root entry first. It replaced two prefix seeks by exact probes landing on the same keys, saved no key decode or run seek, and added one filter rejection (the probe of `c10`). It was removed.

3. **Publish `(Old, Dlt)`, and read the after state as the overlay.**
   - Rewrite the four readers to read `Old ⊎ Dlt` for the after state.
   - *Exit:* `check_readers` and the transcript comparison pass, and the after snapshot is gone.
4. **Hand-written view circuit.**
   - The view side reads its inputs from `Dlt` (still RF1/RF2), each row keyed by its full trie path.
   - The joins are keyed by `(custkey)` and `(custkey, orderkey)`, still on today's snapshot-reader joins.
   - The placement map is dropped.
   - *Exit:* the transcript comparison with native mode passes.
5. **Buffered-execution session.**
   - The join-plan type, plus one operator that drives the scan session. The operator supplies `Maintain`'s `on_group` callback, which evaluates `ΔJ` and `ΔV` over a group's buffers and emits rows; the session then releases the buffers. It replaces step 4's two joins, which now take turns per group inside the callback.
   - It is exact for any plan with path or range reads.
   - Collection decodes only buffered entries (step 2's rules).
   - *Exit:* the counters show each buffered old entry read and decoded once, and none of Q3's invoices decoded beyond their keys. The transcript comparison passes.
6. **Measurement, new version.**
   - Add the counters to `measure.py`, and run the native and reconstructed (buffered) modes on Linux, with the session's `Pass` knob as an A/B.
   - *Exit:* raw artifacts under `target/`; [measurement.md](measurement.md) reports base and IVM side by side.
7. **Beyond RF1/RF2 (optional).**
   - Customer updates, line items added to existing orders, and orders that change customer.
   - Each needs index maintenance to produce a complete `Dlt`. That is base-phase work; view maintenance is unchanged.
   - *Exit:* new transcript fixtures pass in every mode.
8. **Deferred:**
   - selective execution ([deferred.md](deferred.md#selective-execution));
   - plans derived from the spec;
   - more than one worker, sharded by `custkey`;
   - groupings, whose state the write-up leaves for later work.

## Unconfirmed choices

- The session as one DBSP operator over the ternary streaming interface, or the quaternary one for wider plans.
- `ΔJ` internal to the operator unless a plan exports it.
- Step 4 as an intermediate milestone before the session.
- Changes beyond RF1/RF2 kept out of scope until step 7.
- `prefix_successor` returning `Option`, with `None` meaning the range runs to the end of the index, rather than a sentinel key.
- `RunMerge` finding the minimum key by linear scan over its run cursors, not a heap. Snapshots hold few runs.
- The once-filled value slot as the shape of lazy values in the file cursor (a `OnceCell`), with a hint field that keeps the last value cursor for `first_with_hint`.
- `Maintain`'s `reject` as an optional per-entry test.
- The counting rules of `RunReads`: opening a cursor counts the key and value it lands on; value seeks count no seek; each probe of a predicate seek counts a key or value decode; a reverse seek counts one key decode, though the file reader may read the key twice. Lazy values (step 2) change the first of these: a landing counts the key only. In-memory runs follow the same rules in their own fields (2026-11-10): a value counts once per position, through a `Cell<bool>` that every key move clears and every value move sets, and a predicate seek counts one key read for each probe.
- The read owner reuses the `WriteOwner` enum, so one list names the components for reads and writes. It is a separate thread-local from the write owner, so a read scope never changes who owns files created inside it. `RunReads::current` stays the total over all owners.
- A `filter_rejections` run counter, so that native join probes that a file filter answers stay visible.
- A separate `weight_decodes` run counter. The file format stores a value and its weight apart: landing on a value deserializes the value, and the weight is deserialized only when `weight`, `map_times`, or `map_values` asks. `CursorList`'s zero-weight skip reads weights at entries that no reader takes, so a separate counter shows that cost. Folding weights into value decodes would hide it.
- `cursor_opens` is the two logical cursors (one over `Old`, one over `Dlt`), not the run cursors.
- `ScanSession::new` takes `Old` as a snapshot and opens its `RunMerge` itself, so the opening's key decode falls inside the session's counted region.
- `RunMerge::exhaust`, for a skipped prefix with no successor, rather than stepping to the end.
- `decode_payload` keys on the stream id, not on the unfolded key.
