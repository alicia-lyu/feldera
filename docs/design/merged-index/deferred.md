# Deferred designs

The designs and work items here are not implemented. None is a prerequisite for R3 reconstruction or R4 verification. They are kept because a later workload might need them.

- A single-owner atomic protocol for publishing before/after views.
- Shared scan sessions that let several readers share one range cursor.
- Later work on the general merged-index definition, outside its roadmap.
- Native packing of values.

## What R3 does instead

The R3 implementation (see [reconstruction.md](reconstruction.md)) uses no single owner and no atomic append-and-publish step in the circuit. The harness-side `ManualIndex` appends to the Customer–Orders–Lineitem (COL) merged index on a DBSP auxiliary thread, outside the circuit. For each transaction it:

1. appends the transaction delta, which yields a before and an after snapshot;
2. publishes both, with the placement map, through `Q3State::publish`;
3. runs the DBSP transaction, whose supplied-snapshot join calls its readers once, at the commit flush;
4. calls `Q3State::finish`.

`publish` refuses a second publish before `finish`, so one publish covers exactly one transaction. The protocol below is a stronger design: it makes publication, completion, and failure recovery part of a single owner's contract. R3 does not implement it. Where the two disagree, the code and [reconstruction.md](reconstruction.md) govern.

## Single-owner atomic protocol

> [!NOTE]
> **Glossary**
>
> **rekeys** — moves to changed physical key prefixes.
>
> **View lease** — Ownership of the temporary source read handles used by a computation.

Each reader holds a **view lease**.
One owner appends the complete transaction-supplied delta to the same merged index before opening any
after-maintenance reader. If related Lineitem locations must change, the transaction supplies those row changes;
the storage adapter does not infer them from an Order change. Publishing these views to operators is
distinct from committing externally visible results.
The runtime must report completion of all consumers, including output-state updates and work across steps.
Physical cursor EOF alone does not establish that completion.

> [!NOTE]
> **Glossary**
>
> **Seal** — Make the complete pending update set immutable and readable.

```text
process_batch(input, consumer_plan):
    owner = begin_owner(total_memory_budget)
    try:
        owner.before = reference_current_source_batches()
        update_base_storage_and_indexes(input.supplied_changes)  # appends to this index exactly once
        owner.after = reference_source_batches_after_complete_append()
        owner.publish_to(consumer_plan)                       # views no longer change

        provider = ReconstructedStateProvider(owner)
        run_existing_ivm_paths(input.deltas, provider, consumer_plan)
        await consumer_plan.all_tasks_and_output_updates_finished()
        provider.close_all_source_and_sorted_cursors()
        assert owner.outstanding_view_leases == 0

        complete_source_and_output_transaction(owner)         # no second weighted insertion
    except failure:
        cancel_and_join_all_consumers()                       # no concurrent reader left
        close_all_cursors_and_release_returned_values()
        abort_or_recover_at_commit_boundary(owner)
        raise failure
    finally:
        owner.release_snapshot_references()
```

The after-maintenance read resolves complete-tuple signed updates already appended to the index; it must preserve both
payloads of a replacement at an unchanged folded identity. The completion/abort calls state required runtime
transaction behavior, not a new WAL design. On failure, recovery must determine which changes entered the
maintained view before
retry. Source records and output progress must not advance independently. Temporary read handles reference
existing batches; they create no tuple copies, historical-version catalog, or expiration policy.

## Shared scan sessions

> [!NOTE]
> For IVM, [shared-traversal.md](shared-traversal.md) (2026-10-29) supersedes this section as the target design: one forward scan session per transaction that collects each touched group once and hands its buffers to the execution (buffered execution, as amended on 2026-11-09). The material below remains background for full-query execution.

Shared scan sessions are a **deferred, optional abstraction** for either full-query execution or IVM.
The current experiments use direct reconstruction. The design below applies only if later workloads
justify coordinating multiple readers over the same ranges; it is not a prerequisite for reconstruction
or runtime integration.

> [!NOTE]
> **Glossary**
>
> **Scan session** — An owner of one physical range cursor and the buffers used by its registered readers.

Each **scan session** has its own cursor position and reader buffers. A durable
index handle may be shared; mutable cursor position belongs to a session. This follows the validated
[`mi_db` session design](../../../../mi_db/docs/architecture.md#merged-index-scan-sessions): register readers
before scanning, share one buffer per record type, and give same-type readers independent positions.
Validation applies to the scan-session section, not the entire architecture document. The separate
[buffer guard](../../../../mi_db/docs/architecture.md#pending-buffer-guard) warns at 80% and fails before
an insertion reaches its limit; that section describes no spill or reread and is not assumed validated.
The pseudocode below is a proposed Feldera adaptation with explicit overflow choices, not an implemented
API or a claim about `mi_db` code.

The cited document is modified working-tree content inspected on 2026-09-28, based on `mi_db` revision
`d50d29dfe559258351c1071e59ca5365eacf09d0`. Its SHA-256 is
`d043b9b28121fe044980fde032d95e4b00c331e1d7a137ce709bd70eb6914e79`.

### Optional grouping of compatible requests

If shared sessions are implemented, choose sharing before scanning and register every reader first.
A session may scan a coalesced range and readers may filter within it; arbitrary overlapping requests do
not automatically share a mutable cursor. The following pseudocode is only for that deferred design.

```text
prepare_access(requests, owner):
    for group in choose_compatible_scan_groups(requests):
        session = ScanSession(
            view_lease = owner.acquire(group.view),
            range = encode_bounds(group.covered_keys),
            decode_fields = union_of_fields_needed_by_type(group),
            memory = owner.reserve_child_budget(group.buffer_limit))
        register_all_typed_readers(session, group)
        session.seal_registration()

        for request in group:
            state_cursor = reconstruct_requested_state(request, session.readers_for(request))
            if proves_required_order(state_cursor, request.key_and_value_comparator):
                publish_cursor(request, state_cursor)
            elif maintained_path_matches(request, owner):
                close_unused_readers(state_cursor)
                publish_cursor(request, open_source_access_path(request, owner))
            else:
                publish_cursor(request, external_sort_and_consolidate(
                    state_cursor, request.key_and_value_comparator,
                    owner.reserve_sort_budget(), account_all_temporary_io))
```

Compatibility includes read-state identity, encoded range, decoder requirements, and forward traversal.
If sessions are introduced, independent states or ranges use independent sessions in this design. Sharing
one traversal across before/after states requires access to both immutable snapshot inventories and their
weighted payloads; the batch lifecycle permits it but does not imply it is already implemented.
Readers needing an independent seek close and reopen their own access instead of repositioning a cursor
under other consumers. Replayed reads are
charged. Existing operator seek/order behavior must still be honored.

### Shared typed buffers and reader positions

> [!NOTE]
> **Glossary**
>
> **Typed buffer** — A sequence of decoded records of one type shared by all readers requesting that type.

Readers requesting the same record type share a **typed buffer**.
The session stores `{physical_cursor, range, view_lease, buffers_by_type, readers, eof, memory_account}`.
A reader starts with `{type, next_sequence=0, outstanding_lease=empty, closed=false}`. Sequence numbers
are absolute and remain valid after reclaiming prefixes. The scheduler serializes `next`, `release`, and
`close` for one session; its physical cursor and buffer registry must not be mutated concurrently. A wait
yields control without holding a lock that prevents another reader from progressing. Decoder projections
are fixed before scanning; a record is decoded once to the union of required fields for that type.
Predicates and relational reconstruction are outside this
physical record distribution step, as in the
[`mi_db` typed-buffer design](../../../../mi_db/docs/architecture.md#shared-typed-buffers).

```text
next(reader):
    require not reader.closed and reader.outstanding_lease is empty
    loop:
        buffer = session.buffers[reader.type]
        if buffer.contains(reader.next_sequence):
            lease = buffer.borrow_with_budgeted_reload(reader.next_sequence)
            if lease is WAIT or RESOURCE_LIMIT: return lease
            reader.outstanding_lease = lease
            return lease
        if session.eof:
            return EOF                                      # this reader has drained its suffix

        record = session.physical_cursor.peek_bounded()      # cursor page is budgeted
        if record == EOF:
            session.eof = true
            continue
        if no_active_reader(record.type):
            session.physical_cursor.advance()
            continue

        needed = bounded_decode_size(record) + queue_metadata_cost
        if not session.memory.try_reserve(needed):
            return handle_pressure_without_advancing_cursor(record, needed)
        decoded = decode_registered_fields(record)           # allocation covered by reservation
        session.buffers[record.type].append(decoded)
        session.physical_cursor.advance()

release(reader):
    require reader.outstanding_lease exists
    reader.outstanding_lease = empty
    reader.next_sequence += 1
    reclaim(reader.type)

reclaim(type):
    live = active_readers_of(type)
    cut = min(r.next_sequence for r in live) if live else buffers[type].end_sequence
    buffers[type].release_entries_before(cut)                # releases byte reservations too

close(reader):
    if reader.closed: return
    require reader.outstanding_lease is empty
    reader.closed = true
    remove_reader_from_registry(reader)
    reclaim(reader.type)
    if no_active_readers(): close_physical_cursor_and_release_view_lease()
```

The caller releases each borrowed record before requesting another. Holding a field beyond release requires
a copy charged to the consumer's budget. Closing is idempotent and requires releasing outstanding borrows
first. Batch cancellation stops all consumers before cleanup; session shutdown cannot leave borrowed pointers
into reclaimed storage. Reloading a spilled entry reserves memory before reading; it can yield or fail
rather than exceed the budget. Physical EOF
still permits every reader to drain buffered records. For two Lineitem readers starting at sequence zero,
reader A consuming records 0–9 does not free them while B remains at zero. After B releases record 0, that
record can be reclaimed. Closing B lets A's consumed prefix be reclaimed immediately. Other record types
have separate buffers and positions but compete for the same session byte limit.

### Overflow must have a progress path

Charge unique decoded records once, plus allocated capacity, queue metadata, reader positions, and in-flight
decode storage. Budget reservations precede allocation; a record larger than the entire budget requires a
streamed/spilled representation or an explicit error. A size estimate must be conservative and enforced.
Per-session limits are suballocations of a total budget also covering storage cache, operator state, sort
runs, pending updates, and consumer copies. Spilling does not make its indexing metadata free.

```text
handle_pressure_without_advancing_cursor(record, needed):
    reclaim_all_consumed_prefixes()
    if room_for(needed): return RETRY
    if scheduler_can_run_a_reader_that_will_release_space():
        wake_that_reader()
        return WAIT_FOR_RELEASE                             # yield; do not block its thread
    if configured_policy == SPILL:
        spill_unborrowed_unread_entries_preserving_sequences()
        return RETRY if room_for(needed) else RESOURCE_LIMIT
    if configured_policy == REREAD:
        detach_lagging_reader_to_independent_pinned_view_scan()
        return RETRY if room_for(needed) else RESOURCE_LIMIT
    return RESOURCE_LIMIT
```

`RETRY` and `WAIT_FOR_RELEASE` are internal scheduler results, not tuples returned to the operator. The
runtime adapter resumes the same request. Spilled entries keep their sequence identities, and later reads
load them through budgeted buffers; borrowed entries remain pinned. A reread uses the same immutable view
and exact continuation token, including position within decoded contributions if one physical key yields several
records. It must neither duplicate nor skip a record and must reserve its own cursor memory before detaching.
If a continuation cannot be represented safely, reject that fallback rather than guessing a seek key.

Backpressure alone cannot resolve a dependency cycle: the reader that pins data might be waiting for the
reader currently requesting more. The scheduler must establish a runnable consumer that can free space;
otherwise spill, reread, or fail the batch explicitly. Never drop required rows, silently enlarge the budget,
or spin on `RETRY` without progress. Spill/reread are proposed extensions to the cited `mi_db` guard.

Conformance checks should interleave same-type and different-type readers, drain buffered suffixes after
physical EOF, close lagging readers, and hold borrowed values during pressure. Also test registration after
sealing, independent seeks, cross-step view lifetime, oversized records, dependency cycles, external-order
changes, and failure during commit. Compare both state-provider results and the shared IVM output with the
retained-state baseline; report high-water bytes and every spill, reread, sort, and access-path write.

## Selective execution

Deferred on 2026-11-09. The thesis write-up moved selective execution out of Chapter 3 into the companion note `key-trie-execution-refinements` (a PDF linked from `key-trie-merged-index-order.tex`). [shared-traversal.md](shared-traversal.md) now follows buffered execution only. Its earlier text, with the material below in full, is in git history (commit `d8c1dbd11`).

Selective execution visited only the instance nodes that requests needed, and kept only the entries that later requests could still name. The removed material:

- **Requests.** A driving row made a request to a reader, anchored at the instance node of the row's values cut to the reader's key. A base delta row made a *static* request, known before the scan. A derived row made a *dynamic* request when it was computed.
- **Order of requests.** Each reader owned a queue, and the session merged the queues' heads by key prefix, which is record order with ancestors first. A dynamic request entered its queue by a sorted insert.
- **Ready pairs.** A pair of a delta term was ready when all its rows had been read. At each node, the session computed every ready pair and made the requests its rows produced before choosing a child.
- **`Visit`.** The recursive traversal over instance nodes, with `Advance` and `AtRangeEnd`. It entered a child only if a request needed it or it had to be entered to compute requests, and otherwise skipped the child's range by one seek.
- **Assumption (A3), available requests.** Every request names entries still ahead of the cursor or kept for its consumer, and before the session passes a child's range it knows every request that can name an entry there. It had to be shown per circuit. The write-up's examples that violate it: a coarse request (join `J` with `L` on `(custkey)` alone) and sibling tag order (`W = Invoice ⋈ (L ⋈ O)`).
- **Kept path.** The entries of the current root-to-node path, kept for path reads such as `J⁺`.
- **Width for child prefixes.** A per-domain width let the session compute a child domain's prefix length to seek straight to it. Buffered execution needs only the root's width, to infer groups from `Dlt`.
- **Result.** On the worked transaction, two forward logical seeks served all four reads, and `v2`, `v8`, and `c6` were never decoded.

Buffered execution replaces it as the fallback and as the only target. Reviving selective execution means restoring the request queues and `Visit` over the same `RunMerge`, `Old`, and `Dlt`, and showing (A3) for the circuit.

## General merged-index definition: later work

These items lie outside the roadmap in [general-definition.md](general-definition.md).

- **Variable-width keys,** such as VARCHAR. They need an escaped, terminated encoding, and the unique-parsing argument has to change to match.
- **Generated readers and joins,** replacing the exact-shape matching in `Q3MergedIndexBinding`.
- **Inferring the trie** from foreign-key and primary-key metadata.
- **Nullable key columns.** There are two candidate encodings: a marker byte before the value (one extra byte per field), or two consecutive tags per nullable domain, the lower meaning NULL (no extra bytes).
- **Placements on views.** The validator rejects them because views have no primary key (decision of 2026-10-20). Allowing them needs a way to declare or infer a view's unique key, for example in the spec or from the view's query.
- **Nullable and other payload types.** A payload column must be NOT NULL and have a packed encoding: an integer, BOOLEAN, DATE, TIME, TIMESTAMP, DECIMAL, or VARCHAR. Nullable columns need a packed `Option` encoding; floats, binary strings, and the other SQL types need their own.
- **Quoted identifiers in specs.** Relation and column names are unquoted SQL identifiers, matched in the compiler's canonical casing. A table whose name needs quotes cannot be named yet.

## Single circuit with merged indexes (open question, 2026-10-26)

Can one circuit maintain the base relations, the intermediate views, and the final view when merged indexes are included? The harness keeps two circuits and the JSON hop for now; this entry records what a study must settle.

### Why the prototype cannot

`ManualIndex` is maintained outside the circuit and read through snapshots published before each transaction (`Q3State::publish`). Its update must therefore be computed before the step starts, from data the driver already has.

### Why stacking needs more

In a stacked tree, merged index MI_A feeds an intermediate view V, and V is held in MI_B. V's delta exists only inside the step, so an index maintained outside the circuit cannot consume it within the same step.

### Candidates

- Merged-index maintenance at step time inside the runtime, with the index still a physical structure owned by the merged-index pass.
- One transaction per stage, which changes when outputs commit.

### Reference: typed edge between separate circuits

This applies if the study chooses separate circuits.

Today each forwarded row passes through five conversions:

1. typed tuple to JSON value (`val_to_json`);
2. to a `Customer`, `Order` or `Line` struct (`parse_*`);
3. to a JSON value (`row.json()`);
4. to bytes (`to_vec`);
5. parsed back into the view input's struct and tuple (`DeCollectionStream::insert`).

The Catalog has no batch insert, and its input handles hide the typed `ZSetHandle`. Either option below should begin by splitting the `forward` timer into read (`net_delta`) and feed (`apply_optional`), so the cost breaks down by measurement.

- **Option 1: driver only, one JSON hop kept.** Read the base delta by downcasting `cursor.val()` to the `Tup*` types (precedent: `verification.rs`). Feed the view with `cursor.serialize_val` bytes straight into `insert` and `delete`. This touches `workload::net_delta`, `main.rs` `Inputs` and `apply_optional`, and a few test helpers.
- **Option 2: no JSON.** Return the view circuit's existing typed input handles (`handle_s0` to `handle_s2`, already created in Catalog-mode generated code) beside its Catalog. Generation needs either a post-process step in `generate.sh` or a "Catalog plus input handles" mode in `ToRustVisitor.preorder(DBSPCircuit)` with a `CompilerOptions` flag and a test. The driver gets a typed view feeder (`ZSetHandle::push(tuple, ±1)`), and `DeleteStyle::WholeRow` goes away. The base circuit stays in Catalog mode, and the view keeps its Catalog for V verification, `storage_evidence`, `finish_loading` and the checkpoint test.
- **Not recommended: plain `--handles` with `--q3-merged-index`.** It drops the Catalog, which breaks V verification, storage evidence, loading and the checkpoint test, and it drops the base circuit's tables-as-outputs.

Unaffected under both options: `reconstruction::prepare`, `merged-index-col` `q3.rs`, `Q3State`, the `verify.sh` transcript comparison, `measure.py`, `summarize.py` (which keeps `forward_ms`), and the COL `generated.rs`. The "as JSON" wording in RESULTS.md, measurement.md, CIRCUIT.md and WORKFLOW.md would need updating.

## Native packing

Native packing (packed tuples, narrowed decimals) is not planned. Two barriers stand in order:

1. Native values are sorted and compared within a key, so a packed form needs an order-preserving encoding or a decode for each comparison.
2. Native types are shared by the whole program and by DBSP's storage format, so the change needs a new storage format version for every pipeline.

Neither barrier is a decoding cost. No native packing number has been measured.
