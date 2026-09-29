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
   regular payload, a signed weight, and one old/new bit. There are no payload lists, per-record `format`
   fields, or redundant `kind` fields.
5. An **immutable batch** contains many records and keys; it can reside in memory or in a **layer file**.
   Examples showing one key are excerpts from batches, not one file per key.
6. Existing state and the incoming delta belong to the same merged index. Append the complete delta before
   computing the view delta. Completion does not append those weights a second time.
7. Step 2 supplies folding/unfolding primitives and raw weighted byte cursors. Typed query returns,
   reconstruction, shared scan sessions, and runtime transaction integration remain subsequent work.
8. The transaction supplies all intended related row changes. Storage neither synthesizes Lineitem moves
   from an Order change nor enforces relational consistency. Input records include their intended old/new
   extended keys. Structural decoding and storage-protocol checks remain the adapter's responsibility.
9. Follow Feldera's existing delta/trace semantics: existing contributions are OLD; both the negative
   old-payload contribution and positive new-payload contribution in the incoming delta are NEW. The
   required physical bit is adapter metadata; existing root Z-sets do not contain this bit.
10. This is not a multi-version database. OLD/NEW describe one transaction's computation. There is no
    historical-version catalog, time-travel API, record/file expiration date, or new deletion policy.
11. K alone identifies a record and is unique in each accumulated endpoint. Signed delta contributions may
    share K. Physical columns and backend delta grouping do not change this identity.

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
Neither payload bytes, weight, phase, nor an occurrence counter may be appended to K.

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

### V: one payload, one weight, one bit

```text
FlatValue {
    payload: PayloadBytes,
    weight: ZWeight,                   # Feldera's signed i64 weight
    is_new: bool,                      # serialized as one byte: 0=OLD, 1=NEW
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

This index stores the supplied base-relation records before Q3's predicates. For example, a Customer whose
segment is `AUTOMOBILE` is a valid stored row but does not pass Q3's `segment = BUILDING` predicate.
Its Orders and Lineitems can still contribute to intermediate states, such as per-order line revenue.
If only the Customer's segment changes to `BUILDING`, reconstruction needs those unchanged Orders and
Lineitems. The index therefore does not discard source records because their current joined result fails
a query predicate. Query filtering belongs to the reconstruction/operator layer. This follows the
[manuscript's source-index scope](../../../../DBSP_w_merged_index/dbsp-merged-index-feasibility.tex#L120).
The earlier phrase "query-ineligible" meant "currently fails a query predicate"; it did not mean invalid
input or a deleted record. A separately designed filtered index would have a different input contract.

Each record associates K with one V containing the data, weight, and bit. K alone is the record identity;
a payload replacement updates that same identity. Endpoint traversal orders records by K alone. V does not
extend K or participate in endpoint lookup.
Physical field placement may use multiple columns. The logical flat KV contract does not require packing
all fields into one physical column.

Feldera uses the word **column** for a level of its layer-file hierarchy: a row in one level can own a
group of rows in the next. These are not ordinary independent field arrays; see the
[file-format definition](../../../crates/dbsp/src/storage/file.rs#L3). Its name `auxiliary data` denotes
the data slot attached to a level's search key. The layout below splits V between the second-level
payload and its weight/bit data slot.
The earlier wording incorrectly made that API terminology sound like a property of the merged-index data.
A physical layout must preserve the K-only identity and lookup contract, regardless of how it stores V's fields.

### Unique endpoint records and signed replacement deltas

For each endpoint, `state[K]` is absent or contains one payload and its nonzero weight. A payload change
updates the same record identity. Here `P100` and `P120` differ in extended price:

```text
OLD accumulated endpoint:
  K -> { payload=P100, weight=+1 }

Incoming delta D1, logical signed contributions:
  K -> { payload=P100, weight=-1, is_new=1 }
  K -> { payload=P120, weight=+1, is_new=1 }

NEW accumulated endpoint:
  K -> { payload=P120, weight=+1 }
```

The two delta contributions describe one record's replacement. They are not two records in either
accumulated endpoint. Immutable batches may retain contributions from several updates until merging;
physical contributions must not be mistaken for the visible endpoint map.

K-only identity does not allow summing all weights for K while discarding the associated data: the delta's
weights sum to zero, but its payload changes. Feldera cancels a retraction against the matching payload
contribution. Payload equality is needed to apply signed changes correctly; it does not create another
record identity. Insertion and retraction of the same payload cancel; different payloads do not.

```text
resolve_endpoint(K, contributing_batches):
    totals = sum signed weights separately for each equal payload at K
    discard payloads with zero total
    require at most one remaining payload       # guaranteed by the input's endpoint uniqueness
    return absent or (K, remaining_payload, its_weight)

OLD = resolve using pre-append contributions
NEW = resolve using pre-append contributions plus all current delta contributions
```

The endpoint-uniqueness assertion belongs in adapter validation tests; it is not FK enforcement or permission
to manufacture related-row changes. A partially merged batch need not itself be a complete endpoint and
must not be subjected to this assertion. Both signs of the current delta are NEW contributions. Reading only
NEW-marked records returns the delta, not the NEW accumulated endpoint.

## Snapshots, append order, and the old/new bit

### Feldera determines old/new semantics

This is a compatibility requirement, not an independently chosen visibility policy. Existing Feldera root
Z-sets carry tuple weights without an old/new boolean. Root trace time is
[`()`](../../../crates/dbsp/src/time.rs#L214); general nested circuits can use non-unit timestamps, which
this bit does not replace. A negative weight means retraction, not membership in the old accumulated state.

| Existing runtime representation | Required merged-index interpretation |
| --- | --- |
| Incoming delta batch, including both signs of an upsert | Effective NEW contributions |
| Delayed accumulated trace at the requesting operator's boundary | OLD endpoint |
| Current accumulated trace including the delta | NEW endpoint: sum OLD and NEW contributions |
| Existing transaction accumulation/flush boundary | Boundary controlling when the incoming contributions become prior state |

The [upsert implementation](../../../crates/dbsp/src/operator/dynamic/input_upsert.rs#L628) emits signed
replacement changes. [Trace integration](../../../crates/dbsp/src/operator/dynamic/trace.rs#L591) combines
`Z1Trace` feedback with append; [append](../../../crates/dbsp/src/operator/dynamic/trace.rs#L820) inserts the
incoming batch. [Delayed access](../../../crates/dbsp/src/operator/dynamic/trace.rs#L745) returns a snapshot
of the delayed trace. These stream/trace connections carry the distinction that the proposed flat format
exposes through batch membership and its effective bit. Existing operators do not inspect that bit.

For example, the [root incremental join](../../../crates/dbsp/src/operator/dynamic/join.rs#L499) computes:

```text
L_new = L_old + deltaL
R_new = R_old + deltaR
deltaJoin = join(L_old, deltaR) + join(deltaL, R_new)
          = join(L_old, deltaR) + join(deltaL, R_old) + join(deltaL, deltaR)
```

The cross term occurs once. Supplying `R_old` to the second term would omit it; supplying `L_new` to the
first would count it twice. The merged-index provider must return the endpoint requested by the unchanged
operator, with identical tuples and weights, not ask that operator to interpret a new flag.

The [accumulating join](../../../crates/dbsp/src/operator/dynamic/join.rs#L698) likewise connects the left
delta to the current right trace and the right delta to `left_trace.accumulate_delay_trace()`.
[`AccumulateDelayTrace`](../../../crates/dbsp/src/operator/dynamic/accumulate_trace.rs#L1375) caches its
snapshot and refreshes it on initialization or a flush-triggered evaluation. Runtime
[transactions](../../../crates/dbsp/src/circuit/schedule.rs#L190) can span multiple steps. Therefore a file
boundary, one incoming chunk, cursor exhaustion, or one runtime step must not independently promote NEW
to OLD. Step 1 must bind each state access to its actual runtime read site; Step 3 must drive the storage
protocol below from those existing accumulation/flush boundaries. Step 2's explicit begin/finish calls are
storage-test scaffolding, not a replacement scheduler or proof of completed operator integration.

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
run an OLD-record purge. Existing backend resource cleanup is separate: the
[file-reader contract](../../../crates/storage/src/lib.rs#L404) and
[POSIX destructor](../../../crates/dbsp/src/storage/backend/posixio_impl.rs#L615) can delete an uncheckpointed
file when its final owner disappears. They do not inspect the old/new bit or use an expiration date.
This plan adds neither multi-versioning nor a retention/deletion policy. Use temporary read handles for
the current computation and release them normally when their callers finish.

### One index, two read views, one insertion of the delta

The transaction supplies a complete batch with all intended Customer, Orders, and extended Lineitem changes.
The following is the integration contract, not a claim that a global base-table layer already does this in Feldera:

```text
begin source transaction t
old = merged_index.snapshot()

apply supplied changes to base-relation storage and its indexes
    merged_index.append(t, complete_folded_delta_with_NEW_bits)   # exactly once, as one index update

new = merged_index.snapshot()                                    # includes this appended batch
compute_view_delta(input_delta, old, new)
wait for all maintenance consumers and output-state updates
complete transaction t                                          # no second weighted insertion
```

Accumulated NEW reads include the current delta. OLD reads use the pre-append batch inventory. The operator
algorithm chooses the view; for example, `deltaL join R_new + L_old join deltaR` counts simultaneous changes
once. The storage adapter does not choose join algebra or enforce consistency between changed relations.

In current Feldera, [Z-set input](../../../crates/dbsp/src/operator/dynamic/input.rs#L297)
assembles buffered changes into delta batches; a retained integral is created through
[trace insertion](../../../crates/dbsp/src/operator/dynamic/trace.rs#L820).
[Map/upsert input](../../../crates/dbsp/src/operator/dynamic/input.rs#L525) retains keyed state
and emits signed replacements, using its [trace feedback](../../../crates/dbsp/src/operator/dynamic/upsert.rs#L74).
There is no universal automatically materialized base index for every Z-set input. Wiring this index update
before its NEW-state consumers belongs to the subsequent circuit integration.

### Reusing the bit over successive batches

The bit has boundary-relative meaning, as defined by the runtime mapping above. A NEW contribution from
completed transaction t is part of existing OLD state for t+1. Do not flip every record on disk and do not
interpret old physical NEW bits as updates in every later batch.

Use immutable storage plus small batch-handle metadata:

```text
BatchHandle {
    data: Arc<FlatBatchData>,          # memory rows or layer-file reader
    role: Base | Delta(batch_id),
}

effective_phase(handle, stored_value):
    if handle.role == Base: return OLD
    assert stored_value.is_new == true
    return NEW
```

Represent the handle inside `FlatKvBatch`: snapshots contain `Arc<FlatKvBatch>`, and each such wrapper
contains `Arc<FlatBatchData>` plus its role. Implement `FlatKvBatch::clone` by sharing `FlatBatchData`.
`Base` handles normalize the exposed bit to OLD without rewriting their shared data. Current `Delta(t)`
handles expose the physical NEW bits. Snapshot handles are immutable: changing the current index's role
creates replacement metadata handles sharing the same data, and cannot mutate an already captured view.
Raw storage inspection can still see the original stored bit; read-view cursors return the effective bit.
Serialize the role beside each current inventory file reference, so reopen cannot mistake a prior NEW bit
for a new transaction. `Base` needs no source-batch ID. `Delta(t)` uses the current transaction identifier
for protocol checks. This is metadata for the active inventory, not a catalog of historical snapshots;
Step 2 tests descriptor round trips without claiming atomic runtime recovery. Compaction can later emit
physical OLD bits, but completion/reopen must work before compaction runs.

For the first storage prototype, serialize one active logical delta and suspend background merging during
its maintenance. Several staging chunks may belong to that same delta; their arrival does not finish it.
Expose the spine merger's existing private pause/resume mechanism through a new scoped internal
guard holding the exclusive `&mut Spine<FlatKvBatch>` and its batch inventory. While suspended, the guard is
the same index's active inventory: snapshots and
append use it, and incoming files are not placed in another index or inserted only after maintenance.

```text
begin(t):
    guard = suspend_and_take_spine_batches()     # wait for in-flight merges; transfer Arc handles only
    original = guard.inventory.clone()          # clone Arc handles, not rows; retain abort inventory
    old = SpineSnapshot::with_batches(factories, original.clone())

append(delta_batches):
    guard.inventory.extend(delta_batches as Delta(t))
    new = SpineSnapshot::with_batches(factories, guard.inventory.clone())
                                               # now readable before view maintenance

finish():
    resume_spine(each handle in guard.inventory rebound to Base)
    advance completed_batch_id                 # resume transfers handles; it does not add delta again

abort():
    resume_spine(original)                     # restore pre-batch membership; discard new references
```

Expose `guard.snapshot()` using `SpineSnapshot::with_batches`. Do not call ordinary `Spine::ro_snapshot()`
while the guard owns the drained inventory: that method reads the merger, not the guard's batches. The
exclusive borrow prevents insertion or snapshotting through a competing live-spine API. On finish, allocate
new `Arc<FlatKvBatch>` Base wrappers sharing the same `FlatBatchData`; never mutate snapshot-owned wrappers.
If dropped before finish, the guard resumes the original inventory. Preparing large delta batches must use
explicit staging limits/spill before append; do not wait for
merge-driven backpressure while merging is suspended. Between transactions, the generic merger reads effective
Base values and writes flat consolidated records with physical OLD bits. Source append writes NEW bits.
This conservative compaction barrier is a prototype choice with a measurable latency cost, not a performance claim.
Durable rollback and atomic publication with base storage/output progress remain Step 3 integration work.

## Necessary changes to Feldera storage

### Reuse existing file columns for signed contributions

Use Feldera's existing [two-column writer](../../../crates/dbsp/src/storage/file/writer.rs#L1564).
Here a column means a hierarchy level as described above, not one SQL field. Keep the folded K in the
first level and store each signed payload contribution as a separate second-level row:

```text
column 0: search key = FoldedKey; data = ()
column 1: search key = PayloadBytes; data = (weight: ZWeight, is_new: bool)

# Logical D1 above, represented by one parent row and two contribution rows:
column 0: K -> child rows [a, b)
column 1: P100 -> (-1, NEW)
          P120 -> (+1, NEW)
```

Every second-level row contains exactly one payload, weight, and bit. Its associated parent supplies K.
The raw cursor flattens this physical grouping into `(K, V)` contributions. No V contains a packed list.
The complete accumulated endpoint still has at most one payload for K. Multiple physical columns are an
implementation choice that reuses the file format; the logical index has one folded search key.

Both writer levels already require unique keys within each group. Write payload rows with `write1`, then
write their parent K with `write0`. Consolidate identical payload contributions in a homogeneous phase
before writing. The index identifier in K selects the payload decoder; no extra kind field is needed.
Do not add a repeated-key writer mode or change file seek algorithms. Reuse existing outer-key
seek/next and child-group cursors; prefix termination is checked against the folded parent K.

There is one backend requirement beyond endpoint ordering: the existing
[batch cursor contract](../../../crates/dbsp/src/trace/cursor.rs#L42) orders values within a key, and
[the spine merger](../../../crates/dbsp/src/trace/spine_async/list_merger.rs#L197) compares those values.
Use `PayloadBytes`' ordinary byte comparison for this internal delta grouping. No SQL field order,
numeric order, or big-endian payload encoding is required. Encode equal payloads identically with the
existing serializer, so its byte equality agrees with payload equality. Weight and phase are excluded
from that comparison. This internal order is needed only to reuse Feldera's batch/merge contract; queries
cannot request a range or ordering by it. Eliminating it entirely would require replacing that contract.

### Batch and raw-cursor contracts

Implement `FlatKvBatch` under `crates/dbsp/src/trace/ord/flat_kv/`. Put prototype Q3 codecs and the storage
owner under `crates/dbsp/src/trace/merged_index/`. This step exposes crate-internal APIs only.

```text
raw batch row:       (FoldedKey, FlatValue { one payload, weight, bit })
BatchReader::Key:   dynamic FoldedKey
BatchReader::Val:   dynamic PayloadBytes
BatchReader::R:     DynZWeight
BatchReader::Time:  ()
```

The [Batch contract](../../../crates/dbsp/src/trace.rs#L846) can retain its logical key/value cursor:
the custom cursor traverses the outer K and its contribution rows with `step_key`/`step_val`.
The raw cursor exposes one flat contribution at a time, with K resolved from the parent row. Phase belongs
to the raw view interface and batch role, not to generic tuple equality.
Generic consolidation runs on homogeneous effective phase: incoming NEW chunks during staging or Base
batches between transactions. The active combined raw scan merges ordered iterators without cancelling
OLD and NEW contributions irreversibly in storage during the active batch. Endpoint reconstruction may
sum/cancel both phases for the NEW endpoint, as specified above; OLD reads still have the base contributions.

The generic tuple cursor omits phase, so the custom builders must preserve it explicitly:

- Delta staging constructors, including `Batcher::new_batcher` and `dyn_from_tuples`, stamp NEW.
- `Builder::for_merge` reads input batch roles, rejects mixed effective phases, and stamps every output
  row with their common effective bit. Merge NEW staging chunks only with NEW; merge Base only with Base.
- Persistence/conversion paths preserve the source role and effective bit. Override generic conversion
  defaults that would route Base data through a NEW-stamping constructor.
- Completion rebinds all active handles to Base before resuming generic compaction. A Base-output builder
  writes OLD even when its input files retain physical NEW bits from earlier transactions.

The raw cursor exposes the stored/effective bit as specified above; the generic tuple cursor exposes only
payload and weight. Test construction, conversion, spill, and merge paths separately for phase preservation.

Implement memory and file variants; ordered builders; chunked `MergeBatcher` staging; cursor navigation;
metadata counts/bounds; `persisted`/`from_path`; and storage-destination merging for spilled staging chunks.
Count outer K rows for `key_count()` and signed contribution rows for `len()`; they can differ in delta
batches. Builders write one payload/weight/bit per contribution row.
Reuse `FallbackValBatch` for the required `Timed<T>`
associated type; the prototype itself uses unit time. Use existing dynamic-data/factory conventions and
implement byte-wrapper ordering as lexicographic bytes. Report no roaring compatibility for these keys.

Expose `fold`, `unfold`, `append`, `snapshot`, raw `seek_ge`/`next`, and the scoped completion/abort protocol.
The cursor returns borrowed bytes, signed weight, and the effective bit; advancing invalidates its borrowed
row. A raw cursor can combine immutable batches without materializing the complete accumulated relation.
The Q3 codec's decoded enum is a utility for validation, not a typed query-accessor/session API.

## Verification and implementation sequence

Use new Rust tests and an independent `BTreeMap<Key, (Payload, Weight)>` endpoint oracle.
Represent input changes separately as signed contributions. Group the complete delta by K and apply its
matching-payload cancellations before validating the resulting endpoint; input ordering must not cause a
false uniqueness violation. Cover cancellation and multiple updates within one transaction. Earlier
excluded semantic-checker scripts are not acceptance evidence. Test transaction-supplied changes exactly as given;
do not introduce storage-layer FK checks or automatic child moves in test helpers.

1. **Codec and storage format:** golden key bytes, fold/unfold, malformed records, prefix scans, no END/LOOKUP
   fields, no per-record format/kind fields, and one payload per contribution row.
2. **File layout and cursors:** existing unique outer keys across data/index-block boundaries; multiple
   signed payload contributions under one K; forward/reverse/exact seeks; prefix termination; flattened
   cursor agreement with logical KV contributions; payload serializer round trips without field-order encoding.
3. **Batch algebra:** same-key payload replacement, signed multiplicity, matching-payload cancellation,
   unique K in both endpoints, memory/file agreement, several updates to the same K, spilled staging, and
   persistence/reopen. A zero net weight change must still preserve a changed payload.
4. **Snapshot ownership:** verify shared data/file identity before/after snapshot (`Arc`/file identity), no
   snapshot-triggered writes, unchanged old reads after append, and old reads surviving later compaction.
5. **Phase lifecycle:** both replacement contributions are NEW; old/new endpoint oracle agreement; two or
   more transactions with the test merger held idle, plus a separate merge-normalization case;
   role rebinding with no tuple rewrite; descriptor reopen preserves
   Base/Delta roles; no phase staleness or second application at completion.
6. **Transaction boundary:** supplied related-row changes appear exactly once; changing only an Order never
   manufactures Lineitem records; supplied old/new Lineitem locations both survive; abort restores original
   membership; guard cleanup works on early return; no backpressure deadlock during suspended merging.
7. **Runtime compatibility gate:** at integration, compare reconstructed OLD/NEW tuples and weights with
   the actual delayed/current traces at each bound read site. Include a same-key upsert, simultaneous changes
   on both join inputs (cross term exactly once), and one transaction spanning multiple steps/chunks before
   flush. Verify no early role promotion and identical outputs with retained state. This gate is required
   before replacing operator state; Step 2's endpoint oracle alone does not establish runtime compatibility.
8. **Scope checks:** no `OrderParent` data, parent lookup, relational validation, typed scan session,
   reconstructed integrator, or operator rewiring is introduced by this storage step.

Implement in three reviewable milestones: key codecs and existing-file layout; flat KV access and batch/merger
conformance; batch ownership/phase protocol. Each milestone includes focused tests and relevant rustdocs.
Run formatting, crate checks, and affected storage/spine tests. End-to-end Q3 equivalence, shared-reader
memory limits, crash recovery across source/output state, and performance measurement remain later gates.
