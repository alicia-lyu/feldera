# Maintenance measurement (R5)

R5 compares native and reconstructed maintenance of `V = Customer ⋈ Orders ⋈ Lineitem` under identical RF1/RF2 refreshes with file-backed maintained state. Reconstructed mode reads the Customer–Orders–Lineitem (COL) merged index in place of four traces (see [reconstruction.md](reconstruction.md)).

This file defines what is measured and what remains pending. Commands (the runner, input generation, the pilot) are in [WORKFLOW.md](WORKFLOW.md). Numbers are in [RESULTS.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/RESULTS.md); their raw artifacts are local-only, in git-ignored `target/` directories. Following [decisions.md](decisions.md), nothing here infers an unmeasured quantity.

## Status

- Harness, runner, and macOS smoke runs (measurement version 2) are done.
- macOS runs are smoke tests only. Their highest RSS (43.2 MiB) is far below the 85% pressure threshold of the 512 MiB runtime target (435.2 MiB), so they show neither memory pressure nor beyond-memory behavior.
- The smoke runs' write and storage gap is explained down to record size; see [the investigation](#write-and-storage-investigation-2026-10-14). The merged index still stores more bytes than the four join traces it replaces.
- Pending: the Linux pilot, pressure instrumentation, and the three-repetition campaign, all below. No input-preparation helper exists for the campaign.

## Definitions

- **Setup versus measured.** Initial loading is setup and is excluded. The measured sequence is the refreshes only, from the first refresh to the last refresh's publication and cleanup.
- **Completion boundary** (version 2). `completion_boundary=last_refresh_complete`. There is no forced compaction, no final compaction wait, and no extra publication transaction. Loading waits only for naturally eligible background merges, then publishes once with an empty transaction. Version 1 forced a final compaction; its results are historical and not comparable (see RESULTS.md).
- **Refresh deltas stay whole.** Each refresh is complete in RAM. An oversized legitimate refresh must fail rather than be split to fit a budget.
- **Durability.** Refresh completion implies none. Checkpoint costs are deferred.
- **Equality of inputs.** Both modes receive the same logical refreshes; the runner records input and executable SHA-256 hashes in a manifest (`measure.py`).
- **Excluded.** Independent full-join validation runs only on request (`--no-verify` omits it) and is outside the measured interval. Reconstruction decoding, copying, sorting, spill, and output cleanup remain inside their real intervals.

Per-refresh phases (`discovery`, `checks_input_preparation`, `index`, `q3`, `publication_cleanup`, `combined_maintenance = index + q3`), sequence wall time and CPU, and the storage, RSS, and cache columns are defined in RESULTS.md beside their values. Backend read and write counters are storage-backend operations, not OS I/O. OS I/O is available only through cgroup `io.stat` on Linux.

## RF2 discovery

The driver keeps one `BaseState` of shared Catalog batch snapshots and replaces it after each transaction (`CatalogHandle::concat` consumes its output mailbox, so a second copy of the relations is not kept). RF2 order IDs are resolved from the current maintained Orders snapshot. Complete deletion rows come from native Lineitem primary-key ranges, or from the merged index's `(custkey, orderkey)` line range in reconstructed mode. Requested IDs are deduplicated; absent Orders are ignored and counted. Discovery never rescans base files or replays refreshes.

## Buffer cache

The driver's snapshot reads and the manual-index auxiliary thread would otherwise use the runtime's fixed 256 MiB fallback cache, outside `StorageOptions.cache_mib`. They run inside `Runtime::with_buffer_cache` (`crates/dbsp/src/circuit/runtime.rs`), bound to the configured worker cache, so the reported cache capacity covers driver reads, Q3 workers, and the manual index. No new cache is created.

## Report fields

The JSON report (`q3-ivm-baseline/src/main.rs`) records:

- `measurement_version` (2), `completion_boundary` (`last_refresh_complete`), and `compaction_policy` (`no forced compaction; loading waits for normal background merges only; no final wait`, or with `--setup-full-compaction`, `setup full compaction after loading, before the refreshes; no compaction requested after any refresh; no final wait`);
- `configuration.setup_full_compaction` (boolean) and `load.setup_full_compaction`: `null` by default; with the flag, `wait_for_compaction_ms`, `elapsed_ms` (request full compaction plus wait for compaction to finish), and a `resources` block with that setup's writes by owner. `load.background_merge_wait_ms` excludes this time;
- `sequence.snapshot_publication_transactions` (0; the loading phase reports 1);
- `load.background_merge_wait_ms` and `load.observed_storage_bytes` (loading only, not part of the refresh sequence);
- `sequence.observed_storage_bytes`, taken after the last refresh without a compaction wait, so background merges may still be active; it is not a settled size;
- per Catalog relation: `snapshot_batch_count`, `memory_batch_count`, `storage_batch_count`, `file_backed`; and `manual_index_storage` for the auxiliary index. Memory batches are permitted, so `file_backed` does not mean every batch is on disk;
- OS I/O counters, `null` where unavailable (macOS);
- `backend_write_by_owner` in every `resources` block (each refresh and the sequence): write operations and bytes per owner, keyed `query`, `base_relation` and `merged_index`. The owners sum to `backend_write_operations` and `backend_write_bytes`, which are unchanged. A file's owner is fixed when its writer is created, and a spine's owner when the spine is created; a background compaction runs under its spine's owner. `query` covers join traces, V and query execution; `base_relation` covers the input-map state and the base tables' delta accumulators; `merged_index` covers `ManualIndex`. A native run reports no `merged_index` writes. `measure.py` rejects a report whose owners do not sum to the totals;
- `sequence.window_started_micros` and `sequence.window_finished_micros` (Unix-epoch microseconds), which place the flush and compaction logs of `--diagnostics` runs inside or outside the window; and `configuration.diagnostics`, the diagnostics directory or `null`.

Process CPU includes natural background merges inside the interval. Reconstruction's temporary-batch decoding, copying, and sorting are charged to `q3_ms`.

## Workloads

Three macOS smoke workloads exist, each starting from 64 Customers, 512 Orders, and 2,048 Lines (table in RESULTS.md): clustered (16 Orders on one Customer per RF1), scattered (16 Orders on 16 Customers), and large-group (one Order with 10,000 Lines). Each runs the sequence RF1, RF2, repeated RF2 (absent Orders), RF1 reinsertion, RF2, empty RF2.

## Pending: Linux campaign

macOS gives no enforced memory limit, so qualification needs Linux. The runner (`measure.py`) already supports `--cgroup-root` and `--total-memory-bytes`: it creates a cgroup v2 child per run with `memory.max` set, `memory.swap.max=0`, copies the input files inside that cgroup before loading, and records `memory.peak`, `memory.events`, and `io.stat`.

Plan (not yet executed):

1. **Inputs.** SF 10 dbgen: `dbgen -s 10 -T c`, `-T o`, and `-U 3` (about 1.5 million Customers, 15 million Orders, 60 million Lines; each RF1 about 15,000 Orders and 60,000 Lines). The sizes are the dbgen scale-factor figures and are unchecked here.
2. **RF2 deletions.** Three sets of 15,000 distinct existing base Orders, never reused across pairs. Clustered: drawn from the first 7,500 Customers' range. Scattered: fixed-seed uniform sample of existing Order rows (record the seed), not of integers in the sparse key range. Pair each with RF1 inputs of the same complete Order and Lineitem groups, mapped to a narrow or broad Customer range respectively. Keep Orders ascending and Lineitems grouped by Order.
3. **Settings.** Fresh process, storage directory, and cgroup per run. `memory.max` 1 GiB, swap 0, cache 128 MiB, runtime RSS target 512 MiB, one worker, 10,000-row loading target, three repetitions; identical across modes. Input hashes must match between modes.
4. **Pilot first** (one repetition, per workload). Require `load.observed_storage_bytes > memory.max` in both modes, aiming at roughly four times the limit. This checks stored-state scale only. Do not substitute transient peak disk size, and do not treat it as proof of memory pressure.
5. **Campaign** (`--runs 3`, fresh destinations) only after both pilots pass and the pressure instrumentation below exists.

On Linux, the storage cache reads state with `O_DIRECT`, while ordinary input reads use the page cache and count against the cgroup; this is from the earlier plan and unverified against current code.

## Pending: pressure evidence

The runner records cgroup limits, peak, events, and I/O. It does not sample runtime memory-pressure levels or maintenance-only reclaim and refault counters (no such code in `measure.py` or `main.rs`). Before any claim of pressure or reclaim:

- Sample `Runtime::current_memory_pressure()` through the refresh sequence and keep each level with its interval.
- Snapshot cgroup `memory.current`, `memory.events`, `memory.stat` reclaim and refault counters, and `memory.pressure` immediately before and after the measured maintenance. Report the deltas apart from loading and input preparation.
- Keep cgroup peak and `io.stat` as run-level evidence. The enforced limit covers the driver, runtime, auxiliary threads, configured cache, and cgroup-charged input page cache together.
- As a probe, repeat a pilot with a 192 MiB runtime target and check that the samples show the intended levels. An optional delta stress case concatenates eight complete RF1 sets in RAM; a run that exceeds its budget must fail as one complete refresh.

Until these are recorded in RESULTS.md, with raw artifacts, make no memory-pressure, reclaim, or beyond-memory claim.

## Write and storage investigation (2026-10-14)

In the macOS smoke runs, reconstructed mode wrote more and stored more than native mode, although it drops four join traces. This section records why, from direct evidence. Raw artifacts are local and git-ignored: `target/r5-diagnostics/` (before the fixes), `target/r5-posix-fix/`, and `target/r5-posix-fix-setup-compaction/`. Each holds the three workloads, three repetitions per mode, with `--diagnostics`.

### Terms

- **Batch:** the delta of one transaction. **Run:** an immutable sorted collection in a Spine, in memory or on disk; `memory_batch_count` and similar code fields count runs.
- **Compaction:** LSM compaction. Background compaction starts when a Spine level reaches 8 runs (`MIN_LEVEL0_MERGE_BATCHES`). A requested full compaction is `DBSPHandle::start_compaction`, or `initiate_compaction` on the merged index's Spine.
- **Base-relation state:** the base tables stored by primary key (Feldera's "input map").
- **Operations**, named separately: finish a transaction, request full compaction, wait for compaction to finish, publish snapshots, release snapshots.

### State kept by each mode

Only the joins differ. Both modes keep base-relation state (`add_input_map_persistent`), the delta accumulators (`dyn_shard_accumulate`), and V (`register_materialized_output_zset_persistent`). Native `dyn_join_generic` builds four join traces with `dyn_shard_accumulate_trace`. Reconstructed `dyn_join_generic_with_snapshot_readers` builds none; it reads the merged index, one Spine that `ManualIndex::new` creates on its auxiliary thread.

### Instruments

- **Writes by owner.** Each file writer records an owner when it is created: `query` (join traces, V, query execution), `base_relation`, or `merged_index`. A Spine's owner is set at creation and carried into its background compaction. The owners always sum to the backend totals (tests in `crates/dbsp/tests/write_attribution_*.rs`, check in `measure.py`).
- **`--diagnostics`.** A flush log, a compaction log, and each Spine's run contents at the end of the window (see [WORKFLOW.md](WORKFLOW.md)). In all runs the flush log matches the counters, and every run's block bytes sum to its file size. Turning diagnostics on leaves large-group's totals unchanged.

The logs name Spines by circuit node. Matching record counts and block bytes identifies them: in native mode, `AccumulateZ1Trace nn42`, `nn37`, `nn56`, and `nn51` are the Customer, Orders, Orders ⋈ Customer, and Lineitem join traces, and `nn65` is V. In reconstructed mode, `nn51` is V.

### Cause 1: the POSIX writer flushed every block after the first MiB (fixed)

- **Evidence.** `VectoredWrite::flush` (`posixio_impl.rs`) cleared its buffers but not its byte count, so once a file had buffered 1 MiB, every later block flushed on its own. In large-group, the merged index's 1,590,272-byte compaction output took one 1 MiB flush plus 68 small ones; every other file took one flush.
- **Fix.** Reset the count on flush (`f7fec2391`, an upstream DBSP change). Regression test: `crates/dbsp/tests/posix_flush_threshold.rs` (385 flushes before, 3 after).
- **Effect.** Large-group reconstructed writes fall from 72 operations to 5; the merged index's from 69 to 2. Bytes are unchanged (2,171,392 B). Native is unchanged (6 operations, 765,952 B).

### Cause 2: the merged index reached the compaction threshold one refresh earlier

- **Evidence.** Loading is one Customer transaction and six Order and Lineitem transactions. The merged index takes a run from all 7; each native trace from at most 6. At the first refresh, the merged index reaches 8 runs and compacts: in large-group, 12,625 records in and 12,625 out, all written to disk, including the 10,000 RF1 Lines that RF2 then deletes. Native traces reach 8 runs one refresh later, so their compaction cancels RF1 against RF2 (22,048 records in, 2,048 out).
- **Consequence.** Writes inside the window depend on where each Spine stands against the threshold when the window opens, not on how much state it keeps. In clustered and scattered, native compactions sometimes finish after the window closes, which is why those results varied between repetitions.
- **Treatment.** `--setup-full-compaction` (`035eeab8e`) starts both modes from one run per Spine: after loading, request full compaction, wait for compaction to finish, then publish and start the refreshes. No compaction is requested after any refresh. The loading itself is unchanged: the merged index takes a run from every transaction that touches any of its tables, so it reaches the threshold sooner by design.

### Results after both changes (observations; all repetitions identical)

| | Native | Reconstructed |
| --- | --- | --- |
| Setup compaction writes, base relation | 3 ops, 206,848 B | 3 ops, 206,848 B |
| Setup compaction writes, query | 5 ops, 471,552 B | 1 op, 282,624 B (V) |
| Setup compaction writes, merged index | 0 | 1 op, 317,440 B |
| Join state on disk (four join traces, or the merged index) | 188,928 B for 3,136 rows | 317,440 B for 2,624 rows |
| Writes inside the window, all owners | 0 | 0 |
| Disk usage at window end | 678,400 B | 806,912 B |

The six refreshes stay in memory in both modes, so the window shows no writes. That is not evidence of a disk saving.

### Remaining gap: record size (unresolved)

With equal compaction and the POSIX fix, the merged index holds fewer rows than the four join traces (2,624 against 3,136, because it does not repeat Orders ⋈ Customer) but takes 1.68 times the bytes. Data blocks dominate in both. Per record, the merged index costs about 121 B; in large-group, each additional Lineitem record costs about 123 B. The native Lineitem join trace costs 69 B per row (141,312 B for 2,048 rows).

Deduction, not measured: the merged index's Spine stores keys and values as `Vec<u8>` (`OrdIndexedWSetFactories<DynData, DynData, DynZWeight>` over `Vec<u8>`, in `ManualIndex::new`). Each record therefore carries a 25-byte folded key and an `rkyv`-encoded payload (`PayloadBytes`), each inside a byte vector with its own length and alignment, where native traces store typed tuples. Attributing the 54-byte difference to these parts needs a measurement that varies the encoding.

## Deferred

Broader updates, residual Q3 (see [deferred.md](deferred.md)), periodic checkpoint measurements, and Q5/Q10.
