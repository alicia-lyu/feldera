# Shared partner reads: one buffered scan session per transaction

> [!NOTE]
> **Source:** Chapters 2 and 3 of `docs/key-trie-merged-index-order.tex` in the thesis repository (renamed on 2026-11-09 from `key-trie-laminar-ranges.tex`). Chapter 2, "Interesting orderings for queries: one group at a time", gives the scan session (`sec:session`, `alg:session`, `thm:session`), the plan run over one buffered group (`sec:demand`, `alg:buffered-pipeline`), and skip scans (`sec:skip`, `alg:skip`, `lem:skip`). Chapter 3, "Interesting orderings for view maintenance: driving deltas", gives `sec:ivm`, `sec:ivm-assume`, `sec:ivm-reads` (`lem:partner`, `lem:locality`), `sec:ivm-buffered` (`alg:ivm-buffered`, `prop:group-delta`), `sec:ivm-relax`, and the appendices `sec:overlay` (`prop:overlay`) and `sec:prototype` (`tab:prototype`). This doc cites them by label, since numbers drift.
>
> **Scope:** the write-up now has buffered execution only. Selective execution (requests, `Visit`, assumption (A3)) moved to the companion note `key-trie-execution-refinements` (a PDF linked from the `.tex`). This doc follows the write-up; the removed material is summarized in [deferred.md](deferred.md#selective-execution).
>
> **Notation:** `X⁻` and `X⁺` are the states before and after the transaction. `Δ` is a transaction delta, and a "run" is an immutable sorted collection. A derived row is named by its instance node, such as `((custkey, orderkey) = (c5, o5))`.
>
> **Status:** the algorithm is settled in the write-up. Roadmap step 1, the run counters, is implemented and reported by the R5 harness in both modes. Step 2, decode-free storage and the buffered scan session, is implemented (2026-11-09), and in-memory runs count their accesses in separate fields (2026-11-10). Step 3, publishing `(Old, Dlt)`, is implemented (2026-11-10). Step 4, the hand-written view circuit, is implemented (2026-11-10) and carries step 7: the circuit admits general deltas. Step 5, typed readers filled at collection, is implemented (2026-11-11). Step 6 is not. This doc is the meta plan for the coding steps. Choices marked *unconfirmed* await the user. The confirmed ones are in [decisions.md](decisions.md) under 2026-10-29, 2026-11-03, 2026-11-09, 2026-11-10 (including the step-4 entries), and 2026-11-11 (step 5).

This doc covers:

- the algorithm as Chapters 2 and 3 state it;
- an audit of the write-up against the code;
- how the write-up's concepts map onto Feldera code;
- why Feldera's storage needs two changes before the write-up's decode claims hold;
- the scan session that executes the algorithm;
- how today's prototype differs from the write-up;
- design work the write-up leaves to the implementation;
- the coding roadmap.

The reconstructed view circuit follows both of the write-up's assumptions, (A1) and (A2) (step 4).

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

`J` and `V` stay separate DBSP operators (decision of 2026-11-11), and each old entry is still read from the index once. DBSP flushes an operator only after its predecessors have flushed, so `V` starts only after `J` has passed every touched customer. What a transaction collected therefore stays until `V` finishes. The implementation keeps no group buffers past collection: each entry is projected into the typed buffers of its readers, and the group's raw buffers are dropped ([Readers](#readers-step-5)). Releasing each group before the next is collected would need the two joins to take turns per group, which DBSP's scheduler does not do.

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

In DBSP, each join is an operator of its own, scheduled separately. Each receives its whole driving delta at the commit flush. The circuit keeps them (decision of 2026-11-11): the circuit is the plan, and only each join's partner read is taken from the session, as the batches of a typed reader. Selections, groupings, and every other operator stay DBSP's. DBSP's scheduler is unchanged.

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
| `Dlt` | `ManualIndex`'s `Request::Append` handler builds the run with `build_batch`, inserts a shared reference into the spine, and returns the run; `Q3State::publish(old, dlt)` takes it beside `Old`. `reconstruction::prepare` completes it for every change (see the `Dlt` row below). | None. |
| a node's prefix; a child domain's prefix | `KeyEncoder::prefix` and `then_tag` in `crates/dbsp/src/trace/merged_index/mod.rs`. The folded key is `[tag enc(value)]* 0 id`. | None. |
| `SkipPrefix`: seek past a prefix | Only `Cursor::seek_key`, which is forward-only. No prefix successor exists. | A byte-string successor, with carry and the all-`0xFF` case. |
| the root prefix of a `Dlt` key | Value widths exist only as `OrderedKey::WIDTH` on Rust types. `Domain` has a name and columns only. | A width per domain in `KeyTrie` and `MergedIndexRustEmitter`. Only the root's is used: a group is the first `1 + root_width` bytes of a key. |
| `Peek` and `Step` | `CursorList` merges the runs and reads every run's weight at each landing (`is_zero_weight`, `skip_zero_weight_keys_forward`). `FileIndexedWSetCursor::move_key` also positions the value cursor, which deserializes the first value. | `RunMerge`, and lazy value positioning ([No extra decoding](#no-extra-decoding)). |
| seek to a group | `CursorList::seek_key` seeks every run, then re-merges. | The session's `PassTo`, which counts logical seeks; the run cursors count per-run seeks. |
| `Collect`'s range test | `key().starts_with(prefix)` after a step, as in `scan` and the merged-index tests. | None. |
| decode | `unfold` and `decode_payload` in the client crate. | Decode only buffered entries, from key bytes ([No extra decoding](#no-extra-decoding)). |
| the four readers | `customers_after`, `orders_before`, `joined_orders_after`, and `lineitems_before` in `merged-index-col/src/q3.rs`. Since step 5 the first, second, and fourth return a typed reader's batches (`Old`'s for a before read, `Old`'s and `Dlt`'s unconsolidated for an after read, `prop:overlay`), and `J⁺` is `J`'s join on each requested path over two readers ([Readers](#readers-step-5)). They open no cursor on the index. A request outside the touched groups returns an error, which the joins' `expect` turns into a panic of the worker. | None (step 5). |
| session operator | Not built. DBSP's joins stay, and the session runs in `Q3State::collect` (decision of 2026-11-11). | None. |
| snapshot input | Before each transaction the harness puts `(Old, Dlt)` in a shared slot (`Q3State::publish`) and runs `Q3State::collect` once, which fills the readers. The driver then hands the readers' `Dlt` batches, the driving deltas, to the view circuit's `add_input_stream` handles, and at the commit the joins read the readers' batches. `Q3State::finish` releases the readers and returns the `SessionReads`. | None (step 5). One copy remains: `J⁺`'s partner rows. |
| joins taking turns | A DBSP operator flushes only after all its predecessors have finished flushing (`FlushState::UnflushedDependencies` in `dynamic_scheduler.rs`). `V`'s join therefore starts only after `J`'s has passed every touched customer, so `Q3State` holds the readers from `collect` until `finish`. | Kept (decision of 2026-11-11). |
| `Dlt` complete under (A2) | The base transaction runs first. `reconstruction::prepare` folds its net delta into index rows, and `ManualIndex::append` builds the run before the view transaction starts. Each changed order gets a before owner and an after owner; a moved order takes its line items with it, read from `Old`; line items with no owner wait in `OrphanLines`. The owner of an order that is not in the delta comes from the base `Orders` state: the pre-transaction `BaseState` (driver) or `Reference` (tests), counted under `BaseRelation`. | None (step 4). The view circuit's inputs come from `Dlt`. |
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

One scan session executes buffered execution. It is schema-independent, and it drives the traversal: it passes the untouched groups, collects each touched group, and hands the group's buffers to the execution through a callback. The callback is the permanent interface (decision of 2026-11-09). In roadmap step 2 the callbacks are tests. Since step 5 the callback is `reader::collect`'s, which routes each group's entries into the typed readers ([Readers](#readers-step-5)); DBSP's joins evaluate `ΔJ` and `ΔV` (decision of 2026-11-11).

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

DBSP's run cursors count the storage costs: per-run seeks, filter rejections, and key, value, and weight decodes. In-memory runs count their key, value, and weight accesses in separate fields, `memory_key_reads`, `memory_value_reads`, and `memory_weight_reads` (see [decisions.md](decisions.md)), by the same rules as the file decodes: a landing reads a key, a value counts once per position, and each weight returned reads a weight. A memory access deserializes nothing, so the two kinds are never added.

### Ownership

| Owner | Holds |
| --- | --- |
| The session | `RunMerge` over `Old`, the cursor over `Dlt`, the knob, the current group's buffers, and `SessionCounts` |
| The callback (`reader::collect`) | The routing of a group's entries into the readers, which hold the typed rows until `finish` |
| DBSP's joins | The evaluation of the delta rules, and derived deltas such as `ΔJ` |
| DBSP's run cursors | The run counters: per-run seeks, key decodes, value decodes, and weight decodes |

## Readers (step 5)

A *reader* is one partner read of one join (decision of 2026-11-11). It declares one stream of the index and a projection of that stream's entries into the join's typed rows `(K, V)`. Its buffer is its records, in the join's type.

```text
collect(session, readers):                 inputs = the readers' streams
  session.Maintain(inputs, on_group = (g, buffers) ->
    for each entry e of buffers, Old's then Dlt's:
      key ← unfold(e.key)                  once per entry
      for each reader r of e's stream: r.half(e's half).push(r.project(key, e.payload), e.weight)
    drop buffers)
  seal each reader: Old's rows → one batch, Dlt's rows → another
```

A sealed reader (`ReaderRuns`) gives three reads:

| Read | Batches | Q3's use |
| --- | --- | --- |
| `before()` | `Old`'s | `O⁻`, `L⁻` |
| `after()` | `Old`'s and `Dlt`'s, in one snapshot, unconsolidated (`prop:overlay`) | `C⁺`, and `J⁺`'s two lookups |
| `delta()`, or `take_delta()` | `Dlt`'s, cloned, or moved out (the reader keeps an empty `Dlt` batch, so it must serve no after read) | the driving deltas: `ΔC` cloned (`C⁺` shares it), `ΔO` and `ΔL` moved |

The batches hold every row of the touched groups. The join seeks only its driving delta's keys, so the kind of a read, path or range, follows from the reader's key: `C⁺` keyed by `custkey` takes one customer, and `O⁻` keyed by `custkey` takes the customer's subtree of orders. A key above a stream's own node, such as `O⁻`'s, is how a join key flows up the trie.

Differences from the write-up (`def:session`), and their costs:

- Buffers are per reader, not per stream, so an entry that two readers take is stored twice, once in each shape. In Q3, an old order goes to `O⁻` and to `J⁺`'s orders reader. Index reads and payload decodes are unchanged.
- `J⁺` reads two streams. It is not one reader but `J`'s join re-evaluated per requested path, over two one-stream readers; its rows are the one copy left at the DBSP boundary.

## Current implementation against the write-up

This table refreshes `tab:prototype`.

| Aspect | Prototype today | Write-up |
| --- | --- | --- |
| Reads | `C⁺`, `O⁻`, `J⁺`, `L⁻`, versions as in the table above | Same |
| `V`'s key | `(custkey, orderkey)`; `J` joins on `(custkey)` and keeps `(custkey, orderkey)` (step 4) | `(custkey, orderkey)` |
| Delta delivery | Each join receives its whole driving delta once, at the transaction's flush; its partner reads are the batches of typed readers that one session filled | Per touched group, from its buffers |
| Base deltas | The driving deltas are the readers' `Dlt` batches, handed whole to DBSP's `Input` operator; `prepare` completes `Dlt` for general changes | The complete changes of the stored entries, `Dlt`, from index maintenance (A2) |
| Versions | `Old` for before reads; `Old` then `Dlt`, unconsolidated, for after reads | Same |
| Owner-key change | `prepare` emits both folded keys of each moved line item, reading its lines from `Old` | Same |
| Cursors and seeks | One session per transaction: one forward cursor over `Old`, each buffered old entry read once, untouched groups passed. The partner reads are the readers' batches and open no cursor on the index | Same |
| Key order | Folded byte keys compared as bytes; signed integers in value order, tested | Folded order |
| Counters | `SessionCounts` and the run counters (`index_session_counts`, `index_run_reads.session`; measurement version 8) | Same |

## Design work the write-up leaves to the implementation

These are implementation questions, not algorithm questions:

- **Feeding the view side from `Dlt`.** Done in step 4, and without a row-by-row push since step 5: the driving deltas are the readers' `Dlt` batches, handed whole to the input handles. One copy remains at the DBSP boundary, `J⁺`'s partner rows.
- **The cost model.** A logical seek costs one seek per run of the snapshot. Decoding has four levels: key, value, weight, and payload. [No extra decoding](#no-extra-decoding) sets the rules and the counters that check them.
- **The join plan.** The DBSP circuit itself (decision of 2026-11-11); each join's partner read is a typed reader.
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
   - *Exit:* DBSP unit tests (filter `run_reads`) pin the counts of single moves on an in-memory run and a file run, and one seek per run through `CursorList`. A COL test pinned the four readers of step 1 on the worked transaction (without `v2` and `v8`, since COL has no invoices), over a file run plus an in-memory run. Step 4 retired it (`todays_readers_run_reads_on_the_worked_transaction`) with the readers it measured; the table below is history.
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
     | Memory key reads (`M`, `Dlt`) | 5 | 11 |
     | Memory value reads | 5 | 8 |
     | Memory weight reads | 5 | 8 |
     | Payload decodes from `Dlt` | 5 | |
     | Logical seeks, steps, positions | 0, 6, 7 | |

     *Added 2026-11-10:* the memory reads. The session reads `M`'s key at the opening and `Dlt` whole (4 keys, 5 values, 5 weights).

     **Pinned: native and the session on the same rows** (*added 2026-11-10; rewritten the same day under one pressure mechanism, see [decisions.md](decisions.md#2026-11-10-one-memory-pressure-mechanism-for-both-sides-of-the-worked-transaction-comparison)*). Both sides run the same rows, the same `runtime_config`, and the same two arms (`worked_transaction::Arm` in `q3-ivm-baseline/src/main.rs`):

     - *Default:* no memory target, so the pressure stays low.
     - *Pressured:* a 1 MiB memory target, and a wait until the runtime reports high pressure before the load.

     Nothing relieves the pressure by hand, and the runs are wherever the runtime's mechanism puts them. The old state is loaded through `transact` (`c5`, `o3`, `l7`, `l8`, `o4`, `l9`, `c6`, `c7`), and the transaction replaces `c5` and adds `o5` with `l10` and `l11`.

     - The native side is the view worker's run reads (`native_worked_transaction_tests`, native build only, since the `merged-index` build's view circuit reads the index). The transaction goes through `transact`.
     - The session side is `Maintain` over the index's `Old` and `Dlt` (`session_worked_transaction_tests`, `merged-index` build). `Old` is the index's snapshot before the transaction, which `ManualIndex`'s `Spine` placed. `Dlt` is built by `build_batch` with no fixed location, so it follows `min_step_storage_bytes` like a native operator's output. The transaction runs on the base circuit and the index; the session then collects the groups, and the view circuit runs on the driving deltas it buffered. Since step 4 the view circuit admits the replacement of `c5`.

     The pressure and the layout each side got, in five repeats of each test (identical every time):

     | Side | Arm | Memory pressure (start, after load, after transaction) | Layout |
     | --- | --- | --- | --- |
     | Native | default | Low, Low, Low | Every read is a memory read, and the storage directory gains no file. |
     | Native | pressured | Critical, Critical, Critical | Every read is a decode, so every run is in a file (the directory gains 8 files in the load and 8 in the transaction). Whether a trace's run or an operator's delta batch is which file is not reported. |
     | Session | default | Low, Low, Low | `Old` is one memory run (8 entries) and `Dlt` a memory run (5 entries). |
     | Session | pressured | Critical, Critical, Critical | `Old` is one file run (8 entries) and `Dlt` a file run (5 entries); the directory gains 5 files in the load and 5 in the transaction. |

     The file counts of the two sides are not comparable: they include the base circuit's files and the operators' delta batches, and the session's transaction does not run the view circuit.

     What each side read, over the transaction:

     | Reading | Arm | Seeks | Filter rejections | Key decodes | Value decodes | Weight decodes | Memory key | Memory value | Memory weight |
     | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
     | Native, view worker | default | 7 | 0 | 0 | 0 | 0 | 16 | 41 | 38 |
     | Native, view worker | pressured | 6 | 1 | 16 | 41 | 38 | 0 | 0 | 0 |
     | Session | default | 0 | 0 | 0 | 0 | 0 | 11 | 11 | 11 |
     | Session | pressured | 0 | 0 | 11 | 11 | 11 | 0 | 0 | 0 |

     Native's reads, derived per operator by a scout with no code run, and equal to the pins (7 probes; 16, 41, and 38 reads). The "after" trace of a join is the old run plus the transaction's delta run, so it has two runs; the "before" trace has the old run.

     | Operator | Probes | Key | Value | Weight |
     | --- | --- | --- | --- | --- |
     | `ΔO ⋈ C` after the insert | 2 | 3 | 8 | 6 |
     | `ΔC ⋈ O` before the insert | 1 | 2 | 8 | 8 |
     | `J`'s `plus`, merging the two join outputs | 0 | 3 | 5 | 5 |
     | `ΔL ⋈ J` after the insert | 2 | 4 | 7 | 6 |
     | `ΔJ ⋈ L` before the insert (swapped: `L` drives and `ΔJ` is sought) | 2 | 4 | 13 | 13 |
     | Total | 7 | 16 | 41 | 38 |

     Values and weights exceed keys for four reasons: each delta value makes a full pass over the trace's values and then rewinds; `CursorList` reads a weight at each value landing, for its zero-weight skip; a seek that stays on the same key counts that key's value again; and `J`'s `plus` reads every output row. In the pressured arm one of the seven probes is answered by a run's filter, which counts a filter rejection and no seek (6 + 1). It is `ΔL`'s probe of `o5` into `J`'s old run, which has no entry of `o5`.

     The session's reads, derived in a comment of `check` in `session_worked_transaction_tests`:

     | Part | Reads |
     | --- | --- |
     | `Old`, one run | 7 key reads (the positions `c5`, `o3`, `l7`, `l8`, `o4`, `l9`, and `c6`, which ends the group), and 6 value and 6 weight reads (the six entries taken; `c7` is never reached) |
     | `Dlt`, read whole | 4 key reads (`c5`, `o5`, `l10`, `l11`), and 5 value and 5 weight reads (`c5` has two entries) |
     | Seeks | None reach a run: `Old` opens on `c5`, the smallest key, so the seek to `c5`'s prefix moves nothing |

     How to read the numbers:

     - *Placement changes the kind of the counts, not the counts.* Native decodes in files exactly what it reads in memory (16, 41, 38), apart from the one rejected probe, and the session decodes 11, 11, 11 where it reads 11, 11, 11. The test pins the totals in every layout (`Totals`), and the split between memory reads and decodes by the layout it observed, so that it does not choose a layout.
     - *Where native's reads go.* Five operators make them (the table above), each probing a trace, and every probe is a seek into a run. Both sides read the old entries that the transaction touches (`c5`, `o3`, `o4`, `l7`, `l8`, `l9`) and the transaction's delta. Native reads them through the cursors of its traces, one probe at a time, and the session in one pass over the index in key order.
     - *Part of native's count computes the view change from the circuit's own intermediate results*, which the session does not do (it hands the buffers to `on_group` and evaluates the delta rules over them, with no run cursor; this test does not run that evaluation). That part is:
       - `J`'s `plus`: 3 keys, 5 values, and 5 weights, with no probe. The rest of native's count, 13 keys, 36 values, and 33 weights, still includes the next two items.
       - The sought `ΔJ`, in `ΔJ ⋈ L`: `L` drives and `ΔJ`, an intermediate result, is sought, so part of that row reads `ΔJ` and part reads `L`'s old entries. The counters do not separate them, so the row's 2 probes and 4, 13, and 13 reads are not split.
       - The delta runs that the after traces re-read, in `ΔO ⋈ C` and `ΔL ⋈ J`. Each after trace holds the delta run beside the old run, and the counters do not separate that run's reads either.
     - *What the session reads that native does not*: the position `c6` that ends the group (one key read, with no value or weight), and the index's one key per entry (7 positions for 6 old entries, where a native trace groups values under one key). It also reads `Dlt` once, whole, as one run holding all three streams' deltas.
     - *What native reads that the session does not*: `J`'s `plus` and the intermediate `ΔJ` (above), each delta's reads by each operator that uses it, the passes over a trace's values per delta value (the values and weights exceed the keys for this reason), and seven probes where the session makes none.
     - *The totals favor the session at this scale* (11 keys, 11 values, 11 weights, no seek, against 16, 41, 38, and 7 probes), but the comparison does not yet isolate the design: the session's count lacks the evaluation of the delta rules, and native's includes the part above. The pinned numbers show only that, for these eight old and five delta rows, the session traverses the index once and reads those totals.

     Caveats:

     - Eight old rows and five delta rows: the counts show the mechanism, not a scale.
     - The placement is whatever each arm's pressure produced (Critical in every sample, in 5 of 5 repeats), not a typical workload's. At Critical pressure every run, the delta batches included, is in a file, so the old state is not in files with the delta in memory as a long-running pipeline would have it. The user decided on 2026-11-10 not to imitate that placement.
     - The session does not compute the view change itself: the view circuit's joins do, over copies of the buffered partner vectors (since step 5, over the readers' batches).
     - Pressure is sampled. The tests print the level at three points and pin counts, never a level or a layout, except that a native run whose reads are all decodes pins the 6 + 1 split.
     - Real conclusions wait for a full realistic workload.

     **Pinned: the A/B.** `Old` has 18 keys (17 in a file run, `c7` in a memory run) over customers `c3`, `c5`, `c6`, `c7`, `c8`, `c9`. `Dlt` touches `c5` (old entries `c5`, `o3`, `l7`, `l8`, `o4`, `l9`; delta entries `-c5`, `+c5'`, `o5`, `l10`, `l11`), `c8` (a delta line item; its old customer is rejected by the segment filter, so its order and line item are passed unread), and `c10` (past the end of `Old`). Both passes buffer the same entries, and decode seven old payloads, eight delta payloads, seven values, and seven weights:

     | Pass | Logical seeks | Logical steps | Positions | Run seeks | Key decodes |
     | --- | --- | --- | --- | --- | --- |
     | `Seek` | 4 | 6 | 10 | 5 | 10 |
     | `Step` | 0 | 18 | 18 | 0 | 17 |

     What the table shows:

     - Seeking and stepping buffer the same entries. Stepping decodes seven more keys here, one for each key that seeking jumps over (`o1` and `l1`; `o6` and `l12`; `o8` and `l13`; `o9`), and takes 18 steps against 6.
     - Memory runs add seeks but no decodes, so the key decodes of a pass equal its file-run positions.
     - *Before 2026-11-09* a second knob, `GroupStart::ProbeThenRange`, probed the root entry first. It replaced two prefix seeks by exact probes landing on the same keys, saved no key decode or run seek, and added one filter rejection (the probe of `c10`). It was removed.

3. **Publish `(Old, Dlt)`, and read the after state as the overlay.** *Done 2026-11-10.*
   - `ManualIndex::append` returns `(Old, Dlt)`: the snapshot before the append and the appended run, which the spine shares. `Q3State::publish` replaces the two snapshots (it took a placement map until step 4 dropped it).
   - *As of step 3:* the four readers were not rewritten. `Q3State` built their after view as `Old`'s runs with `Dlt` as one more run (none when `Dlt` is empty), and the after readers read it through `CursorList`, which consolidated across runs. That gives the same Z-set as `Old ⊎ Dlt` (`prop:overlay`). Step 4 replaced this: the readers read the session's buffers, which keep the two apart.
   - The COL baseline already modeled the after state as `Old` plus `Dlt`, so its counts are unchanged. In the harness, the after reads now take `Dlt` itself, not a copy that the spine may have spilled, so the after readers' counts can move from file decodes to `memory_*` reads (measurement version 7). `Dlt` follows step placement (2026-11-10): it is in memory unless the runtime is at Critical memory pressure, where it is a file.
   - *Exit:* `check_readers` and the transcript comparison pass, and the after snapshot is gone. `after_reads_overlay_dlt_on_old` (COL) pins the overlay and the accessors.
4. **Hand-written view circuit.** *Done 2026-11-10* (commits `0dea7a708` to `0d77f3741`; decisions of 2026-11-10 in [decisions.md](decisions.md)). It replaces the generated reconstructed circuit and meets (A1) and (A2).
   - `merged-index-col/src/q3.rs`:
     - One `ScanSession` per transaction, `Q3State::collect(Pass)`. It buffers the touched groups of `{C, O, L}`, and `finish` releases them. `J` and `V` are separate operators, and `V` flushes only after `J` has passed every customer, so the groups stay until the view transaction commits.
     - The four readers are descriptions read through a borrowing `VirtualBuffer` (`Old` for before reads; `Old` then `Dlt`, unconsolidated, for after reads). A request outside the touched groups errors, and the join's `expect` panics.
     - The `*Batch` aliases are now `*Vector` (decision of 2026-11-10). `scan`, `scan_entry`, the after-overlay snapshot, the placements, and `IndexReads` are gone. The harness's interface is `publish(old, dlt)`, `collect`, `push_driving_deltas`, and `finish() -> SessionReads`.
     - In `crates/dbsp`, `on_group` takes the group's buffers by value, so the caller decides when to release them. `SessionCounts` gained `Serialize` and `AddAssign`.
   - `q3-ivm-baseline/q3-trie-view-circuit` (committed): `J` on `(custkey)` keeps `(custkey, orderkey)`; `V` on `(custkey, orderkey)`; the inputs are keyed by their full trie paths; there is no `ERROR_VIEW`; the harness gets the input handles through an `Arc<OnceLock<TrieInputs>>`.
   - `reconstruction::prepare(change, old, orphans, base_owner)` completes `Dlt` for every change (the case table is in its doc comment): the owner table (`cb`, `ca`), moves, and `OrphanLines`, the side store by `orderkey` that sits beside `ManualIndex` as `Pipeline.orphans`. Both RF1/RF2 rejections and the placement map are gone. `prepare` and the append are timed as `index_ms` inside `base_ms`.
   - Harness: the view phase collects the groups, pushes the driving deltas, then runs the transaction. `generate.sh` takes no option, the view circuit lost its `merged-index` feature, and `verify.sh` compares two transcript pairs (`rf1_rf2_transcript` and `general_transcript`, the latter written to `$Q3_GENERAL_TRANSCRIPT`).
   - Measurement version 8: `index_run_reads` is `{session}`, with the new `index_session_counts` and `index_upkeep_run_reads` ([measurement.md](measurement.md#report-fields)).
   - *Exit:* both transcript comparisons with native pass.
   - *New tests:*
     - `prepare`, one for each row of the case table: `lines_of_an_order_with_one_owner_fold_from_the_delta_alone`, `a_moved_order_takes_its_old_and_new_lines_with_it`, `a_deleted_order_leaves_its_kept_lines_in_the_side_store`, `an_arriving_order_takes_the_orphan_lines`, and `a_net_delta_that_contradicts_the_state_is_rejected`;
     - COL: `virtual_buffer_reads_match_a_brute_force` (a property test), `joined_orders_are_read_at_the_customer_and_order_of_the_request`, `collect_reads_the_index_once_and_the_readers_not_at_all`, `driving_deltas_carry_trie_keys`, `publication_collection_and_release_follow_one_order`, and `cancelled_keys_have_no_values_and_untouched_groups_are_refused`;
     - `q3-trie-view-circuit`: `deltas_beyond_rf1_rf2_match_the_brute_force_join` and `single_stream_transactions_and_empty_ones_match`;
     - harness: `general_transcript`, `random_general_changes_match_the_reference_join`, and `native_deltas_match_full_join_across_updates`, which now runs in both builds (its orphan line included); `session_worked_transaction_tests` also runs the view circuit.
   - *Retired tests:* `rejects_missing_placement_and_incomplete_rf2`, `publication_and_batch_local_placement_are_required`, `candidate_locations_follow_each_snapshot_without_duplicating_rows`, `requested_orders_require_nonempty_candidate_locations`, `cancelled_and_missing_keys_have_no_values`, and `todays_readers_run_reads_on_the_worked_transaction` (its table under step 1 stays as history).
5. **Typed readers filled at collection.** *Done 2026-11-11* (decisions of 2026-11-11 in [decisions.md](decisions.md)). DBSP's joins stay; only their partner reads change.
   - `crates/dbsp/src/trace/merged_index/reader.rs` ([Readers](#readers-step-5)): `Reader<D, K, V>`, `reader::collect`, `ReaderRuns`, and `TouchedGroups`.
   - `merged-index-col/src/q3.rs`: four readers (customers by `custkey`, orders by `custkey`, orders by `(custkey, orderkey)`, and line items by `(custkey, orderkey)`). `C⁺`, `O⁻`, and `L⁻` check that each requested customer was collected and return the reader's batches. `J⁺` is `J`'s join on each requested path. `VirtualBuffer`, `Version`, `Node`, and the per-stream groups are gone. `take_driving_deltas` replaces `push_driving_deltas`: `ΔO` and `ΔL` are moved out of their readers, and `ΔC` is cloned.
   - `q3-trie-view-circuit`: three `add_input_stream` handles (`TrieInputs`) replace the `ZSetHandle`s; they take whole batches, through DBSP's `Input` operator as in native mode.
   - Harness: the view phase is `collect`, the transaction, and `finish`. Measurement version 10.
   - *Exit:*
     - Both transcript comparisons pass (`verify.sh`).
     - Each buffered old entry is read and decoded once: `session_run_reads_on_the_worked_transaction` and `collect_reads_the_index_once_and_the_readers_not_at_all` keep their pins, and a second reader of a stream costs no payload decode (`a_second_reader_of_a_stream_costs_no_payload_decode`).
     - COL has no invoices. A position outside `inputs` costing a key decode only is pinned by the session's counter tests (step 2).
   - *New tests:*
     - the `merged_index::reader` tests, among them the property test `readers_match_a_brute_force`;
     - COL's `readers_match_a_brute_force`, renamed from `virtual_buffer_reads_match_a_brute_force`, whose before and after reads now cover the whole touched groups.
   - *Not done:* reading the readers' rows without building batches (`J⁺`'s copy remains), and releasing each group before the next.
6. **Measurement, new version.**
   - Add the counters to `measure.py`, and run the native and reconstructed (buffered) modes on Linux, with the session's `Pass` knob as an A/B.
   - *Exit:* raw artifacts under `target/`; [measurement.md](measurement.md) reports base and IVM side by side.
7. **Beyond RF1/RF2.** *Done 2026-11-10, as part of step 4.* `prepare` supports customer updates, line items added to existing orders, orders that change customer, and orphan line items. *Exit:* `general_transcript` passes in both modes.
8. **Deferred:**
   - selective execution ([deferred.md](deferred.md#selective-execution));
   - plans derived from the spec;
   - more than one worker, sharded by `custkey`;
   - groupings, whose state the write-up leaves for later work.

## Unconfirmed choices

- The session as one DBSP operator over the ternary streaming interface, or the quaternary one for wider plans.
- `ΔJ` internal to the operator unless a plan exports it.
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

Step 4 (2026-11-10):

- No `ERROR_VIEW` in the hand-written circuit; the generated circuit's error view is absent from the reconstructed build.
- The IVM window (settled on 2026-11-11, measurement version 9): in reconstructed mode `q3_ms` and `ivm_ms` include the session's collection. Completing `Dlt` stays in `index_ms`. Since step 5 (version 10) `forward_ms` holds reconstructed mode's hand-off of the readers' `Dlt` batches, as it holds native's feed.
- `on_group` takes the buffers by value, not by reference, so that `collect` can keep them until `finish`.
- The compiler hook (`--q3-merged-index`, `Q3MergedIndexBinding.java`) stays in Java, unused; removing it is a follow-up.
- Owner lookup sources: for an order that is not in the delta, the driver passes the pre-transaction `BaseState` and tests pass `Reference`, both counted under `BaseRelation`.

Step 5 (2026-11-11):

- `ReaderRuns::take_delta` leaves the reader an empty `Dlt` batch, so it is called only on readers whose after read is unused (orders by customer, line items).
- Each group's `GroupBuffers` are built and then dropped once routed, rather than the session writing into the readers directly. `ScanSession` is unchanged.
- The contract check: a partner read walks its driving delta's keys and errors on a customer outside the touched groups, as step 4's readers did.
- A projection is a function pointer, `fn(&Key, &Payload) -> Result<(K, V), String>`, which errors on an entry of another stream.
- `take_driving_deltas` errors on a second call in one transaction, rather than handing over empty deltas.
