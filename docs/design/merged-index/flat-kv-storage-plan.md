# Step 2: Flat weighted records on Feldera's LSM

Date: 2026-09-28. Status: implementation plan; no runtime implementation is claimed.

## Accepted contract and references

Implement the storage substrate for a Customer–Orders–Lineitem merged index. The user's clarifications in
this conversation govern this plan. Attached manuscripts provide design and algebraic references; their
historical implementation proposals are not additional user instructions.

| Reference | Use in this plan |
| --- | --- |
| [Interesting-orderings record structure](../../../../merged_index_interesting_orderings/main.tex#L362) | Domain-tagged fields, followed by the index-identifier domain and its value; payload stays outside K. |
| [Interesting-orderings folding example](../../../../merged_index_interesting_orderings/main.tex#L545) | Concrete shape of the Customer–Orders–Lineitem keys. |
| [DBSP source schema](../../../../DBSP_w_merged_index/dbsp-merged-index-feasibility.tex#L120) | Extended Lineitem records already carry customer, order, and line identity. |
| [DBSP signed changes](../../../../DBSP_w_merged_index/dbsp-merged-index-feasibility.tex#L205) | Retractions have negative weights and match complete tuples, including payload. |
| [DBSP old/new algebra](../../../../DBSP_w_merged_index/dbsp-merged-index-feasibility.tex#L508) | Before state and before-plus-delta state; this plan appends changes before view maintenance. |
| [Operator-state guide](../../../../DBSP_w_merged_index/operator-state.tex#L39) | Numeric summaries, complete output tuples, and affected-key support have different roles. |
| [mi_db scan sessions](../../../../mi_db/docs/architecture.md#merged-index-scan-sessions) | Reference for the later typed-accessor/session layer, outside this storage step. |
| [Multi-pipeline manuscript](../../../../query_execution_using_MI/main.tex#L88) | Future pipeline composition; this step covers one merged index. |

### Clarifications incorporated

1. `INDEX` is the domain tag introducing the index identifier. There is no additional `END` marker.
   Numeric tag values are encoding choices; parent-before-child order constrains their relative values.
2. There is no `OrderParent` record, lookup namespace, or required reverse-parent lookup in this adapter.
3. Scans seek a prefix and advance the storage iterator until the prefix changes. No `successor` API is needed.
4. Each logical record is one flat `(K, V)` pair; physical columns may split its fields. `V` contains one
   regular payload and a signed weight. There are no payload lists, per-record `format`
   fields, or redundant `kind` fields.
5. An **immutable batch** contains many records and keys; it can reside in memory or in a **layer file**.
   Examples showing one key are excerpts from batches, not one file per key.
6. Existing state and the incoming delta belong to the same merged index. Append the complete delta before
   computing the view delta. Completion does not append those weights a second time.
7. Step 2 supplies folding/unfolding primitives and raw weighted byte cursors. Typed query returns,
   reconstruction, shared scan sessions, and runtime transaction integration remain subsequent work.
8. The transaction supplies all intended related row changes. Storage neither synthesizes Lineitem moves
   from an Order change nor enforces relational consistency. Input records include their intended before/after
   extended keys. Structural decoding and storage-protocol checks remain the adapter's responsibility.
9. Follow existing operator wiring: retain the input delta and supply read handles for accumulated state
   before and after appending it. There is no old/new or base/pending bit in stored records.
10. Snapshot handles own references to immutable batches. They introduce no transaction timestamp,
    historical version catalog, expiration policy, or batch-role override. Normal LSM compaction continues.
11. K alone identifies a record and is unique in the accumulated source state both before and after
    applying the supplied delta. Signed delta contributions may share K.
12. Maintained view definitions contain no selection predicates. Q3's segment/date filters, aggregation,
    ordering, and limit belong to the query consuming the unfiltered joined view.

## Flat record representation

### K: the paper's folded key

Use this explicit prototype encoding. These numeric assignments are local choices, not requirements of the
paper and not a claim of binary compatibility with LeanStore:

```text
domain tags: INDEX = 0x00, CUSTOMER = 0x01, ORDERS = 0x02, LINEITEM = 0x03
index identifiers: customer = 0x01, orders = 0x02, lineitem = 0x03

fold_i32(x) = big_endian_u32((x as u32) XOR 0x80000000)

C(c):     CUSTOMER | fold_i32(c) | INDEX | customer
O(c,o):   CUSTOMER | fold_i32(c) | ORDERS | fold_i32(o) | INDEX | orders
L(c,o,l): CUSTOMER | fold_i32(c) | ORDERS | fold_i32(o)
          | LINEITEM | fold_i32(l) | INDEX | lineitem
```

Keys occupy 7, 12, and 17 bytes respectively. `INDEX` introduces a one-byte identifier whose domain and
width complete the key grammar. Its value sorts below child-domain tags, placing a Customer before its
Orders and an Order before its Lineitems. Zero is not an extra terminator or a requirement for all encodings.
Neither payload bytes, weight, nor an occurrence counter may be appended to K.

Provide checked `fold`/`unfold` functions and prefix constructors. The storage index compares K as opaque
lexicographic bytes. Reject malformed lengths, domain sequences, and index identifiers during decoding.

```text
scan_prefix(snapshot, prefix):
    cursor = snapshot.raw_cursor()
    cursor.seek_ge(prefix)             # first matching row, including repeated equal keys
    while cursor.valid() and cursor.key().starts_with(prefix):
        yield cursor.key(), cursor.value()
        cursor.next()                 # next flat record, including another record with equal K
```

Customer prefixes end after `c`; order prefixes end after `o`; a line-only prefix additionally includes the
Lineitem domain tag. Exact lookup seeks the complete K and consumes all equal-K records. Range termination
does not require manufacturing a next existing key.

### V: one payload and one weight

```text
FlatValue {
    payload: PayloadBytes,
    weight: ZWeight,                   # Feldera's signed i64 weight
}

Customer payload = { segment: String }
Orders payload   = { order_day: i32, ship_priority: i32 }
Lineitem payload = { ship_day: i32, extended_price_cents: i64, discount_hundredths: i64 }
```

Dates use integer days since the Unix epoch; price and discount use exact scale-two integers.
The payload fields above specify logical types. Use Feldera's existing serializer for V; only K needs an
order-preserving byte encoding. V has no query-visible comparison order or application-defined endianness
requirement. The internal ordering required by Feldera's delta-batch merger is specified below.
The index identifier in K selects the payload schema. `FlatValue` adds no format or kind field.

The index stores the supplied Customer, Orders, and extended Lineitem records. The maintained view is
an unfiltered join of those relations, retaining the keys, segment, order date/priority, ship date, price,
and discount. Q3's predicates are applied by the consuming query, followed by revenue aggregation and
ordering/limit. Join equality conditions still define how the relations connect. See the revised
[Q3 view definition](README.md#how-q3-works-with-reconstructed-state).

The attached manuscript's filtered aggregate-first circuit is a historical reference, not this revised
view definition. For example, a Customer outside BUILDING and its joined line rows belong to the maintained
view; the consuming Q3 query excludes them. Changing that customer's segment updates the joined rows.

Each record associates K with one V containing the data and weight. K alone is the record identity;
a payload replacement updates that same identity. Source-state traversal orders records by K alone. V does not
extend K or participate in lookup by K.
Physical field placement may use multiple columns. The logical flat KV contract does not require packing
all fields into one physical column.

Feldera uses the word **column** for a level of its layer-file hierarchy: a row in one level can own a
group of rows in the next. These are not ordinary independent field arrays; see the
[file-format definition](../../../crates/dbsp/src/storage/file.rs#L3). Its name `auxiliary data` denotes
the data slot attached to a level's search key. The layout below splits V between the second-level
payload and its weight data slot.
The earlier wording incorrectly made that API terminology sound like a property of the merged-index data.
A physical layout must preserve the K-only identity and lookup contract, regardless of how it stores V's fields.

### Unique records and signed replacement deltas

In the source state before and after applying a delta, `state[K]` is absent or contains one payload and
its nonzero weight. A payload change updates the same identity. Here `P100` and `P120` differ in price:

```text
Existing batch B:
  K -> { payload=P100, weight=+1 }

Supplied delta batch D:
  K -> { payload=P100, weight=-1 }
  K -> { payload=P120, weight=+1 }

before snapshot references B:       K -> P100, weight +1
snapshot after append references B,D: K -> P120, weight +1
```

K-only identity does not allow summing weights for K while discarding its data: the delta's weights sum
to zero, but its payload changes. A retraction cancels the matching payload contribution. Payload equality
serves signed-change algebra; it does not introduce another record identity.

```text
read_source_state(snapshot, K):
    rows = all contributions for K in snapshot.batches
    totals = sum weights separately for each equal payload
    discard payloads whose total is zero
    require at most one remaining payload    # source primary-key assumption, checked in tests
    return absent or (K, remaining_payload, its_weight)
```

A partially merged batch need not contain the complete source state. Test key uniqueness only after
combining all contributions in the requested snapshot. The delta is a separate signed input collection;
reading it alone does not return accumulated state.

## Accumulated state, snapshots, and maintenance

### Compatibility with Feldera operators

Use the existing input-delta/current-state/delayed-state distinction. No storage bit or transaction-ID
field is needed. [Feldera's root time is `()`](../../../crates/dbsp/src/time.rs#L214); general nested
circuits have their own timestamp semantics, outside this root-circuit storage adapter.

The [manuscript's notation](../../../../DBSP_w_merged_index/dbsp-merged-index-feasibility.tex#L213) is:
`R_minus` (R⁻) means the accumulated relation before changes; `R_plus` (R⁺) means the accumulated relation
after changes; `deltaR = R_plus - R_minus`. These are not the negative and positive parts of the delta.
An unchanged record is present in both accumulated states. Their stored contributions need not be disjoint.

| Operator input | Read handle |
| --- | --- |
| Input delta `deltaR` | Retained delta batches |
| Before state `R_minus` | Snapshot taken before appending the delta |
| After state `R_plus` | Snapshot taken after appending the delta; includes prior state plus delta |

The [upsert implementation](../../../crates/dbsp/src/operator/dynamic/input_upsert.rs#L628) emits signed
replacements. [Trace integration](../../../crates/dbsp/src/operator/dynamic/trace.rs#L591) connects
`Z1Trace` feedback to [append](../../../crates/dbsp/src/operator/dynamic/trace.rs#L820).
[Delayed access](../../../crates/dbsp/src/operator/dynamic/trace.rs#L745) returns a snapshot of delayed
accumulated state. The adapter supplies identical tuples and weights at those state-access sites.

For example, the [root join](../../../crates/dbsp/src/operator/dynamic/join.rs#L499) computes:

```text
R_plus = R_minus + deltaR
deltaJoin = join(L_minus, deltaR) + join(deltaL, R_plus)
```

The simultaneous-change term occurs once. A scan of the live index alone cannot recover `R_minus` after
compaction has cancelled its contributions against the delta. Retain the before snapshot before append;
it preserves the needed immutable batches. Do not infer before/after membership from row values or weights.

The [accumulating join](../../../crates/dbsp/src/operator/dynamic/join.rs#L698) uses corresponding current
and delayed state. Its [delayed-state operator](../../../crates/dbsp/src/operator/dynamic/accumulate_trace.rs#L1375)
caches a handle across runtime steps until a flush-triggered evaluation. Step 1 must bind actual operator
read sites; Step 3 drives read-handle lifetime and input completion through existing accumulation/flush
wiring. Chunk arrival, file creation, and a single runtime step do not by themselves complete maintenance.

### What trace and batch mean, and where their data lives

**Trace** is Feldera's existing name for an indexed collection of accumulated weighted updates; see its
[module documentation](../../../crates/dbsp/src/trace.rs#L1). Differential Dataflow uses the same
[collection-trace terminology](https://docs.rs/differential-dataflow/latest/differential_dataflow/trace/index.html).
Here, use "accumulated state" in explanations and `Trace`/`Spine` when naming the actual Rust interfaces.
The name does not mean a transaction log or a requirement to retain transaction timepoints.

A **delta** is the signed input change set. A **batch** is a storage object holding weighted contributions.
A delta may span several batches. A compaction output can combine contributions from earlier state and
newly appended changes. Delta, batch, file, and maintained view are different objects.

| Stage | Membership in the merged index | Possible storage |
| --- | --- | --- |
| Input buffered or sorted into staging chunks | Not yet appended | Bounded memory buffers and spill files |
| Append the sealed delta batches | Immediately part of the same index, before view maintenance | In-memory batches or layer-file readers |
| Maintenance reads and background compaction | Already indexed; no need to wait for compaction | Memory and disk; ordinary compaction may combine prior state and delta |
| Maintenance completion | Delta already indexed; no second append | Release computation-owned handles; ordinary compaction continues |

Current Feldera's [accumulator](../../../crates/dbsp/src/operator/dynamic/accumulator.rs#L295) inserts input
batches into a spine. [Fallback builders](../../../crates/dbsp/src/trace/ord/fallback/val_batch.rs#L445) choose
memory or storage for merge output. This demonstrates available storage paths, not a guarantee that every
input constructor automatically spills. The new adapter must explicitly bound staging buffers and select
file-backed output when spilling. A completed staging file has an on-disk key index, but is not part of the
merged index until its batch is appended. Conversely, an appended memory batch is already indexed and
visible without an SSTable being written.

### Snapshot ownership does not copy the database

The exact existing call path is
[`Spine::ro_snapshot`](../../../crates/dbsp/src/trace/spine_async.rs#L2524),
[`From<&Spine>`](../../../crates/dbsp/src/trace/spine_async/snapshot.rs#L170), and
[`get_batches`](../../../crates/dbsp/src/trace/spine_async.rs#L467).
The result is a [`SpineSnapshot`](../../../crates/dbsp/src/trace/spine_async/snapshot.rs#L56) containing
`Vec<Arc<B>>` and factories. It allocates a vector and clones references, not the batches behind them.
It does not deep-copy tuples, memory batches, or layer files; write files; insert updates; register a
historical version; schedule compaction; or assign expiration times. Metadata work is proportional to the
number of batches, not the number of records. Constructing/using its cursor is a separate operation.

Holding this value keeps the referenced Rust objects alive. Dropping it releases references; it does not
purge base records. Existing backend resource cleanup is separate: the
[file-reader contract](../../../crates/storage/src/lib.rs#L404) and
[POSIX destructor](../../../crates/dbsp/src/storage/backend/posixio_impl.rs#L615) can delete an uncheckpointed
file when its final owner disappears. There is no status bit to inspect and no expiration date.
This plan adds neither multi-versioning nor a retention/deletion policy. Use temporary read handles for
the current computation and release them normally when their callers finish.

### References, merging, and disk reads

A snapshot reference **does not prevent compaction**. The merger can read referenced input batches and
publish replacement batches into the live spine. The snapshot continues to read its original immutable
batches. References preserve those objects' lifetime; they do not lock the merger.

A reference to a file-backed batch holds its reader and metadata, not all its records. The
[file reader](../../../crates/dbsp/src/storage/file/reader.rs#L605) looks up blocks in the cache and reads
missing blocks as the cursor traverses them. Taking a snapshot does not load every referenced file into
RAM. Existing memory batches remain in memory while referenced. Cursor metadata, buffers, and cached
blocks consume memory and must be budgeted; do not claim zero memory overhead.

The same snapshot behavior works when the live spine merges earlier state with newly appended changes.
For example, after compaction replaces B and D with M, a before snapshot can still reference B, while the
live spine reads M. A previously captured after snapshot can still read B and D. All after-state reads
produce the same consolidated relation. This uses normal immutable-file replacement and reference lifetime.

The input delta is separate when appended, but compaction outputs need not remain exclusively base or
pending. No such category is stored on either rows or batches:

```text
before handle -> B             # R_minus
input handle  -> D             # deltaR
after handle  -> B + D         # R_plus

ordinary compaction publishes M = consolidate(B + D):
live index    -> M
before handle -> B             # unchanged, still R_minus
input handle  -> D             # unchanged, still deltaR
after handle  -> B + D         # unchanged, still R_plus
```

Here B may stand for several batches. The essential requirement is retaining the appropriate handles
until consumers finish, not forbidding compaction from combining prior state with newly appended changes.

### Append before view maintenance

Index membership and incorporation into a maintained view are separate facts. A delta can already be in
the source index while operators are still using it to compute the view delta. The operator graph retains
the delta input and the appropriate before/after handles; no per-row pending marker is needed.

```text
before = merged_index.snapshot()
delta = prepare_complete_folded_delta()           # bounded staging; may be file-backed

update source storage and its indexes:
    merged_index.append(delta.clone_handle())     # same immutable data, exactly one insertion

after = merged_index.snapshot()
view_delta = compute_view_delta(delta, before, after)
incorporate(view_delta, maintained_view)
wait for all maintenance consumers to finish
release(delta, before, after)                     # release handles, not live index data
complete maintenance                             # no second append, no bit clearing
```

The clone in this proposed API shares batch data/file readers; it must not copy all tuples. The index
and the delta-stream consumer can own references to the same immutable batches. Retaining the separate
delta input ensures it remains available even if the live index compacts it with prior state.

For the prototype, serialize append-and-snapshot publication for one maintenance input so `after` contains
exactly its intended changes, then run all consumers against those fixed handles. Multiple consumers may
finish at different times; release computation-owned references only after their last use. Do not require
all views to finish before inserting the delta into the source index. Existing runtime scheduling controls
admission of further inputs and completion; there is no new transaction timestamp or batch-ID scheme.

Normal spine insertion, backpressure, and background compaction remain enabled. There is no status-aware
merge, maintenance-long pause, reset compaction, inventory rebinding, or changed file-retention policy.
If a runtime transaction fails, source-index and output rollback/recovery must follow the existing runtime
transaction protocol. A snapshot is a read handle, not by itself an atomic rollback implementation; binding
that protocol remains Step 3. Storage-only tests exercise append/read behavior without claiming recovery.

## Necessary changes to Feldera storage

### Reuse existing file columns for signed contributions

Use Feldera's existing [two-column writer](../../../crates/dbsp/src/storage/file/writer.rs#L1564).
Here a column means a hierarchy level as described above, not one SQL field. Keep the folded K in the
first level and store each signed payload contribution as a separate second-level row:

```text
column 0: search key = FoldedKey; data = ()
column 1: search key = PayloadBytes; data = weight: ZWeight

# The supplied replacement delta has two contribution rows under one K:
column 0: K -> child rows [a, b)
column 1: P100 -> -1
          P120 -> +1
```

Every second-level row contains exactly one payload and weight. Its associated parent supplies K.
The raw cursor flattens this physical grouping into `(K, V)` contributions. No V contains a packed list.
The accumulated source state, before or after applying the delta, has at most one payload for K. Multiple
physical columns are an implementation choice that reuses the file format; the logical index has one folded search key.

Both writer levels already require unique keys within each group. Write payload rows with `write1`, then
write their parent K with `write0`. Consolidate contributions with equal K and payload
before writing. The index identifier in K selects the payload decoder;
no extra kind field is needed.
Do not add a repeated-key writer mode or change file seek algorithms. Reuse existing outer-key
seek/next and child-group cursors; prefix termination is checked against the folded parent K.

There is one backend requirement beyond ordering by K: the existing
[batch cursor contract](../../../crates/dbsp/src/trace/cursor.rs#L42) orders values within a key, and
[the spine merger](../../../crates/dbsp/src/trace/spine_async/list_merger.rs#L197) compares those values.
Use `PayloadBytes`' ordinary comparison for this internal contribution grouping. No SQL field order,
numeric order, or big-endian payload encoding is required. Encode equal payloads identically with the
existing serializer, so its byte equality agrees with payload equality. Weight is excluded from the
comparison. This internal order is needed only to reuse Feldera's batch/merge contract; queries cannot request a range
or ordering by it. Eliminating it entirely would require replacing that contract.

### Batch and raw-cursor contracts

Implement `FlatKvBatch` under `crates/dbsp/src/trace/ord/flat_kv/`. Put prototype Q3 codecs and the storage
owner under `crates/dbsp/src/trace/merged_index/`. This step exposes crate-internal APIs only.

```text
raw batch row:       (FoldedKey, FlatValue { one payload, weight })
BatchReader::Key:   dynamic FoldedKey
BatchReader::Val:   dynamic PayloadBytes
BatchReader::R:     DynZWeight
BatchReader::Time:  ()
```

The [Batch contract](../../../crates/dbsp/src/trace.rs#L846) supplies the internal cursor over folded K
and payload. The raw KV cursor flattens parent and child rows into K and one V. Generic consolidation sums
weights for matching `(K, payload)`, including cancellation between prior state and newly appended changes.
Before-state readers remain valid through their immutable snapshot references; no contribution needs to
carry a status bit to protect it from compaction.

All builders, staging merges, spills, persistence, and conversion paths preserve payload and signed weight.
Reuse ordinary `BatchReader`/`Batch` value semantics. The later reconstructed-state adapter returns the
query operator's expected tuples and weights; this storage step does not implement that typed interface.

Implement memory and file variants; ordered builders; chunked `MergeBatcher` staging; cursor navigation;
metadata counts/bounds; `persisted`/`from_path`; and storage-destination merging for spilled staging chunks.
Count outer K rows for `key_count()` and signed contribution rows for `len()`; they can differ in delta
batches. Builders write one payload/weight per contribution row.
Reuse `FallbackValBatch` for the required `Timed<T>`
associated type; the prototype itself uses unit time. Use existing dynamic-data/factory conventions and
implement byte-wrapper ordering as lexicographic bytes. Report no roaring compatibility for these keys.

Expose `fold`, `unfold`, `append`, `snapshot`, and raw `seek_ge`/`next`.
The cursor returns borrowed key/payload bytes and signed weight; advancing invalidates its borrowed row.
A raw cursor can combine immutable batches without materializing the complete accumulated relation.
The Q3 codec's decoded enum is a utility for validation, not a typed query-accessor/session API.

## Verification and implementation sequence

Use Rust tests and an independent `BTreeMap<Key, (Payload, Weight)>` source-state oracle. Represent input
changes separately as signed contributions. Apply the complete change set per K before checking uniqueness;
its ordering must not cause a false violation. Test supplied related-row changes exactly as given.

1. **Key and payload codecs:** golden key bytes, fold/unfold, malformed keys, serializer round trips,
   prefix scans, no extra END/LOOKUP tags, kind fields, or status bits, and one payload per contribution row.
2. **File layout and cursors:** many outer keys across blocks; same-K signed contributions; forward,
   reverse, and exact seeks; flattened cursor agreement; ordinary payload/weight consolidation.
3. **Weighted state:** replacements, cancellation, signed multiplicity, unique K in before/after source
   states, zero net weight with a changed payload, repeated updates to one K, persistence, and reopen.
4. **References and I/O:** snapshots perform no file-block reads or writes. Seeking a small range reads
   needed blocks. Force compaction of prior batches plus delta while before, after, and delta handles
   remain referenced; all three must retain their distinct intended results. Use files larger than the
   cache and measure resident memory; compare shared file/data identity to rule out tuple copies.
5. **Append timing:** the delta is visible in the live source index before view maintenance begins; the
   before snapshot excludes it and the after snapshot includes it. Completion performs no second append,
   bit clearing, or special compaction. Consecutive inputs obtain the correct separate read handles.
6. **Runtime integration gate:** compare reconstructed before/after tuples with actual delayed/current
   state at each bound read site. Include simultaneous join changes, multiple input chunks and runtime
   steps, and consumers finishing at different times. Compare the maintained unfiltered join and separately
   filtered/aggregated Q3 result against independent evaluation. This gate belongs to integration;
   storage tests alone do not prove operator compatibility or transaction recovery.
7. **Scope:** no status bits, transaction-ID labels, batch-role reinterpretation, storage-generated related
   changes, selection predicates in maintained views, typed scan sessions, or reconstructed operators in Step 2.

Implement in three milestones: codecs and file layout; memory/file batches and ordinary weighted merging;
append/snapshot/I/O tests. Each includes focused tests and rustdocs. Run formatting, crate checks, and
affected storage/spine tests. Step 3 integrates typed reconstruction and existing runtime transaction
recovery. Preserve normal immutable-file compaction and write amplification behavior. Measure range-read
I/O and total resident memory before making performance claims.
