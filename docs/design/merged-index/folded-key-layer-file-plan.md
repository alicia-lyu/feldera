# Step 2: Folded-key weighted records on Feldera's LSM

Date: 2026-09-28. Status: implementation plan; no runtime implementation is claimed.

## Contract and references

Implement the storage substrate for one Customer–Orders–Lineitem merged index. The user's clarifications
in this conversation govern this plan. The attached manuscripts are design and algebraic references;
their historical implementation proposals are not additional instructions.

| Reference | Use in this plan |
| --- | --- |
| [Interesting-orderings record structure](../../../../merged_index_interesting_orderings/main.tex#L362) | Domain-tagged fields, then the index-identifier domain and value; payload stays outside K. |
| [Interesting-orderings folding example](../../../../merged_index_interesting_orderings/main.tex#L545) | Customer–Orders–Lineitem folded-key shape. |
| [DBSP source schema](../../../../DBSP_w_merged_index/dbsp-merged-index-feasibility.tex#L120) | Extended Lineitem records carry customer, order, and line identity. |
| [DBSP signed changes](../../../../DBSP_w_merged_index/dbsp-merged-index-feasibility.tex#L205) | Retractions use negative weights and match complete tuples. |
| [DBSP before/after algebra](../../../../DBSP_w_merged_index/dbsp-merged-index-feasibility.tex#L508) | State before changes and state after applying the delta. |
| [Operator-state guide](../../../../DBSP_w_merged_index/operator-state.tex#L39) | Numeric summaries, output tuples, and affected-key support have distinct roles. |
| [mi_db scan sessions](../../../../mi_db/docs/architecture.md#merged-index-scan-sessions) | Later typed-accessor/session layer, outside Step 2. |
| [Multi-pipeline manuscript](../../../../query_execution_using_MI/main.tex#L88) | Future pipeline composition; this step covers one merged index. |

The transaction supplies all intended related-row changes and both keys of a moved row. Storage does not
move descendants, enforce relational consistency, or add an `OrderParent` lookup record. Step 2 provides
folding/unfolding, weighted batches, snapshots, and raw cursors. Typed reconstruction, shared scan sessions,
view-operator wiring, and runtime transaction recovery follow in Step 3.

> [!NOTE]
> **Reading guide:** The main text states the intended behavior. “Implementer suggestion”
> callouts identify possible Feldera API choices and can be skipped on a first read. Each
> suggestion is provisional: it has not been comprehensively audited or reviewed by a
> human expert.

## What trace and batch mean, and where their data lives

**Trace** is Feldera's name for an indexed collection of accumulated weighted updates; see its
[module documentation](../../../crates/dbsp/src/trace.rs#L1). Differential Dataflow uses the same
[term](https://docs.rs/differential-dataflow/latest/differential_dataflow/trace/index.html).
Here, “accumulated state” describes the contents; `Trace` and `Spine` name Rust interfaces. Trace does
not imply a transaction log, time-travel API, or stored transaction timepoints.

A **delta** is the signed input change set. A **batch** is an immutable storage object containing many
weighted records and keys; one delta may span several batches. A batch can reside in memory or in a
**layer file**. Feldera calls each nesting level of that file a **column**; it is not a SQL attribute
or a separate field array. A file-backed batch keeps a reader, not all file rows, in memory.

| Stage | Merged-index membership | Storage and result |
| --- | --- | --- |
| Buffer/sort input | Not appended | Bounded memory and, when needed, spill files |
| Append sealed delta batches | Immediately indexed, before view maintenance | Memory batches or indexed layer files |
| Maintain view | Delta remains indexed; readers use captured handles | Background compaction may already merge batches |
| Finish maintenance | No second append | Release handles after their last consumer finishes |
| Compaction covering prior state and delta | Same accumulated relation | New immutable batch/file replaces inputs; matching signed rows cancel |

Compaction is not a maintenance barrier. It may happen earlier or later than the final table row.
A merge that sees only some contributions for K can still output several payload rows; when it sees
all contributions for a valid K, its output has one surviving row. A staging file has an on-disk
index but is not part of the merged index until appended. An appended memory batch is indexed without
first becoming a file. Staging must have a memory bound and spill when needed.

> [!NOTE]
> **Implementer suggestion — provisional.** The details in this block have not been
> comprehensively audited or reviewed by a human expert.
>
> [Feldera's accumulator](../../../crates/dbsp/src/operator/dynamic/accumulator.rs#L295)
> inserts batches into a spine; [fallback builders](../../../crates/dbsp/src/trace/ord/fallback/val_batch.rs#L445)
> can choose memory or storage. The new adapter must implement the bounded staging choice explicitly.

## Merged-index constitution

The constitution defines **which sources belong to this merged index**, **which ordered fields
form each source's key**, and **which fields across sources share a domain**. A domain name
denotes the same logical identity across those fields and assigns them one byte tag. The
folding rule belongs to the merged index and source together; another merged index may use a
different key path for the same source. Use this small notation for the constitution. Phase 1
may represent it as a static Rust declaration rather than implementing a parser or macro:

```text
merged_index CustomerOrdersLineitem {
    domain customer = 0x01
    domain order    = 0x02
    domain line     = 0x03

    source Customer         id 0x01 key (customer_id: customer)
    source Orders           id 0x02 key (customer_id: customer, order_id: order)
    source ExtendedLineitem id 0x03 key (customer_id: customer, order_id: order,
                                          line_id: line)
}
```

The three `source` entries are the source roster. Within each entry, the ordered `key` fields
name the source fields used for this index. The `customer` domain groups `customer_id` from all
three sources under tag `0x01`; the `order` domain groups `order_id` from Orders and extended
Lineitem under tag `0x02`. This definition states relationships among source fields; the
record encoding below puts their tags into K. The `id` distinguishes the source in the final
key bytes and is separate from the domain tags. Domain sharing is explicit: fields can share a
domain despite different names, and matching names alone do not establish one.
Field names here describe logical fields in the supplied extended records; typed source adapters
bind their actual Rust fields. In Rust, use a generic `MergedIndex<D>` base whose shared methods
fold and unfold keys, construct prefixes, and later own the common batch/file machinery. The
`MergedIndexDefinition` parameter `D` supplies the source roster, ordered key fields, domain
tags, typed source-key projection, and payload schemas. `CustomerOrdersLineitemIndex` is a
concrete wrapper around `MergedIndex<CustomerOrdersLineitemDefinition>`; its definition supplies
the constitution above. The generic base must take all source-specific choices from `D`, so
another merged index can reuse it with a different definition and fold the same source
differently. This is composition rather than class inheritance in Rust.

## Record representation

### K: the paper's folded key

Use this prototype encoding. Numeric tags are local choices, not requirements of the paper or a claim
of LeanStore binary compatibility:

```text
domain tags: INDEX = 0x00, CUSTOMER = 0x01, ORDERS = 0x02, LINEITEM = 0x03
index identifiers: customer = 0x01, orders = 0x02, lineitem = 0x03

fold_i32(x) = big_endian_u32((x as u32) XOR 0x80000000)

C(c):     CUSTOMER | fold_i32(c) | INDEX | customer
O(c,o):   CUSTOMER | fold_i32(c) | ORDERS | fold_i32(o) | INDEX | orders
L(c,o,l): CUSTOMER | fold_i32(c) | ORDERS | fold_i32(o)
          | LINEITEM | fold_i32(l) | INDEX | lineitem
```

K occupies 7, 12, or 17 bytes. `INDEX` introduces the identifier; it is not an additional `END`
marker. Its value sorts below child-domain tags, placing a Customer before its Orders and an Order
before its Lineitems. Payload, weight, and occurrence counters never extend K. Reject malformed
lengths, domain sequences, and identifiers in checked `fold`/`unfold` functions. The storage index
compares K as opaque lexicographic bytes.

```text
scan_prefix(snapshot, prefix):
    cursor = snapshot.raw_cursor()
    cursor.seek_ge(prefix)
    while cursor.valid() and cursor.key().starts_with(prefix):
        yield cursor.key(), cursor.value()
        cursor.next()  # can return another contribution with equal K
```

A Customer prefix ends after `c`, an Order prefix after `o`, and a line-only prefix also includes
the Lineitem domain tag. Exact lookup consumes all equal-K contributions. The iterator's `next`
advances to the next pair; scans stop when the prefix changes, without a `successor` API.

### Payload, signed weight, and replacement

```text
MergedIndexValue {
    payload: PayloadBytes,
    weight: ZWeight,  # Feldera's signed i64 weight
}

Customer payload = { segment: String }
Orders payload   = { order_day: i32, ship_priority: i32 }
Lineitem payload = { ship_day: i32, extended_price_cents: i64, discount_hundredths: i64 }
```

Dates are days since the Unix epoch; price and discount use exact scale-two integers. The index
identifier selects the payload schema. Feldera's serializer encodes V; only K needs an
order-preserving byte encoding. The file merger orders payloads for internal equality and merging,
not to provide a query-visible payload order. V has no format field, kind field, or packed
payload list.

At a completed transaction boundary, accumulated source state has at most one active payload
per K. A signed delta may have several rows for K. Feldera's
[upsert input](../../../crates/dbsp/src/operator/dynamic/input_upsert.rs#L628)
already emits a negative weight for the previous row and a positive weight for its replacement:

```text
Before append:       B contains (K, P100, +1)
Signed input delta:  D contains (K, P100, -1), (K, P120, +1)
After append:        B + D has (K, P120, +1) after consolidation
Full compaction:     M contains (K, P120, +1), replacing B and D in the live index
```

The old and new payloads share folded K. Cancellation matches complete `(K, payload)` contributions;
summing only by K would erase the replacement's zero net key weight. A physical batch may contain
several payload rows for K even after its delta has been incorporated into the view. A fully
consolidated batch formed from valid accumulated state has one surviving payload row per present K.

```text
read_source_state(snapshot, K):
    totals = map from payload to signed weight
    for (payload, weight) in snapshot.contributions_at(K):
        totals[payload] += weight
    for (payload, weight) in totals:
        if weight != 0:
            yield (K, payload, weight)  # zero, one, or several rows before validation
```

The raw cursor returns stored contributions; the state reader sums them and returns every
nonzero result. If a transaction inserts `(K, P120, +1)` without retracting an existing
`(K, P100, +1)`, the post-append read returns both; it cannot silently
choose one. The transaction must reject that result or rely on an existing source key constraint
that rejects it. The delta is also retained as a separate operator input; reading it alone is
not a read of accumulated state.

### Validate K uniqueness before incorporating state

[`Writer2`](../../../crates/dbsp/src/storage/file/writer.rs#L1564) checks K uniqueness within one
batch and payload uniqueness within each K group; it cannot establish the accumulated-state
invariant across LSM batches. Step 2 exposes the raw cursor and tests this scan; Step 3 binds it
to source/view commit:

```text
changed_K = distinct folded keys in the signed delta
for K in changed_K:
    active = read_source_state(after_snapshot, K)
    require count(active) <= 1
```

Take distinct K before projecting or summing the delta: its key-only net weight can be zero for
a replacement. Unchanged K remain valid by induction. A failed check aborts the runtime
transaction; storage does not repair it or choose a last writer. The scan costs one LSM seek per
changed K plus traversal of its contributions. Step 3 may omit it only after proving that
existing source key constraints enforce this invariant for all three indexed relations.

### Maintained view and consuming query

The index stores supplied Customer, Orders, and extended Lineitem records. The maintained Q3 view
is their unfiltered join, retaining keys, segment, order date/priority, ship date, price, and
discount. The consuming Q3 query applies segment/date filters, revenue aggregation, ordering,
and limit; see the [view definition](README.md#how-q3-works-with-reconstructed-state). Join equality
still connects the relations. Thus a Customer outside BUILDING and its joined line rows remain in
the maintained view; a later segment change updates those joined rows. The manuscript's filtered,
aggregate-first circuit is a historical comparison, not this view definition.

## Snapshots and maintenance

### Compatibility with Feldera operators

The [DBSP notation](../../../../DBSP_w_merged_index/dbsp-merged-index-feasibility.tex#L213) is:
`R_minus` (R⁻) is accumulated state before the delta, `R_plus` (R⁺) is accumulated state after
it, and `deltaR = R_plus - R_minus`. These are not the negative and positive parts of the
signed delta; unchanged records appear in both states.

| Operator input | Read handle |
| --- | --- |
| `deltaR` | Retained input delta batches |
| `R_minus` | Snapshot taken before appending the delta |
| `R_plus` | Snapshot taken after appending it; includes prior state plus delta |

> [!NOTE]
> **Implementer suggestion — provisional.** The details in this block have not been
> comprehensively audited or reviewed by a human expert.
>
> [Feldera's root time is `()`](../../../crates/dbsp/src/time.rs#L214).
> Other circuit timestamp semantics are outside this root-circuit adapter.

For example, the [root join](../../../crates/dbsp/src/operator/dynamic/join.rs#L499) computes:

```text
R_plus = R_minus + deltaR
deltaJoin = join(L_minus, deltaR) + join(deltaL, R_plus)
```

The simultaneous-change term occurs once. A live-index scan cannot recover `R_minus` after
compaction has cancelled contributions against the delta; capture its snapshot before append.
Some operators use an earlier snapshot across several execution steps. Feldera's
[delayed snapshot](../../../crates/dbsp/src/operator/dynamic/trace.rs#L745) and
[snapshot keeper](../../../crates/dbsp/src/operator/dynamic/accumulate_trace.rs#L1375) are examples.
Step 3 connects handle lifetime to the last operator use; an input chunk or file creation does
not establish maintenance completion.

### Snapshot ownership does not copy the database

A snapshot copies references to immutable batches, not their tuples or layer files. Creating it
does not read file blocks, write files, or schedule compaction; its metadata work scales with
batch count. A file-backed reference holds a reader and metadata. Cursors load requested blocks
through the cache as they advance. Existing memory batches remain resident while referenced;
readers, buffers, and cached blocks still consume memory.

A snapshot keeps its original immutable batches readable while the live spine compacts them.
For example, a before snapshot can keep B, an after snapshot can keep B and D, and the live
index can replace B and D with M. The snapshot does not lock compaction. Dropping a handle
releases references; it does not purge records or use an expiration date. This plan adds no
historical-version catalog or file-retention policy.

> [!NOTE]
> **Implementer suggestion — provisional.** The details in this block have not been
> comprehensively audited or reviewed by a human expert.
>
> [`Spine::ro_snapshot`](../../../crates/dbsp/src/trace/spine_async.rs#L2524)
> constructs a [`SpineSnapshot`](../../../crates/dbsp/src/trace/spine_async/snapshot.rs#L56)
> from the [batch inventory](../../../crates/dbsp/src/trace/spine_async.rs#L467) by cloning
> `Arc<B>` references ([conversion](../../../crates/dbsp/src/trace/spine_async/snapshot.rs#L170)).
> The [file reader](../../../crates/dbsp/src/storage/file/reader.rs#L605) loads blocks on demand.
> Backend cleanup can delete an uncheckpointed file after its final owner disappears; see the
> [file-reader contract](../../../crates/storage/src/lib.rs#L404) and
> [POSIX destructor](../../../crates/dbsp/src/storage/backend/posixio_impl.rs#L615).

### Append before view maintenance

```text
before = merged_index.snapshot()
delta = prepare_complete_folded_delta()           # bounded staging; may be file-backed

update source storage and its indexes:
    merged_index.append(delta.clone_handle())     # shares data; one insertion

after = merged_index.snapshot()
view_delta = compute_view_delta(delta, before, after)
incorporate(view_delta, maintained_view)
wait for all consumers of before, after, and delta
release(delta, before, after)
complete maintenance                             # no second append
```

`clone_handle` shares immutable batch data/file readers with the delta-stream consumer; it does
not copy tuples. Serialize append-and-snapshot publication for one prototype maintenance input
so `after` includes exactly its intended changes. The source index can contain a delta that is
not yet incorporated into the maintained view; the operator graph retains `delta`, `before`,
and `after` separately, without status bits or batch-role labels. Normal spine insertion,
backpressure, and compaction continue.

Multiple consumers may finish at different times. Step 3 binds completion, further-input
admission, and failure rollback to the existing runtime transaction protocol. A snapshot is a
read handle, not atomic rollback. Storage-only tests do not claim crash recovery.

## Feldera batch implementation

### Layer-file columns and weighted rows

Use the existing [two-column writer](../../../crates/dbsp/src/storage/file/writer.rs#L1564)
for every batch. It stores folded K at level 0 and one payload/weight contribution per child
row at level 1:

```text
column 0: search key = FoldedKey; data = ()
column 1: search key = PayloadBytes; data = weight: ZWeight

# Replacement delta under one folded K:
column 0: K -> child rows [a, b)
column 1: P100 -> -1
          P120 -> +1
```

K remains the paper's folded key; payload is never appended to it. Consolidate equal
`(K, payload)` contributions before writing a batch. File scans seek K and stop when its
prefix changes. Payload order is internal to batch merging, not a query-level range order.
The second-level row groups and payload index also exist for singleton groups; measure
their file-size, cache, and read costs before making performance claims.

> [!NOTE]
> **Implementer suggestion — provisional.** The details in this block have not been
> comprehensively audited or reviewed by a human expert.
>
> Write child payload rows with `write1`, then their parent K with
> `write0`. The file format requires unique keys within each group, so it needs no repeated-K
> writer mode or seek change. Reuse level-0 seek/next and child-group cursors. The index
> identifier selects the payload decoder. The [batch cursor contract](../../../crates/dbsp/src/trace/cursor.rs#L42)
> and [spine merger](../../../crates/dbsp/src/trace/spine_async/list_merger.rs#L197)
> compare payloads within K. Use deterministic `PayloadBytes` comparison and serialize equal
> payloads identically; weight is excluded from the comparison. No SQL field order or
> big-endian payload encoding is required.

### Batch and raw-cursor contracts

Expose `fold`, `unfold`, `append`, `snapshot`, and a cursor with `seek_ge`/`next`.
For a requested K or range, the cursor advances through the relevant immutable batches in
key order and yields their stored `(K, payload, weight)` contributions. The state reader
then sums contributions with equal `(K, payload)`. This reads the requested rows and file
blocks as needed; it does not load the entire accumulated relation into memory. For the
replacement above, a lookup of K reads the relevant rows from B and D, then the state
reader returns only `(K, P120, +1)`; unrelated keys are not materialized. Typed query access
and shared scan sessions follow in Step 3.

> [!NOTE]
> **Implementer suggestion — provisional.** The details in this block have not been
> comprehensively audited or reviewed by a human expert.
>
> Implement `MergedIndexBatch` under
> `crates/dbsp/src/trace/ord/merged_index/`; put Q3 codecs and the storage owner under
> `crates/dbsp/src/trace/merged_index/`. The [Batch contract](../../../crates/dbsp/src/trace.rs#L846)
> uses `FoldedKey` as `Key`, `PayloadBytes` as `Val`, `DynZWeight` as `R`, and `()` as
> `Time`. Implement memory/file variants, ordered builders, chunked `MergeBatcher` staging,
> cursor navigation, metadata counts/bounds, `persisted`/`from_path`, and storage-destination
> merges for spills. `key_count()` counts K groups; `len()` counts contribution rows. Reuse
> `FallbackValBatch` for required `Timed<T>`; follow dynamic-data/factory conventions, order
> byte wrappers lexicographically, and report no roaring compatibility. The cursor returns
> borrowed row bytes that become invalid when it advances. Keep these APIs crate-internal.

## Verification and implementation sequence

> [!NOTE]
> **Provisional sequence.** These phases are suggestions, not a comprehensively audited or
> human-expert-reviewed implementation program. Each phase should begin with a focused plan
> that checks the relevant Feldera APIs, then implement only its stated boundary and record
> evidence before starting the next phase.

### Phase 1: Encoding and one readable layer-file batch

- **Status:** Implemented and verified on 2026-09-29; see the
  [Phase 1 implementation note](phase-1-implementation-note.md) for the encoding,
  file mapping, and test evidence.
- **Plan:** Confirm the folded-key bytes, payload serialization, and two-column file mapping.
- **Build:** Implement the generic `MergedIndex<D>` folding base, its
  Customer–Orders–Lineitem definition, and a small readable two-column layer-file fixture.
- **Evidence to advance:** Keys and payloads round-trip; a seek finds the intended records.

### Phase 2: Batches and cursors

- **Plan:** Audit the batch interfaces and choose how memory-backed and file-backed batches
  expose the same ordered cursor, including bounded staging.
- **Build:** Add both batch forms and reads over multiple keys and batches.
- **Evidence to advance:** The cursor returns requested signed contributions after spill and
  reopen; file-backed reads fetch needed blocks without materializing the whole index.

### Phase 3: Signed merging and K uniqueness

- **Plan:** Locate the consolidation and uniqueness-check boundaries; confirm whether source
  key constraints can establish the accumulated-state invariant.
- **Build:** Merge signed contributions by `(K, payload)` and provide a changed-K validation
  helper for Step 3.
- **Evidence to advance:** A valid replacement leaves one active payload, an incomplete one is
  detected, and ordinary compaction preserves the same accumulated state. A merge covering all
  contributions for a valid K leaves one active payload. Use an independent signed-weight
  oracle when checking these outcomes.

### Phase 4: Append and snapshots

- **Plan:** Specify ownership and lifetime of the before, delta, and after handles using the
  existing append and snapshot APIs.
- **Build:** Append the delta once and expose the two accumulated-state reads to storage callers.
- **Evidence to advance:** Each handle returns its intended rows while normal compaction merges
  earlier and delta batches; snapshot creation does not copy the relation.

### Step 3 handoff

Plan operator read-site wiring, typed reconstruction, and runtime completion/rollback separately.
Step 2's storage evidence does not establish those integration properties. Each phase should
leave a short implementation note, focused tests, and any API findings for the next planner.
