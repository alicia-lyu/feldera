# Maintenance measurement (R5)

R5 compares native and reconstructed maintenance of `V = Customer ⋈ Orders ⋈ Lineitem` under identical RF1/RF2 refreshes with file-backed maintained state. Reconstructed mode reads the Customer–Orders–Lineitem (COL) merged index in place of four traces (see [reconstruction.md](reconstruction.md)).

This file defines what is measured and what remains pending. Commands (the runner, input generation, the pilot) are in the [Linux handoff](WORKFLOW.md#linux-handoff) section of WORKFLOW.md. The version 2 macOS smoke numbers are in [history.md](history.md#2026-10-08-r5-version-2-smoke-runs); [RESULTS.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/RESULTS.md) holds the column definitions and the macOS smoke runs at measurement version 2 (2026-10-22) and version 3 (2026-10-25). Raw artifacts are local-only, in git-ignored `target/` directories. Following [decisions.md](decisions.md), nothing here infers an unmeasured quantity.

## Status

- Harness, runner, and macOS smoke runs (measurement version 2) are done.
- The macOS smoke runs were repeated in the current configuration after Step 5 (2026-10-22, in RESULTS.md); storage matched the packed, elided campaign byte for byte.
- Measurement version 3 (two chained circuits, refresh pre-read, input eviction) ran the same smoke workloads on 2026-10-25 (in RESULTS.md). Storage matched again, and forwarding the base delta as JSON took more time than either circuit in large-group.
- macOS runs are smoke tests only. Their highest RSS (47.0 MiB in the 2026-10-22 rerun) is far below the 85% pressure threshold of the 512 MiB runtime target (435.2 MiB), so they show neither memory pressure nor beyond-memory behavior.
- The smoke runs' write and storage gap is explained down to record size; see [the investigation](history.md#2026-10-14-write-and-storage-investigation). With the packed payloads of [Packed, untagged values](#packed-untagged-values-2026-10-19-to-2026-10-20), the merged index's file is smaller than the four join traces' files in these runs.
- Pressure instrumentation is implemented, with Linux paths not yet run (see [Pending: pressure evidence](#pending-pressure-evidence)). Pending: the Linux pilot and the three-repetition campaign, all below. Input preparation (`prepare_inputs.py`) exists and ran end to end on macOS at SF 0.01; no SF 10 input has been generated.

## Definitions

- **Setup versus measured.** Initial loading is setup and is excluded. The measured sequence is the refreshes only, from the first refresh to the last refresh's publication and cleanup.
- **Completion boundary** (version 2). `completion_boundary=last_refresh_complete`. There is no forced compaction, no final compaction wait, and no extra publication transaction. Loading waits only for naturally eligible background merges, then publishes once with an empty transaction. Version 1 forced a final compaction; its results are historical and not comparable (see [history.md](history.md#2026-10-08-r5-version-1-smoke-runs)).
- **Two circuits** (version 3). The base tables and the view are separate circuits in separate runtimes, chained by the base circuit's net delta; `q3_ms` is the view circuit alone, and `base_ms` is reported beside `combined_maintenance_ms`, not added to it. The report records both circuits' storage directories, cache shares (`configuration.circuits`) and per-circuit storage and cache occupancy (`sequence.circuits`). `measurement_version` 2 results timed base-relation maintenance inside `q3_ms` and are not comparable.
- **Refresh deltas stay whole.** Each refresh is complete in RAM. An oversized legitimate refresh must fail rather than be split to fit a budget.
- **Refresh files are pre-read** (version 3, before any smoke run at that version). After loading and setup compaction, and before the window opens, the driver parses every refresh file into memory: RF1 into the `Change` the loop applies, RF2 into its key set. The loop consumes each by value and drops it once applied. The window therefore times no file read or parse: RF1 `checks_input_preparation_ms` no longer includes them, and RF2 `discovery_ms` is the key lookup alone. Version 2 timed them inside the window, so those two columns are not comparable. The pre-read memory is ordinary process memory, equal in both modes; it counts toward DBSP's RSS-based pressure level and the window peak. `refresh_inputs` reports the row counts and `VmRSS` before and after the pre-read (Linux).
- **Input page cache evicted.** On Linux the driver calls `posix_fadvise(DONTNEED)` on every base input and refresh file it read, after the pre-read and before the window; inside the cgroup, `measure.py` does the same for each source and private copy (after an `fsync`, because fresh copies are dirty). The driver reports `inputs_evicted` with the cgroup's `memory.current` before and after. The DBSP buffer cache is left warm, since clearing it would need a new DBSP API; its occupancy is recorded at window start and end (`sequence.cache_occupancy_at_window_start_bytes`).
- **Window versus lifetime.** `resources.peak_rss_bytes` is the process-lifetime high-water mark and includes loading. The window figures below cover the refreshes only.
- **Durability.** Refresh completion implies none. Checkpoint costs are deferred.
- **Equality of inputs.** Both modes receive the same logical refreshes; the runner records input and executable SHA-256 hashes in a manifest (`measure.py`).
- **Excluded.** Independent full-join validation runs only on request (`--no-verify` omits it) and is outside the measured interval. Reconstruction decoding, copying, sorting, spill, and output cleanup remain inside their real intervals.

Per-refresh phases (`discovery`, `checks_input_preparation`, `base`, `forward`, `index`, `q3`, `publication_cleanup`, `combined_maintenance = index + q3`), sequence wall time and CPU, and the storage, RSS, and cache columns are defined in RESULTS.md. Backend read and write counters are storage-backend operations, not OS I/O. On Linux the driver also records OS physical `read_bytes` and `write_bytes` from `/proc/self/io` (`linux_physical_io` in `q3-ivm-baseline/src/measurement.rs`); these count the whole process, and they are `null` on macOS. When `--cgroup-root` is given, the runner additionally records the run cgroup's `io.stat` (`cgroup_evidence` in `measure.py`).

## RF2 discovery

The driver keeps one `BaseState` of shared Catalog batch snapshots and replaces it after each transaction (`CatalogHandle::concat` consumes its output mailbox, so a second copy of the relations is not kept). RF2 order IDs are resolved from the current maintained Orders snapshot. Complete deletion rows come from native Lineitem primary-key ranges, or from the merged index's `(custkey, orderkey)` line range in reconstructed mode. Requested IDs are deduplicated; absent Orders are ignored and counted. Discovery never rescans base files or replays refreshes.

## Buffer cache

The driver's snapshot reads and the manual-index auxiliary thread would otherwise use the runtime's fixed 256 MiB fallback cache, outside `StorageOptions.cache_mib`. They run inside `Runtime::with_buffer_cache` (`crates/dbsp/src/circuit/runtime.rs`), bound to the configured worker cache, so the reported cache capacity covers driver reads, Q3 workers, and the manual index. No new cache is created.

## Report fields

The JSON report (`q3-ivm-baseline/src/main.rs`) records:

- `measurement_version` (3), `completion_boundary` (`last_refresh_complete`), and `compaction_policy` (`no forced compaction; loading waits for normal background merges only; no final wait`, or with `--setup-full-compaction`, `setup full compaction after loading, before the refreshes; no compaction requested after any refresh; no final wait`);
- `configuration.front_code_byte_keys` and `configuration.elide_value_headers` (booleans), with the writer counters `front_coding` and `value_header_elision` (blocks written encoded, `layout_fallbacks`, `size_fallbacks`; see [WORKFLOW.md](WORKFLOW.md));
- `configuration.setup_full_compaction` (boolean) and `load.setup_full_compaction`: `null` by default; with the flag, `wait_for_compaction_ms`, `elapsed_ms` (request full compaction plus wait for compaction to finish), and a `resources` block with that setup's writes by owner. `load.background_merge_wait_ms` excludes this time;
- `sequence.snapshot_publication_transactions` (0; the loading phase reports 1);
- `load.background_merge_wait_ms` and `load.observed_storage_bytes` (loading only, not part of the refresh sequence);
- `sequence.observed_storage_bytes`, taken after the last refresh without a compaction wait, so background merges may still be active; it is not a settled size;
- per Catalog relation: `snapshot_batch_count`, `memory_batch_count`, `storage_batch_count`, `file_backed`; and `manual_index_storage` for the auxiliary index. Memory batches are permitted, so `file_backed` does not mean every batch is on disk;
- OS I/O counters, `null` where unavailable (macOS);
- `backend_write_by_owner` in every `resources` block (each refresh and the sequence): write operations and bytes per owner, keyed `query`, `base_relation` and `merged_index`. The owners sum to `backend_write_operations` and `backend_write_bytes`, which are unchanged. A file's owner is fixed when its writer is created, and a spine's owner when the spine is created; a background compaction runs under its spine's owner. `query` covers join traces, V and query execution; `base_relation` covers the input-map state and the base tables' delta accumulators; `merged_index` covers `ManualIndex`. A native run reports no `merged_index` writes. `measure.py` rejects a report whose owners do not sum to the totals;
- `os` (`std::env::consts::OS`), which the summarizer uses to decide whether Linux-only fields may be `null`;
- `refresh_inputs`: `per_refresh` row counts (RF1 `orders` and `lines`; RF2 `order_keys`), and `vm_rss_before_pre_read_bytes` and `vm_rss_after_pre_read_bytes` (Linux; `null` elsewhere), which measure the pre-read's fixed overhead;
- `inputs_evicted`: `evicted` (true only on Linux with every file advised), `files`, `failures`, and `cgroup_memory_current_before_bytes` and `..._after_bytes` (`null` outside cgroup v2). `evicted` is false on macOS;
- per refresh, `memory_pressure.before` and `memory_pressure.after`: the level (`low`, `moderate`, `high`, `critical`) of `base_runtime` and `q3_runtime`, sampled at the refresh's boundaries. DBSP updates its level about once a second from RSS, so a sample can lag;
- `sequence.window_peak_rss_bytes` and `sequence.window_end_rss_bytes` (Linux only, `null` elsewhere): `VmHWM` after `clear_refs` reset it at window start, and `VmRSS`, at window end. `window_peak_rss_error` holds the reason the reset failed, in which case the peak is `null`;
- `sequence.cgroup` (Linux with cgroup v2; `null` otherwise): `path`; `before`, `after` and `delta` of `memory.current`, `memory.events`, the named subset of `memory.stat` that the kernel has (`anon`, `file`, `pgfault`, `pgmajfault`, `pgscan`, `pgsteal`, `workingset_refault[_anon|_file]`) and `memory.pressure` (`delta` gives total stall microseconds); and `peak` with `window_peak_supported`, `window_peak_bytes` (from resetting `memory.peak` by writing to it and reading it through the same open file, kernel 6.12 and later; UNVERIFIED on the project's Linux host), `lifetime_peak_at_end_bytes`, and `max_sampled_memory_current_bytes` (the fallback: the maximum over the window start, each refresh end and the window end). A missing file gives `null` for its fields. These are evidence of memory use, not proof of pressure;
- `sequence.window_started_micros` and `sequence.window_finished_micros` (Unix-epoch microseconds), which place the flush and compaction logs of `--diagnostics` runs inside or outside the window; and `configuration.diagnostics`, the diagnostics directory or `null`.

In version 3, `q3_ms` is the view circuit's transaction alone, and `base_ms` times the base circuit's transaction: the primary-key input maps (`add_input_map_persistent`, owner `base_relation`) and their delta accumulators. In version 2, including the 2026-10-22 macOS rerun, `q3_ms` was the single `circuit.transaction()` and covered both; that base maintenance is common to both modes and dilutes any version 2 ratio between them.

Process CPU includes natural background merges inside the interval. Reconstruction's temporary-batch decoding, copying, and sorting are charged to `q3_ms`.

## Workloads

Three macOS smoke workloads exist, each starting from 64 Customers, 512 Orders, and 2,048 Lines (table in RESULTS.md): clustered (16 Orders on one Customer per RF1), scattered (16 Orders on 16 Customers), and large-group (one Order with 10,000 Lines). Each runs the sequence RF1, RF2, repeated RF2 (absent Orders), RF1 reinsertion, RF2, empty RF2.

## Pending: Linux campaign

macOS gives no enforced memory limit, so qualification needs Linux. The runner (`measure.py`) already supports `--cgroup-root` and `--total-memory-bytes`: it creates a cgroup v2 child per run with `memory.max` set, `memory.swap.max=0`, copies the input files inside that cgroup before loading, and records `memory.peak` (`null` on a kernel without that file), `memory.events`, and `io.stat`.

Plan (not yet executed):

1. **Inputs.** SF 10 dbgen: `dbgen -s 10 -T c`, `dbgen -s 10 -T o`, and `dbgen -s 10 -U 3`; a bare `-U 3` would use scale factor 1, the default in dbgen's `driver.c` (about 1.5 million Customers, 15 million Orders, 60 million Lines; each RF1 about 15,000 Orders and 60,000 Lines). The sizes are the dbgen scale-factor figures and are unchecked here.
2. **RF2 deletions.** Three sets of distinct existing base Orders, each as large as its pair's RF1 (15,000 at SF 10 by the dbgen formula), never reused across pairs. Clustered: the Orders of the lowest-keyed Customers, in (Customer, Order) order, each pair taking the next block; the Customer prefix is derived from the data and recorded in `prepare-manifest.json`. (An earlier plan named the first 7,500 Customers; at SF 10 those hold about five times the 45,000 Orders needed.) Scattered: fixed-seed uniform sample of existing Order rows, not of integers in the sparse key range. RF1 keeps dbgen's complete Order and Lineitem groups and changes only each Order's Customer, chosen independently of the RF2 sets ([decisions.md](decisions.md#2026-10-23-linux-campaign-refresh-selection)). Clustered RF1 draws Customers from a Zipf distribution over a seeded random permutation of Customer keys (`--zipf-exponent`, default 1.0): popularity skew without key locality. Scattered RF1 is uniform over all Customer keys, including keys with no Orders. Orders stay ascending and Lineitems grouped by Order. The manifest records, per pair, the distinct Customers and the Order counts of the top 1, 10, and 100 Customers.
3. **Settings.** Fresh process, storage directory, and cgroup per run. `memory.max` 1 GiB, swap 0, cache 128 MiB, runtime RSS target 512 MiB, one worker, 10,000-row loading target, three repetitions; identical across modes. Input hashes must match between modes.
4. **Pilot first** (one repetition, per workload and per front-coding arm of step 6; the exact command is in the [Linux handoff](WORKFLOW.md#linux-handoff)). Require `load.observed_storage_bytes > memory.max` in both modes, aiming at roughly four times the limit. This checks stored-state scale only. Do not substitute transient peak disk size, and do not treat it as proof of memory pressure.
5. **Campaign** (`--runs 3`, fresh destinations) only after every pilot passes and the pressure instrumentation below exists.
6. **Front-coding A/B** (user decision of 2026-10-22). Arm A is the campaign above, with `--front-code-byte-keys`. Arm B repeats it without that flag: same inputs, settings, `--runs 3`, and fresh destinations, with `--elide-value-headers` and `--setup-full-compaction` still on, so the arms differ in the key encoding alone.
   - Front coding applies only to byte-string keys, so it changes only the merged index. Native keys are not byte strings. Each arm still runs both modes, because `measure.py` always does, and native's storage must match across arms; a mismatch means the arms are not comparable.
   - Compare the reconstructed mode across arms: the merged index's storage, writes by owner (`merged_index`), `index_ms`, `q3_ms`, and process CPU. On macOS, front coding changed only the key level (132,096 B to 33,280 B; see [history.md](history.md#2026-10-16-to-2026-10-19-record-size-typed-value-steps)) and wrote 17 front-coded blocks with no fallbacks. Its decoding cost has not been measured.
   - Both the pilot and the campaign run both arms, which doubles their runs and disk use. The arms alternate per workload (A then B for clustered, then for scattered), so slow drift on the machine does not fall on one arm.

On Linux, storage files open with `O_DIRECT`, while input reads go through the page cache and count against the cgroup. This was checked against the code:

- `StorageCacheConfig::to_custom_open_flags` (`crates/feldera-types/src/config.rs`) returns `O_DIRECT` for `FelderaCache` on Linux, and 0 for `PageCache` and on other systems.
- `cache_flags` (`crates/dbsp/src/storage/backend.rs`) applies those flags when the POSIX backend creates and opens a file (`posixio_impl.rs`).
- The driver selects `FelderaCache` (`main.rs`).
- The `.tbl` inputs are read with `BufReader` over `File` (`main.rs`, `workload.rs`), so they use the page cache. The runner's input copy (`private_inputs`) also runs inside the cgroup.
- `FBuf::ALIGNMENT` is 512 (`crates/storage/src/fbuf.rs`). A filesystem that needs larger direct-I/O alignment (a 4Kn disk) or that rejects `O_DIRECT` (tmpfs, some overlay filesystems) may fail; whether this build then fails is not tested here. Put the storage path on a local ext4 or xfs filesystem.

## Pending: pressure evidence

The instrumentation is implemented (Linux paths unrun and untested on Linux; macOS records `null`) but has produced no Linux data. Implemented:

- Per-refresh `Runtime::current_memory_pressure()` levels for both runtimes (`refreshes[].memory_pressure`).
- A cgroup snapshot of `memory.current`, `memory.events`, `memory.stat` reclaim and refault counters, and `memory.pressure` before the first refresh and after the last, with deltas (`sequence.cgroup`), apart from loading and input preparation.
- A window peak: `VmHWM` reset through `clear_refs` (`sequence.window_peak_rss_bytes`), and `memory.peak` reset by writing to it, with the sampled `memory.current` maximum as the fallback.
- Pre-reading of the refresh files and eviction of the input page cache (see [Definitions](#definitions)).

Still pending:

- The first Linux pilot, which shows whether the `clear_refs` and `memory.peak` resets work on the host.
- Keeping the runner's run-level cgroup peak and `io.stat` as run-level evidence (unchanged). The enforced limit covers the driver, runtime, auxiliary threads, configured cache, and cgroup-charged input page cache together.
- As a probe, repeat a pilot with a 192 MiB runtime target and check that the samples show the intended levels. An optional delta stress case concatenates eight complete RF1 sets in RAM; a run that exceeds its budget must fail as one complete refresh.
- Any claim of pressure or reclaim. The new fields are evidence to read, not proof.

### Two pressure signals

DBSP's pressure level and the cgroup limit measure different things:

- **DBSP's signal.** The `rss-monitor` thread (`crates/dbsp/src/circuit/runtime.rs`) calls `process_rss_bytes` (`crates/dbsp/src/utils.rs`, `memory_stats().physical_mem`) once a second. This is process RSS only.
  - **Thresholds.** The levels are fractions of `max_rss`, which the driver sets from `--memory-target-bytes` through `CircuitConfig::with_max_rss_bytes` (`main.rs`). Moderate is 0.85, High 0.90, and Critical 0.95 (`crates/feldera-types/src/memory_pressure.rs`). Without a target, the level is always Low.
  - **Hysteresis.** After High, the level falls back only when the raw level reaches Low. After Critical, it falls back only when the raw level reaches Moderate or Low.
- **The cgroup limit.** `memory.max` charges anonymous memory and page cache together. The cache includes the runner's input copies (`private_inputs` in `measure.py`), the driver's `BufReader` input reads, and the refresh-file reads. State files are opened with `O_DIRECT` (`StorageCacheConfig::to_custom_open_flags` for `FelderaCache`), so state I/O adds no page cache.
- **Consequence.** The cgroup can approach `memory.max`, and the kernel can reclaim page cache, while DBSP still reports Low pressure and does not spill. Kernel reclaim in a pilot is therefore not evidence of DBSP memory pressure, and DBSP's level says nothing about cgroup headroom. Report the two signals apart.
- **Mitigations (implemented).** The driver and the runner evict the input page cache after loading, and the driver reads the refresh files into memory before the window. This narrows the gap but does not close it: page cache charged inside the window would stay invisible to DBSP's signal, and pre-read rows count as process memory and so toward DBSP's signal (their size is `refresh_inputs`). The cgroup and the DBSP levels are reported apart (`sequence.cgroup` and `refreshes[].memory_pressure`).

Until these are recorded in RESULTS.md, with raw artifacts, make no memory-pressure, reclaim, or beyond-memory claim.

## Setup compaction and the compaction threshold

A Spine starts a background compaction when one of its levels reaches 8 runs (`MIN_LEVEL0_MERGE_BATCHES`). Writes inside the measured window therefore depend on where each Spine stands against that threshold when the window opens, not on how much state it keeps. `--setup-full-compaction` removes that dependence. After loading, it requests full compaction, waits for compaction to finish, then publishes and starts the refreshes, so both modes start from one run per Spine. No compaction is requested after any refresh.

Loading itself is unchanged. The merged index takes a run from every transaction that touches any of its tables, so it reaches the threshold one refresh earlier than each native trace, by design. The evidence for both points (the 2026-10-14 smoke runs, the POSIX flush fix, and the write attribution by owner) is in [history.md](history.md#2026-10-14-write-and-storage-investigation).

### Terms

- **Batch:** the delta of one transaction. **Run:** an immutable sorted collection in a Spine, in memory or on disk; `memory_batch_count` and similar code fields count runs.
- **Compaction:** LSM compaction. Background compaction starts when a Spine level reaches 8 runs (`MIN_LEVEL0_MERGE_BATCHES`). A requested full compaction is `DBSPHandle::start_compaction`, or `initiate_compaction` on the merged index's Spine.
- **Base-relation state:** the base tables stored by primary key (Feldera's "input map").
- **Operations**, named separately: finish a transaction, request full compaction, wait for compaction to finish, publish snapshots, release snapshots.

## Record size (2026-10-16 to 2026-10-19)

With equal compaction and the POSIX fix, the merged index held fewer rows than the four join traces (2,624 against 3,136, because it does not repeat Orders ⋈ Customer) but took 1.68 times the bytes. Splitting each run's data-block bytes by level (`block_stats`, reported as `key_level` and `value_level` in `run-contents.json`) located the gap, and four changes followed. Each step ran all three workloads, three repetitions per mode, with `--setup-full-compaction --diagnostics`. Every repetition of every workload gave the same bytes, and native mode did not change between steps.

The table keeps the rows that match the current configuration. The rows for the byte-string and typed-value steps are in [history.md](history.md#2026-10-16-to-2026-10-19-record-size-typed-value-steps).

| Step (artifact under `target/`) | Merged-index file | Key-level data | Value-level data | Disk usage at window end |
| --- | --- | --- | --- | --- |
| Packed values, front-coded keys (`r5-packed-values/`) | 151,040 B | 33,280 B | 107,008 B | 640,512 B |
| Packed values, front-coded keys, elided value headers (`r5-packed-elided/`) | 111,104 B | 33,280 B | 67,072 B | 600,576 B |
| Native: four join traces | 188,928 B | 13,312 B | 168,448 B | 678,400 B |

- **Key level.** Every folded key is distinct, so the key level stores all 2,624 keys and shares nothing between them; before front coding it took about 50 B per key (132,096 B; see history.md). Front coding (`front_code.rs`, `--front-code-byte-keys`) stores each key as the length of the prefix it shares with the previous key and the rest of its bytes. It cut the key level to 33,280 B, about 12.7 B per key. The native Lineitem trace's key level takes 11 B per key (5,632 B for 512 keys). In every run the writer front-coded 17 blocks, with no layout or size fallbacks.
- **Packed values.** The next section changes the stored value. The six refreshes still stay in memory, so the window shows no writes in either mode.
- **Retired steps.** Storing `SourcePayload` itself made the value level larger, not smaller, and front coding alone left the merged index 1.37 times the bytes of the four join traces. Both comparisons, and the value-level analysis behind the decision to pack, are in [history.md](history.md#2026-10-19-value-level).

### Packed, untagged values (2026-10-19 to 2026-10-20)

The stored value is now the packed bytes of `encode_payload` in a `Vec<u8>`, decoded with the row's folded key (see [col-storage.md](col-storage.md)). Three changes are attributed separately, as [decisions.md](decisions.md) records:

1. **The enum.** The stored `SourcePayload` carried a tag and room for the largest variant.
2. **The byte-string header.** Every `Vec<u8>` value pays a 16 B `ArchivedVec` root. `--elide-value-headers` drops it on disk and rebuilds it on read (`front_code.rs`, `encode_values`). It does not depend on what the bytes encode.
3. **Packing.** The fields are written back to back, little-endian, with no padding. Strings are a LEB128 length and UTF-8 bytes, and decimals take the narrowest integer that holds their precision.

#### Layout files

`layout_tests.rs` wrote, for each source, 2,048 rows in the merged index's layout (folded key, then value and weight), once per encoding, with no compression. Each figure is the value-level data-block bytes of the file, read back from it. The artifact is `target/r5-packed-values/layout-tests.txt` (test `value_level_bytes_by_encoding`, which pinned these numbers). At roadmap Step 5 that test was reduced to the packed and native measurements, now `packed_value_bytes_against_native_lineitem`; the retired typed and `rkyv` figures are recorded in [col-storage.md](col-storage.md), and the full table with those columns is in [history.md](history.md#2026-10-20-layout-files-typed-and-rkyv-columns).

| Source | Packed | Packed, elided |
| --- | --- | --- |
| Customer | 82,944 B (40.50 B per row) | 46,592 B (22.75) |
| Orders | 66,560 B (32.50) | 37,376 B (18.25) |
| Lineitem | 82,944 B (40.50) | 51,712 B (25.25) |

The native baseline is the `Tup4` value of the generated circuit's Lineitem join trace, written with four lines per order key: 132,608 B (64.75 B per row). That file equals the real native trace's value level in `target/r5-front-coded/` (132,608 B). The native value is not a byte string, so elision does not apply to it.

#### R5 smoke runs

`measure.py --cache-bytes 33554432 --memory-target-bytes 536870912 --batch 512 --runs 3 --no-verify --diagnostics --setup-full-compaction --front-code-byte-keys`, with `--elide-value-headers` added for the second campaign. Inputs are the three smoke workloads of `target/r5-smoke-inputs/` (large-group, clustered, scattered). The artifacts are `target/r5-packed-values/` and `target/r5-packed-elided/`, each with a `summary.json` of the per-run figures.

- Every repetition of every workload gave the same bytes in both modes, and the three workloads agree with each other.
- The native figures equal those of `r5-front-coded/` in all six campaigns (`target/r5-packed-values/native-sanity.txt`): four join traces of 188,928 B, with 13,312 B of key level, 168,448 B of value level, and 678,400 B of disk usage at the window end.
- Key front coding wrote the same 17 blocks as before, with no fallbacks. Value-header elision wrote 13 blocks. It had no layout fallback and one size fallback, a block that elision would not have shrunk by a 512-byte sector.
- The table above has the merged index's rows. Packed values changed only the value level, from 212,992 B to 107,008 B. Elision changed it again, to 67,072 B.

#### Three comparisons

Each compares measured figures. None infers an unmeasured quantity.

The comparisons below cite `rkyv`-bytes and typed-value figures; those are in [history.md](history.md), together with the enum comparison.

- **Structure, with the same encoding.** The merged index's Lineitem value as `rkyv` bytes took 148,992 B in its layout file, against 132,608 B for the native value. With the header elided it took 120,832 B, less than the native value in these files. The two files differ in shape (one value per folded key, against four lines per order key), and nothing here separates that from the header.
- **Byte-string header.** Elision shrank every byte-string layout file: `rkyv` bytes from 82,944, 66,560, and 148,992 B to 56,832, 37,376, and 120,832 B, and packed bytes from 82,944, 66,560, and 82,944 B to 46,592, 37,376, and 51,712 B. In R5 it shrank the packed value level from 107,008 B to 67,072 B. It is a disk-format change that any byte-string value can use.
- **Packing.** Without elision, packing left the Customer and Orders layout files unchanged and shrank Lineitem's, from 148,992 B to 82,944 B. With elision, it left Orders unchanged and shrank Customer's from 56,832 B to 46,592 B and Lineitem's from 120,832 B to 51,712 B. In R5 the packed value level was 107,008 B, against 173,056 B for `rkyv` bytes in `r5-level-baseline/`. Key front coding does not change the value level (`r5-typed-values/` and `r5-front-coded/` both have 212,992 B), so the two runs compare.

#### What packing does and does not show

Packing is neither a unique advantage of the merged index nor orthogonal to it.

- Its byte savings do not come from the merged structure, and native could reach a similar gain.
- The merged index makes packing cheap. Keys are unique, so values need only byte equality. The payload format is private. Every read goes through one decode point, `decode_payload`.
- In native, packing is costly. Values are sorted and compared within a key, so a packed form needs an order-preserving encoding or a decode before each comparison. Native types are shared by the whole program and by DBSP's storage format, so the change needs a new format version ([deferred.md](deferred.md#native-packing)).
- Decoding is not a differentiator: DBSP's cursors deserialize `rkyv` items in both modes.

No figure here is a native packing result, and no run here measures decoding CPU. Both stay unmeasured.

## Deferred

Broader updates, residual Q3 (see [deferred.md](deferred.md)), periodic checkpoint measurements, and Q5/Q10.
