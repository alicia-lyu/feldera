# Reconstruction (R3) and RF1/RF2 verification (R4)

R3 maintains `V = Customer ⋈ Orders ⋈ Lineitem` (TPC-H Q3 without predicates) without the four join integrator traces. Instead, the joins read requested key ranges from the Customer–Orders–Lineitem (COL) merged index. R4 checks the result against an independent evaluation.

Scope: loading and the refresh functions RF1 (insert complete order groups) and RF2 (delete them), with [one DBSP worker](decisions.md). Commands are in [WORKFLOW.md](WORKFLOW.md); the index layout is in [col-storage.md](col-storage.md); out-of-scope changes are in [deferred.md](deferred.md).

## Access path

An integrator is logical accumulation; a retained keyed trace supplies its accumulated output. Reconstruction substitutes the trace reads of the join with reads of the COL index, and stops the independent accumulation of the replaced traces. The reconstructed state must satisfy the reused join's contract: keys, weighted values, absence, ordering, snapshot visibility, and lifetime. With `B = Orders ⋈ Customer`, the generated deltas are (equations as in [CIRCUIT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md#reading-the-generated-program)):

```text
ΔB = ΔO ⋈ (C₀ + ΔC) + O₀ ⋈ ΔC
ΔV = ΔL ⋈ (B₀ + ΔB) + L₀ ⋈ ΔB
```

The COL keys nest customer, order, and line prefixes, so the four reads (`C` after, `O` before, `B` after, `L` before) are range reads by customer or by customer and order. Base storage, the index, materialized `V`, and remaining state are accounted for separately. The four read bindings and their types are in [native-state-read-contracts.md](native-state-read-contracts.md).

## Trace omission

`dyn_join_generic_with_snapshot_readers` in `crates/dbsp/src/operator/dynamic/join.rs` (the supplied-snapshot join; typed wrapper `join_with_snapshot_readers`) builds no integrate or delay trace for either input. It keeps the shard-and-accumulate stage for each delta and wires two `JoinTrace` operators. Each `JoinTrace` takes its state from a caller-supplied reader, called at flush, in place of a trace. The two "Empty ... state" streams only preserve binary-operator scheduling.

The reader contract is in the function's doc comment: the left reader returns right state after the current update; the right reader returns left state before it. The caller keeps the state behind each snapshot valid through the flush.

The generated Q3 circuit uses two such joins, which together replace four traces (see [native-state-read-contracts.md](native-state-read-contracts.md)). The `--q3-merged-index` option emits them (`Q3MergedIndexBinding.java`, `ToRustVisitor.java`; run `generate.sh --q3-merged-index`).

Q3 inserts nothing into the index. The readers in `merged-index-col/src/q3.rs` only read snapshots, and the module comment says so. The sole writer is `ManualIndex::append` in the harness (`q3-ivm-baseline/src/reconstruction.rs`), outside the circuit.

## Transaction contract

A DBSP transaction is `start_transaction`, any number of `step()` calls, then `commit_transaction` (`DBSPHandle` in `crates/dbsp/src/circuit/dbsp_handle.rs`). It is not an OLTP commit:

- There is no rollback and no isolation.
- Outputs appear at commit.
- The logical clock advances once per transaction.

The reconstructed joins call their readers once, at the commit flush. In `JoinTrace` (`join.rs`), a step that is not a flush yields an empty output and returns before any reader runs.

Per transaction, the harness does the following in order (`q3-ivm-baseline/src/main.rs`):

1. `prepare` the change: validate it (`BaseState::validate`, `workload.rs`) and build the index rows and the placement map (`reconstruction::prepare`).
2. `ManualIndex::append` the rows. It yields the `before` and `after` snapshots.
3. `Q3State::publish(before, after, placements)`.
4. Run the transaction.
5. `Q3State::finish()`.

`publish` returns an error if a previous publication has not been finished; `finish` returns an error if nothing was published (`merged-index-col/src/q3.rs`). Consequently several transactions cannot share one publication: each has its own before and after.

The test `published_views_cover_multiple_transaction_steps` (`q3-ivm-baseline/src/main.rs`) covers one transaction with two steps under a single publish: Orders in the first step, Lineitems in the second.

To batch more changes, use one larger transaction. Its full change set must be known before it starts, because the index is updated first.

The merged index is owned by `ManualIndex` on an auxiliary thread of the base runtime (beside the base tables, with the base buffer cache and storage directory), outside the Q3 graph. `append` builds the transaction's index rows, adds them to the index, and returns the immutable before and after snapshots, which `publish` hands to the readers together with the placement map. The index uses the native `Spine`. The harness saves it with the base circuit's prepared checkpoint UUID before committing the base circuit, so checkpoint dependency collection includes its batch files (test `checkpoint_restores_q3_and_manual_index_at_completed_boundary`). Independently stacked circuits and their checkpoint protocol are not implemented (see [deferred.md](deferred.md)).

## Placement map

The placement map (order → customer) tells the readers where to look in the index, whose keys begin with the customer. It is built by `reconstruction::prepare` from the Orders rows of the same transaction, and nothing else. It is held as `BTreeMap<i64, BTreeSet<i64>>`: each order maps to a set of candidate customers, and both temporal readers probe every candidate in their own snapshot. RF1/RF2 preparation supplies exactly one candidate per order. The sets exist for future workloads; the tests with several candidates are COL reader tests (below), not harness tests.

A requested order with a missing or empty candidate set is an error.

### Accepted limitation

Changes outside RF1/RF2 are unsupported by design, for example a Lineitem added to an existing order, a Customer update, a replacement, or a key move. See [decisions.md](decisions.md).

- `prepare` rejects a Lineitem whose Order is absent from the transaction, or whose direction differs from its Order's.
- `BaseState::validate` rejects duplicate, replacement, and key-moving changes. This is also what makes folded keys unique.
- A change that reached the circuit by another route would hit an `expect` in `merged-index-col/src/q3.rs` (for example "Q3 B+ reconstruction failed") and panic the worker.

## Cost of reconstruction

The readers decode the requested key ranges and copy them into temporary native batches that the existing join consumes. This copying, decoding, and any sorting is charged to Q3 maintenance. A direct reconstructed cursor could avoid the temporary rows; it is not implemented. The choice of container for the placement map is likewise open. No cost figure is claimed here; see [measurement.md](measurement.md).

## Verification (R4)

The test `verification::rf1_rf2_transcript` (`q3-ivm-baseline/src/verification.rs`) runs deterministic loading and RF1/RF2 transactions in both generated modes. Customers load first. The transactions cover several customers and orders, distinct lines with identical payloads, groups larger than the splitter boundary, repeated refresh cycles, empty transactions, and multi-step transactions. It checks that:

- every committed delta and materialized `V` equals the difference of `Reference::evaluate` before and after, an independent full join over primary-key maps;
- in reconstructed mode, the four reader outputs equal rows computed directly from the logical records, without index reads (`check_readers`).

`verify.sh` regenerates both modes, runs both suites, and compares the two JSON transcripts byte for byte. It also runs the COL suite, formatting, and Clippy with warnings denied, then restores native generation. Its logs and transcripts are written to the git-ignored `target/r4-verification/` and are local-only.

Other tests that bear on reconstruction:

| Test | Location | Covers |
| --- | --- | --- |
| `supplied_snapshot_join_matches_native_signed_updates` | `crates/dbsp/src/operator/dynamic/join.rs` | The supplied-snapshot join equals the native join on simultaneous and multi-step transactions |
| `Q3MergedIndexBindingTest` | `sql-to-dbsp-compiler/SQL-compiler` | The hook accepts the supported circuit and rejects others |
| `rf1_rf2_match_full_join` | `q3-ivm-baseline/src/main.rs` | RF1 and RF2 against the full join (reconstructed mode) |
| `rejects_missing_placement_and_incomplete_rf2` | same | The limitation above |
| `grouped_load_keeps_oversized_order_and_lines_together` | same | Grouped loading |
| `four_readers_preserve_weighted_before_after_relations` | `merged-index-col/src/q3.rs` | Reader outputs |
| `candidate_locations_follow_each_snapshot_without_duplicating_rows` | same | Several candidate customers per order |
| `requested_orders_require_nonempty_candidate_locations` | same | Missing or empty candidates |
| `publication_and_batch_local_placement_are_required` | same | `publish`/`finish` lifecycle |

Test counts and pass results from the earlier plans are not repeated; rerun `verify.sh` for current results.

Not covered: Customer updates, independent line changes, orphans, replacements, key moves, and general-update maintenance.
