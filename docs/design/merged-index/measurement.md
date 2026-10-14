# Maintenance measurement (R5)

R5 compares native and reconstructed maintenance of `V = Customer ⋈ Orders ⋈ Lineitem` under identical RF1/RF2 refreshes with file-backed maintained state. Reconstructed mode reads the Customer–Orders–Lineitem (COL) merged index in place of four traces (see [reconstruction.md](reconstruction.md)).

This file defines what is measured and what remains pending. Commands (the runner, input generation, the pilot) are in [WORKFLOW.md](WORKFLOW.md). Numbers are in [RESULTS.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/RESULTS.md); their raw artifacts are local-only, in git-ignored `target/` directories. Following [decisions.md](decisions.md), nothing here infers an unmeasured quantity.

## Status

- Harness, runner, and macOS smoke runs (measurement version 2) are done.
- macOS runs are smoke tests only. Their highest RSS (43.2 MiB) is far below the 85% pressure threshold of the 512 MiB runtime target (435.2 MiB), so they show neither memory pressure nor beyond-memory behavior.
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

- `measurement_version` (2), `completion_boundary` (`last_refresh_complete`), and `compaction_policy` (`no forced compaction; loading waits for normal background merges only; no final wait`);
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

## Deferred

Broader updates, residual Q3 (see [deferred.md](deferred.md)), periodic checkpoint measurements, and Q5/Q10.
