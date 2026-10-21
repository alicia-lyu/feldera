# History

Dated records of work and measurements that no longer describe the current configuration.
Each section is a snapshot. It is not authoritative, and its figures are not comparable with
current ones unless it says so. Current facts live in the topic files listed in the
[reading guide](README.md#reading-guide). Raw artifacts named here were local and git-ignored.

## 2026-10-06: compatibility gaps before R3

Moved from [native-state-read-contracts.md](native-state-read-contracts.md). R3 has since closed
the runtime-integration gap for RF1/RF2. [col-storage.md](col-storage.md) describes the current
storage design.

| Boundary | Native request | Storage prototype at 2026-10-06 |
| --- | --- | --- |
| Key placement | `B⁺` and `L⁻` request order-ID keys; `O⁻` requests customer-ID keys. Logical equality uses native IDs; folded bytes encode physical placement. | The reusable COL definition (`merged-index-col/src/lib.rs`) places Customer at `(c)`, Orders at `(c,o)`, and extended Lineitem at `(c,o,l)`. Prefix helpers require the customer ID as well as the order ID. |
| Identity widths | Customer and order IDs are `i64`; line number is `i32`. | COL uses `SourceKey::Customer(i64)`, `Orders(i64, i64)`, and `Lineitem(i64, i64, i32)`. |
| Payload representation | `SqlString`, `Date`, and fixed-precision `SqlDecimal` types, as listed above. | COL stores segment as `SqlString`, dates as `Date`, ship priority as `i32`, price as `SqlDecimal<15, 2>`, and discount as `SqlDecimal<4, 2>`. |
| Cursor representation | Each join reads its typed key/value relation, including the joined `B` projection. | The general merged-index batch (`crates/dbsp/src/trace/merged_index/batch.rs`, a public module) stores folded byte keys and serialized payload bytes and exposes consolidated byte rows. Payload codecs live in COL; the typed projections are the Q3 readers described below. |
| Logical versus physical Lineitem | `s5` carries order ID and line number, with no customer ID. | The `LINEITEM` index, named `LineitemByCustomer` (`merged-index-col/src/lib.rs`), folds the extended row under a customer-leading key; the producer supplies the customer from the same transaction's Orders. |
| Runtime integration | The default generated joins build native retained traces and read current and delayed snapshots. | Default generation still does. With `--q3-merged-index`, the compiler instead emits the supplied-snapshot joins that read the merged index (R3 below). |

## 2026-10-08: R5 version 2 smoke runs

Moved from [RESULTS.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/RESULTS.md). Configuration of these runs: before POSIX flush fix
`f7fec2391`, byte-string values, no setup compaction, no front coding. The definitions of the
reported columns remain in RESULTS.md.

### Verification at the time

On 2026-10-08, the revised `verify.sh` passed **18 native, 15 reconstructed,
21 COL, and 12 Python runner tests (the count then; `test_measure.py` now defines 19)**, with exact R4 transcript agreement.
Formatting, strict experiment/COL Clippy, shell syntax, and experiment-scoped
whitespace checks passed. Artifacts are in
`target/r4-verification/20261008T215445Z-46398/` within this experiment.
Five focused DBSP tests passed, covering background-idle versus full-compaction
status and existing single-batch compaction cases. Strict production DBSP
library Clippy passed. Default native generation was restored. Regression
coverage includes RAM/disk runs surviving refresh completion and an empty
sequence recording zero sequence time and CPU.

### Results and qualification

All **18 fresh processes completed**: three repetitions per mode for each
workload. Settings were one maintenance worker, a 512-row loading target,
32 MiB cache, 512 MiB runtime RSS target, no compression, native join fetch,
`min_storage_bytes=0`, and a 300-second loading merge-wait timeout. All reports
use measurement version 2 and the last-refresh completion boundary. Source
revisions were `603e02be3` for clustered and `a718f8fce` for scattered/large-group;
the latter adds documentation only. The host was macOS 26.3.1 on arm64.

**Each timing value, write counter, observed storage value, and cache occupancy
is its own median. Maximum sampled storage and maximum RSS are maxima across
three fresh runs.** Medians need not add up.

| Workload | Mode | Foreground ms | Sequence ms | Sequence CPU ms |
| --- | --- | ---: | ---: | ---: |
| clustered | native | 2.114 | 2.193 | 3.558 |
| clustered | reconstructed | 4.013 | 4.092 | 6.942 |
| scattered | native | 2.367 | 2.448 | 3.759 |
| scattered | reconstructed | 3.696 | 3.789 | 6.450 |
| large-group | native | 112.944 | 113.154 | 104.047 |
| large-group | reconstructed | 133.199 | 133.343 | 135.723 |

| Workload | Mode | Write ops | Write bytes | Observed storage | Max sampled peak storage | Max peak RSS | Cache occupancy |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| clustered | native | 3 | 218,624 | 218,624 | 392,192 | 20,774,912 | 431,592 |
| clustered | reconstructed | 4 | 813,056 | 818,688 | 827,392 | 26,247,168 | 895,648 |
| scattered | native | 3 | 348,672 | 359,424 | 364,032 | 20,791,296 | 439,880 |
| scattered | reconstructed | 4 | 822,272 | 822,272 | 826,880 | 26,296,320 | 903,936 |
| large-group | native | 6 | 765,952 | 765,952 | 765,952 | 34,848,768 | 788,152 |
| large-group | reconstructed | 72 | 2,171,392 | 2,171,392 | 2,171,392 | 45,285,376 | 2,163,872 |

Backend reads were **zero operations and zero bytes in every run**. OS physical
I/O counters are unavailable on macOS. Natural background activity can continue
after the sequence counter interval, while final storage/cache observations are
collected. These observations are not settled sizes or a measurement of all
subsequent compaction costs.

Every process retained zero `Reference` bytes, used seven logical loading
transactions plus one loading publication transaction, and retained at most
516 loading rows. **The initial tiny base remained entirely in RAM, with zero
stored file bytes.** Natural refresh merges produced disk batches alongside RAM
runs; Customers remained in RAM. The reconstructed reports confirm absence of
the replaced historical join traces. Large-group fan-out was 10,000 without
splitting the refresh. Independent full-join correctness checks were performed
in the separate verification suites, outside these performance processes.

The maximum RSS over all 18 processes was **45,285,376 bytes (43.2 MiB)**. This is
below the first runtime pressure threshold, **85% of 512 MiB = 435.2 MiB**.
The runs did not reach DBSP RSS pressure. They are **macOS smoke tests**, not
beyond-memory measurements or equal-total-memory comparisons. The Linux
campaign and direct pressure instrumentation remain pending; the
[campaign description](measurement.md)
specifies SF10 existing-base deletion workloads, equal memory limits, the pilot
command, and evidence required to establish actual pressure. Refresh completion
has no durability guarantee; checkpoint costs remain deferred.

Input/executable hashes, configurations, phases, profiles, and graphs are
preserved under ignored `target/r5-smoke-natural/`:

- [Clustered manifest](../../../target/r5-smoke-natural/clustered/manifest.json).
- [Scattered manifest](../../../target/r5-smoke-natural/scattered/manifest.json).
- [Large-group manifest](../../../target/r5-smoke-natural/large-group/manifest.json).
- [Aggregate summary](../../../target/r5-smoke-natural/summary.json).

The runner restored default native generation after every campaign. The v1
section below preserves historical forced-consolidation results and their
previous timing boundaries.

## 2026-10-08: R5 version 1 smoke runs

Moved from [RESULTS.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/RESULTS.md). Configuration of these runs: before POSIX flush fix
`f7fec2391`, byte-string values, no setup compaction, no front coding. The artifacts are under the
repository-root `target/r5-smoke/`.

Superseded by R5 v2 above. Kept because the artifacts exist under the repository-root `target/r5-smoke/` (local-only). Do not compare v1 and v2 timings: their completion boundaries differ.

### R5 v1 harness verification and macOS smoke evidence

These measurements used forced final compaction and an extra publication
transaction. They preserve the earlier implementation's behavior and costs.

On 2026-10-08, `./verify.sh` passed 18 native tests, 15 reconstructed tests,
21 COL tests, and 10 Python runner tests. The native and reconstructed R4
transaction transcripts matched exactly. Formatting, strict experiment
Clippy in both modes, COL Clippy, shell syntax, and whitespace checks passed.
Artifacts are under `target/r4-verification/20261008T201153Z-91462/` in this
experiment. Five focused DBSP tests also passed: three single-batch compaction
cases and two configured-cache binding/restoration cases. Strict production
DBSP library Clippy passed. The broader DBSP test-code Clippy check found 56
existing lint errors outside the modified files; its output is retained at
`target/r5-dbsp-clippy-tests.log` in this experiment.

#### v1 workload definitions

All three workloads start with **64 Customers, 512 Orders, and 2,048 Lines**.
The workload label describes the Orders and Lines added by each RF1:

| Workload type | Orders per RF1 | Customers referenced | Lines per Order | Total Lines per RF1 |
| --- | ---: | ---: | ---: | ---: |
| **clustered** | 16 | 1 shared Customer | 4 | 64 |
| **scattered** | 16 | 16 distinct Customers | 4 | 64 |
| **large-group** | 1 | 1 Customer | 10,000 | 10,000 |

**RF1 inserts complete Order/Lineitem groups. RF2 deletes the requested Orders
and all their current Lines.** Each campaign executes the same six-refresh
sequence: RF1, RF2, repeated RF2 after those Orders are absent, RF1 reinsertion,
RF2, and an empty RF2. Delete lists include a duplicate requested ID and an
absent ID. All base Orders are ascending and Lineitems are grouped by Order.

**Mode:** `native` uses the generated native Q3 circuit; `reconstructed` uses
manual merged-index maintenance and the reconstructed Q3 circuit.

Each mode uses a separate preserved release executable, three fresh processes,
and fresh storage directories. Settings are one worker, a 512-row loading
target, a 32 MiB configured cache, a 512 MiB runtime memory-pressure target,
`min_storage_bytes=0`, no compression, native join fetch enabled, and a
300-second drainage timeout. Independent verification is disabled for these
measurements; the small-workload tests above supply separate full-join evidence.
Initial loading is excluded. Sequence latency includes final compaction,
publication of compacted snapshots, and output cleanup. The `index_ms + q3_ms`
subtotal excludes discovery, checks/input preparation, and publication/cleanup.

#### Timing definitions and results

**All timing columns cover the complete six-refresh sequence, with initial
loading excluded.** Independent correctness checking is disabled in these runs.

| Column | Definition |
| --- | --- |
| **Foreground ms** | Sum of each refresh's elapsed wall time: input reads/RF2 discovery, operational checks, input preparation, index maintenance, Q3 transaction, and publication/cleanup. Final drainage is measured separately. |
| **Sequence ms** | Elapsed wall time from the start of the first refresh through completion of final drainage. Includes foreground work, final drainage, and between-refresh measurement/logging overhead. |
| **Final drainage ms** | Historical forced consolidation after the six refreshes: native requests compaction of all circuit traces; reconstructed requests it for its remaining circuit traces plus the auxiliary merged index. Each trace is reduced to at most one batch. Includes waits, an empty snapshot-publication transaction, and cleanup; included only in the v1 Sequence ms. |
| **Sequence CPU ms** | Process user + system CPU time over the sequence, summed across driver, runtime, and auxiliary threads, including final drainage. |

The results are from three fresh runs on
`macOS-26.3.1-arm64-arm-64bit-Mach-O`, at revision `42bbbfa3c`.
**Each numeric column is its own median; displayed medians need not add up.**

| Workload | Mode | Foreground ms | Sequence ms | Final drainage ms | Sequence CPU ms |
| --- | --- | ---: | ---: | ---: | ---: |
| clustered | native | 2.635 | 17.147 | 13.957 | 6.253 |
| clustered | reconstructed | 2.988 | 19.341 | 16.180 | 7.461 |
| scattered | native | 2.471 | 16.325 | 13.722 | 6.942 |
| scattered | reconstructed | 5.279 | 19.288 | 13.852 | 11.407 |
| large-group | native | 99.263 | 121.413 | 24.168 | 105.249 |
| large-group | reconstructed | 120.953 | 142.452 | 21.320 | 129.226 |

#### Resource definitions and results

**Write counters cover the six refreshes and final drainage. Storage, RSS, and
cache columns are bytes.** Each numeric column below is also its own median.

| Column | Definition |
| --- | --- |
| **Write ops** | Number of storage-backend write operations during the sequence, from write-latency histogram counts. |
| **Write bytes** | Bytes submitted to storage-backend writes during the sequence, from block-size histogram sums. |
| **Settled storage** | Sum of regular-file lengths in the run's storage directory after final drainage. |
| **Sampled peak storage** | Maximum observed sum of regular-file lengths in the storage directory during maintenance, sampled every 10 ms and once at the end. Shorter peaks may be missed. |
| **Peak RSS** | Process resident-memory high-water mark since process start, including initial loading and all driver/runtime threads. |
| **Cache occupancy** | Bytes occupied in the configured runtime cache after final drainage; its capacity is 32 MiB. |

Backend reads were **zero operations and zero bytes in every run**; this small
maintained state fit in the configured cache. **OS physical read/write bytes are
unavailable on macOS.** The write counters above measure storage-backend activity;
they do not establish physical device I/O or durability.

| Workload | Mode | Write ops | Write bytes | Settled storage | Sampled peak storage | Peak RSS | Cache occupancy |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| clustered | native | 6 | 670,720 | 679,424 | 1,320,960 | 22,085,632 | 1,609,664 |
| clustered | reconstructed | 4 | 803,840 | 808,448 | 1,293,312 | 27,213,824 | 1,807,976 |
| scattered | native | 6 | 670,720 | 679,424 | 1,320,960 | 22,102,016 | 1,609,664 |
| scattered | reconstructed | 4 | 803,840 | 808,448 | 1,293,312 | 27,213,824 | 1,807,976 |
| large-group | native | 6 | 862,208 | 870,912 | 1,133,568 | 38,567,936 | 1,609,664 |
| large-group | reconstructed | 4 | 1,091,072 | 1,095,680 | 1,484,288 | 50,118,656 | 1,807,976 |

All 18 processes completed. Both modes used seven logical loading transactions
and retained at most 516 loading rows. Reports confirmed zero retained
`Reference` bytes, no normal RF2 full scans, file-backed Customers, Orders,
Lineitems and `V`, a file-backed manual index in reconstructed mode, and absence
of the replaced historical join traces there. Repeated RF2 reported the deleted
Orders as absent, while duplicate requested IDs were deduplicated. Large-group
refreshes reported a maximum fan-out of 10,000 without splitting the refresh.

These old runs used a 512 MiB runtime RSS target, whose 85% pressure threshold
was 435.2 MiB. The highest reported RSS was 50,135,040 bytes (about 47.8 MiB),
well below that threshold. The configured cache was 32 MiB, but occupancy stayed
below 2 MiB and backend reads were zero. The smoke therefore does not establish
runtime memory pressure or beyond-memory behavior.

Campaign manifests, input/executable SHA-256 hashes, full phase measurements,
and driver/profile/graph artifacts are retained under the repository's ignored
`target/r5-smoke/` directory:

- [Clustered campaign](../../../target/r5-smoke/clustered.json).
- [Scattered campaign](../../../target/r5-smoke/scattered.json).
- [Large-group campaign](../../../target/r5-smoke/large-group.json).

The runner restored native generation after each campaign. These measurements
are macOS smoke evidence. Equal enforced total memory and beyond-memory
performance require the pending Linux cgroup v2 campaign with swap disabled.
Refresh completion has no durability guarantee. Checkpoint costs, broader
updates, residual Q3, and Q5/Q10 remain deferred.

## 2026-10-14: write and storage investigation

Moved from [measurement.md](measurement.md). The rationale for `--setup-full-compaction` and the
by-design threshold effect stay in measurement.md. Configuration of these runs: before and after
POSIX flush fix `f7fec2391` as each part says, byte-string values, no front coding.

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

## 2026-10-16 to 2026-10-19: record size, typed-value steps

Moved from [measurement.md](measurement.md). The live table keeps the packed and native rows.

With equal compaction and the POSIX fix, the merged index held fewer rows than the four join traces (2,624 against 3,136, because it does not repeat Orders ⋈ Customer) but took 1.68 times the bytes. Splitting each run's data-block bytes by level (`block_stats`, reported as `key_level` and `value_level` in `run-contents.json`) located the gap, and four changes followed. Each step ran all three workloads, three repetitions per mode, with `--setup-full-compaction --diagnostics`. Every repetition of every workload gave the same bytes, and native mode did not change between steps.

| Step (artifact under `target/`) | Merged-index file | Key-level data | Value-level data | Disk usage at window end |
| --- | --- | --- | --- | --- |
| Byte-string values (`r5-level-baseline/`) | 317,440 B | 132,096 B | 173,056 B | 806,912 B |
| Typed values (`r5-typed-values/`) | 358,400 B | 132,096 B | 212,992 B | 847,872 B |
| Typed values, front-coded keys (`r5-front-coded/`) | 259,584 B | 33,280 B | 212,992 B | 749,056 B |

Bullets that depend on the first three rows:

- **Value level.** Storing `SourcePayload` itself, instead of each variant's `rkyv` bytes inside a `Vec<u8>`, made the value level larger, not smaller: 212,992 B against 173,056 B. See "Value level" below.
- **Gap after front coding.** The merged index took 1.37 times the bytes of the four join traces' files (259,584 B against 188,928 B), and its value level was most of it.

## 2026-10-19: value level

Moved from [measurement.md](measurement.md).

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

Measured on aarch64 by `layout_tests.rs` in `merged-index-col`, before Step 5 retired these measurements. Each value-level figure is from a layer file of 2,048 rows of one source, written in the merged index's layout: folded key, then value and weight.

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

## 2026-10-20: layout files, typed and `rkyv` columns

Moved from [measurement.md](measurement.md). The live table keeps the packed and packed-elided
columns. `layout_tests.rs` writes, for each source, 2,048 rows in the merged index's layout
(folded key, then value and weight), once per encoding, with no compression. Each figure is the
value-level data-block bytes of the file, read back from it. The artifact was
`target/r5-packed-values/layout-tests.txt` (test `value_level_bytes_by_encoding`).

| Source | Typed | `rkyv` bytes | `rkyv` bytes, elided | Packed | Packed, elided |
| --- | --- | --- | --- | --- | --- |
| Customer | 166,400 B (81.25 B per row) | 82,944 B (40.50) | 56,832 B (27.75) | 82,944 B (40.50) | 46,592 B (22.75) |
| Orders | 166,400 B (81.25) | 66,560 B (32.50) | 37,376 B (18.25) | 66,560 B (32.50) | 37,376 B (18.25) |
| Lineitem | 166,400 B (81.25) | 148,992 B (72.75) | 120,832 B (59.00) | 82,944 B (40.50) | 51,712 B (25.25) |

The enum comparison, moved from "Three comparisons":

- **Enum.** The typed enum took 166,400 B in each layout file. The `rkyv` bytes of each source's own payload took 82,944, 66,560, and 148,992 B. In R5 the merged index's value level was 212,992 B with the enum (`r5-typed-values/`) and 173,056 B with `rkyv` bytes (`r5-level-baseline/`).
