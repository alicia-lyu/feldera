# Reconstruction (R3)

R3 is the generic contract by which an IVM view circuit reads its partner state from a merged index instead of from join integrator traces. Its one instance is Q3: `V = Customer ⋈ Orders ⋈ Lineitem` (TPC-H Q3 without predicates) is maintained without the four join integrator traces, and the joins read requested key ranges from the Customer–Orders–Lineitem (COL) merged index. R4 checks the result against an independent evaluation; its tests and the general-delta rules are in the harness's [CLAUDE.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CLAUDE.md).

Scope: loading, the refresh functions RF1 (insert complete order groups) and RF2 (delete them), and general deltas of the base tables (since 2026-11-10), with [one DBSP worker](decisions.md). The rules that hold for any index and harness (transaction order, accounting, general deltas, the side store for rows with no owner) are in [experiments/CLAUDE.md](../../../sql-to-dbsp-compiler/experiments/CLAUDE.md); this file does not restate them. Commands are with their directory ([WORKFLOW.md](WORKFLOW.md) lists the cross-directory ones); the index layout is in [storage-layout.md](storage-layout.md); out-of-scope changes are in [deferred.md](deferred.md).

## Access path

An integrator is logical accumulation; a retained keyed trace supplies its accumulated output. Reconstruction substitutes the trace reads of the join with reads of the COL index, and stops the independent accumulation of the replaced traces. The reconstructed state must satisfy the reused join's contract: keys, weighted values, absence, ordering, snapshot visibility, and lifetime. With `B = Orders ⋈ Customer`, the generated deltas are (equations as in [CIRCUIT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md#reading-the-generated-program)):

```text
ΔB = ΔO ⋈ (C₀ + ΔC) + O₀ ⋈ ΔC
ΔV = ΔL ⋈ (B₀ + ΔB) + L₀ ⋈ ΔB
```

The COL keys nest customer, order, and line prefixes, so the four reads (`C` after, `O` before, `B` after, `L` before) are path or range reads of a touched customer's group. Base storage, the index, materialized `V`, and remaining state are accounted for separately. The four read bindings and their types are in [CIRCUIT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md).

## Trace omission

`dyn_join_generic_with_snapshot_readers` in `crates/dbsp/src/operator/dynamic/join.rs` (the supplied-snapshot join; typed wrapper `join_with_snapshot_readers`) builds no integrate or delay trace for either input. It keeps the shard-and-accumulate stage for each delta and wires two `JoinTrace` operators. Each `JoinTrace` takes its state from a caller-supplied reader, called at flush, in place of a trace. The two "Empty ... state" streams only preserve binary-operator scheduling.

The reader contract is in the function's doc comment: the left reader returns right state after the current update; the right reader returns left state before it. The caller keeps the state behind each snapshot valid through the flush.

The reconstructed Q3 circuit uses two such joins, which together replace four traces (see [CIRCUIT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md)). Since 2026-11-10 the circuit is hand-written (`q3-ivm-baseline/q3-trie-view-circuit`): `J` joins on `(custkey)` and keeps `(custkey, orderkey)`, and `V` joins on `(custkey, orderkey)`, so each join's key is on the trie (A1). The compiler hook `--q3-merged-index` that used to emit the circuit was removed on 2026-11-13.

Q3 inserts nothing into the index. The readers in `merged-index-col/src/q3.rs` only keep the entries that the scan session routes to them, and the module comment says so. The sole writer is `ManualIndex::append` in the harness (`q3-ivm-baseline/src/reconstruction.rs`), outside the circuit.

## Transaction contract

A DBSP transaction is `start_transaction`, any number of `step()` calls, then `commit_transaction` (`DBSPHandle` in `crates/dbsp/src/circuit/dbsp_handle.rs`). It is not an OLTP commit:

- There is no rollback and no isolation.
- Outputs appear at commit.
- The logical clock advances once per transaction.

The reconstructed joins call their readers once, at the commit flush, and the readers hand over the requested slices that the session filled before the transaction. In `JoinTrace` (`join.rs`), a step that is not a flush yields an empty output and returns before any reader runs.

Per transaction, a harness does the following in order. The steps are generic; the names are Q3's (`q3-ivm-baseline/src/main.rs`; another index would have its own publish, collect, and finish):

1. Run the base transaction, then `prepare` its net delta into the complete rows of `Dlt` (`reconstruction::prepare`; see [General deltas](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CLAUDE.md)). The CLI drivers' inputs are first checked by `BaseState::validate` (`workload.rs`).
2. `ManualIndex::append` the rows. It yields `Old`, the snapshot before the append, and `Dlt`, the appended run. `prepare` and the append are timed as `index_ms`, inside `base_ms`.
3. `Q3State::publish(old, dlt)`, then `Q3State::collect(pass)`: one scan session fills the typed readers. Before reads take `Old`'s batch; after reads take `Old`'s and `Dlt`'s, unconsolidated (`prop:overlay`).
4. The driving deltas that `Q3State::collect` returned go to the view circuit's `add_input_stream` handles (`Pipeline::build_view_delta`), timed in `delta_ms`. Then run the view transaction; at its commit, the joins read the readers' batches.
5. `Q3State::finish()` releases the readers and returns the session's reads.

`publish` returns an error if a previous publication has not been finished; `finish` returns an error if nothing was published (`merged-index-col/src/q3.rs`). Consequently several transactions cannot share one publication: each has its own `Old` and `Dlt`.

The test `published_views_cover_multiple_transaction_steps` (`q3-ivm-baseline/src/main.rs`) covers one transaction with two steps under a single publish: Orders in the first step, Lineitems in the second.

To batch more changes, use one larger transaction. Its full change set must be known before it starts, because the index is updated first.

The merged index is owned by `ManualIndex` on an auxiliary thread of the runtime that hosts the base and view circuits (with that runtime's buffer cache and storage directory, which the index readers share), outside the Q3 graph. `append` builds the transaction's index rows, adds them to the index, and returns `Old` and `Dlt`, which `publish` hands to the session. The index uses the native `Spine`. `ManualIndex::save` and `restore` can take a checkpoint UUID, but the harness no longer checkpoints: one runtime hosts both circuits, `init_circuits` rejects an initial checkpoint for several circuits, and the checkpoint test of the two-runtime pipeline was removed on 2026-11-05. Independently stacked circuits and their checkpoint protocol are not implemented (see [deferred.md](deferred.md)).

## General deltas and verification

The case table for general deltas, the `OrphanLines` side store, the `BaseState` and `Reference` owner lookups, and the R4 verification table (tests and transcript comparison) are in [q3-ivm-baseline/CLAUDE.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CLAUDE.md). The rule that every delta row carries its trie keys, and that a request outside a reader's requests is a broken contract, is in [experiments/CLAUDE.md](../../../sql-to-dbsp-compiler/experiments/CLAUDE.md).

## Cost of reconstruction

The collection reads `Dlt` once, builds the driving deltas once, scans each touched group of `Old` once, and unfolds each kept entry once, straight into its reader's batch; the algorithm and its costs are in [shared-traversal.md](shared-traversal.md#the-implementation-selective-routing). The partner batches are the requested slices of the index, and the routing only borrows the driving deltas, which move to the circuit's input handles. This copying and decoding is charged to Q3 maintenance. No cost figure is claimed here; see [MEASUREMENT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/MEASUREMENT.md).
