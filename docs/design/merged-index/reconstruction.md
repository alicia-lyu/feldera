# Reconstruction (R3) and verification (R4)

R3 maintains `V = Customer ⋈ Orders ⋈ Lineitem` (TPC-H Q3 without predicates) without the four join integrator traces. Instead, the joins read requested key ranges from the Customer–Orders–Lineitem (COL) merged index. R4 checks the result against an independent evaluation.

Scope: loading, the refresh functions RF1 (insert complete order groups) and RF2 (delete them), and general deltas of the base tables (since 2026-11-10), with [one DBSP worker](decisions.md). Commands are in [WORKFLOW.md](WORKFLOW.md); the index layout is in [col-storage.md](col-storage.md); out-of-scope changes are in [deferred.md](deferred.md).

## Access path

An integrator is logical accumulation; a retained keyed trace supplies its accumulated output. Reconstruction substitutes the trace reads of the join with reads of the COL index, and stops the independent accumulation of the replaced traces. The reconstructed state must satisfy the reused join's contract: keys, weighted values, absence, ordering, snapshot visibility, and lifetime. With `B = Orders ⋈ Customer`, the generated deltas are (equations as in [CIRCUIT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md#reading-the-generated-program)):

```text
ΔB = ΔO ⋈ (C₀ + ΔC) + O₀ ⋈ ΔC
ΔV = ΔL ⋈ (B₀ + ΔB) + L₀ ⋈ ΔB
```

The COL keys nest customer, order, and line prefixes, so the four reads (`C` after, `O` before, `B` after, `L` before) are path or range reads of a touched customer's group. Base storage, the index, materialized `V`, and remaining state are accounted for separately. The four read bindings and their types are in [native-state-read-contracts.md](native-state-read-contracts.md).

## Trace omission

`dyn_join_generic_with_snapshot_readers` in `crates/dbsp/src/operator/dynamic/join.rs` (the supplied-snapshot join; typed wrapper `join_with_snapshot_readers`) builds no integrate or delay trace for either input. It keeps the shard-and-accumulate stage for each delta and wires two `JoinTrace` operators. Each `JoinTrace` takes its state from a caller-supplied reader, called at flush, in place of a trace. The two "Empty ... state" streams only preserve binary-operator scheduling.

The reader contract is in the function's doc comment: the left reader returns right state after the current update; the right reader returns left state before it. The caller keeps the state behind each snapshot valid through the flush.

The reconstructed Q3 circuit uses two such joins, which together replace four traces (see [native-state-read-contracts.md](native-state-read-contracts.md)). Since 2026-11-10 the circuit is hand-written (`q3-ivm-baseline/q3-trie-view-circuit`): `J` joins on `(custkey)` and keeps `(custkey, orderkey)`, and `V` joins on `(custkey, orderkey)`, so each join's key is on the trie (A1). The compiler hook `--q3-merged-index` (`Q3MergedIndexBinding.java`, `ToRustVisitor.java`) that used to emit the circuit stays in Java, unused.

Q3 inserts nothing into the index. The readers in `merged-index-col/src/q3.rs` only read the buffers of the scan session, and the module comment says so. The sole writer is `ManualIndex::append` in the harness (`q3-ivm-baseline/src/reconstruction.rs`), outside the circuit.

## Transaction contract

A DBSP transaction is `start_transaction`, any number of `step()` calls, then `commit_transaction` (`DBSPHandle` in `crates/dbsp/src/circuit/dbsp_handle.rs`). It is not an OLTP commit:

- There is no rollback and no isolation.
- Outputs appear at commit.
- The logical clock advances once per transaction.

The reconstructed joins call their readers once, at the commit flush, and the readers hand over the requested slices that the session filled before the transaction. In `JoinTrace` (`join.rs`), a step that is not a flush yields an empty output and returns before any reader runs.

Per transaction, the harness does the following in order (`q3-ivm-baseline/src/main.rs`):

1. Run the base transaction, then `prepare` its net delta into the complete rows of `Dlt` (`reconstruction::prepare`; see [General deltas](#general-deltas)). The CLI drivers' inputs are first checked by `BaseState::validate` (`workload.rs`).
2. `ManualIndex::append` the rows. It yields `Old`, the snapshot before the append, and `Dlt`, the appended run. `prepare` and the append are timed as `index_ms`, inside `base_ms`.
3. `Q3State::publish(old, dlt)`, then `Q3State::collect(pass)`: one scan session fills the typed readers. Before reads take `Old`'s batch; after reads take `Old`'s and `Dlt`'s, unconsolidated (`prop:overlay`).
4. `Q3State::take_driving_deltas` hands the readers' `Dlt` batches to the view circuit's `add_input_stream` handles, timed in `forward_ms`. Then run the view transaction; at its commit, the joins read the readers' batches.
5. `Q3State::finish()` releases the readers and returns the session's reads.

`publish` returns an error if a previous publication has not been finished; `finish` returns an error if nothing was published (`merged-index-col/src/q3.rs`). Consequently several transactions cannot share one publication: each has its own `Old` and `Dlt`.

The test `published_views_cover_multiple_transaction_steps` (`q3-ivm-baseline/src/main.rs`) covers one transaction with two steps under a single publish: Orders in the first step, Lineitems in the second.

To batch more changes, use one larger transaction. Its full change set must be known before it starts, because the index is updated first.

The merged index is owned by `ManualIndex` on an auxiliary thread of the runtime that hosts the base and view circuits (with that runtime's buffer cache and storage directory, which the index readers share), outside the Q3 graph. `append` builds the transaction's index rows, adds them to the index, and returns `Old` and `Dlt`, which `publish` hands to the session. The index uses the native `Spine`. `ManualIndex::save` and `restore` can take a checkpoint UUID, but the harness no longer checkpoints: one runtime hosts both circuits, `init_circuits` rejects an initial checkpoint for several circuits, and the checkpoint test of the two-runtime pipeline was removed on 2026-11-05. Independently stacked circuits and their checkpoint protocol are not implemented (see [deferred.md](deferred.md)).

## General deltas

Every delta row the view gets must carry its folded key (A2). A line item folds under its order's customer, so `reconstruction::prepare` gives every changed order a before owner `cb` and an after owner `ca`, each a customer or none. They come from the net delta's deletion and insertion of the order, or else from the base `Orders` state, which the transaction left unchanged (`BaseState` taken before the transaction in the driver, `Reference` in tests; the reads count under `BaseRelation`). The order's lines before are `L⁻`, read from `Old` at `(cb, order)` or, with no owner, from `OrphanLines`. The lines after are `L⁺ = L⁻ + ΔL`.

| Case | `Dlt` line rows | Side store |
| --- | --- | --- |
| same owner `c` | `ΔL` at `c` | unchanged |
| the order moves | `−L⁻` at `cb`, `+L⁺` at `ca` | unchanged |
| the order is deleted, lines kept | `−L⁻` at `cb` | order ↦ `L⁺` |
| the order arrives for orphan lines | `+L⁺` at `ca` | remove the order |
| orphan lines only | none | order ↦ `L⁺` |

Customers and orders fold from the net delta alone: an order folds at its own `o_custkey`, so customers need no side store. `OrphanLines` is part of base maintenance and sits beside `ManualIndex` as `Pipeline.orphans`. Native accepts orphan lines and never joins them. `prepare` rejects only a net delta that contradicts the state.

`BaseState::validate` stays an RF1/RF2 check of the CLI drivers' inputs and is not where support is decided. A request outside a reader's requests (the keys of its driving delta, which `capture` and the routing build from `Dlt`) is a broken contract: the partner read returns an error and the join's `expect` in `merged-index-col/src/q3.rs` panics the worker. Folded keys stay unique because of the base tables' primary keys.

## Cost of reconstruction

The collection reads `Dlt` once, builds the driving deltas `ΔC`, `ΔO`, and `ΔL` once each, and each reader's requests (`C⁺` requests the customers of `ΔO`, `O⁻` those of `ΔC`, `J⁺` the nodes of `ΔL`), and scans each touched customer's group of `Old` once, reading and decoding only the entries that some reader keeps; each kept entry is unfolded once, straight into its reader's batch (roadmap step 5 of [shared-traversal.md](shared-traversal.md)). The partner batches are the requested slices of the index: `C⁺` holds `Old`'s row and `ΔC`'s rows of the customers of `ΔO`, `O⁻` the orders of the customers of `ΔC`, `J⁺` the joined orders at the nodes of `ΔL` (built at routing), and `L⁻` the lines at the nodes where `ΔJ` has a row. The driving deltas are moved to the circuit's input handles; the routing only borrows them, and nothing is cloned. This copying and decoding is charged to Q3 maintenance. No cost figure is claimed here; see [measurement.md](measurement.md).

## Verification (R4)

The tests `verification::rf1_rf2_transcript` and `verification::general_transcript` (`q3-ivm-baseline/src/verification.rs`) run deterministic loading and transactions in both modes. The first covers RF1/RF2. The second covers general deltas: customer updates, line items added to existing orders, orders that change customer, deleted orders with kept lines, and orphans. The description below is of the first. Customers load first. The transactions cover several customers and orders, distinct lines with identical payloads, groups larger than the splitter boundary, repeated refresh cycles, empty transactions, and multi-step transactions. It checks that:

- every committed delta and materialized `V` equals the difference of `Reference::evaluate` before and after, an independent full join over primary-key maps;
- in reconstructed mode, the four reader outputs equal rows computed directly from the logical records, without index reads (`check_readers`).

`verify.sh` runs both suites and compares the two pairs of JSON transcripts byte for byte (`rf1_rf2_transcript`; `general_transcript`, written to `$Q3_GENERAL_TRANSCRIPT`). It also runs the COL suite, formatting, and Clippy with warnings denied. Its logs and transcripts are written to the git-ignored `target/r4-verification/` and are local-only.

Other tests that bear on reconstruction:

| Test | Location | Covers |
| --- | --- | --- |
| `supplied_snapshot_join_matches_native_signed_updates` | `crates/dbsp/src/operator/dynamic/join.rs` | The supplied-snapshot join equals the native join on simultaneous and multi-step transactions |
| `readers_match_a_brute_force` (and the other `merged_index::reader` tests) | `crates/dbsp/src/trace/merged_index/reader/tests.rs` | The driving deltas and requests of `capture`, the routing of kept entries by folded key, the `RowBuilder`, and one decode for an entry that two readers keep |
| `Q3MergedIndexBindingTest` | `sql-to-dbsp-compiler/SQL-compiler` | The hook accepts the supported circuit and rejects others |
| `rf1_rf2_match_full_join` | `q3-ivm-baseline/src/main.rs` | RF1 and RF2 against the full join (reconstructed mode) |
| `native_deltas_match_full_join_across_updates` | same (both builds) | Customer-only and line-only changes, an order that moves, and an orphan line, against the full join |
| `random_general_changes_match_the_reference_join` | same | Random general changes against the reference join |
| `lines_of_an_order_with_one_owner_fold_from_the_delta_alone`, `a_moved_order_takes_its_old_and_new_lines_with_it`, `a_deleted_order_leaves_its_kept_lines_in_the_side_store`, `an_arriving_order_takes_the_orphan_lines`, `a_net_delta_that_contradicts_the_state_is_rejected` | `q3-ivm-baseline/src/reconstruction.rs` | One for each row of the case table above, and the one rejection |
| `deltas_beyond_rf1_rf2_match_the_brute_force_join` | `q3-trie-view-circuit/src/tests.rs` | The hand-written circuit against a brute-force join |
| `grouped_load_keeps_oversized_order_and_lines_together` | same | Grouped loading |
| `four_readers_preserve_weighted_before_after_relations` | `merged-index-col/src/q3/tests.rs` | Reader outputs |
| `readers_match_a_brute_force` | `merged-index-col/src/q3/tests.rs` | The four partner reads against a brute force (property test) |
| `publication_collection_and_release_follow_one_order` | same | `publish`/`collect`/`finish` lifecycle |

Test counts and pass results from the earlier plans are not repeated; rerun `verify.sh` for current results.

Not covered: more than one worker, and checkpoints of a runtime with several circuits.
