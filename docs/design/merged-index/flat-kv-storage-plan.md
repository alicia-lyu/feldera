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
4. Each physical row is one flat `(K, V)` pair. `V` contains one regular payload, a signed weight, and one
   old/new bit. There are no payload lists, per-record `format` fields, or redundant `kind` fields.
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

Customer payload = segment_length:u32 | segment:UTF8
Orders payload   = order_day:i32 | ship_priority:i32
Lineitem payload = ship_day:i32 | extended_price_cents:i64 | discount_hundredths:i64
```

Payload scalar encodings are big-endian, dates are integer days since the Unix epoch, and price/discount
use exact scale-two integers for the Q3 prototype. The index identifier in K selects the payload schema.
The file serializer provides record framing; `FlatValue` adds no format or kind column. Keep query-ineligible
records, since later updates can make them eligible.

Each file row contains K and exactly one `FlatValue` as auxiliary data in a single file column. Multiple
rows may have the same K. Among equal keys, order rows by canonical payload bytes and then phase for
deterministic iteration. This tie ordering does not add fields to the storage key or index payload columns.

The algebraic identity is `(K, payload)`. Weight and phase are not tuple-identity fields. Only identical
complete tuples consolidate by summing weights; zero sums disappear. Phase distinguishes when a contribution
entered the active batch, independently of its sign. The incoming batch is a Z-set of complete signed changes.

### Same-key replacement, with no packed value

Here `P100` and `P120` denote complete line payloads differing in extended price:

```text
Existing immutable batch B0, excerpt:
  K -> { payload=P100, weight=+1, is_new=0 }

Incoming immutable batch D1, excerpt:      # two distinct flat rows with the same K
  K -> { payload=P100, weight=-1, is_new=1 }
  K -> { payload=P120, weight=+1, is_new=1 }

OLD endpoint: P100 has weight +1.
NEW endpoint: P100 has weight +1-1=0; P120 has weight +1.
```

A deletion is a negative complete-tuple contribution, even though it is NEW to the current batch.
An insertion and a retraction of the identical tuple in the same incoming batch cancel before storage.
Changes with different payloads must never cancel merely because their folded keys are equal.

The bit selects contributions, not disjoint endpoint relations. For each complete tuple `x = (K, payload)`:

```text
weight_old(x) = sum(weight of x in effective OLD records)
weight_new(x) = weight_old(x) + sum(weight of x in effective NEW records)
```

Suppress zero totals when exposing either endpoint. NEW-state reconstruction must include both phases;
reading only NEW-marked records would return the input delta, not accumulated state.

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

### Equal folded keys must remain separate flat rows

The current [one-column writer](../../../crates/dbsp/src/storage/file/writer.rs#L1481) requires
strictly increasing keys. Its [index search](../../../crates/dbsp/src/storage/file/reader.rs#L1222)
can return an arbitrary matching child on equal bounds. These contracts do not support the replacement
example as-is. Relaxing the writer assertion alone would risk skipping records across block boundaries.

Add an explicit repeated-key mode to the flat one-column writer/reader. Keep the default unique-key mode
for existing batches. Record the mode in file metadata; old files retain unique-key behavior.

- Accept nondecreasing K in repeated-key mode. The flat batch builder validates payload tie ordering.
- Forward seek chooses the earliest eligible child and first row with `K >= target`; reverse seek chooses
  the latest eligible child and last row with `K <= target`. Respect the cursor's remaining row range.
- Handle equal keys spanning data blocks and several index levels, including exact-key and batched fetch paths.
  Use the ordinary cursor fallback until optimized batched fetch has equivalent duplicate-aware tests.
- `next`/`previous` advance one physical row, even when K remains equal. Prefix/exact scans consume every such row.
- Preserve existing file/cache/checksum and ordinary unique-key behavior. No hidden payload/phase suffix in K.

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
the custom cursor groups adjacent equal-K flat rows for `step_key`/`step_val`, but never packs payloads into
one stored value. Phase belongs to the raw view interface and batch role, not to generic tuple equality.
Generic consolidation runs on homogeneous effective phase: incoming NEW chunks during staging or Base
batches between transactions. The active combined raw scan merges ordered iterators without cancelling
OLD and NEW contributions irreversibly in storage during the active batch. Endpoint reconstruction may
sum/cancel both phases for the NEW endpoint, as specified above; OLD reads still have the base contributions.

Implement memory and file variants; ordered builders; chunked `MergeBatcher` staging; cursor navigation;
metadata counts/bounds; `persisted`/`from_path`; and storage-destination merging for spilled staging chunks.
Count distinct K values for `key_count()` and physical tuple rows for `len()`; repeated-key files cannot
use their row count as their distinct-key count. Builders write each payload/weight as one flat row.
Reuse `FallbackValBatch` for the required `Timed<T>`
associated type; the prototype itself uses unit time. Use existing dynamic-data/factory conventions and
implement byte-wrapper ordering as lexicographic bytes. Report no roaring compatibility for these keys.

Expose `fold`, `unfold`, `append`, `snapshot`, raw `seek_ge`/`next`, and the scoped completion/abort protocol.
The cursor returns borrowed bytes, signed weight, and the effective bit; advancing invalidates its borrowed
row. A raw cursor can combine immutable batches without materializing the complete accumulated relation.
The Q3 codec's decoded enum is a utility for validation, not a typed query-accessor/session API.

## Verification and implementation sequence

Use new Rust tests and an independent `BTreeMap<(key, payload), weight>` endpoint oracle. Earlier excluded
semantic-checker scripts are not acceptance evidence. Test transaction-supplied changes exactly as given;
do not introduce storage-layer FK checks or automatic child moves in test helpers.

1. **Codec and storage format:** golden key bytes, fold/unfold, malformed records, prefix scans, no END/LOOKUP
   fields, no per-record format/kind fields, and one physical tuple per file row.
2. **Duplicate-key file support:** many equal keys across data/index-block boundaries; forward/reverse/exact
   seeks; lower/upper extremes; prefix termination; many different keys per file; unique-key regression tests.
3. **Batch algebra:** same-key payload replacement, signed multiplicity, cancellation only by complete tuple,
   memory/file agreement, more than two equal-K rows, spilled staging, and persistence/reopen.
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

Implement in three reviewable milestones: codecs and duplicate-key file mode; flat batches and merger
conformance; batch ownership/phase protocol. Each milestone includes focused tests and relevant rustdocs.
Run formatting, crate checks, and affected storage/spine tests. End-to-end Q3 equivalence, shared-reader
memory limits, crash recovery across source/output state, and performance measurement remain later gates.
