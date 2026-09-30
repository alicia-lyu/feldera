# Phase 3 implementation plan: signed merging and K uniqueness

Date: 2026-09-30. Status: planned; implementation has not started.

The [Step 2 storage plan](folded-key-layer-file-plan.md#phase-3-signed-merging-and-k-uniqueness)
governs this phase. Build on the verified
[Phase 2 implementation](phase-2-implementation-note.md): immutable in-memory batches and a raw
cursor that preserves contributions from separate batches. Phase 3 adds signed consolidation,
exact-key accumulated reads, and a changed-key uniqueness helper. Keep these APIs crate-internal.

## Governing semantics

Consolidation identity is the complete `(K, payload)` pair. Add its signed `ZWeight` contributions
with checked arithmetic and omit the pair only when its total is zero. Preserve distinct payloads
at the same K. A replacement has `(K, old, -1)` and `(K, new, +1)`; summing by K would erase it.

An accumulated key is unique when at most one distinct payload has a nonzero total. Zero survivors
means absence. This check does not require the surviving weight to be `+1`: a single payload with
weight `2` or a negative weight passes this particular invariant. Source multiplicity and other
input validity rules remain separate obligations.

Extract distinct changed K values from the signed delta before any key-only projection or sum.
Validate those keys against all contributions in the logical after-state, including both the
before-state and the delta. The helper's caller must supply that complete view. Checking changed
keys establishes global uniqueness only if the before-state was already valid and every changed
key is included. Restored or otherwise untrusted state requires a separate full validation policy.

Compaction can merge any subset of batches. Its result may contain several payloads for a K,
including negative contributions; it must preserve the represented signed relation. Only a merge
covering all contributions for a valid accumulated K is expected to leave at most one payload.
Uniqueness failure must report an error; it must never select a payload, discard a conflicting
row, or repair the state.

## Existing implementation and constraints

- [`memory_batch.rs`](../../../crates/dbsp/src/trace/merged_index/memory_batch.rs) owns
  `MergedIndexBatch`, `build_batch`, and `RawBatchCursor`. The builder sorts and consolidates
  within one batch. The cursor orders contributions by `(K, payload, batch position)`, seeks
  to the first K greater than or equal to a target, and retains equal pairs across batches.
- [`mod.rs`](../../../crates/dbsp/src/trace/merged_index/mod.rs) owns `FoldedKey`, `PayloadBytes`,
  the definition interface, and the Phase 1 layer-file fixture. The byte wrappers already provide
  ordering and equality; merging needs no source-specific decoding.
- [`CursorList`](../../../crates/dbsp/src/trace/cursor/cursor_list.rs) and the existing
  [spine snapshot](../../../crates/dbsp/src/trace/spine_async/snapshot.rs) are useful integration
  precedents. Reuse must preserve checked weight arithmetic and the raw-versus-consolidated
  distinction. Phase 3 does not install a new spine merger.
- [Input upsert](../../../crates/dbsp/src/operator/dynamic/input_upsert.rs) retracts old values
  and adds replacements. That behavior alone does not prove uniqueness for every folded source
  and every accepted update path. Retain explicit validation until Step 3 establishes that proof
  for the actual source constraints and caller wiring.

The existing test workloads are small in-memory inputs. Phase 3 makes no bounded-memory claim
for batch construction, changed-key collection, or materialized read results.

## Module boundaries

Use sibling modules under `crates/dbsp/src/trace/merged_index/`. Keep source codecs and
physical storage ownership outside the signed merge and validation algorithms.

| Module | Responsibility | Dependencies |
| --- | --- | --- |
| `signed_merge.rs` | Ordered row contract, checked full-pair consolidation, merge errors | Byte wrappers and `ZWeight` |
| `memory_batch.rs` | Existing batch construction, raw cursor, adapter to the row contract | In-memory DBSP batches |
| `state.rs` | Changed-key extraction, exact-K reads, uniqueness checks and conflict details | Signed merge API |
| `mod.rs` | Module declarations and narrowly scoped exports | These modules and existing codecs |

The dependency direction is `memory_batch -> signed_merge` and `state -> signed_merge`.
Neither `signed_merge` nor `state` imports `VecIndexedWSet`, `Writer2`, file readers, spine types,
snapshot handles, transaction types, or the Customer–Orders–Lineitem definition. Tests may compose
the modules through their crate-internal interfaces.

### Ordered contribution source

Define one small internal `SignedRowCursor` trait in `signed_merge.rs`. Its operations are
`seek_ge(key)`, `row()`, and `advance()`, with an associated source error type. All operations
are fallible so future file reads can propagate errors without changing the consolidation API.
`row()` borrows key and payload bytes and returns a copied `ZWeight`; its borrow ends before the
source advances. Adapt `RawBatchCursor` using an infallible source error.

The source must produce nondecreasing `(K, payload)` rows across its complete input set. Equal
pairs may occur repeatedly. `seek_ge` resets the position, including after exhaustion, and clears
any pending consolidation state in the wrapper. End-of-input and a source error are distinct.
Expose only forward traversal plus this resettable seek; reverse traversal, prefix APIs, and
storage lifecycle methods are unnecessary for Phase 3.

The adapter owns ordering across its underlying batches. Phase 3 uses the existing raw cursor's
batch-position tie order. Phase 4 must provide a sorted source across its file or mixed batches;
concatenating individually sorted batches does not satisfy the contract.

### Signed consolidation

Implement `ConsolidatedCursor<C: SignedRowCursor>` with a fallible `next_row()` that yields an
owned `(FoldedKey, PayloadBytes, ZWeight)` and a resettable `seek_ge`. Copy only the current pair
needed while advancing the source. Consume all adjacent contributions for that pair before
returning it; skip zero totals and retain the next pair for the following call.

Also provide `next_row_for_key(key)` for a caller that has sought to K. It uses the same pair
consolidation routine but stops before consuming a different key, including when every pair at K
cancels. This prevents an exact-key read from consolidating an unrelated later key merely to
discover that its range has ended.

Use `checked_add` for every addition. Report overflow with the offending key and payload, and
propagate source errors without converting them to end-of-input. After an error, callers discard
that scan or reset it with a successful seek. A failed seek leaves the wrapper invalid until a
later successful seek; no read or advance may use its stale buffered row. A caller that needs an atomic result must finish
the fallible scan before publishing any output; already yielded rows are not a successful batch.

Checked `ZWeight` accumulation can fail on an intermediate sum even when the mathematical final
sum would fit. Preserve that conservative policy, consistent with the current batch builder;
do not claim permutation-independent success for overflow cases. Compaction equivalence tests
must use weights whose intermediate sums fit for all tested groupings.

Use the same consolidated stream for accumulated reads and compaction fixtures. For the small
in-memory compaction fixture, feed its owned rows into the existing `build_batch` after propagating
scan errors. The extra collection and sort are acceptable at this phase's workload size. Do not
add a generic output-builder or batch-storage framework: Phase 4 can consume these ordered rows
with its selected file builder and stage output until the merge succeeds.

### Accumulated reads and uniqueness

Put the following helpers in `state.rs`; names describe the proposed API rather than a new public
storage interface:

- `collect_changed_keys(delta_cursor)` scans raw signed delta rows and returns sorted, distinct
  `FoldedKey` values. Collect keys without summing weights across payloads. A delta already
  consolidated by full pair is also sufficient: removing an exactly canceled pair cannot hide
  an actual state change. Extra keys, including keys from zero rows, are harmless to validation.
- `read_source_state(after_cursor, key)` seeks a `ConsolidatedCursor` to the exact key and returns
  all `(PayloadBytes, ZWeight)` survivors for that key. The supplied source covers before plus
  delta, as one globally ordered view. A lower-bound hit on a later key means an
  empty result. Use `next_row_for_key` to stop at the next key; do not mistake a shared byte
  prefix for equality.
- `validate_changed_keys(after_cursor, changed_keys)` validates keys in sorted, distinct order
  against the complete after-state. Return success for zero or one survivor. Return a structured
  uniqueness error containing the folded key and two distinct nonzero payload/weight witnesses
  when a second survivor is found. Distinguish conflicts from source and overflow errors.

Share an internal exact-key scan between reads and validation so validation can stop after its
second survivor without allocating every payload. Keep the all-survivor read available for
storage consumers; the validator must never make that read silently choose one row.

Phase 3 tests construct the logical after-state from before and delta batches in memory. These
helpers do not append a delta or create, retain, publish, or roll back snapshots. Returning `Ok`
means the supplied keys passed against the supplied view; it does not commit a transaction.

## Implementation sequence

1. Confirm the existing cursor and consolidation APIs against the ordering, seek, and checked
   arithmetic requirements above; record why native consolidation is reusable or insufficient.
   Add the ordered source contract and the in-memory adapter. Preserve raw cursor behavior and
   cover the contract with a tiny test source independent of DBSP storage, including injected
   read errors. Keep the trait small enough for a later file adapter.
2. Add `ConsolidatedCursor` and typed merge errors. Test full-pair grouping, zero suppression,
   seeks, exhaustion, checked overflow, and source error propagation. Build an in-memory
   compaction fixture through the existing batch builder.
3. Add changed-key extraction, exact-key accumulated reads, and uniqueness validation in
   `state.rs`. Test replacement semantics against complete in-memory after-state inputs.
4. Add an independent signed-weight oracle and compaction-equivalence cases. Verify the same
   relation before and after replacing a subset of input batches with its merged output.
5. Run focused and existing merged-index tests, review module dependencies, and write a short
   Phase 3 implementation note. Update the umbrella status only after recording passing evidence
   and any concrete API deviations from this plan.

## Verification cases

Use a test-only ordered map keyed by complete `(K, payload)` with a wider integer accumulator
as the oracle. The oracle must not call the production consolidator, state reader, or batch
builder to derive expected results. Compare complete sorted nonzero rows and weights, including
negative results. Keep overflow expectations separate from algebraic equality tests.

| Area | Required evidence |
| --- | --- |
| Cross-batch merge | Equal pairs across several batches sum once; distinct payloads stay separate |
| Cancellation | Exact pair cancellations disappear; an empty or fully canceled input yields no rows |
| Signed weights | Negative totals survive; repeated positive copies form one payload with weight `2` |
| Cursor behavior | Exact and missing seeks, reset after exhaustion, later keys, and shared prefixes |
| Failures | Positive and negative overflow; source errors during seek, read, and advance propagate |
| Basic state changes | Insert leaves one payload; complete delete leaves none; replacement leaves the new payload |
| Uniqueness | Two nonzero payloads fail, including an insert of a new payload without retracting the old one |
| Delta candidates | `old -1, new +1` retains K despite zero key-only weight; repeated K is deduplicated |
| Validation scope | Only requested changed keys are checked; tests document the valid-before precondition |
| Partial compaction | A subset merge may retain multiple payloads; adding untouched batches preserves the oracle state |
| Full compaction | All contributions for valid K reduce to zero or one payload and agree with accumulated reads |
| Storage independence | A small non-DBSP cursor runs the same merger and validator, including fallible input |

Include several deterministic generated batch partitions and merge orders with bounded weights.
Exercise empty batches and cancellation split across batches. Use existing test dependencies;
no new property-testing framework is required for this phase.

Extend `crates/dbsp/src/trace/merged_index/tests.rs` for cross-module regression coverage; keep
small source-contract unit tests beside the new modules when they need private access.
Run `cargo fmt --check` and `cargo test -p dbsp --lib trace::merged_index`, including the
existing Phase 1 and Phase 2 tests. Record exact commands, environment constraints, and results
in the implementation note. For this plan's documentation changes, run Markdown lint, local
link checks, and `git diff --check`; Rust tests apply when implementation begins.

## Phase 4 and Step 3 handoff

| Phase 3 deliverable | Phase 4 responsibility |
| --- | --- |
| Ordered, fallible contribution contract | File and mixed-batch cursor adapters and their lifetimes |
| Checked full-pair consolidation semantics | Selected file/spine merge integration and error propagation |
| Ordered owned merged rows | File output construction, incomplete-output cleanup, and reopen evidence |
| Changed-key and after-state validation helpers | Stable before, delta, and after storage views for callers |
| In-memory semantic evidence | Batch limits, bounded staging/spill, append once, and snapshot ownership |

Phase 4 chooses the concrete DBSP batch, append, and snapshot APIs. If it uses native merge
facilities instead of this cursor implementation, it must demonstrate the same full-pair signed
semantics and checked-error policy. Physical grouping of a parent K and payload children is
insufficient to establish uniqueness across batches.

Step 3 separately owns operator read-site wiring, typed payload reconstruction, proof of source
constraint coverage, and transaction completion/rollback. It must arrange validation of every
changed K against the complete after-state before accepting the transaction, and reject on
conflict or merge/read failure. Phase 3 establishes the helper's behavior; Phase 4 establishes
the storage views; neither phase establishes runtime transaction integration.

Advance to Phase 4 when the independent oracle agrees with accumulated reads and partial/full
compaction, replacements and conflicts behave as specified, errors propagate, existing tests
pass, and the implementation note records the module boundary and remaining integration work.
