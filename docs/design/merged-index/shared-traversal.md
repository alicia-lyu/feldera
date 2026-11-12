# Shared partner reads: one buffered scan session per transaction

> [!NOTE]
> **Source:** Chapters 2 and 3 of `docs/key-trie-merged-index-order.tex` in the thesis repository (renamed on 2026-11-09 from `key-trie-laminar-ranges.tex`). Chapter 2, "Interesting orderings for queries: one group at a time", gives the scan session (`sec:session`, `alg:session`, `thm:session`), the plan run over one buffered group (`sec:demand`, `alg:buffered-pipeline`), and skip scans (`sec:skip`, `alg:skip`, `lem:skip`). Chapter 3, "Interesting orderings for view maintenance: driving deltas", gives `sec:ivm`, `sec:ivm-assume`, `sec:ivm-reads` (`lem:partner`, `lem:locality`), `sec:ivm-buffered` (`alg:ivm-buffered`, `prop:group-delta`), `sec:ivm-relax`, and the appendices `sec:overlay` (`prop:overlay`) and `sec:prototype` (`tab:prototype`). This doc cites them by label, since numbers drift.
>
> **Scope:** the write-up now has buffered execution only. Selective execution (requests, `Visit`, assumption (A3)) moved to the companion note `key-trie-execution-refinements` (a PDF linked from the `.tex`). This doc follows the write-up for the algorithm. The implementation adds selective execution's requests at routing, inside each touched group, but not its skipping (see [The implementation: routing by requests](#the-implementation-routing-by-requests) and the status in [deferred.md](deferred.md#selective-execution)). The removed material is summarized there.
>
> **Notation:** `X⁻` and `X⁺` are the states before and after the transaction. `Δ` is a transaction delta, and a "run" is an immutable sorted collection. A derived row is named by its instance node, such as `((custkey, orderkey) = (c5, o5))`.
>
> **Status:** the algorithm is settled in the write-up. Roadmap step 1, the run counters, is implemented and reported by the R5 harness in both modes. Step 2, decode-free storage and the buffered scan session, is implemented (2026-11-09), and in-memory runs count their accesses in separate fields (2026-11-10). Step 3, publishing `(Old, Dlt)`, is implemented (2026-11-10). Step 4, the hand-written view circuit, is implemented (2026-11-10) and carries step 7: the circuit admits general deltas. Step 5, typed readers filled at collection, is implemented (2026-11-11). Step 5 was revised on 2026-11-12 (step 5b: requested slices and routing by requests). Step 6 is not done at version 11. This doc is the meta plan for the coding steps. Choices marked *unconfirmed* await the user. The confirmed ones are in [decisions.md](decisions.md) under 2026-10-29, 2026-11-03, 2026-11-09, 2026-11-10 (including the step-4 entries), and 2026-11-11 (step 5), and 2026-11-12 (step 5b).

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

`J` and `V` stay separate DBSP operators (decision of 2026-11-11), and each old entry is still read from the index once. The implementation keeps no group buffers (decision of 2026-11-12). Each reader keeps its requested slice, the rows that its driving delta requests, from collection until `finish`. DBSP flushes an operator only after its predecessors have flushed, so `V` starts only after `J` has passed every touched customer, and what a transaction collected stays until `V` finishes. The slices are small, because only requested entries are kept. Releasing each group before the next is collected would need the two joins to take turns per group, which DBSP's scheduler does not do.

### The implementation: routing by requests

The implementation follows buffered execution's guarantees (each kept old entry is read once, groups no delta touches are passed) but replaces its buffers with requests (decisions of 2026-11-12). It restores selective execution's requests, kept path, and ready pairs inside one touched group, and not its `Visit` ([deferred.md](deferred.md#status-on-2026-11-12-requests-apply-at-routing-skipping-stays-deferred)). It is *selective decoding with whole-group positioning* (an unconfirmed name).

```text
Phase A  capture(Dlt)             one pass over Dlt, in folded-key order
  touched groups; ΔC, ΔO, ΔL built once into the hand-off structure (DrivingDeltas);
  request prefixes of C⁺ (from ΔO), O⁻ (from ΔC), J⁺ (from ΔL), cut at each reader's request node

Phase B  for each touched group, one forward scan of Old, with the delta nodes stepped between its entries
  at each position, from the folded key alone: does some reader request it?
    yes: read, weight-sum, and decode the payload once; each keeper unfolds it into its builder
    no:  a key read and a step, nothing more
  at a customer: C⁺'s row = Old's row + ΔC's rows
  at an order:   ΔJ = ΔO ⋈ C⁺ + O⁻ ⋈ ΔC, so L⁻ requests the lines of each order where ΔJ has a row;
                 J⁺'s row where ΔL is
```

Costs:

- Phase A decodes each `Dlt` entry once (`payload_decodes_dlt`) and builds each driving delta once. The deltas move into the circuit's input handles, with no clone.
- Phase B reads a key at every position of a touched group (the over-read of [deferred.md](deferred.md#whole-group-over-read-for-narrow-requests)) and decodes only kept entries (`payload_decodes_old`).
- The joins read only the requested slices, under the `requested_slice` read owner.
- `ΔJ` is computed exactly, as a request set, and is never handed to the circuit.

Ready pairs are Q3's hook (`q3.rs`), not derived from the spec: folded keys put a customer before its orders and each order before its lines, so `ΔJ` at `(c, o)` needs only `ΔO(c, o)` and `ΔC(c)`, both known before `(c, o)`'s lines. This is assumption (A3) for Q3 by trie order, not shown in general. The [Readers](#readers-step-5) section specifies the pieces.

### Worked transaction

The transaction replaces `c5` by `c5'` and inserts order `o5` with line items `l10` and `l11`. The group of `c5` holds the old entries `c5`, `o3`, `o4`, `l7`, `l8`, `l9`, and the `Dlt` entries `−c5`, `+c5'`, `+o5`, `+l10`, `+l11`. The next group, `c6`, ends the range.

| Step | Session | Routing | Evaluation |
| --- | --- | --- | --- |
| 1 | Pass to `(custkey = c5)`: one logical seek, landing on `c5` | — | — |
| 2 | Scan `c5`, `o3`, `l7`, `l8`, `o4`, `l9`, with the delta nodes `o5`, `l10`, `l11` stepped between them; read `c6`'s key, which ends the group | `c5`: kept by `C⁺` (requested by `ΔO`); `o3`, `o4`: kept by `O⁻` (requested by `ΔC`); `l7`, `l8`, `l9`: kept by `L⁻` (`ΔJ` has a row at `o3`, `o4`) | `ΔJ` = 5 rows, computed at the orders; `J⁺` at `(c5, o5)` for `l10`, `l11` |
| 3 | Next touched group, if any | — | The joins, at the flush: `ΔV` = 8 rows (6 from `L⁻ ⋈ ΔJ`, 2 from `ΔL ⋈ J⁺`) |

Because `c5` changes, `O⁻` and `L⁻` request the whole group, so every entry is still decoded: each of `c5`, `o3`, `l7`, `l8`, `o4`, and `l9` is read from `Old` once and decoded once, and `c6` only has its key read. For an RF1-like insert, an order `o5` with line items under an unchanged `c5`, only the customer is decoded: `C⁺` requests `c5`, `ΔC` is empty so `O⁻` requests nothing, `L⁻` requests `(c5, o5)`, which has no old lines, and `o3`, `l7`, `l8`, `o4`, and `l9` cost a key read each. `ΔV` is unconsolidated, and DBSP's consolidation of the output batch removes the pairs of opposite weight. The write-up's Q3 also has invoices `v2` and `v8` under `c5`; they are positions the scan reaches and keeps nowhere. COL's tests have no invoices.

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

One scan session executes buffered execution. It is schema-independent, and it is a cursor over the touched groups of `Old`: it passes the untouched groups, positions once at each touched group, and lets the caller scan the group's range entry by entry, deciding from the folded key alone whether to read an entry's payload. It buffers nothing and calls nothing back: the `on_group` callback of 2026-11-09 and the group buffers are gone (decision of 2026-11-12). Since step 5 the caller is `reader::route` and Q3's hook, which put each kept entry into the typed readers ([Readers](#readers-step-5)); DBSP's joins evaluate `ΔJ` and `ΔV` (decision of 2026-11-11).

### Inputs

- `Old`, one pinned snapshot, read through `RunMerge`.
- The touched groups: a sorted list of group prefixes, the first `1 + root_width` bytes of a key (the tag and value of the root domain). `reader::capture` builds it from `Dlt` in one pass before the session. The session never sees `Dlt`.
- The index definition `D` (`MergedIndexDefinition`), which supplies the payload decoder (`D::decode_payload(stream_id, bytes)`). The stream id selects the payload type, so decoding needs no `unfold`. The session is bound to `D`: `ScanSession<D>`.
- One knob, below.

A trie has exactly one root domain (decision of 2026-11-09). Two root domains mean two key tries, each stored as its own merged index, and several streams may still reside at the root domain's node. The Java `MergedIndexValidator` rejects any other trie, so `root_width()` is a plain `usize`; it panics on a hand-built trie with no single root, and so does `capture` (decision of 2026-11-10: the compiler's validator is the only trie check).

The groups are inferred from `Dlt`: the group of a `Dlt` key is its first `1 + root_width` bytes. The session needs no list of requests, since a touched customer is exactly a customer with a `Dlt` entry.

### Operations

```text
ScanSession<D>(Old: snapshot → RunMerge, touched: [group prefix], pass: Seek|Step)

  NextGroup():                             enters the next touched group g
    Seek: Old.seek(g)                      one logical seek if any run moves
    Step: Old.step() while Old.key() < g   the untouched groups in between are never read

  Key():            Old.key() if it starts with g, else nothing      the merged key, already read; no value or weight
  TakeAndDecode():  Old.take, D::decode_payload(id, bytes) for each  id is the last byte of the key
  Step():           Old.step()
```

The caller's loop is alg:session's, without buffers (`reader::route` and Q3's hook are the body):

```text
capture(Dlt)                               touched groups, driving deltas, requests
for g in touched:  session.NextGroup()
  while session.Key():                     one contiguous scan of g's range, no skips
    route(position)                        keep or not, from the key alone; TakeAndDecode once if kept
    session.Step()
```

The `reject` and `Skip` of the write-up's `alg:skip` are gone with the buffers. Skipping the subtrees that no reader requests inside a touched group is the deferred refinement ([deferred.md](deferred.md#whole-group-over-read-for-narrow-requests)); the cursor's seek makes it a local change.

### Rules the session follows

- A payload is read, weight-summed (`RunMerge::take`), and decoded only when the caller takes the position. A position nobody keeps costs a key decode and a step, nothing more.
- The session never reads `Dlt`. `Old` and `Dlt` stay apart and unconsolidated, since a join is bilinear (`prop:overlay`); the readers' requested slices combine them where a join needs the after state (`C⁺`).
- The session does not seek past domains whose streams nobody keeps, such as Q3's invoices. Each such position costs a key decode and nothing else (decision of 2026-11-09).
- A delta-only node, such as `(c5, o5)`, has no entry in `Old`; the routing steps it between the entries of `Old` in key order.

### Knob

`Pass` is the one knob. It is an A/B test with equal outputs.

| Knob | Values | Compares |
| --- | --- | --- |
| `Pass` | `Seek`, `Step` | Passing the untouched groups between two touched ones by one logical seek against stepping through their entries. |

A group read has no exact-key probe (decision of 2026-11-09). A probe of the group's root entry with `seek_key_exact` moves a run whose filter admits the key onto it, but a run whose filter rejects the key stays behind and must be sought anyway, since it can still hold entries below the root. The per-run seeks are then those of a prefix seek, plus the filter checks. The saving of 2026-11-04 holds for path reads only, which take a single record.

### Counters

`SessionCounts` holds:

- `groups`;
- `cursor_opens` (one: the logical cursor over `Old`; since 2026-11-12 the session opens none over `Dlt`);
- `logical_seeks`: seeks over `Old` that moved some run. A seek that finds `Old` already at or past its target costs nothing and counts nothing, so a first group that `Old` starts on counts no seek;
- `logical_steps`;
- `positions`: the first key of `Old`, and each key that a step or seek landed on. Each costs one key decode in a file run;
- `payload_decodes_old`: one per payload of a position that a reader kept; `payload_decodes_dlt`: one per `Dlt` entry that a driving delta took, filled in by `capture`, which reads `Dlt`;
- `cancelled`: values whose weights cancelled across runs.

DBSP's run cursors count the storage costs: per-run seeks, filter rejections, and key, value, and weight decodes. In-memory runs count their key, value, and weight accesses in separate fields, `memory_key_reads`, `memory_value_reads`, and `memory_weight_reads` (see [decisions.md](decisions.md)), by the same rules as the file decodes: a landing reads a key, a value counts once per position, and each weight returned reads a weight. A memory access deserializes nothing, so the two kinds are never added.

### Ownership

| Owner | Holds |
| --- | --- |
| The session | `RunMerge` over `Old`, the touched-group list, the knob, and `SessionCounts` |
| The capture and the routing (`reader::capture`, `reader::route`, Q3's hook) | `Dlt`'s driving deltas and requests, and the kept entries as typed rows in the readers, which hold them until `finish` |
| DBSP's joins | The evaluation of the delta rules, and derived deltas such as `ΔJ` |
| DBSP's run cursors | The run counters: per-run seeks, key decodes, value decodes, and weight decodes |

## Readers (step 5)

A *reader* is one partner read of one join (decisions of 2026-11-11 and 2026-11-12). It holds the requests of its driving delta, the delta input of the join it serves, and a partner batch holding only the rows that the driving delta requests: the requested slice of the index. The deltas are not the readers': `capture` builds `ΔC`, `ΔO`, and `ΔL` once each into the structure that the driver moves to the circuit (a `Delta<R>` per stream-driven reader, paired with its reader by `Capture`), and the routing borrows them. Its static plan is a `ReaderDefinition`, in the reader's type:

| Constant or function | Meaning |
| --- | --- |
| `PARTNER_STREAM` | The index stream whose entries the partner batch keeps, or none, when the routing builds the rows (`J⁺`) |
| `REQUEST_DEPTH` | The number of leading key fields that name the request node: 1 is the group, 2 is `(custkey, orderkey)`. The prefix width follows from the trie (`KeyTrie::prefix_width`). |
| `DRIVING_DELTA` | `Stream(id)`: the `Dlt` rows of one stream, unfolded into a batch the reader owns. `Join`: the output of a join of other readers' rows, evaluated at routing; the reader keeps its requests only. |
| `unfold`, `unfold_delta` | The functions from an index entry, its unfolded key and decoded payload, to a row of the partner batch and of the driving delta |

Only the requests come from the data. The four Q3 readers:

| Reader | Requests of | Keeps (partner rows) |
| --- | --- | --- |
| `C⁺` | `ΔO` | the customer of each order in `ΔO`, after: `Old`'s row and `ΔC`'s rows, consolidated |
| `O⁻` | `ΔC` | all orders of each customer in `ΔC`, before |
| `J⁺` | `ΔL` | `J` at each `(custkey, orderkey)` of `ΔL`, after, built at routing |
| `L⁻` | `ΔJ`, requests only | the lines of each order where `ΔJ` has a row, before |

```text
capture(Dlt, readers):                         phase A: one pass over Dlt, in folded-key order
  for each entry d: touched += group(d.key)
    for each reader R with DRIVING_DELTA = Stream(d.stream):   decode and unfold d once
      R.delta.push(R.unfold_delta(d), d.weight)                the one copy
      R.requests += prefix(d.key, R.REQUEST_DEPTH)             sorted, since Dlt is sorted
  seal each driving delta

scan(session, readers):                        phase B: one contiguous scan per touched group
  for g in touched:
    for each Old position k, in folded-key order:
      for each stored reader R: keep[R] = R.stream = stream(k) and prefix(k) ∈ R.requests   a forward pointer, no search
      hook(k)                                 Q3: the customer step at the customer, the order step at each order,
                                              including orders only ΔO mentions, stepped between the entries of Old
      if any keeps k: decode k's payload once; each keeper unfolds it straight into its partner builder
```

Q3's hook computes `J` because the generic machinery has no notion of a join. At a customer it forms the customer after (`Old`'s rows plus `ΔC`'s); at an order it forms `J⁺`'s rows (customer after times order after) where `ΔL` is, and `ΔJ = ΔO ⋈ C⁺ + O⁻ ⋈ ΔC` where `ΔO` or `ΔC` is. A node where the two terms cancel has no row in `ΔJ` and is not requested by `L⁻`, so the requests equal the nodes of the circuit's own `ΔJ`, which is never handed over. Folded keys put a customer before its orders and each order before its lines, so one forward pass knows both terms before an order's lines arrive.

A `RowBuilder` pushes each row straight into DBSP's builder: one copy per kept row. It requires nondecreasing keys and errors on a key that goes back. The rows of one key may arrive in any order, because the packed payload bytes do not order like the typed values (little-endian integers, length-prefixed strings), so it sorts and consolidates the values of a key when the key ends.

The circuit's input handles take the driving deltas by move (`Delta::into_batch`); no after read shares them, so nothing is cloned. A partner read, called by the join, errors on a delta key outside its reader's requests.

A join with a snapshot reader lets its delta drive and does not sample the partner. Its calls on the partner batch, and its reader callback, are counted under the `requested_slice` read owner, apart from the DBSP operators' own reads of state and deltas.

## Current implementation against the write-up

This table refreshes `tab:prototype`.

| Aspect | Prototype today | Write-up |
| --- | --- | --- |
| Reads | `C⁺`, `O⁻`, `J⁺`, `L⁻`, versions as in the table above | Same |
| `V`'s key | `(custkey, orderkey)`; `J` joins on `(custkey)` and keeps `(custkey, orderkey)` (step 4) | `(custkey, orderkey)` |
| Delta delivery | Each join receives its whole driving delta once, at the transaction's flush; its partner reads are the requested slices (the readers' batches) that one scan filled | Per touched group, from its buffers |
| Base deltas | `capture` builds `ΔC`, `ΔO`, and `ΔL` once from `Dlt` (phase A) into `DrivingDeltas`, which the driver moves to DBSP's `Input` operator handles; `prepare` completes `Dlt` for general changes | The complete changes of the stored entries, `Dlt`, from index maintenance (A2) |
| Versions | `Old` for before reads; `Old` then `Dlt`, unconsolidated, for after reads | Same |
| Owner-key change | `prepare` emits both folded keys of each moved line item, reading its lines from `Old` | Same |
| Cursors and seeks | One session per transaction: one forward cursor over `Old`, a key read at every position of a touched group, each kept entry decoded once, untouched groups passed. The partner reads are the requested slices and open no cursor on the index | Same |
| Key order | Folded byte keys compared as bytes; signed integers in value order, tested | Folded order |
| Counters | `SessionCounts` and the run counters (`index_session_counts`, `index_run_reads.session`). Version 11: the read owner `requested_slice` counts the reconstructed joins' reads of the slices and, pending, routing's reads of `ΔC` and `ΔO` rows (now a cursor seek per key under `query`); `partner_trace` (pending) will count native's trace reads; delta-side reads stay under `query`, labeled "DBSP operator reads"; `delta_ms` (pending) books the preparation of the view's transaction delta in the IVM window in both modes, and `forward_ms` keeps only the net-delta read; phase A's `Dlt` reads count under `merged_index` (pending) | Same |

## Design work the write-up leaves to the implementation

These are implementation questions, not algorithm questions:

- **Feeding the view side from `Dlt`.** Done in step 4, and without a row-by-row push since step 5: phase A builds the driving deltas once, and they move whole to the input handles. No driving delta is copied.
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
  5b. **Requested slices and routing by requests.** *Done in code on 2026-11-12, except the pending items below* (decisions of 2026-11-12 in [decisions.md](decisions.md)). Each reader holds its driving delta's requests and keeps only the rows it requests ([The implementation: routing by requests](#the-implementation-routing-by-requests), [Readers](#readers-step-5)).
   - `reader::capture` (phase A) replaces the session's reading of `Dlt`. `ScanSession` is a cursor over the touched groups (`new(old, touched, pass)`, `next_group`, `key`, `take_and_decode`, `step`). `reader::route` and Q3's hook scan each group once.
   - `GroupBuffers`, `on_group`, `reject`, `Projection`, `ReaderRuns`, `TouchedGroups`, `values_at`, and `RequestedRows` are gone. `ΔJ` and `J⁺` are computed at routing.
   - No driving delta is cloned: `C⁺`'s batch holds `ΔC`'s rows of the customers of `ΔO`.
   - The `requested_slice` read owner counts the joins' reads of the slices.
   - *Pending:* the `partner_trace` read owner (native's trace reads, partner sampling included); routing's reads of `ΔC` and `ΔO` rows by a forward merge with the scan, under `requested_slice`; phase A's `Dlt` reads under `merged_index`; `collect` capturing every owner on the driver's thread; `delta_ms`.
   - *Exit:*
     - Both transcript comparisons pass (`verify.sh`) on the merged tree.
     - A test asserts that `collect` leaves zero reads under `query` on the driver's thread.
     - The session and reader counter pins hold: kept entries are decoded once, unrequested entries cost a key read.
   - *Not done:* skipping the subtrees that no reader requests inside a touched group: the scan still reads a key at every position of a touched group ([deferred.md](deferred.md#whole-group-over-read-for-narrow-requests)).
6. **Measurement, new version.** *macOS part done 2026-11-11; Linux pending.* The version stayed 10 (decision of 2026-11-11). Step 5b bumps it to 11 (read owners `requested_slice` and `partner_trace`; `delta_ms`; [measurement.md](measurement.md#report-fields)); no version 11 result exists, and the smoke rerun at version 11 is pending.
   - Done: the driver's `--session-pass seek|step` selects `Pass` (`configuration.session_pass`); `measure.py` forwards it and checks `index_session_counts`, `index_upkeep_run_reads`, and `profiled_cpu_ms`; `summarize.py` reports them. Every run reads one set of inputs in place and starts cold (decision of 2026-11-11).
   - Done: the macOS smoke run, both modes, both passes, three repetitions ([measurement.md](measurement.md#version-10-smoke-2026-11-11)). All state stayed in memory. The passes collected the same entries; Step reached about 400,000 more positions than Seek and took about 30 ms more `ivm_ms` over six refreshes.
   - Pending: the Linux pilot and campaign, four arms (prefix truncation × pass; [WORKFLOW.md](WORKFLOW.md#campaign-and-four-arms)).
   - *Exit:* raw artifacts under `target/`; [measurement.md](measurement.md) reports base and IVM side by side.
7. **Beyond RF1/RF2.** *Done 2026-11-10, as part of step 4.* `prepare` supports customer updates, line items added to existing orders, orders that change customer, and orphan line items. *Exit:* `general_transcript` passes in both modes.
8. **Deferred:**
   - selective execution ([deferred.md](deferred.md#selective-execution));
   - plans derived from the spec;
   - more than one worker, sharded by `custkey`;
   - groupings, whose state the write-up leaves for later work.

## Unconfirmed choices

- `prefix_successor` returning `Option`, with `None` meaning the range runs to the end of the index, rather than a sentinel key.
- `RunMerge` finding the minimum key by linear scan over its run cursors, not a heap. Snapshots hold few runs.
- The once-filled value slot as the shape of lazy values in the file cursor (a `OnceCell`), with a hint field that keeps the last value cursor for `first_with_hint`.
- The counting rules of `RunReads`: opening a cursor counts the key and value it lands on; value seeks count no seek; each probe of a predicate seek counts a key or value decode; a reverse seek counts one key decode, though the file reader may read the key twice. Lazy values (step 2) change the first of these: a landing counts the key only. In-memory runs follow the same rules in their own fields (2026-11-10): a value counts once per position, through a `Cell<bool>` that every key move clears and every value move sets, and a predicate seek counts one key read for each probe.
- The read owner reuses the `WriteOwner` enum, so one list names the components for reads and writes. It is a separate thread-local from the write owner, so a read scope never changes who owns files created inside it. `RunReads::current` stays the total over all owners.
- A `filter_rejections` run counter, so that native join probes that a file filter answers stay visible.
- A separate `weight_decodes` run counter. The file format stores a value and its weight apart: landing on a value deserializes the value, and the weight is deserialized only when `weight`, `map_times`, or `map_values` asks. `CursorList`'s zero-weight skip reads weights at entries that no reader takes, so a separate counter shows that cost. Folding weights into value decodes would hide it.
- `cursor_opens` is the logical cursors, not the run cursors: one, over `Old` (the session opened a second over `Dlt` until 2026-11-12).
- `ScanSession::new` takes `Old` as a snapshot and opens its `RunMerge` itself, so the opening's key decode falls inside the session's counted region.
- `RunMerge::exhaust`, for a skipped prefix with no successor, rather than stepping to the end (unused by the session since 2026-11-12, which skips nothing inside a group).

Step 4 (2026-11-10):

- No `ERROR_VIEW` in the hand-written circuit; the generated circuit's error view is absent from the reconstructed build.
- The IVM window (settled on 2026-11-11, measurement version 9): in reconstructed mode `q3_ms` and `ivm_ms` include the session's collection. Completing `Dlt` stays in `index_ms`. Since step 5 (version 10) `forward_ms` holds reconstructed mode's hand-off of the readers' `Dlt` batches, as it holds native's feed.
- The compiler hook (`--q3-merged-index`, `Q3MergedIndexBinding.java`) stays in Java, unused; removing it is a follow-up.
- Owner lookup sources: for an order that is not in the delta, the driver passes the pre-transaction `BaseState` and tests pass `Reference`, both counted under `BaseRelation`.

Step 5 (2026-11-11):

- `take_driving_deltas` errors on a second call in one transaction, rather than handing over empty deltas.

Step 5b (2026-11-12). The user confirmed on 2026-11-12 (so these are in [decisions.md](decisions.md), not open): `requested_slice`, `partner_trace`, `DrivingDelta`, `unfold`, measurement version 11, readers holding prefixes, and `delta_ms`. Still unconfirmed, in the audit's order (largest effect first):

1. The name "selective decoding with whole-group positioning" for the current hybrid.
2. The doc title "one buffered scan session per transaction", which no longer fits an implementation with no group buffers.
3. Q3's hook computing `J` (ready pairs) rather than a generic `DrivingDelta::Join`.
4. A `ReaderDefinition` with `PARTNER_STREAM = None` for `J⁺` and `L⁻`'s request-only role, so the generic machinery stops at stored readers.
5. A `RowBuilder` that sorts the values of one key locally and drops a key whose values all cancel.
6. The readers behind one lock in `Q3State`.
7. The write-up and the companion note live in the thesis repository and are not edited here; their text still says buffered execution only.
