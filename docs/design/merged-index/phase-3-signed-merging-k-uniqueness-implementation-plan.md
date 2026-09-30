# Phase 3 implementation plan: signed merging and K uniqueness

Date: 2026-09-30. Status: planned; implementation has not started.

The [Step 2 storage plan](folded-key-layer-file-plan.md#phase-3-signed-merging-and-k-uniqueness)
governs this phase. Build on the verified
[Phase 2 implementation](phase-2-implementation-note.md): immutable in-memory batches and a raw
cursor that preserves contributions from separate batches. Phase 3 adds signed consolidation,
exact-key accumulated reads, and a changed-key uniqueness helper. Keep these APIs crate-internal.

## Plan outline

1. **Settle arithmetic and reuse:** prefer Feldera's `CursorList` and native batch mergers;
   resolve their overflow-contract mismatch with a trace maintainer before implementation.
2. **Compose full-pair consolidation:** retain signed `(K, payload)` totals and suppress zeros
   using existing batches, cursors, and builders.
3. **Validate changed keys:** read the complete after-state for every changed K and report
   multiple surviving payloads through a storage-independent validation helper.
4. **Verify semantics:** use an independent signed-weight oracle for replacements, conflicts,
   cancellations, and partial/full compaction.
5. **Hand off to Phase 4:** record the approved arithmetic and reused APIs; Phase 4 owns files,
   bounded staging, append, and snapshots. Step 3 owns runtime acceptance and rollback.

## Human expert input

**H1 requires a decision before implementing consolidation.** The remaining rows distinguish
confirmation of the existing semantic contract from later storage and runtime integration.
These are review points for the plan; no implementation or policy approval is implied.

| ID and reviewer | Input needed | Timing and effect |
| --- | --- | --- |
| H1: DBSP trace maintainer | Approve native weight arithmetic, conservative checked rejection, or a specified wider-accumulator policy. Is merge-order-dependent rejection acceptable? | Before consolidation implementation; selects native reuse or the minimal checked fallback |
| H2: Source/runtime expert | Confirm that K uniqueness means at most one nonzero payload, and identify who enforces any `+1` multiplicity requirement | Confirm before using the validator as a source-validity guarantee; current storage contract checks survivor count only |
| H3: Storage/runtime expert | Establish valid-before-state provenance and complete changed-key coverage, including restore/bootstrap policy | Before relying on incremental validation in integration; Phase 3 tests state the preconditions explicitly |
| H4: Storage/transaction expert | Confirm the stable after-state view and the point where validation failure prevents transaction acceptance | During Phase 4/Step 3 planning; does not add snapshots or rollback to Phase 3 |

For H1, the recommended starting point is native reuse, with a documented arithmetic contract.
The existing checked builder is a concrete reason to require this decision: native merging
cannot simply be substituted while claiming the same overflow behavior. If checked rejection
remains mandatory, approve only the small missing arithmetic helper described below.

Ordering, exact-key seek behavior, pair cancellation, and current API signatures can be settled
by code inspection and tests. They do not require human policy decisions. Source constraints
may eventually discharge a runtime check, but removing validation requires an explicit proof
covering all source/update paths rather than an assumption based on upsert behavior.

## Governing semantics

Consolidation identity is the complete `(K, payload)` pair. Add its signed `ZWeight` contributions
and omit the pair only when its total is zero. The initial checked-arithmetic requirement is
subject to the explicit H1 decision below; it must not change implicitly during reuse. Preserve distinct payloads
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

## Reuse existing Feldera components

**Use existing batch, cursor, and builder machinery first.** The API audit found one material
mismatch: native aggregation does not expose the checked-overflow error promised by the initial
plan. Resolve review H1 below before selecting the consolidation path. Do not silently weaken
that promise or build a second general merge framework.

| Existing component | Phase 3 use | Constraint |
| --- | --- | --- |
| `VecIndexedWSet` / `MergedIndexBatch` | Retain current in-memory storage | File and fallback batch selection stays in Phase 4 |
| `BatchReader::cursor` and `Cursor` | Read, seek, and traverse existing batches | Native cursor operations do not expose a fallible read interface |
| `CursorList` | Preferred accumulated read engine: merge equal key/value pairs and skip zero totals | Uses generic weight addition, without checked-overflow errors |
| `merge_batches` / `merge_batches_by_reference` | Preferred compaction engine over selected batches | Same arithmetic decision as accumulated reads |
| `ListMerger` and existing batch builders | Reuse through native merge functions | Avoid a parallel output-builder abstraction |
| `RawBatchCursor` | Retain raw delta reads; supply ordered rows if checked aggregation is required | Already implements cross-batch ordering |
| `build_batch` | Retain input normalization and checked fallback output construction | Current checked policy must be reconciled with native merging |
| `FoldedKey`, `PayloadBytes`, source codecs | Reuse full-pair equality and byte ordering | No decoding needed for merge or uniqueness |

The concrete merge entry points are in
[`trace.rs`](../../../crates/dbsp/src/trace.rs); their implementation uses
[`ListMerger`](../../../crates/dbsp/src/trace/spine_async/list_merger.rs).
[`CursorList`](../../../crates/dbsp/src/trace/cursor/cursor_list.rs) already aggregates equal
key/value pairs and suppresses zero weights. Both aggregation paths use generic
[`Weight::add_assign`](../../../crates/dbsp/src/dynamic/weight.rs), while
[`ZWeight`](../../../crates/dbsp/src/algebra/zset.rs) is `i64`. These APIs do not return a checked
addition error. Input normalization alone cannot prove that later cross-batch sums will fit.

### Consolidation decision after expert review

The preferred reuse path is `CursorList` for accumulated reads and native batch merge functions
for compaction, provided the trace maintainer approves their arithmetic contract for this index.
Document the accepted overflow assumptions and their enforcement, and explicitly reconcile them
with the checked behavior of the Phase 2 builder. Checking the final native result after an
unchecked sum is not an overflow safeguard.

If checked rejection remains required, reuse `RawBatchCursor` for ordering and add only a small
full-pair accumulation loop with `checked_add`. Do not duplicate its multiway ordering algorithm.
Reuse `build_batch` to construct small in-memory compaction results after successful accumulation.
This is the fallback for a demonstrated arithmetic mismatch, not permission to introduce a new
batch, heap, cursor, or storage framework. Widened accumulation is a separate policy option for
H1; it must specify range checks and compaction behavior before implementation.

Use the selected arithmetic policy consistently for accumulated reads and compaction. Checked
accumulation may reject an intermediate sum even when the mathematical final sum fits, and
contribution order can affect that rejection. Tests of relation equivalence use bounded weights
whose intermediate sums fit under every tested grouping; dedicated tests cover the approved
boundary behavior.

### Module ownership and API scope

| Module | Responsibility | Reuse boundary |
| --- | --- | --- |
| `memory_batch.rs` | Existing batch construction and raw cursor | Retain Phase 2 storage and ordering |
| `signed_merge.rs` | Thin composition of the chosen Feldera read/merge facilities | Add custom arithmetic only if H1 requires it |
| `state.rs` | Distinct changed keys and uniqueness validation | Consume complete per-key accumulated results |
| `mod.rs` | Narrow crate-internal declarations and exports | Retain existing codecs and types |

Keep uniqueness policy separate from consolidation. The merger emits every distinct payload
with a nonzero total; `state.rs` determines whether there is more than one. No merge operation
chooses a winning payload or rejects a partial compaction merely for containing several payloads.

Use `CursorList` and ordinary DBSP cursor operations behind a small `read_source_state` helper
when the native path is selected. The checked fallback reads the same exact-K range from the
existing raw cursor and groups full pairs. The helper returns owned `(PayloadBytes, ZWeight)`
rows, preserving all survivors. Seek to K, compare the complete key bytes, and stop before
aggregating another key; shared prefixes are not equality. Keep borrowed cursor rows local to
this helper and copy a pair before advancing when accumulation requires ownership.

Make `validate_changed_keys` accept sorted distinct keys and a per-key read callback returning
`Result<Vec<(PayloadBytes, ZWeight)>, E>`. This ordinary function boundary lets Phase 3 use memory
batches and Phase 4 supply stable storage views without exposing batch ownership in the validator.
Preserve read errors separately from a structured conflict containing K and two distinct
nonzero payload/weight witnesses. A single key's materialized result is acceptable for this
phase's small workloads; bounded reads can be designed against actual Phase 4 requirements.

Collect candidate keys from the raw delta without summing weights across payloads. A delta
already consolidated by full pair is sufficient: dropping exactly canceled pairs cannot hide an
actual state change. Extra candidate keys, including keys from zero rows, are harmless.

**Do not introduce the previously proposed `SignedRowCursor` trait or `ConsolidatedCursor` as
Phase 3 prerequisites.** Existing Feldera cursor APIs should carry traversal. A future file I/O
error model must be derived from the selected native file and snapshot APIs in Phase 4, rather
than imposed through a speculative Phase 3 cursor abstraction.

The caller supplies the complete logical after-state: all before contributions plus delta.
The validation module does not import file readers, writers, spines, snapshot owners, transaction
objects, or source-specific definitions. Thin consolidation glue may reference existing DBSP
batch/cursor types; that reuse does not transfer file lifecycle ownership into Phase 3.

## Implementation sequence

1. **Resolve H1 and record the reuse choice.** Confirm the concrete native cursor and merge
   contracts, choose the arithmetic policy with the trace maintainer, and record any deviation
   from Phase 2 behavior. Resolve H2's source invariant before presenting this helper as a
   complete source-validity check. Keep the governing storage contract synchronized.
2. **Compose signed reads and compaction.** Use the selected existing facilities behind narrow
   crate-internal helpers. Preserve raw delta cursor behavior. Add a checked grouped loop only
   if the approved policy cannot be met by native consolidation.
3. **Add changed-key validation.** Extract distinct keys and validate complete after-state
   reads through the callback boundary. Return all survivors from the reader and explicit
   conflicts from the validator. No transaction publication or rollback belongs here.
4. **Prove semantics independently.** Compare signed rows and partial/full compaction against
   the map oracle. Test the chosen arithmetic contract and preserve existing Phase 1/2 coverage.
5. **Record the handoff.** Run the implementation checks and write a Phase 3 implementation
   note identifying reused APIs, any justified custom logic, expert decisions, and pending
   Phase 4/Step 3 integration. Update the umbrella status only after evidence passes.

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
| Arithmetic and failures | Approved overflow behavior; per-key reader errors propagate separately from conflicts |
| Basic state changes | Insert leaves one payload; complete delete leaves none; replacement leaves the new payload |
| Uniqueness | Two nonzero payloads fail, including an insert of a new payload without retracting the old one |
| Delta candidates | `old -1, new +1` retains K despite zero key-only weight; repeated K is deduplicated |
| Validation scope | Only requested changed keys are checked; tests document the valid-before precondition |
| Partial compaction | A subset merge may retain multiple payloads; adding untouched batches preserves the oracle state |
| Full compaction | All contributions for valid K reduce to zero or one payload and agree with accumulated reads |
| Validation independence | A small per-key callback tests validation without DBSP storage, including read errors |

Include several deterministic generated batch partitions and merge orders with bounded weights.
Exercise empty batches and cancellation split across batches. Use existing test dependencies;
no new property-testing framework is required for this phase.

Extend `crates/dbsp/src/trace/merged_index/tests.rs` for cross-module regression coverage; keep
small source-contract unit tests beside the new modules when they need private access.
For the future implementation gate, run `cargo fmt --check` and
`cargo test -p dbsp --lib trace::merged_index`, including the
existing Phase 1 and Phase 2 tests. Record exact commands, environment constraints, and results
in the implementation note.

For the current planning-only change, run Markdown lint, local link checks, and
`git diff --check`. Rust tests apply when implementation begins.

## Phase 4 and Step 3 handoff

| Phase 3 deliverable | Phase 4 responsibility |
| --- | --- |
| Reused native cursor/batch facilities and approved arithmetic | File/mixed batch selection and native merge integration |
| Full-pair semantic tests and any justified checked helper | Preserve arithmetic and surface errors through actual storage APIs |
| In-memory compaction evidence | File construction, incomplete-output cleanup, and reopen evidence |
| Changed-key and after-state validation helpers | Stable before, delta, and after storage views for callers |
| In-memory semantic evidence | Batch limits, bounded staging/spill, append once, and snapshot ownership |

Phase 4 chooses the concrete DBSP batch, append, and snapshot APIs. It should continue using native merge
facilities where they meet the approved full-pair and arithmetic contract. Any checked fallback
requires an explicit compatible compaction integration; acceptance of the native arithmetic
policy cannot be deferred until files are added. Physical grouping of a parent K and payload children is
insufficient to establish uniqueness across batches.

Step 3 separately owns operator read-site wiring, typed payload reconstruction, proof of source
constraint coverage, and transaction completion/rollback. It must arrange validation of every
changed K against the complete after-state before accepting the transaction, and reject on
conflict or merge/read failure. Phase 3 establishes the helper's behavior; Phase 4 establishes
the storage views; neither phase establishes runtime transaction integration.

Advance to Phase 4 when the independent oracle agrees with accumulated reads and partial/full
compaction, replacements and conflicts behave as specified, errors propagate, existing tests
pass, and the implementation note records the module boundary and remaining integration work.
