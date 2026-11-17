# Shared partner reads: one scan session per transaction, selective routing

> [!NOTE]
> **Source:** Chapters 2 and 3 of `docs/key-trie-merged-index-order.tex` in the thesis repository (renamed on 2026-11-09 from `key-trie-laminar-ranges.tex`). Chapter 2, "Interesting orderings for queries: one group at a time", gives the scan session (`sec:session`, `alg:session`, `thm:session`), the plan run over one buffered group (`sec:demand`, `alg:buffered-pipeline`), and skip scans (`sec:skip`, `alg:skip`, `lem:skip`). Chapter 3, "Interesting orderings for view maintenance: driving deltas", gives `sec:ivm`, `sec:ivm-assume`, `sec:ivm-reads` (`lem:partner`, `lem:locality`), `sec:ivm-buffered` (`alg:ivm-buffered`, `prop:group-delta`), `sec:ivm-relax`, and the appendices `sec:overlay` (`prop:overlay`) and `sec:prototype` (`tab:prototype`). This doc cites them by label, since numbers drift.
>
> **Scope:** the write-up now has buffered execution only. Selective execution (requests, `Visit`, assumption (A3)) moved to the companion note `key-trie-execution-refinements` (a PDF linked from the `.tex`). This doc follows the write-up for the algorithm. The implementation adds selective execution's requests at routing, inside each touched group, but not its skipping (see [The implementation: selective routing](#the-implementation-selective-routing) and the status in [deferred.md](deferred.md#selective-execution)). The removed material is summarized there.
>
> **Notation:** `X⁻` and `X⁺` are the states before and after the transaction. `Δ` is a transaction delta, and a "run" is an immutable sorted collection. A derived row is named by its instance node, such as `((custkey, orderkey) = (c5, o5))`.
>
> **Status:** the algorithm is settled in the write-up and implemented. Choices marked *unconfirmed* await the user; the confirmed ones are in [decisions.md](decisions.md). The step-by-step status log is in [history.md](history.md#2026-11-17-shared-traversal-status-log-audit-and-coding-roadmap).

This doc covers:

- the algorithm as Chapters 2 and 3 state it;
- how the write-up's concepts map onto Feldera code;
- why Feldera's storage needs two changes before the write-up's decode claims hold;
- the scan session and the readers that execute the algorithm, in summary;
- how today's prototype differs from the write-up;
- design work the write-up leaves to the implementation.

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

#### Partner reads shared across operators

`J` and `V` stay separate DBSP operators (decision of 2026-11-11), and each old entry is still read from the index once. The implementation keeps no group buffers (decision of 2026-11-12). Each reader keeps its requested slice, the rows that its driving delta requests, from collection until `finish`. DBSP flushes an operator only after its predecessors have flushed, so `V` starts only after `J` has passed every touched customer, and what a transaction collected stays until `V` finishes. The slices are small, because only requested entries are kept. Releasing each group before the next is collected would need the two joins to take turns per group, which DBSP's scheduler does not do.

### The implementation: selective routing

The implementation follows buffered execution's guarantees (each kept old entry is read once, groups no delta touches are passed) but replaces its buffers with requests (decisions of 2026-11-12). It restores selective execution's requests, kept path, and ready pairs inside one touched group, and not its `Visit` ([deferred.md](deferred.md#status-on-2026-11-12-requests-apply-at-routing-skipping-stays-deferred)).

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

## The write-up's concepts in Feldera code

The facts were checked against the code on 2026-11-09.

| Concept | Feldera today | Gap |
| --- | --- | --- |
| `Old` | `ManualIndex::append` takes `before` from `trace.ro_snapshot()`. A `SpineSnapshot` holds shared references to its runs, so a later compaction does not change it (`prop:overlay`(iii)). | None. |
| `Dlt` | `ManualIndex`'s `Request::Append` handler builds the run with `build_batch`, inserts a shared reference into the spine, and returns the run; `Q3State::publish(old, dlt)` takes it beside `Old`. `reconstruction::prepare` completes it for every change (see the `Dlt` row below). | None. |
| a node's prefix; a child domain's prefix | `KeyEncoder::prefix` and `then_tag` in `crates/dbsp/src/trace/merged_index/mod.rs`. The folded key is `[tag enc(value)]* 0 id`. | None. |
| `SkipPrefix`: seek past a prefix | `prefix_successor` in `crates/dbsp/src/trace/merged_index/mod.rs` returns the least byte string above every string with a prefix (`None` for an all-`0xFF` prefix). `Cursor::seek_key` is forward-only. | None for the successor; the session does not skip subtrees inside a touched group ([deferred.md](deferred.md#whole-group-over-read-for-narrow-requests)). |
| the root prefix of a `Dlt` key | `Domain` carries a byte `width`, and `KeyTrie::root_width` gives the root domain's. A group is the first `1 + root_width` bytes of a key. | None. |
| `Peek` and `Step` | `CursorList` merges the runs and reads every run's weight at each landing (`is_zero_weight`, `skip_zero_weight_keys_forward`). `FileIndexedWSetCursor::move_key` also positions the value cursor, which deserializes the first value. | `RunMerge`, and lazy value positioning ([No extra decoding](#no-extra-decoding)). |
| seek to a group | `CursorList::seek_key` seeks every run, then re-merges. | The session's `PassTo`, which counts logical seeks; the run cursors count per-run seeks. |
| `Collect`'s range test | `key().starts_with(prefix)` after a step, as in `scan` and the merged-index tests. | None. |
| decode | `unfold` and `decode_payload` in the client crate. | Decode only kept entries, from key bytes ([No extra decoding](#no-extra-decoding)). |
| the four readers | `customers_after`, `orders_before`, `joined_orders_after`, and `lineitems_before` in `merged-index-col/src/q3.rs`. Since step 5 the first, second, and fourth return a typed reader's batches (`Old`'s for a before read, `Old`'s and `Dlt`'s unconsolidated for an after read, `prop:overlay`), and `J⁺` is `J`'s join on each requested path over two readers ([Readers](#readers-step-5)). They open no cursor on the index. A request outside the touched groups returns an error, which the joins' `expect` turns into a panic of the worker. | None (step 5). |
| session operator | Not built. DBSP's joins stay, and the session runs in `Q3State::collect` (decision of 2026-11-11). | None. |
| snapshot input | Before each transaction the harness puts `(Old, Dlt)` in a shared slot (`Q3State::publish`) and runs `Q3State::collect` once, which fills the readers. The driver then hands the readers' `Dlt` batches, the driving deltas, to the view circuit's `add_input_stream` handles, and at the commit the joins read the readers' batches. `Q3State::finish` releases the readers and returns the `SessionReads`. | None (step 5). One copy remains: `J⁺`'s partner rows. |
| joins taking turns | A DBSP operator flushes only after all its predecessors have finished flushing (`FlushState::UnflushedDependencies` in `dynamic_scheduler.rs`). `V`'s join therefore starts only after `J`'s has passed every touched customer, so `Q3State` holds the readers from `collect` until `finish`. | Kept (decision of 2026-11-11). |
| `Dlt` complete under (A2) | The base transaction runs first. `reconstruction::prepare` folds its net delta into index rows, and `ManualIndex::append` builds the run before the view transaction starts. Each changed order gets a before owner and an after owner; a moved order takes its line items with it, read from `Old`; line items with no owner wait in `OrphanLines`. The owner of an order that is not in the delta comes from the base `Orders` state: the pre-transaction `BaseState` (driver) or `Reference` (tests), counted under `BaseRelation`. | None (step 4). The view circuit's inputs come from `Dlt`. |
| counters | Buffer-cache hits and misses, and Bloom and range filter statistics. `RunReads` (`trace/cursor/run_reads.rs`) counts per-run seeks in both run cursors, and filtered probes and key, value, and weight decodes in the file cursor, as per-thread running totals split by read owner (`with_read_owner`). `DBSPHandle::run_reads` reports the workers' totals. `SessionCounts` (`merged_index/session.rs`) counts the session's logical moves and payload decodes. | None. |

## No extra decoding

`thm:session` promises that the cursor decodes each position it reaches once. The write-up decodes a whole record per position, of any stream, because naming a position's stream and key takes its decode. Feldera differs in three ways, and each decides what a position costs.

1. **Key and value are separate columns.** A file run stores them apart, and the cursor deserializes each on demand. Until 2026-11-09 it deserialized the first value on every landing (`FileIndexedWSetCursor::move_key`); now a landing reads the key only.
2. **The key names the entry.** The folded key `[tag value]* 0 id` gives an entry's node and stream from its bytes alone. A position that no reader keeps needs a key decode and nothing else.
3. **A snapshot is several runs.** One logical cursor is `k` run cursors, and entries may cancel across runs, such as a deletion not yet compacted. `CursorList` reads every run's weight at each landing in order to skip zero-weight keys.

### What counts as a decode

| Level | Where | Allowed for |
| --- | --- | --- |
| Key decode | Storage deserializes a key when a run cursor lands on it. | Every position the session reaches, plus the first key at or past each seek target in each run. |
| Value decode | Storage deserializes a value. | Kept old entries only, once each, plus the counted cancelled entries. |
| Weight decode | Storage deserializes a weight. | Kept old entries only, once each, plus the counted cancelled entries. |
| Payload decode | The client's `decode_payload`, plus `unfold` if the full key is parsed. | Kept old entries only, once each. |

An entry is *kept* when some reader requests it, which routing decides from its folded key alone. In the worked transaction the kept old entries are `c5`, `o3`, `l7`, `l8`, `o4`, and `l9`. `c6` costs a key decode, to end `c5`'s range, and nothing else.

### Remedies

Both belong to step 2 of the coding roadmap (now in [history.md](history.md#2026-11-17-shared-traversal-status-log-audit-and-coding-roadmap)).

1. **Lazy value positioning in DBSP's file cursor.** `move_key` positions only the key cursor. The value cursor is filled the first time `val`, `weight`, or `map_times` asks. Laziness lives in DBSP's own cursor, not in a cursor only the session uses, so native joins get the same saving and the comparison stays fair.
2. **A merging cursor of the session's own, `RunMerge`.** It holds one cursor per run of `Old`, finds the minimum key by linear scan, and steps and seeks reading no value or weight. It sums weights only when the session takes an entry. An entry whose weights cancel across runs is read once and dropped. This is an inherent cost of reading an uncompacted run set, and it is counted, not hidden.

The session also routes an entry from its key bytes (the stream id, the last byte), decodes its payload once if the stream is in `inputs`, and reads `Dlt` once with one forward cursor. `Dlt` is an in-memory run, so its values are already deserialized.

### How the counters check the rules

The counters count each level separately, together with logical seeks and per-run seeks. DBSP's run cursors count per-run seeks, key decodes, value decodes, and weight decodes of file runs, and the key, value, and weight reads of in-memory runs apart (step 1 of the coding roadmap in history.md). The scan session counts groups, cursor opens, logical seeks and steps, positions reached, payload decodes, and cancelled entries (`SessionCounts`, step 2). The exit checks of steps 2 and 5 assert two things:

- value, weight, and payload decodes from `Old` equal the number of kept old entries, plus the counted cancelled entries;
- key decodes are at most the positions reached, plus one per run per seek.

On the worked transaction, with one group under `Seek` (the session's knob, below), this means seven key decodes (`c5` at open, then `o3`, `l7`, `l8`, `o4`, `l9`, and `c6`), and six value, six weight, and six payload decodes from `Old`. It means no seek: `Old` opens on `c5`, so the pass to `c5`'s prefix finds every run at or past it and moves nothing. The plan expected one logical seek and two per-run seeks, but a seek that moves no run costs nothing and counts nothing. The A/B below pins seeks that do move runs.

## The scan session

One scan session per transaction reads the touched groups for selective routing. It is schema-independent, and it is a cursor over the touched groups of `Old`: it passes the untouched groups, positions once at each touched group, and lets the caller scan the group's range entry by entry, deciding from the folded key alone whether to read an entry's payload. It buffers nothing and calls nothing back: the `on_group` callback of 2026-11-09 and the group buffers are gone (decision of 2026-11-12). Since step 5 the caller is `reader::route` and Q3's hook, which put each kept entry into the typed readers ([Readers](#readers-step-5)); DBSP's joins evaluate `ΔJ` and `ΔV` (decision of 2026-11-11).

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

DBSP's run cursors count the storage costs: per-run seeks, filtered probes (`filtered_probes`: exact probes that a file run's filters answered with no seek; the filters are the run's key range, checked first, then one membership filter: a Roaring bitmap, which is exact, for integer keys whose span in the run fits in 32 bits, else a Bloom filter, which may answer "maybe" for an absent key; a folded key is a byte string, so the merged index's runs use the Bloom filter; memory runs have no filters), and key, value, and weight decodes. In-memory runs count their key, value, and weight accesses in separate fields, `memory_key_reads`, `memory_value_reads`, and `memory_weight_reads` (see [decisions.md](decisions.md)), by the same rules as the file decodes: a landing reads a key, a value counts once per position, and each weight returned reads a weight. A memory access deserializes nothing, so the two kinds are never added.

Seeks count what the file reader does:

- **Forward.** A `seek_key` that moves counts one seek and one key decode, for the row it lands on. The reader's search (`advance_to_value_or_larger`) compares the target with each probed key where it lies in the block, without deserializing it. A seek whose target is at or below the current key does not move and counts nothing. A predicate seek deserializes each probed key for its predicate, and counts each.
- **Reverse.** On a file run, a reverse seek that lands on a row counts two decodes of its key, or of its value for a value seek. `rewind_to_last_le` deserializes what it finds (`best_match`), and the cursor then deserializes it again (decision of 2026-11-16). A memory run counts one. One real decode goes uncounted: a cursor before its first row stays there after a reverse seek, even when `best_match` found and deserialized a key, and the cursor cannot tell that from a seek that found nothing (pinned by `a_reverse_key_seek_from_before_the_first_row_counts_no_key`). Counting it would need the file reader to report whether `best_match` found a row; no merged-index or Q3 path seeks backward, so the miss reaches no report.
- **What a probe compares.** A probe and a decode see the same bytes. A merged-index key is a `FoldedKey`, the tagged bytes `[tag value]* 0 id` in one `Vec<u8>`. A file run stores every key serialized by rkyv, and rkyv's serialized ("archived") form of a `FoldedKey` is a length followed by those bytes. A probe compares them in place; a decode copies them into an owned `Vec`, which is what `key_decodes` counts. A native run's typed tuple keys are compared field by field in their archived form, in the same way.

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

A `RowBuilder` pushes each row straight into DBSP's builder, one batch per slice and one copy per kept row. It takes rows an *entry* at a time: the rows that one folded key yields, which are at most three (an `Old` row, and `Dlt`'s `-old` and `+new`). Within an entry the rows may be reordered and consolidated among themselves (a cancelling pair is dropped, and `Old` plus `-old` plus `+new` is `+new`), because the packed payload bytes do not order like the typed values (little-endian integers, length-prefixed strings). Across entries `(K, V)` must already arrive strictly ascending, which the folded keys' order gives, and the builder errors otherwise: there is no general sort. An entry of more than three rows is an error. This is the user's decision of 2026-11-12 (option 1).

The circuit's input handles take the driving deltas by move (`Delta::into_batch`); no after read shares them, so nothing is cloned. A partner read, called by the join, errors on a delta key outside its reader's requests.

A join with a snapshot reader lets its delta drive and does not sample the partner. Its calls on the partner batch, and its reader callback, are counted under the `requested_slice` read owner, apart from the DBSP operators' own reads of state and deltas.


The library side of both sections is mapped in [merged_index/CLAUDE.md](../../../crates/dbsp/src/trace/merged_index/CLAUDE.md), and the Q3 readers and hook in [merged-index-col/CLAUDE.md](../../../sql-to-dbsp-compiler/experiments/merged-index-col/CLAUDE.md).

## Current implementation against the write-up

This table refreshes `tab:prototype`.

| Aspect | Prototype today | Write-up |
| --- | --- | --- |
| Reads | `C⁺`, `O⁻`, `J⁺`, `L⁻`, as listed in [merged-index-col/CLAUDE.md](../../../sql-to-dbsp-compiler/experiments/merged-index-col/CLAUDE.md) | Same |
| `V`'s key | `(custkey, orderkey)`; `J` joins on `(custkey)` and keeps `(custkey, orderkey)` (step 4) | `(custkey, orderkey)` |
| Delta delivery | Each join receives its whole driving delta once, at the transaction's flush; its partner reads are the requested slices (the readers' batches) that one scan filled | Per touched group, from its buffers |
| Base deltas | `capture` builds `ΔC`, `ΔO`, and `ΔL` once from `Dlt` (phase A) into `DrivingDeltas`, which the driver moves to DBSP's `Input` operator handles; `prepare` completes `Dlt` for general changes | The complete changes of the stored entries, `Dlt`, from index maintenance (A2) |
| Versions | `Old` for before reads; `Old` then `Dlt`, unconsolidated, for after reads | Same |
| Owner-key change | `prepare` emits both folded keys of each moved line item, reading its lines from `Old` | Same |
| Cursors and seeks | One session per transaction: one forward cursor over `Old`, a key read at every position of a touched group, each kept entry decoded once, untouched groups passed. The partner reads are the requested slices and open no cursor on the index | Same |
| Key order | Folded byte keys compared as bytes; signed integers in value order, tested | Folded order |
| Counters | `SessionCounts` and the run counters (`index_session_counts`, `index_run_reads.session`). Version 11: the read owner `requested_slice` counts the reconstructed joins' reads of the slices and routing's reads of `ΔC` and `ΔO` rows (a forward merge, no seek); `partner_trace` counts native's trace reads, partner sampling included; delta-side reads stay under `query`, labeled "DBSP operator reads"; `delta_ms` books the preparation of the view's transaction delta in the IVM window in both modes, and `forward_ms` keeps only the net-delta read; phase A's `Dlt` reads count under `merged_index`; `collect` captures every owner on the driver's thread (`driver_run_reads`) | Same |

## Design work the write-up leaves to the implementation

These are implementation questions, not algorithm questions:

- **Feeding the view side from `Dlt`.** Done in step 4, and without a row-by-row push since step 5: phase A builds the driving deltas once, and they move whole to the input handles. No driving delta is copied.
- **The cost model.** A logical seek costs one seek per run of the snapshot. Decoding has four levels: key, value, weight, and payload. [No extra decoding](#no-extra-decoding) sets the rules and the counters that check them.
- **The join plan.** The DBSP circuit itself (decision of 2026-11-11); each join's partner read is a typed reader.
- **Workers.** One worker, as decided on 2026-10-12. Sharding by `custkey` is deferred.

## Unconfirmed choices

None open. Each choice says what it is, why, and the alternative it rejects. The user's confirmations are in [decisions.md](decisions.md) (2026-11-13 and 2026-11-16). Not listed: choices that ordinary engineering practice settles, counting rules that only count work done, and the details of applying a minor adjustment the user asked for. Q3's hook, its generic successor, and `ERROR_VIEW` are in [deferred.md](deferred.md).
