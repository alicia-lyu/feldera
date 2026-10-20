# Maintenance measurement (R5)

R5 compares native and reconstructed maintenance of `V = Customer ⋈ Orders ⋈ Lineitem` under identical RF1/RF2 refreshes with file-backed maintained state. Reconstructed mode reads the Customer–Orders–Lineitem (COL) merged index in place of four traces (see [reconstruction.md](reconstruction.md)).

This file defines what is measured and what remains pending. Commands (the runner, input generation, the pilot) are in [WORKFLOW.md](WORKFLOW.md). Numbers are in [RESULTS.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/RESULTS.md); their raw artifacts are local-only, in git-ignored `target/` directories. Following [decisions.md](decisions.md), nothing here infers an unmeasured quantity.

## Status

- Harness, runner, and macOS smoke runs (measurement version 2) are done.
- macOS runs are smoke tests only. Their highest RSS (43.2 MiB) is far below the 85% pressure threshold of the 512 MiB runtime target (435.2 MiB), so they show neither memory pressure nor beyond-memory behavior.
- The smoke runs' write and storage gap is explained down to record size; see [the investigation](#write-and-storage-investigation-2026-10-14). With the packed payloads of [Packed, untagged values](#packed-untagged-values-2026-10-19-to-2026-10-20), the merged index's file is smaller than the four join traces' files in these runs.
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
- `configuration.front_code_byte_keys` and `configuration.elide_value_headers` (booleans), with the writer counters `front_coding` and `value_header_elision` (blocks written encoded, `layout_fallbacks`, `size_fallbacks`; see [WORKFLOW.md](WORKFLOW.md));
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

### Record size (2026-10-16 to 2026-10-19)

With equal compaction and the POSIX fix, the merged index held fewer rows than the four join traces (2,624 against 3,136, because it does not repeat Orders ⋈ Customer) but took 1.68 times the bytes. Splitting each run's data-block bytes by level (`block_stats`, reported as `key_level` and `value_level` in `run-contents.json`) located the gap, and four changes followed. Each step ran all three workloads, three repetitions per mode, with `--setup-full-compaction --diagnostics`. Every repetition of every workload gave the same bytes, and native mode did not change between steps.

| Step (artifact under `target/`) | Merged-index file | Key-level data | Value-level data | Disk usage at window end |
| --- | --- | --- | --- | --- |
| Byte-string values (`r5-level-baseline/`) | 317,440 B | 132,096 B | 173,056 B | 806,912 B |
| Typed values (`r5-typed-values/`) | 358,400 B | 132,096 B | 212,992 B | 847,872 B |
| Typed values, front-coded keys (`r5-front-coded/`) | 259,584 B | 33,280 B | 212,992 B | 749,056 B |
| Packed values, front-coded keys (`r5-packed-values/`) | 151,040 B | 33,280 B | 107,008 B | 640,512 B |
| Packed values, front-coded keys, elided value headers (`r5-packed-elided/`) | 111,104 B | 33,280 B | 67,072 B | 600,576 B |
| Native: four join traces | 188,928 B | 13,312 B | 168,448 B | 678,400 B |

- **Key level.** Every folded key is distinct, so the key level stores all 2,624 keys and shares nothing between them; before front coding it took about 50 B per key. Front coding (`front_code.rs`, `--front-code-byte-keys`) stores each key as the length of the prefix it shares with the previous key and the rest of its bytes. It cut the key level to 33,280 B, about 12.7 B per key. The native Lineitem trace's key level takes 11 B per key (5,632 B for 512 keys). In every run the writer front-coded 17 blocks, with no layout or size fallbacks.
- **Value level.** Storing `SourcePayload` itself, instead of each variant's `rkyv` bytes inside a `Vec<u8>`, made the value level larger, not smaller: 212,992 B against 173,056 B. See "Value level" below.
- **Gap after front coding.** The merged index took 1.37 times the bytes of the four join traces' files (259,584 B against 188,928 B), and its value level was most of it.
- **Packed values.** The next section changes the stored value. The six refreshes still stay in memory, so the window shows no writes in either mode.

#### Value level (2026-10-19)

The payload fields are the same Q3 projections before and after typing (`59b566016`) and the Steps 1–2 merge (`0669eba04`):

- Customer: `segment`.
- Orders: `order_date` and `ship_priority`.
- Lineitem: `ship_date`, `extended_price` and `discount`.

What changed is the stored type:

- **Before typing:** `encode_payload` archived only the row's own variant struct into a `Vec<u8>`, so each row was as long as its own variant.
- **After typing:** the stored value is `SourcePayload`. Its archive, `Archived<SourcePayload>`, is generated by `#[derive(Archive)]`. Like any Rust enum, it is fixed-size and holds:
  - a discriminant (the tag) that says which variant follows;
  - room for the largest variant.

The enum layout came from the typed-values commit `59b566016`; the Steps 1–2 merge kept it unchanged.

Measured on aarch64 by `layout_tests.rs` in `merged-index-col`. Each value-level figure is from a layer file of 2,048 rows of one source, written in the merged index's layout: folded key, then value and weight.

| Source | Typed item | Typed values on disk | Byte-string item | Byte-string values on disk |
| --- | --- | --- | --- | --- |
| Customer | 80 B | 166,400 B (81.25 B per row) | 40 B | 82,944 B (40.50 B per row) |
| Orders | 80 B | 166,400 B (81.25 B per row) | 32 B | 66,560 B (32.50 B per row) |
| Lineitem | 80 B | 166,400 B (81.25 B per row) | 72 B | 148,992 B (72.75 B per row) |
| Native Lineitem trace value | 64 B | 132,608 B (64.75 B per row) | | |

An item is the `rkyv` archive of one value and its weight.

- **Typed:** `SourcePayload`.
- **Byte string:** the variant's own archive in a `Vec<u8>`, the encoding used before `59b566016`.
- **Native:** `Tup4<i32, SqlDecimal<15,2>, SqlDecimal<4,2>, Date>`, the value of the generated circuit's Lineitem join trace. The native-shaped file has four lines per order key. It takes exactly the bytes of the real native Lineitem trace in `target/r5-front-coded/` (132,608 B).

Findings:

- **The layer file stores variable-size values without padding them to a common size.** `ValueBuilder::try_add_item` appends each item's archive after the previous one and records its offset. Byte-string files differ by source, and each takes its item size plus 0.5 to 0.75 B per row on disk.
- **Byte strings carry a 16 B header.** A byte-string item is the variant's archive, a 16 B `ArchivedVec` header (relative pointer and length) and the 8 B weight, with no padding: Lineitem is 48 + 16 + 8 = 72 B.
- **Native rows pay alignment instead.** A native Lineitem item is a 48 B tuple and the 8 B weight, rounded up to 64 B because the tuple's decimals need 16-byte alignment. So before typing, a merged-index Lineitem value cost 8 B more than a native one: the 16 B header, less the 8 B of alignment padding that the native value pays.
- **Typing added the enum.** `Archived<SourcePayload>` is 64 B with 16-byte alignment, whatever the variant.
  - A dump shows each Orders value as the tag byte (`01`) and 8 bytes of data in a 64 B slot.
  - A Lineitem value is the tag byte (`02`), 15 zero bytes, then the 48 B `Archived<LineitemPayload>`. Its decimals need 16-byte alignment, so the tag takes 16 B.
  - Native values have no tag.
- **Same in both modes, not a cause of the gap.** `SqlDecimal` is always an `i128` with 16-byte alignment. Inside `Archived<LineitemPayload>`, the compiler moves `ship_date` after the two decimals, which leaves 12 bytes of padding at the end.

The user's decision of 2026-10-19 (see [decisions.md](decisions.md)) is to drop the enum and store packed bytes. The next section measures it.

### Packed, untagged values (2026-10-19 to 2026-10-20)

The stored value is now the packed bytes of `encode_payload` in a `Vec<u8>`, decoded with the row's folded key (see [col-storage.md](col-storage.md)). Three changes are attributed separately, as [decisions.md](decisions.md) records:

1. **The enum.** The stored `SourcePayload` carried a tag and room for the largest variant.
2. **The byte-string header.** Every `Vec<u8>` value pays a 16 B `ArchivedVec` root. `--elide-value-headers` drops it on disk and rebuilds it on read (`front_code.rs`, `encode_values`). It does not depend on what the bytes encode.
3. **Packing.** The fields are written back to back, little-endian, with no padding. Strings are a LEB128 length and UTF-8 bytes, and decimals take the narrowest integer that holds their precision.

#### Layout files

`layout_tests.rs` writes, for each source, 2,048 rows in the merged index's layout (folded key, then value and weight), once per encoding, with no compression. Each figure is the value-level data-block bytes of the file, read back from it. The artifact is `target/r5-packed-values/layout-tests.txt` (test `value_level_bytes_by_encoding`, which pins these numbers).

| Source | Typed | `rkyv` bytes | `rkyv` bytes, elided | Packed | Packed, elided |
| --- | --- | --- | --- | --- | --- |
| Customer | 166,400 B (81.25 B per row) | 82,944 B (40.50) | 56,832 B (27.75) | 82,944 B (40.50) | 46,592 B (22.75) |
| Orders | 166,400 B (81.25) | 66,560 B (32.50) | 37,376 B (18.25) | 66,560 B (32.50) | 37,376 B (18.25) |
| Lineitem | 166,400 B (81.25) | 148,992 B (72.75) | 120,832 B (59.00) | 82,944 B (40.50) | 51,712 B (25.25) |

The native baseline is the `Tup4` value of the generated circuit's Lineitem join trace, written with four lines per order key: 132,608 B (64.75 B per row). That file equals the real native trace's value level in `target/r5-front-coded/` (132,608 B). The native value is not a byte string, so elision does not apply to it.

#### R5 smoke runs

`measure.py --cache-bytes 33554432 --memory-target-bytes 536870912 --batch 512 --runs 3 --no-verify --diagnostics --setup-full-compaction --front-code-byte-keys`, with `--elide-value-headers` added for the second campaign. Inputs are the three smoke workloads of `target/r5-smoke-inputs/` (large-group, clustered, scattered). The artifacts are `target/r5-packed-values/` and `target/r5-packed-elided/`, each with a `summary.json` of the per-run figures.

- Every repetition of every workload gave the same bytes in both modes, and the three workloads agree with each other.
- The native figures equal those of `r5-front-coded/` in all six campaigns (`target/r5-packed-values/native-sanity.txt`): four join traces of 188,928 B, with 13,312 B of key level, 168,448 B of value level, and 678,400 B of disk usage at the window end.
- Key front coding wrote the same 17 blocks as before, with no fallbacks. Value-header elision wrote 13 blocks. It had no layout fallback and one size fallback, a block that elision would not have shrunk by a 512-byte sector.
- The table above has the merged index's rows. Packed values changed only the value level, from 212,992 B to 107,008 B. Elision changed it again, to 67,072 B.

#### Three comparisons

Each compares measured figures. None infers an unmeasured quantity.

- **Enum.** The typed enum took 166,400 B in each layout file. The `rkyv` bytes of each source's own payload took 82,944, 66,560, and 148,992 B. In R5 the merged index's value level was 212,992 B with the enum (`r5-typed-values/`) and 173,056 B with `rkyv` bytes (`r5-level-baseline/`).
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
