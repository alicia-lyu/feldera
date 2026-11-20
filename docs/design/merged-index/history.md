# History

Dated records of work and measurements that no longer describe the current configuration.
Each section is a snapshot. It is not authoritative, and its figures are not comparable with
current ones unless it says so. Current facts live in the topic files listed in the
[reading guide](README.md#reading-guide). Raw artifacts named here were local and git-ignored.
"Front coding" in these records is now called prefix truncation (2026-11-11); the code's
identifiers (`--front-code-byte-keys`, `front_code.rs`) keep the old name.

## 2026-10-06: compatibility gaps before R3

Moved from native-state-read-contracts.md. R3 has since closed
the runtime-integration gap for RF1/RF2. col-storage.md describes the current
storage design.

| Boundary | Native request | Storage prototype at 2026-10-06 |
| --- | --- | --- |
| Key placement | `B⁺` and `L⁻` request order-ID keys; `O⁻` requests customer-ID keys. Logical equality uses native IDs; folded bytes encode physical placement. | The reusable COL definition (`merged-index-col/src/lib.rs`) places Customer at `(c)`, Orders at `(c,o)`, and extended Lineitem at `(c,o,l)`. Prefix helpers require the customer ID as well as the order ID. |
| Identity widths | Customer and order IDs are `i64`; line number is `i32`. | COL uses `SourceKey::Customer(i64)`, `Orders(i64, i64)`, and `Lineitem(i64, i64, i32)`. |
| Payload representation | `SqlString`, `Date`, and fixed-precision `SqlDecimal` types, as listed above. | COL stores segment as `SqlString`, dates as `Date`, ship priority as `i32`, price as `SqlDecimal<15, 2>`, and discount as `SqlDecimal<4, 2>`. |
| Cursor representation | Each join reads its typed key/value relation, including the joined `B` projection. | The general merged-index batch (`crates/dbsp/src/trace/merged_index/batch.rs`, a public module) stores folded byte keys and serialized payload bytes and exposes consolidated byte rows. Payload codecs live in COL; the typed projections are the Q3 readers described below. |
| Logical versus physical Lineitem | `s5` carries order ID and line number, with no customer ID. | The `LINEITEM` index, named `LineitemByCustomer` (`merged-index-col/src/lib.rs`), folds the extended row under a customer-leading key; the producer supplies the customer from the same transaction's Orders. |
| Runtime integration | The default generated joins build native retained traces and read current and delayed snapshots. | Default generation still does. With `--q3-merged-index`, the compiler instead emits the supplied-snapshot joins that read the merged index (R3 below). |

## 2026-10-06: R4 verification run

Moved from the former `RESULTS.md` ("R4 shared RF1/RF2 verification"). The current description of the transcript tests is in [q3-ivm-baseline/CLAUDE.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CLAUDE.md).

Three transactions use explicit start/step/commit, submitting Orders and two Lineitem chunks in separate steps; one of these transactions is empty. Other transactions use the normal transaction call. Each run's JSON result file records the transaction names, actual emitted rows and materialized rows, sorted with their weights. It contains no timings or mode-specific fields.

At the time, `verify.sh` ran both experiment suites and COL with Rust 1.96.1 and offline locked dependencies, compared the native and reconstructed JSON result files byte for byte, checked formatting, strict Clippy, shell syntax, and whitespace, and restored default native generation on exit. Separate checks confirmed restoration after success, a failed reconstructed test, and a difference between the two result files.

The complete run on 2026-10-06 passed all 10 native tests, all seven reconstructed tests, and all 21 COL tests. Existing storage, compaction, checkpoint, and unsupported-workload regressions passed. Formatting, Clippy with warnings denied, shell syntax, and whitespace checks passed, and default native generation was restored. The JSON result files covering 13 transactions were identical byte for byte: each is 97,718 bytes with SHA-256 `826d6d3fe047f95752e42cdc88f3633def2ee83033fd24a548a08fb3aa3d67bc`. Logs and JSON result files are stored in the experiment's ignored `target/r4-verification/20261006T222308Z-60261/` directory. The COL count includes the separately committed candidate-location tests; R4 did not modify placement or publication behavior.

The original R1 driver buffered whole base files and retained source maps even with `--no-verify` (from the former q3-ivm-baseline `README.md`).

## 2026-10-08: R5 version 2 smoke runs

Moved from RESULTS.md. Configuration of these runs: before POSIX flush fix
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
campaign description
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

Moved from RESULTS.md. Configuration of these runs: before POSIX flush fix
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

Moved from measurement.md. The rationale for `--setup-full-compaction` and the
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

- **Writes by owner.** Each file writer records an owner when it is created: `query` (join traces, V, query execution), `base_relation`, or `merged_index`. A Spine's owner is set at creation and carried into its background compaction. The owners always sum to the backend totals (tests in `crates/dbsp/tests/attribution_*.rs`, named `write_attribution_*.rs` until 2026-11-13; check in `measure.py`).
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

Moved from measurement.md. The live table keeps the packed and native rows.

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

Moved from measurement.md.

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

Moved from measurement.md. The live table keeps the packed and packed-elided
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

## 2026-11-17: Retired value measurements from the storage layout

Moved from the former `col-storage.md` (now [storage-layout.md](storage-layout.md)).

`layout_tests.rs` measured three encodings of the stored value until Step 5. Only the packed measurement remains; the typed and `rkyv` measurements needed archive derives on the payload types, which the generated types do not have ([decisions.md](decisions.md), 2026-10-21). Their last pinned figures, measured on aarch64 on 2026-10-19 and 2026-10-20, for 2,048 rows of one source in an uncompressed layer file:

| Source | Typed `SourcePayload` item | `rkyv` byte-string item | Typed value level | `rkyv` value level | `rkyv` value level, header elided |
| --- | --- | --- | --- | --- | --- |
| Customer | 80 B | 40 B | 166,400 B | 82,944 B | 56,832 B |
| Orders | 80 B | 32 B | 166,400 B | 66,560 B | 37,376 B |
| Lineitem | 80 B | 72 B | 166,400 B | 148,992 B | 120,832 B |

The archives were `Archived<SourcePayload>` 64 B (align 16), `Archived<CustomerPayload>` 16 B (align 8), `Archived<OrdersPayload>` 8 B (align 4), and `Archived<LineitemPayload>` 48 B (align 16). measurement.md interprets these figures; its raw artifacts are local-only under `target/`.

## 2026-11-17: Roadmap of the general definition (steps 1-5, done)

Moved from [general-definition.md](general-definition.md). All five steps are implemented.

These steps are ordered. Each ends with an exit check and lands with its own doc updates. The steps are not numbered stages, because R1–R5 is the only numbering scheme in these docs.

1. **Key encoders.** Implemented: `OrderedKey`, with its implementations in dbsp, sqllib, and fxp.
   - Exit check: proptests that byte order equals `Ord`, that decoding round-trips, and that widths are exact, including boundary values.
   - Exit check: the i32 and i64 bytes are unchanged.
2. **Static trie and reshaped trait.** Implemented, for keys only: `KeyTrie`, the new `MergedIndexDefinition`, `KeyEncoder` and `KeyDecoder`, and the hand-written COL ported to them with a hand-written `TRIE`.
   - Exit check: the COL tests and `verify.sh` pass with unchanged transcripts. The golden-byte tests are unchanged.
   - Exit check: `check_against_trie` and a proptest tie COL's hard-coded tags and ids to its trie.
3. **Parser and validator** in the SQL compiler, with no emission yet. Implemented; see the end of the Validation section.
   - Exit check: JUnit tests of the derivation (`MergedIndexDerivationTest`). COL comes out as 1, 2, 3; sibling order follows the spec; `custkey via Orders` resolves to `o_custkey`; `*` expands in table order.
   - Exit check: one failing case per validation rule (`MergedIndexValidatorTest`); syntax errors are in `MergedIndexSpecParserTest`.
4. **Emitter, and COL regenerated.** Implemented: `merged-index-col/col.mi`, `generate.sh`, and the committed `src/generated.rs`, which sat beside the hand-written definition until Step 5. `verify.sh` regenerates the module and fails if the committed file differs. The flags report `SpecError`s as compiler errors; the compiler's own source positions refer only to the SQL file.
   - Exit check: the generated and hand-written tries are equal (`generated_tests.rs`, retired with the hand-written definition in Step 5).
   - Exit check: `fold` is byte-identical over property-based keys.
   - Exit check: the prefixes match, and sample payloads encode to identical packed bytes. Since 2026-10-19 the stored value is packed bytes rather than an archive.
5. **Switch.** Implemented: COL uses the generated module, re-exported at the crate root, and the hand-written definition is in `TRASH/`. `lib.rs` keeps only `build_batch`.
   - The typed and rkyv value-level bytes that `layout_tests.rs` pinned are recorded in [storage-layout.md](storage-layout.md), and those measurements are retired. Only the packed measurement remains, so generated types keep plain derives (see [decisions.md](decisions.md), 2026-10-21).
   - Exit check: `verify.sh` transcripts are unchanged.

Later work, outside this roadmap, is listed in [deferred.md](deferred.md).

## 2026-11-17: Shared-traversal status log, audit, and coding roadmap

Moved from [shared-traversal.md](shared-traversal.md). The stale names below are left as history. The coding roadmap and the audit refer to the code and the write-up as they stood in November 2026.

### Status log

The algorithm is settled in the write-up. Roadmap step 1, the run counters, is implemented and reported by the R5 harness in both modes. Step 2, decode-free storage and the scan session, is implemented (2026-11-09), and in-memory runs count their accesses in separate fields (2026-11-10). Step 3, publishing `(Old, Dlt)`, is implemented (2026-11-10). Step 4, the hand-written view circuit, is implemented (2026-11-10) and carries step 7: the circuit admits general deltas. Step 5, typed readers filled at collection, is implemented (2026-11-11). Step 5 was revised on 2026-11-12 (step 5b: requested slices and selective routing). Step 6 is not done at version 11. This doc is the meta plan for the coding steps. Choices marked *unconfirmed* await the user. The confirmed ones are in [decisions.md](decisions.md) under 2026-10-29, 2026-11-03, 2026-11-09, 2026-11-10 (including the step-4 entries), and 2026-11-11 (step 5), and 2026-11-12 (step 5b); minor choices were confirmed on 2026-11-13.

### Audit of the write-up

These points were checked against the code on 2026-11-03 and against the renamed write-up on 2026-11-09. Each was proposed as an edit in the thesis repository's `BUFFER.md`; this repository does not edit the `.tex`.

Applied in the renamed write-up (checked 2026-11-09):

1. **`tab:prototype`, "Moved line items".** Merged into "Owner-key change: Rejected".
2. **"Q3 as compiled today".** The wrong sentence about sorting each reader's driving delta is gone. The text now says each read opens a fresh cursor and seeks for every key, and that `J⁺` and `L⁻` read by `orderkey`.
3. **`tab:prototype`, "Seeks".** It now says `J⁺` reads the customer again for each pair of order and placement customer.
4. **Buffers shared across operators.** In `sec:ivm-buffered`.
5. **(A2)'s last clause.** It now stands beside (A1) as a requirement on the circuit.

Still open (wording only):

1. **Cost wording.** `thm:session`, `lem:skip` ("`g` seeks"), and `tab:prototype` count logical seeks. On a snapshot of `k` runs a logical seek costs `k` physical seeks. Say so in a footnote. Say also that ending a range decodes the next group's first position (`c6`), which `Peek` keeps for the next group.
2. **"Every position decoded".** `thm:session`(i) and `alg:session` decode every position the loop reaches, "whatever its stream", because naming an entry's stream and key takes its decode. In Feldera a folded key names both, so a position needs only a key decode. [No extra decoding](shared-traversal.md#no-extra-decoding) states what Feldera adds.

None of these points changes the algorithm.

### Coding roadmap

The steps are ordered. Each has an exit check and updates its docs. They are not R-numbered, since R1–R5 is the only numbering scheme.

1. **Run counters in DBSP.** *Done on 2026-11-03; extended and measured in both modes on 2026-11-04.*
   - Per-run seeks in both run cursors, `VecIndexedWSetCursor` and `FileIndexedWSetCursor`. Key decodes, value decodes, and weight decodes in the file cursor only, since only file runs deserialize.
   - A thread-local running total, read before and after a region on the same thread. The generic `Cursor` trait gains no counters.
   - Each read also counts for the thread's read owner, set by `with_read_owner` and given by `RunReads::current_for`. COL's `scan` reads the index under `Component::MergedIndex` (named `WriteOwner` until 2026-11-13) and runs its callback under the caller's owner, so index reads stand apart from reads of driving deltas and other batches. *Added on 2026-11-04.*
   - The R5 harness reports each refresh's index run reads per reader, beside the index snapshots' layout (measurement version 5; fields in measurement.md). *Added on 2026-11-04.*
   - For a native comparison, `DBSPHandle::run_reads` asks the workers for their totals per owner, and the harness reports the view worker's reads per refresh in both modes. Native join traces count under `query`; the index readers count under `merged_index`. Only indexed Z-set runs count, so a non-join trace such as `V`'s output is uncounted in both modes. A `seek_key_exact` that a file filter answers counts in `filtered_probes` (named `filter_rejections` until 2026-11-16), not as a seek: native joins probe this way, while the index's `scan` uses plain seeks. The fields were renamed from `*_reads` to `*_decodes` on 2026-11-04. *Added on 2026-11-04.*
   - *Measured on 2026-11-04* (measurement.md): one macOS smoke run per mode at SF 0.01, with RF2 deleting the oldest Orders and native joins reading through run cursors (`fetch_join` off; with it on, the native join batch-fetches into in-memory copies that the counters do not see). Over the sequence, reconstructed mode seeks about 2.4 times as often as native, decodes about 5.9 times the keys and 2.5 times the values, and reads more backend blocks. The counts follow the rules:
     - `J⁺` scans twice per Order where native probes its stored `J` once.
     - The index holds more runs, and every scan seeks each of them.
     - Every index entry has its own key, where a native trace groups values under one.
     - Each scan opens a cursor and ends past its range.

     *Rerun on 2026-11-05* with exact path reads (measurement.md): reconstructed mode seeks about 2.3 times as often as native and decodes about 2.9 times the keys and 1.2 to 1.3 times the values. `J⁺` skips one seek per new Order through the file filter. Backend reads were not comparable in that run: the index's blocks were cached in the base runtime, which wrote them, not the view runtime, which read them.

     *Rerun on 2026-11-05* in one runtime with one buffer cache (measurement.md): every run-read count equals the exact-path run's, in both workloads and modes. Reconstructed backend reads fell from 169 to 72 (clustered) and from 192 to 83 (scattered); native read 7 and 11. At a 256 MiB cache (clustered only), both modes made no backend reads, so the remaining reads at 32 MiB depend on cache capacity, not on placement.

     Steps 2 and 5 address the per-scan cursors, the repeated customer reads, and the landings that take nothing. The per-run multiplier of a logical seek depends on compaction, which no step changes.
   - *Exit:* DBSP unit tests (filter `run_reads`) pin the counts of single moves on an in-memory run and a file run, and one seek per run through `CursorList`. A COL test pinned the four readers of step 1 on the worked transaction (without `v2` and `v8`, since COL has no invoices), over a file run plus an in-memory run. Step 4 retired it (`todays_readers_run_reads_on_the_worked_transaction`) with the readers it measured; the table below is history.
   - *Baseline.* `Old` is a file run holding `c5`, `o3`, `l7`, `l8`, `o4`, `l9`, and `c6`, plus an in-memory run holding `c7`; the after state adds `Dlt`. Each comment in `todays_readers_run_reads_on_the_worked_transaction` derives its count. The counts are the index's own. The driving deltas are in memory and read under the caller's owner, so the test pins their memory reads apart: one key, value, and weight for each key of a delta (`requested_keys` steps through them, and `CursorList` reads the first value and weight under each key).

     | Reader | Per-run seeks | Filter rejections | Key decodes | Value decodes | Weight decodes | Memory key reads | Memory value reads | Memory weight reads |
     | --- | --- | --- | --- | --- | --- | --- | --- | --- |
     | `C⁺` | 3 | 0 | 1 | 2 | 1 | 2 | 3 | 3 |
     | `O⁻` | 2 | 0 | 7 | 8 | 8 | 1 | 0 | 0 |
     | `J⁺` | 5 | 1 | 2 | 3 | 1 | 5 | 5 | 5 |
     | `L⁻` | 6 | 0 | 9 | 9 | 9 | 3 | 0 | 0 |
     | Total | 16 | 1 | 19 | 22 | 19 | 11 | 8 | 8 |

     *Added 2026-11-10:* the memory columns. The in-memory runs are `M` (`c7` alone) and, for the reads of the new state, `Dlt`. Opening reads each run's first key, so every scan reads `M`'s key, and `Dlt`'s too in `C⁺` and `J⁺`. `Dlt` holds two values under `c5` (`new` and `old`), so `C⁺` reads three values and three weights: the first value and its weight at the opening and again after the seek empties the value, then the second value, whose weight is summed with the file run's. `J⁺`'s second probe reads `Dlt`'s `c5` value and weight again, and one more of each at `o5`. `M` is past every range, so `CursorList` never reads its value. The file columns are those of the table above, unchanged.

     *Updated 2026-11-09 for lazy values* (sub-step 2 below): every count is unchanged. `CursorList` asks for the first value of each run at its smallest key, and the file run is at the smallest key at every landing here, so the lazy cursor decodes the same values on demand that it decoded eagerly. Laziness saves a decode only for a run that is not at the smallest key, such as `second` in `cursor_list_reads_values_only_of_the_runs_at_its_smallest_key`; `RunMerge` (roadmap step 2) removes the rest.

     Six old entries are taken (`c5`, `o3`, `l7`, `l8`, `o4`, `l9`), against 22 value decodes. `O⁻` alone reads all seven file keys, and `c6` is decoded three times, as the end of `O⁻`'s range and as `L⁻`'s landing for `o4` and `o5`.
   - The path reads, `C⁺` and `J⁺`, probe their one exact key with `seek_key_exact` (`scan_entry`), as a native join probes its trace: a file run whose filter excludes the key is neither sought nor decoded, and nothing past the entry is read. `J⁺`'s probe of `(c5, o5)`, which only `Dlt` holds, is the one rejection. The range reads, `O⁻` and `L⁻`, keep `scan`, since the filters answer whole keys, not prefixes. Each probe still opens a cursor, which decodes the file run's first key, and its first value through `CursorList`. *Changed on 2026-11-04;* before, the path reads also used `scan`, and the totals were 17 seeks and 22, 25 and 21 decodes.
2. **Decode-free storage and the buffered scan session.** *Done 2026-11-09.* The sub-steps committed one by one:
   - **Prefix successor and per-domain width.**
     - `prefix_successor(prefix) -> Option<Vec<u8>>` in `merged_index/mod.rs`. It drops trailing `0xFF` bytes and increments the last remaining byte. It returns `None` when every byte is `0xFF`, meaning the range runs to the end of the index.
     - `width` on `Domain` in `trie.rs`, `KeyTrie::root_width()`, and a `root_width()` that panics on any trie without exactly one top-level node (*changed on 2026-11-09*; it returned an `Option` before, and a Rust `validate()` rejected such tries until 2026-11-10, when the compiler's validator became the only check). `MergedIndexRustEmitter` derives the width from the domain's Rust type (`OrderedKey::WIDTH`); Java's `KeyTrie` stores none. DECIMAL key domains keep failing to compile.
     - Regenerate `merged-index-col/src/generated.rs`.
     - *Exit:* unit tests for the empty prefix, carry, and all-`0xFF`; a property test that `k.starts_with(p)` holds exactly when `p <= k < succ(p)`; COL's widths are 8, 8, and 4.
   - **Lazy value positioning** in `FileIndexedWSetCursor` ([Remedies](shared-traversal.md#remedies)). *Done 2026-11-09.* The value cursor becomes a once-filled slot (`OnceCell`), and `count_value_decode` moves to where it is filled. A landing reads the key only; the first `val`, `val_valid`, `weight`, `map_times`, or value move fills the slot. A key move keeps the emptied cursor as the hint for the next first value. The `run_reads` tests pin the lazy counts; the COL baseline is unchanged (see the note under its table).
     - *Exit:* a landing alone decodes no value; the first `val()` counts one value decode and a second counts none; `step_val`, `seek_val`, and `weight` before `val` work; a clone works before and after the fill. The pinned counts of `todays_readers_run_reads_on_the_worked_transaction` are recomputed with each derivation in its comment, and the baseline table above is updated. The existing trace tests pass unchanged.
   - **`RunMerge`** in `merged_index/run_merge.rs`. *Done 2026-11-09.* `key`, `step`, `seek`, and `take`. `step` and `seek` read no value or weight; `take` sums weights per value and counts cancelled values. `seek` moves only the runs below its target and reports whether any moved. `exhaust` moves past every key without a read, for a skipped prefix that has no successor.
     - *Exit:* a property test against a consolidated brute force over several runs, file runs among them, with cross-run cancellations and replaced payloads; a counter test that stepping past entries counts no value or weight decode.
   - **`ScanSession`** in `merged_index/session.rs`, with `Maintain`, `PassTo`, `Collect`, `Skip`, the `Pass` knob, and `SessionCounts` ([The scan session](shared-traversal.md#the-scan-session)). *Done 2026-11-09.* `RunMerge` gained `exhaust` for a skipped prefix with no successor.
     - *Exit:* a property test against a brute-force reference that consolidates `Old`, groups `Dlt` by root, filters by prefix and `inputs`, and drops the rest of a rejected node's subtree. It ranges over three small tries (one with a two-byte root and two placements at the root node, one with no root placement), values near `0x00`, `0x01`, `0xFE`, and `0xFF` to force carry, multi-run snapshots with file runs, random `Dlt`, `inputs`, and rejection tests, and both passes. Counter tests show that positions outside `inputs`, and the position that ends a group, cost no value, weight, or payload decode; and pin the seek and step counts of the two passes on a six-group layout and on a rejected subtree. *Changed on 2026-11-09:* the `GroupStart` knob and the root probe were removed, and the proptest ran four knob combinations before. The session became `ScanSession<D: MergedIndexDefinition>`, and the proptest's three fixture tries became small definition types.
   - **COL counter tests, the A/B, and docs.** *Done 2026-11-09.* On the worked transaction in `merged-index-col/src/q3/tests.rs`, `Maintain({C, O, L})` over `CustomerOrdersLineitemDefinition`, which decodes through its `decode_payload`, under `Seek` pins the counts of [How the counters check the rules](shared-traversal.md#how-the-counters-check-the-rules) (`session_run_reads_on_the_worked_transaction`). An A/B test on the same kind of data, extended with untouched customers between touched ones and a synthetic segment filter, pins the run and session counters of both passes, derives each in a comment, and checks equal outputs (`the_passes_pass_untouched_groups_and_rejected_subtrees_in_different_ways`).
     - *Exit:* `CLAUDE.md` and `WORKFLOW.md` gain the new files, the knob, and test filters. The version-6 measurements predate lazy values; R5 timing of the passes belongs to step 6.
  5b. **Requested slices and selective routing.** *Done in code on 2026-11-12, except the pending items below* (decisions of 2026-11-12 in [decisions.md](decisions.md)). Each reader holds its driving delta's requests and keeps only the rows it requests ([The implementation: selective routing](shared-traversal.md#the-implementation-selective-routing), [Readers](shared-traversal.md#readers-step-5)).
   - `reader::capture` (phase A) replaces the session's reading of `Dlt`. `ScanSession` is a cursor over the touched groups (`new(old, touched, pass)`, `next_group`, `key`, `take_and_decode`, `step`). `reader::route` and Q3's hook scan each group once.
   - `GroupBuffers`, `on_group`, `reject`, `Projection`, `ReaderRuns`, `TouchedGroups`, `values_at`, and `RequestedRows` are gone. `ΔJ` and `J⁺` are computed at routing.
   - No driving delta is cloned: `C⁺`'s batch holds `ΔC`'s rows of the customers of `ΔO`.
   - The `requested_slice` read owner counts the joins' reads of the slices.
   - Also done 2026-11-12: the `partner_trace` read owner (native's trace reads, partner sampling included); routing's reads of `ΔC` and `ΔO` rows by a forward merge with the scan, under `requested_slice`; phase A's `Dlt` reads under `merged_index`; `collect` capturing every owner on the driver's thread; `delta_ms`, with phase A in it.
   - *Exit:*
     - Both transcript comparisons pass (`verify.sh`) on the merged tree.
     - A test asserts that `collect` leaves zero reads under `query` on the driver's thread.
     - The session and reader counter pins hold: kept entries are decoded once, unrequested entries cost a key read.
   - *Not done:* skipping the subtrees that no reader requests inside a touched group: the scan still reads a key at every position of a touched group ([deferred.md](deferred.md#whole-group-over-read-for-narrow-requests)).
6. **Measurement, new version.** *macOS part done 2026-11-11; Linux pending.* The version stayed 10 (decision of 2026-11-11). Step 5b bumps it to 11 (read owners `requested_slice` and `partner_trace`; `delta_ms`; measurement.md); the macOS smoke at version 11 ran on 2026-11-12 (measurement.md); Linux pending.
   - Done: the driver's `--session-pass seek|step` selects `Pass` (`configuration.session_pass`); `measure.py` forwards it and checks `index_session_counts`, `index_upkeep_run_reads`, and `profiled_cpu_ms`; `summarize.py` reports them. Every run reads one set of inputs in place and starts cold (decision of 2026-11-11).
   - Done: the macOS smoke run, both modes, both passes, three repetitions (measurement.md). All state stayed in memory. The passes collected the same entries; Step reached about 400,000 more positions than Seek and took about 30 ms more `ivm_ms` over six refreshes.
   - Pending: the Linux pilot and campaign, four arms (prefix truncation × pass; [WORKFLOW.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/LINUX-HANDOFF.md#campaign)). Counts: the eight pilots are 16 runs on the SSD, the campaign is 48 runs on the SSD, and the hard disk gets the eight pilots again as a probe (16 runs; decision of 2026-11-16). Measurement version 12 adds `block_expansions` and `storage_device`.
   - *Exit:* raw artifacts under `target/`; measurement.md reports base and IVM side by side.
7. **Beyond RF1/RF2.** *Done 2026-11-10, as part of step 4.* `prepare` supports customer updates, line items added to existing orders, orders that change customer, and orphan line items. *Exit:* `general_transcript` passes in both modes.
8. **Deferred:**
   - selective execution ([deferred.md](deferred.md#selective-execution));
   - plans derived from the spec;
   - more than one worker, sharded by `custkey`;
   - groupings, whose state the write-up leaves for later work.

## 2026-11-17: Pre-R3 gaps and the removed `--q3-merged-index` binding

Moved from the former `native-state-read-contracts.md`. The compiler hook was removed on 2026-11-13; the Q3 bindings and types that file held are in the harness's `CIRCUIT.md`.

### Compatibility gaps before R3 (archived)

On 2026-10-06, before R3 existed, the storage prototype differed from the native reads in key placement,
identity widths, payload representation, cursor representation, Lineitem placement, and runtime integration.
R3 closed the runtime-integration gap (see [R3 binding](#r3-rf1rf2-binding) and
[reconstruction.md](reconstruction.md)). The row-by-row table is in
[history.md](history.md#2026-10-06-compatibility-gaps-before-r3); col-storage.md is current.

R3 had to implement the four bindings with the documented tuples, signed
weights, key and value order, absence, and before/after visibility, and to stop
the independent accumulation of the replaced traces. Base input storage, the
secondary merged index, materialized `V`, and unreplaced state remain separately
accounted for. R4 owns the broader correctness verification and R5 the
maintenance measurements.

#### R3 RF1/RF2 binding

*Updated 2026-11-10: the hand-written view circuit replaced the compiler's emitter, the placement map is gone, and general deltas are supported (see [reconstruction.md](reconstruction.md)). The text below describes the original RF1/RF2 binding, which the readers' keys have since replaced: the readers take their customer from the delta key and read the touched groups in one scan session.*

The opt-in compiler (`--q3-merged-index`) emitted calls to the COL library's
[Q3 readers](../../../sql-to-dbsp-compiler/experiments/merged-index-col/src/q3.rs).
The manually maintained index has already received its physical updates when Q3
starts. Its immutable before/after snapshots and a temporary order/customer map
from the current Orders delta serve all four readers. RF2 discovery supplies the
complete deleted Orders and Lineitems; it runs outside Q3. Customer deltas occur
only during initial loading, before any Orders are loaded. RF1/RF2 change only
complete order/line groups. Referential integrity is assumed; orphan lines,
customer updates, replacements, and key moves are outside this prototype.

`B⁺` is reconstructed by joining Orders and Customer on the fly. For each
distinct order key requested by `ΔL`, the reader obtains customer placement from
the current Orders delta, seeks that Customer and Order in the after snapshot,
combines their payloads, and multiplies their signed weights. It emits the exact
`B` projection keyed by order ID; no accumulated `B` relation is maintained.
The current reader repeats Customer seeks for different requested orders sharing
a customer. Reusing those reads is another future optimization.

The supplied-state join retains transaction delta collection and `JoinTrace`,
including its flush barrier and signed-delta equations. It omits the four
independent historical join traces. At that barrier, readers decode and copy only
the requested accumulated ranges into typed native batches. Native batch sorting
and snapshot cursors establish tuple order, consolidate matching complete tuples,
and omit zero weights. Source snapshots share batch references without copying
source rows. Temporary reconstructed rows are released when the join finishes.

This requested-range materialization is suboptimal. Its copying, decoding, and
possible sorting are included in Q3 maintenance time. A future direct reconstructed
cursor could avoid temporary batches while preserving the same read contract.
Native memory/file policy applies to these batches; a hard memory cap is not claimed.

Complete circuit stacking is deferred. A future upstream maintenance circuit
would publish source views before Q3 consumes deltas and would coordinate its
checkpoint boundary with Q3. The current manual prototype tests native save/restore
at a completed boundary; it does not implement a cross-circuit recovery protocol.

## 2026-11-17: Dated smoke results from the former `measurement.md`

Moved when `measurement.md` was split. Each section is a snapshot, not authoritative, and its figures are not comparable with current ones. Current definitions are in the harness's `MEASUREMENT.md`.

### Run reads, version 5 smoke (2026-11-04)

One run per workload and mode on macOS, revision `78b10002b`, with the version 4 smoke settings (`--batch 512 --cache-bytes 33554432 --memory-target-bytes 536870912 --runs 1 --no-verify --setup-full-compaction --front-code-byte-keys --elide-value-headers`). Native joins read through run cursors (`native_join_fetch: false`; [decisions.md](decisions.md#2026-11-04-native-joins-read-through-run-cursors)). These are smoke figures: one repetition, no memory pressure, and no claim of cost or saving.

- **Inputs.** dbgen at `-s 0.01` (1,500 Customers, 15,000 Orders, 60,175 Lines; `target/r5-sf001-dbgen/`), prepared by `prepare_inputs.py --seed 1 --pairs 3` into `target/r5-sf001-oldest-inputs/`. Each RF1 inserts 10 Orders (37, 44 and 37 Lines). Each RF2 deletes the next 10 lowest base Order keys, keys 1 to 102 in all, in both workloads. Clustered RF1 put its 10 Orders on 8 to 10 Customers, so at this scale it is barely clustered. The sequence is RF1, RF2, three times.
- **Artifacts.** `target/r5-v5-cursor-join/{clustered,scattered}/manifest.json`, with each run's `driver.json` beside it. An earlier run with the native join fetch on is in `target/r5-v5-oldest-rf2/`; its reconstructed index reads are identical, refresh for refresh.
- **The old smoke inputs** (`target/r5-smoke-inputs/`) are not used: their RF2 deleted the Orders their RF1 had inserted.

The view worker's run reads, summed over the sequence and the owners (`sequence.view_run_reads`), with the process's backend reads:

| Workload | Mode | Seeks | Filtered probes | Key decodes | Value decodes | Weight decodes | Backend reads |
| --- | --- | --- | --- | --- | --- | --- | --- |
| clustered | native | 476 | 60 | 194 | 452 | 436 | 8 |
| clustered | reconstructed | 1,120 | 0 | 1,146 | 1,146 | 1,060 | 175 |
| scattered | native | 480 | 60 | 198 | 456 | 440 | 7 |
| scattered | reconstructed | 1,140 | 0 | 1,146 | 1,146 | 1,052 | 179 |

What the counts show, each against the counting rules:

- **Seeks.** Reconstructed mode seeks about 2.4 times as often. Two causes follow from the code. `J⁺` rebuilds `J` from two index entries, so it scans twice per Order, where native probes its stored `J` trace once. The index also takes a run on every transaction that touches any of its tables, while a native trace takes one only when its own input changes, and every scan seeks every run. In both modes the seeks grow with the run count, since nothing compacts after setup.
- **Filtered probes.** Native has 20 per RF1. That is consistent with one probe for each of the 10 new Orders in each of the two traces keyed by Order, answered by the file run's filter without a seek; the counters do not name the trace. RF2 deletes existing Orders, so it has none. The index's `scan` seeks without asking a filter, so reconstructed mode counts none.
- **Key decodes.** Reconstructed mode decodes about 5.8 to 5.9 times as many keys. A native trace groups the values of a key under one key, so a probe decodes one key and then its values. Every index entry has its own folded key, and a scan steps from key to key. Each scan also decodes the file run's first key when it opens its cursor, and the key past its range when it ends.
- **Value and weight decodes.** Reconstructed mode decodes about 2.5 times as many values. RF2 decodes about three times what RF1 does in reconstructed mode (`L⁻` 56 to 63 values per RF2 against 20 per RF1), because RF2 deletes Orders whose entries are in the file run, while RF1's new Orders live in memory.
- **`O⁻` reads nothing.** RF1 and RF2 change no Customers, so `ΔC` is empty.
- **The non-index view reads.** In reconstructed mode, the rest of the view circuit seeks 18 to 20 times per refresh under `query`, with no decodes: the joins over the readers' in-memory batches.
- **Backend reads.** They are process-wide, so they include the base circuit and the index upkeep. Native reads 7 or 8 blocks, almost all in the first refresh. Reconstructed mode reads in every refresh, 12 to 46 blocks. In these runs (two runtimes) the cause was the cache placement explained under [Exact path reads](#exact-path-reads-2026-11-05); version 6 changed it.

#### Exact path reads (2026-11-05)

The same inputs and settings at revision `6be8e8108`, where the path readers `C⁺` and `J⁺` probe their exact key with `seek_key_exact` ([decisions.md](decisions.md)); `O⁻` and `L⁻` still range-scan. Artifacts: `target/r5-v5-exact-path/{clustered,scattered}/manifest.json`. Native counts are unchanged; its backend reads differ by a few blocks between runs.

| Workload | Mode | Seeks | Filtered probes | Key decodes | Value decodes | Weight decodes | Backend reads |
| --- | --- | --- | --- | --- | --- | --- | --- |
| clustered | native | 476 | 60 | 194 | 452 | 436 | 6 |
| clustered | reconstructed | 1,090 | 30 | 559 | 559 | 559 | 169 |
| scattered | native | 480 | 60 | 198 | 456 | 440 | 9 |
| scattered | reconstructed | 1,110 | 30 | 567 | 567 | 566 | 192 |

Per reader, clustered, against the cursor-join run:

| Reader | Seeks | Filtered probes | Key decodes | Value decodes | Weight decodes |
| --- | --- | --- | --- | --- | --- |
| `C⁺` | 254 → 254 | 0 → 0 | 315 → 112 | 315 → 112 | 289 → 112 |
| `J⁺` | 540 → 510 | 0 → 30 | 594 → 210 | 594 → 210 | 534 → 210 |
| `L⁻` | 210 → 210 | 0 → 0 | 237 → 237 | 237 → 237 | 237 → 237 |

- **Seeks.** `J⁺` loses 30, one per new Order: the node `(c, o)` of an Order that RF1 inserts exists only in the transaction delta, so the file run's filter rejects the probe without a seek. Reconstructed mode now seeks about 2.3 times as often as native.
- **Decodes.** The path readers decode about a third of what they did, since an exact probe no longer steps past its key to end a range. The step past cost most in RF2: the merged cursor over the runs (`CursorList::step_key`) skips keys whose weights cancel, reading each one's weight, and in the after snapshot the deleted Order and its Lines cancel between the file run and the transaction delta. Stepping past the Customer therefore walked that whole deleted subtree before it reached a live key that ended the range. Per probe, `C⁺` decoded 3 keys in RF1 and 7 to 8 in RF2 (24 for 8 probes, 76 to 83 for 10); with exact probes, it decodes 2 in both. Reconstructed mode now decodes about 2.9 times native's keys and 1.2 to 1.3 times its values and weights. `L⁻` is unchanged.
- **Backend reads are not comparable (versions 5 and earlier).** A storage writer puts each block it writes into its own thread's buffer cache. Native's traces are written and read inside the view runtime, so its probes find their blocks cached. `ManualIndex` writes the index on an auxiliary thread bound to the base runtime's cache, and the view worker reads it through the view runtime's cache, which never received those blocks; each refresh probes other Customers and Orders, so it keeps reaching blocks that cache has not held. A diagnostic run (`target/diag-backend-reads/`, instrumented driver, not committed) showed that:
  - with a 256 MiB cache per runtime, both about half full, reconstructed mode still read 12 to 28 blocks per refresh (`target/r5-v5-cache256/clustered/`);
  - every backend read fell inside the view transaction;
  - after one full scan of the index in the view runtime before the window, backend reads were 0 in every refresh, and the decode counts were identical, refresh for refresh.

  Version 6 removed that cause: one runtime has one cache, which the index's writes warm and the view worker's reads use. Backend reads fell but did not reach native's; see [One runtime](#one-runtime-version-6-2026-11-05).

### One runtime (version 6, 2026-11-05)

The base and view circuits run in one DBSP runtime, with one storage directory and one buffer cache of `--cache-bytes` ([decisions.md](decisions.md#2026-11-05-one-runtime-for-the-base-and-view-circuits)). The same inputs and settings as the [exact path reads](#exact-path-reads-2026-11-05) run, at revision `7e73c0b4f` (clean tree), one run per workload and mode on macOS. These are smoke figures: one repetition, no memory pressure, and no claim of cost or saving.

Artifacts: `target/r5-v6-one-runtime/{clustered,scattered}/manifest.json`, with each run's `driver.json` beside it. The baseline is `target/r5-v5-exact-path/`. A diagnostic run at a larger cache is in `target/r5-v6-one-runtime-cache256/clustered/`. All of them are local-only.

The view worker's run reads, summed over the sequence and the owners, with the process's backend reads. The backend reads of version 5 (two runtimes) are in the last column for comparison:

| Workload | Mode | Seeks | Filtered probes | Key decodes | Value decodes | Weight decodes | Backend reads | Backend reads, v5 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| clustered | native | 476 | 60 | 194 | 452 | 436 | 7 | 6 |
| clustered | reconstructed | 1,090 | 30 | 559 | 559 | 559 | 72 | 169 |
| scattered | native | 480 | 60 | 198 | 456 | 440 | 11 | 9 |
| scattered | reconstructed | 1,110 | 30 | 567 | 567 | 566 | 83 | 192 |

- **Run reads are unchanged.** Every seek, filtered probe, and decode count equals the version 5 figure, in both modes and both workloads. Version 6 changes where the blocks are cached and which thread writes them, not what a probe reads from a run. In reconstructed mode the `merged_index` owner accounts for all the filtered probes and decodes and for most of the seeks (clustered 974 of 1,090, scattered 990 of 1,110); the rest are other owners' reads of the view circuit. Native's `merged_index` owner reads nothing.
- **The shared cache is tested directly.** The DBSP tests `init_circuits_read_each_others_cached_blocks` and `separate_runtimes_miss_each_others_blocks` (`crates/dbsp/src/circuit/dbsp_handle.rs`) back the claim that a block one circuit wrote is a cache hit for the other. A file-backed batch built in one circuit's step is read in full by the other's step: on one runtime the file's foreground cache accesses are hits and none is a miss; on two runtimes (the version 5 arrangement) the reader misses. The harness test `index_file_runs_land_in_the_view_workers_cache` (`q3-ivm-baseline/src/main.rs`, reconstructed feature) covers the real path, and it isolates the index thread's own cache binding. Under memory pressure, which the test forces with a tiny memory target, `ManualIndex`'s thread spills its one appended run to a file; the test then checks that the thread and the view worker hold the same cache object (`Arc::ptr_eq`), that every block of that file is in the worker's cache by direct lookup (`Batch::file_block_locations`, `BufferCache::get`), and that the cache is below capacity, so nothing was evicted. It counts no hits or misses, which eviction could change. The binding is also enforced in production: `ManualIndex::new` fails if its thread is not on the runtime's foreground cache, and the control `manual_index_refuses_a_thread_off_the_runtimes_cache` builds an unbound thread (test-only) to show that it does. The earlier test of this claim ran after full compaction, so it read files that the background merger wrote; it passed with the binding removed, and it is gone.
- **Backend reads in reconstructed mode fell, but not to native's level.** Clustered went from 169 to 72, scattered from 192 to 83. Native's moved by a few blocks (6 to 7, 9 to 11), as it did between version 5 runs. Reconstructed mode still reads more than native, per refresh:

  | Mode | Clustered, per refresh | Scattered, per refresh |
  | --- | --- | --- |
  | native | 5, 1, 1, 0, 0, 0 | 10, 0, 1, 0, 0, 0 |
  | reconstructed | 21, 14, 12, 9, 6, 10 | 24, 14, 14, 11, 11, 9 |

- **What the remaining reads depend on.** The cache is nearly full at the start of the window: 31.9 MB of 33.6 MB (95 percent) in both modes and both workloads, and its occupancy hardly moves across the window. In the diagnostic run (clustered, `--cache-bytes 268435456`) the cache held 99.0 MB (native) and 96.8 MB (reconstructed), about 37 percent of capacity, and both modes made **0 backend reads** in every refresh, with run reads identical to the table above. So the remaining reads at 32 MiB depend on the cache's capacity; they are not the placement effect of version 5. The counters do not separate a first read of an uncached block from a re-read after an eviction, so which of the two the remaining reads are is not measured. The diagnostic ran for clustered only.

The per-phase timings, summed over the six refreshes, in milliseconds (`ivm_ms` equals `q3_ms`; `index_ms` is inside `base_ms`; `profiled_cpu_ms` is the CPU profiler's step CPU time of each circuit, definitions under [Report fields](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/MEASUREMENT.md#report-fields)):

| Workload | Mode | `base_ms` | `index_ms` | `ivm_ms` | Profiled base | Profiled q3 |
| --- | --- | --- | --- | --- | --- | --- |
| clustered | native | 0.60 | 0.00 | 1.11 | 0.55 | 1.03 |
| clustered | reconstructed | 0.91 | 0.21 | 1.86 | 0.65 | 1.81 |
| scattered | native | 0.62 | 0.00 | 1.06 | 0.57 | 1.02 |
| scattered | reconstructed | 0.98 | 0.24 | 2.23 | 0.66 | 2.10 |

Each refresh takes well under a millisecond, from one run, so these figures show only that the phases are told apart and that the profiler's times track the phase timers' (`profiled_cpu_ms.q3` is within 0.15 ms of `q3_ms` in every row). Nothing here is a cost comparison; the Linux campaign measures one. Version 5 recorded no profiler times, and its phase timings were taken with two runtimes, so they are not tabulated against these.

### Version 10 smoke (2026-11-11)

Three repetitions per workload, mode, and session pass on macOS, at revision `36bb00a2e` (clean tree, `git_status_short` empty in all four manifests). The settings are the current defaults (decision of 2026-11-11): `--batch 512 --cache-bytes 33554432 --memory-target-bytes 536870912 --runs 3 --no-verify --front-code-byte-keys --elide-value-headers`, with no `--setup-full-compaction`, and `--session-pass seek` or `step`. The inputs are those of the [version 5 smoke](#run-reads-version-5-smoke-2026-11-04) (`target/r5-sf001-dbgen/`, `target/r5-sf001-oldest-inputs/`). These are smoke figures: no memory pressure, and no claim of cost or saving.

Artifacts: `q3-ivm-baseline/target/r5-v10-smoke/{seek,step}-{clustered,scattered}/manifest.json`, with each run's `driver.json` under `runs/`, and `summary/summary.{json,md}` over all four. They are local-only.

What the placement was:

- **Nothing reached disk.** `load.observed_storage_bytes` and `sequence.observed_storage_bytes` are 0, and backend writes and reads are 0, in every run. Every refresh's pressure samples are `low`. With DBSP's default placement (version 7), runs below `min_storage_bytes` (10 MiB) stay in memory at low pressure, so all state was in memory: the index's `Old` was 11 in-memory runs of about 7.7 MB at the first refresh. The run reads below are therefore all `memory_*` reads; every file-decode count is 0.
- **The counts are deterministic.** All three repetitions of each workload, mode, and pass gave identical run reads, session counts, and upkeep reads. Native's counts are identical across the two passes, as they must be, since native has no scan session.
- **Versions 5 and 6 are not comparable.** They ran with setup compaction and, through version 6, with the `min_storage_bytes: Some(0)` override, so their runs were files. This run has neither.

The sequence's counters, summed over the six refreshes (each the same in every repetition). DBSP operator reads sum the view worker's owners; index reads are the scan session's (`index_run_reads.session`); upkeep is `index_upkeep_run_reads`. File decodes and filtered probes are 0 throughout and are left out.

| Workload | Mode | Pass | DBSP operator seeks | DBSP operator memory key / value / weight reads | Index seeks | Index memory key / value / weight reads | Upkeep seeks | Upkeep memory key / value / weight reads |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| clustered | native | either | 858 | 648 / 1,079 / 1,079 | — | — | — | — |
| clustered | reconstructed | seek | 442 | 928 / 1,233 / 1,233 | 368 | 5,118 / 4,709 / 4,709 | 210 | 520 / 177 / 177 |
| clustered | reconstructed | step | 442 | 928 / 1,233 / 1,233 | 0 | 411,205 / 4,709 / 4,709 | 210 | 520 / 177 / 177 |
| scattered | native | either | 870 | 656 / 1,083 / 1,083 | — | — | — | — |
| scattered | reconstructed | seek | 450 | 929 / 1,241 / 1,241 | 394 | 4,547 / 4,115 / 4,115 | 210 | 518 / 177 / 177 |
| scattered | reconstructed | step | 450 | 929 / 1,241 / 1,241 | 0 | 418,176 / 4,115 / 4,115 | 210 | 518 / 177 / 177 |

The scan session's moves (`index_session_counts`), summed over the six refreshes:

| Workload | Pass | Groups | Cursor opens | Logical seeks | Logical steps | Positions | Payload decodes, `Old` / `Dlt` | Cancelled |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| clustered | seek | 56 | 12 | 56 | 4,414 | 4,476 | 4,414 / 295 | 0 |
| clustered | step | 56 | 12 | 0 | 410,598 | 410,604 | 4,414 / 295 | 0 |
| scattered | seek | 60 | 12 | 60 | 3,820 | 3,886 | 3,820 / 295 | 0 |
| scattered | step | 60 | 12 | 0 | 417,584 | 417,590 | 3,820 / 295 | 0 |

What the counts show, against the counting rules:

- **The passes collect the same entries.** Groups and payload decodes are equal between the passes, and so are the index's value and weight reads, which equal the payload decodes (4,414 + 295 = 4,709 in clustered). Each buffered entry reads its value once.
- **Seek makes one logical seek per touched group** (56 and 60) and steps only through the touched groups' entries: its steps equal its `Old` payload decodes.
- **Step walks every position of `Old`.** It makes no seek and reaches about 68,000 to 70,000 positions per refresh; the loaded base tables hold 76,675 rows (1,500 Customers, 15,000 Orders, 60,175 Lines), one index entry each. Each position costs one key read, so its key reads grow by the same amount (5,118 to 411,205 in clustered).
- **Upkeep reads are base maintenance** (`prepare`'s reads of `Old`) and do not depend on the pass.

Per-phase timings, the median over three repetitions, with the minimum and maximum, summed over the six refreshes, in milliseconds. `index_ms` is inside `base_ms`, and `ivm_ms` equals `q3_ms`. Profiled times are the CPU profiler's step CPU time of each circuit. The scan session's collection runs on the driver's thread, outside the view circuit's steps, so it is in `ivm_ms` but not in the profiled q3 time.

| Workload | Mode | Pass | `base_ms` | `index_ms` | `forward_ms` | `ivm_ms` | Profiled base | Profiled q3 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| clustered | native | seek arm | 0.64 (0.57–0.66) | 0.00 | 0.84 (0.80–0.86) | 1.10 (1.02–1.11) | 0.57 | 1.03 |
| clustered | native | step arm | 0.69 (0.54–0.72) | 0.00 | 0.85 (0.77–0.86) | 1.14 (0.95–1.23) | 0.61 | 1.07 |
| clustered | reconstructed | seek | 1.17 (1.10–1.24) | 0.49 (0.48–0.60) | 0.32 (0.31–0.33) | 2.99 (2.83–3.01) | 0.56 | 0.80 |
| clustered | reconstructed | step | 1.22 (1.19–1.27) | 0.51 (0.50–0.58) | 0.35 (0.35–0.37) | 33.38 (33.26–33.92) | 0.62 | 0.94 |
| scattered | native | seek arm | 0.62 (0.59–0.65) | 0.00 | 0.81 (0.81–0.84) | 1.01 (0.99–1.12) | 0.55 | 0.96 |
| scattered | native | step arm | 0.73 (0.67–0.82) | 0.00 | 0.84 (0.83–0.87) | 1.20 (1.12–1.26) | 0.62 | 1.12 |
| scattered | reconstructed | seek | 1.15 (1.09–1.20) | 0.50 (0.47–0.53) | 0.33 (0.31–0.34) | 2.69 (2.60–2.87) | 0.57 | 0.73 |
| scattered | reconstructed | step | 1.24 (1.18–1.40) | 0.56 (0.50–0.62) | 0.34 (0.34–0.37) | 34.66 (34.02–34.69) | 0.62 | 0.95 |

- **Base and IVM side by side.** With Seek, reconstructed mode's `base_ms` is about 1.2 ms against native's 0.6 ms (its `index_ms` is about 0.5 ms of it), and its `ivm_ms` about 2.7 to 3.0 ms against native's 1.0 to 1.1 ms. At this scale these are smoke figures from in-memory state, a few milliseconds over six refreshes, and not a cost comparison.
- **Step costs about 30 ms more `ivm_ms`** than Seek in both workloads, with about 400,000 more key reads; the profiled q3 time rises by 0.14 to 0.22 ms only, since the walk is in the collection, outside the circuit's steps. Which part of `ivm_ms` the collection takes is not timed apart, so no figure for it is given here.
- **`forward_ms`** is smaller in reconstructed mode, which hands whole batches to the input handles, than in native mode, which feeds rows.
- **No pressure, no disk.** These runs show nothing about file runs, the buffer cache, or beyond-memory behavior. The Linux campaign measures those.

### Version 11 smoke (2026-11-12)

The settings and inputs of the [version 10 smoke](#version-10-smoke-2026-11-11), at revision `fba8abbd9`: three repetitions per workload, mode, and session pass. `git_status_short` lists only the untracked agent worktrees under `.claude/`. Version 11 changes what is read and how it is attributed: readers keep only the requested rows and decode only those, and the partner reads have their own owners, `partner_trace` (native) and `requested_slice` (reconstructed), apart from the DBSP operator reads (`query`). It also adds `delta_ms`. As before, nothing reached disk, pressure stayed `low`, and every repetition gave identical counts.

Artifacts: `q3-ivm-baseline/target/r5-v11-smoke/{seek,step}-{clustered,scattered}/manifest.json` and `summary/`, local-only.

Counters, summed over the six refreshes. Reads are memory reads, key / value / weight; file decodes are 0 throughout. Requested-slice reads are the view worker's plus the driver thread's routing reads (`driver_run_reads.requested_slice`: 0 seeks, 56 / 60 / 60 clustered and 60 / 60 / 60 scattered).

| Workload | Mode | Pass | DBSP operator seeks; reads | Partner reads: seeks; reads | Index seeks; reads | `Old` decodes | Upkeep seeks; reads |
| --- | --- | --- | --- | --- | --- | --- | --- |
| clustered | native | either | 0; 470 / 622 / 622 | `partner_trace` 858; 178 / 457 / 457 | — | — | — |
| clustered | reconstructed | seek | 0; 248 / 340 / 340 | `requested_slice` 176; 160 / 471 / 471 | 368; 5,118 / 498 / 498 | 203 | 210; 520 / 177 / 177 |
| clustered | reconstructed | step | 0; 248 / 340 / 340 | `requested_slice` 176; 160 / 471 / 471 | 0; 411,205 / 498 / 498 | 203 | 210; 520 / 177 / 177 |
| scattered | native | either | 0; 474 / 622 / 622 | `partner_trace` 870; 182 / 461 / 461 | — | — | — |
| scattered | reconstructed | seek | 0; 252 / 340 / 340 | `requested_slice` 180; 168 / 475 / 475 | 394; 4,547 / 502 / 502 | 207 | 210; 518 / 177 / 177 |
| scattered | reconstructed | step | 0; 252 / 340 / 340 | `requested_slice` 180; 168 / 475 / 475 | 0; 418,176 / 502 / 502 | 207 | 210; 518 / 177 / 177 |

What the counts show:

- **Native is unchanged.** Its two owners sum to version 10's view reads (858 seeks; 648 / 1,079 / 1,079 clustered). All its seeks are partner seeks.
- **The index reads only what is kept.** Groups, positions, seeks, and key reads equal version 10's, since the session still visits every position of a touched group (the [deferred over-read](deferred.md#whole-group-over-read-for-narrow-requests)). `Old` payload decodes fall from 4,414 to 203 (clustered) and from 3,820 to 207 (scattered); the index's value reads, 498, are those 203 plus `Dlt`'s 295, read once in phase A.
- **The partner reads match in kind.** Reconstructed mode's requested-slice value reads (471 and 475) are close to native's partner-trace value reads (457 and 461): both count one read per joined pair, since a join rereads a partner key's values for each delta value. Native seeks about five times as often (858 against 176), one seek per run of its traces.
- **The DBSP operator reads are the shared work.** Reconstructed mode has no operator seeks, and its operator reads (248 / 340) are below native's (470 / 622). The two circuits differ in their operators, so the rows are not expected to be equal.
- **Upkeep is unchanged** and does not depend on the pass.

Per-phase timings, the median over three repetitions with the minimum and maximum, summed over the six refreshes, in milliseconds. From version 11, `ivm_ms = delta_ms + q3_ms`, so native's `ivm_ms` now includes its JSON conversion (`delta_ms`), which version 10 booked in `forward_ms`.

| Workload | Mode | Pass | `base_ms` | `forward_ms` | `delta_ms` | `q3_ms` | `ivm_ms` |
| --- | --- | --- | --- | --- | --- | --- | --- |
| clustered | native | either | 0.55 (0.54–0.69) | 0.29 | 0.51 | 0.95 (0.95–1.11) | 1.47 (1.46–1.64) |
| clustered | reconstructed | seek | 1.25 (1.10–1.42) | 0.32 | 0.11 | 1.81 (1.64–2.06) | 1.92 (1.74–2.17) |
| clustered | reconstructed | step | 1.03 (1.01–1.07) | 0.30 | 0.10 | 31.74 (31.70–31.81) | 31.84 (31.79–31.91) |
| scattered | native | either | 0.52 (0.50–0.54) | 0.28 | 0.51 | 0.90 (0.89–0.91) | 1.41 (1.40–1.42) |
| scattered | reconstructed | seek | 0.94 (0.91–0.99) | 0.28 | 0.09 | 1.33 (1.32–1.42) | 1.41 (1.40–1.51) |
| scattered | reconstructed | step | 1.09 (0.96–1.10) | 0.30 | 0.10 | 32.83 (32.42–32.95) | 32.93 (32.52–33.06) |

Caveats: these are smoke figures at SF 0.01 with no memory pressure and no file runs, so they support no claim of cost or saving. Reconstructed `ivm_ms` with Seek fell from about 3.0 (version 10, clustered) to 1.9, with the drop in `Old` decodes, but version 10 and 11 differ in what `ivm_ms` covers, so the two are not directly comparable. Step's cost is its 410,000 key reads and is unchanged.

### Record size (2026-10-16 to 2026-10-19)

With equal compaction and the POSIX fix, the merged index held fewer rows than the four join traces (2,624 against 3,136, because it does not repeat Orders ⋈ Customer) but took 1.68 times the bytes. Splitting each run's data-block bytes by level (`block_stats`, reported as `key_level` and `value_level` in `run-contents.json`) located the gap, and four changes followed. Each step ran all three workloads, three repetitions per mode, with `--setup-full-compaction --diagnostics`. Every repetition of every workload gave the same bytes, and native mode did not change between steps.

The table keeps the rows that match the current configuration. The rows for the byte-string and typed-value steps are in [history.md](history.md#2026-10-16-to-2026-10-19-record-size-typed-value-steps).

| Step (artifact under `target/`) | Merged-index file | Key-level data | Value-level data | Disk usage at window end |
| --- | --- | --- | --- | --- |
| Packed values, prefix-truncated keys (`r5-packed-values/`) | 151,040 B | 33,280 B | 107,008 B | 640,512 B |
| Packed values, prefix-truncated keys, elided value headers (`r5-packed-elided/`) | 111,104 B | 33,280 B | 67,072 B | 600,576 B |
| Native: four join traces | 188,928 B | 13,312 B | 168,448 B | 678,400 B |

- **Key level.** Every folded key is distinct, so the key level stores all 2,624 keys and shares nothing between them; before prefix truncation it took about 50 B per key (132,096 B; see history.md). Prefix truncation (`front_code.rs`, `--front-code-byte-keys`) stores each key as the length of the prefix it shares with the previous key and the rest of its bytes. It cut the key level to 33,280 B, about 12.7 B per key. The native Lineitem trace's key level takes 11 B per key (5,632 B for 512 keys). In every run the writer prefix-truncated 17 blocks, with no layout or size fallbacks.
- **Packed values.** The next section changes the stored value. The six refreshes still stay in memory, so the window shows no writes in either mode.
- **Retired steps.** Storing `SourcePayload` itself made the value level larger, not smaller, and prefix truncation alone left the merged index 1.37 times the bytes of the four join traces. Both comparisons, and the value-level analysis behind the decision to pack, are in [history.md](history.md#2026-10-19-value-level).

#### Packed, untagged values (2026-10-19 to 2026-10-20)

The stored value is now the packed bytes of `encode_payload` in a `Vec<u8>`, decoded with the row's folded key (see col-storage.md). Three changes are attributed separately, as [decisions.md](decisions.md) records:

1. **The enum.** The stored `SourcePayload` carried a tag and room for the largest variant.
2. **The byte-string header.** Every `Vec<u8>` value pays a 16 B `ArchivedVec` root. `--elide-value-headers` drops it on disk and rebuilds it on read (`front_code.rs`, `encode_values`). It does not depend on what the bytes encode.
3. **Packing.** The fields are written back to back, little-endian, with no padding. Strings are a LEB128 length and UTF-8 bytes, and decimals take the narrowest integer that holds their precision.

##### Layout files

`layout_tests.rs` wrote, for each source, 2,048 rows in the merged index's layout (folded key, then value and weight), once per encoding, with no compression. Each figure is the value-level data-block bytes of the file, read back from it. The artifact is `target/r5-packed-values/layout-tests.txt` (test `value_level_bytes_by_encoding`, which pinned these numbers). At roadmap Step 5 that test was reduced to the packed and native measurements, now `packed_value_bytes_against_native_lineitem`; the retired typed and `rkyv` figures are recorded in col-storage.md, and the full table with those columns is in [history.md](history.md#2026-10-20-layout-files-typed-and-rkyv-columns).

| Source | Packed | Packed, elided |
| --- | --- | --- |
| Customer | 82,944 B (40.50 B per row) | 46,592 B (22.75) |
| Orders | 66,560 B (32.50) | 37,376 B (18.25) |
| Lineitem | 82,944 B (40.50) | 51,712 B (25.25) |

The native baseline is the `Tup4` value of the generated circuit's Lineitem join trace, written with four lines per order key: 132,608 B (64.75 B per row). That file equals the real native trace's value level in `target/r5-front-coded/` (132,608 B). The native value is not a byte string, so elision does not apply to it.

##### R5 smoke runs

`measure.py --cache-bytes 33554432 --memory-target-bytes 536870912 --batch 512 --runs 3 --no-verify --diagnostics --setup-full-compaction --front-code-byte-keys`, with `--elide-value-headers` added for the second campaign. Inputs are the three smoke workloads of `target/r5-smoke-inputs/` (large-group, clustered, scattered). The artifacts are `target/r5-packed-values/` and `target/r5-packed-elided/`, each with a `summary.json` of the per-run figures.

- Every repetition of every workload gave the same bytes in both modes, and the three workloads agree with each other.
- The native figures equal those of `r5-front-coded/` in all six campaigns (`target/r5-packed-values/native-sanity.txt`): four join traces of 188,928 B, with 13,312 B of key level, 168,448 B of value level, and 678,400 B of disk usage at the window end.
- Key prefix truncation wrote the same 17 blocks as before, with no fallbacks. Value-header elision wrote 13 blocks. It had no layout fallback and one size fallback, a block that elision would not have shrunk by a 512-byte sector.
- The table above has the merged index's rows. Packed values changed only the value level, from 212,992 B to 107,008 B. Elision changed it again, to 67,072 B.

##### Three comparisons

Each compares measured figures. None infers an unmeasured quantity.

The comparisons below cite `rkyv`-bytes and typed-value figures; those are in [history.md](history.md), together with the enum comparison.

- **Structure, with the same encoding.** The merged index's Lineitem value as `rkyv` bytes took 148,992 B in its layout file, against 132,608 B for the native value. With the header elided it took 120,832 B, less than the native value in these files. The two files differ in shape (one value per folded key, against four lines per order key), and nothing here separates that from the header.
- **Byte-string header.** Elision shrank every byte-string layout file: `rkyv` bytes from 82,944, 66,560, and 148,992 B to 56,832, 37,376, and 120,832 B, and packed bytes from 82,944, 66,560, and 82,944 B to 46,592, 37,376, and 51,712 B. In R5 it shrank the packed value level from 107,008 B to 67,072 B. It is a disk-format change that any byte-string value can use.
- **Packing.** Without elision, packing left the Customer and Orders layout files unchanged and shrank Lineitem's, from 148,992 B to 82,944 B. With elision, it left Orders unchanged and shrank Customer's from 56,832 B to 46,592 B and Lineitem's from 120,832 B to 51,712 B. In R5 the packed value level was 107,008 B, against 173,056 B for `rkyv` bytes in `r5-level-baseline/`. Key prefix truncation does not change the value level (`r5-typed-values/` and `r5-front-coded/` both have 212,992 B), so the two runs compare.

##### What packing does and does not show

Packing is neither a unique advantage of the merged index nor orthogonal to it.

- Its byte savings do not come from the merged structure, and native could reach a similar gain.
- The merged index makes packing cheap. Keys are unique, so values need only byte equality. The payload format is private. Every read goes through one decode point, `decode_payload`.
- In native, packing is costly. Values are sorted and compared within a key, so a packed form needs an order-preserving encoding or a decode before each comparison. Native types are shared by the whole program and by DBSP's storage format, so the change needs a new format version ([deferred.md](deferred.md#native-packing)).
- Decoding is not a differentiator: DBSP's cursors deserialize `rkyv` items in both modes.

No figure here is a native packing result, and no run here measures decoding CPU. Both stay unmeasured.

## 2026-11-17: Smoke results of measurement versions 2 to 4 from the former `RESULTS.md`

Moved from `sql-to-dbsp-compiler/experiments/q3-ivm-baseline/RESULTS.md`, read from commit `8456ad616`. These are macOS smoke runs of 2026-10-22 to 2026-10-26.

### R5 results, measurement version 4 (macOS smoke, 2026-10-26)

Version 4 maintains the merged index in the base runtime and books its upkeep in `base_ms`; `ivm_ms` is the view circuit alone. Everything else matches version 3. The revision is `572bd97a0`. These are smoke tests, not qualifying results, and the Linux-only window memory and cgroup fields are `null` by design.

- **Command:** the same as on 2026-10-22 and 2026-10-25.
- **Artifacts:** `target/r5-v4-timing/<workload>/manifest.json` and `target/r5-v4-timing/summary/summary.{json,md}`.
- **Correctness:** `verify.sh` passed at this revision, and the V transcripts are byte-identical to those from before the circuit split (SHA-256 `826d6d3f…` in both modes).

Each cell is the median of three repetitions, with the minimum and maximum in brackets. Times are milliseconds summed over the six refreshes. Base and IVM are reported side by side, as the reporting rule requires. `index` is part of `base` and is shown only as its breakdown. `forward` is harness overhead and belongs to neither.

| Workload | Mode | `base` | of which `index` | `ivm` | `forward` | Sequence wall |
| --- | --- | --- | --- | --- | --- | --- |
| large-group | native | 7.4 [7.2, 7.5] | 0 | 15.2 [14.8, 15.2] | 109.9 [109.0, 110.0] | 208.1 [204.4, 208.3] |
| large-group | reconstructed | 16.3 [16.2, 16.4] | 9.0 [8.9, 9.1] | 17.6 [17.0, 17.7] | 107.8 [107.6, 108.1] | 205.8 [205.2, 206.1] |
| clustered | native | 0.56 [0.53, 0.59] | 0 | 0.93 [0.90, 0.97] | 0.82 [0.80, 0.82] | 3.24 [3.18, 3.25] |
| clustered | reconstructed | 0.63 [0.63, 0.64] | 0.16 [0.16, 0.17] | 1.14 [1.14, 1.20] | 0.78 [0.78, 0.78] | 3.46 [3.45, 3.56] |
| scattered | native | 0.57 [0.56, 0.79] | 0 | 1.04 [1.03, 1.44] | 0.99 [0.99, 1.02] | 3.68 [3.67, 4.48] |
| scattered | reconstructed | 0.82 [0.82, 1.01] | 0.23 [0.21, 0.33] | 1.76 [1.73, 2.35] | 0.98 [0.97, 1.01] | 4.76 [4.64, 5.72] |

| Workload | Mode | Writes in the window | Storage at window end | Peak RSS (process lifetime) |
| --- | --- | --- | --- | --- |
| large-group | native | 0 | 678,400 B | 45,432,832 B |
| large-group | reconstructed | 0 | 600,576 B | 55,607,296 B |
| clustered | native | 0 | 678,400 B | 26,673,152 B |
| clustered | reconstructed | 0 | 600,576 B | 30,720,000 B |
| scattered | native | 0 | 678,400 B | 26,591,232 B |
| scattered | reconstructed | 0 | 600,576 B | 30,982,144 B |

- **Base.** Reconstructed `base` was higher than native `base` in every workload, and its `index` sub-phase is shown beside it. The base circuit's own time is not reported separately from the index here.
- **IVM.** Reconstructed `ivm` was higher than native `ivm` in every workload. Reconstructed IVM reads the index through snapshot readers and does no index upkeep, so no part of its time is maintenance of its own state.
- **Forwarding** still dominates large-group (about 108 to 110 ms of a 206 to 208 ms sequence). It is harness overhead.
- **Storage** matches versions 2 and 3 byte for byte. No writes fall inside the window.
- **No performance claim.** The runs are too short and too small to attribute the differences.

### R5 results, measurement version 3 (macOS smoke, 2026-10-25)

These runs use the two chained circuits, the refresh files read before the window, and the input page cache evicted after loading. The revision is `4d558a222`. Like the 2026-10-22 runs, they are smoke tests, not qualifying results: macOS enforces no memory limit, and most sequences last a few milliseconds. The Linux-only window memory and cgroup fields are `null` here by design. The phases are version 3's: `base_ms` leaves out the merged index and `combined_maintenance_ms` (`index_ms + q3_ms`) includes it. Version 4 reverses this (see "Timing definitions").

- **Command:** the same as on 2026-10-22: `measure.py --cache-bytes 33554432 --memory-target-bytes 536870912 --batch 512 --runs 3 --no-verify --setup-full-compaction --front-code-byte-keys --elide-value-headers`, once per workload, without `--diagnostics`.
- **Artifacts:** `target/r5-v3-timing/<workload>/manifest.json` and `target/r5-v3-timing/summary/summary.{json,md}`.
- **Correctness:** `verify.sh` passed at this revision. Its V transcripts are byte-identical to those from before the split (SHA-256 `826d6d3f…` in both modes).

Each cell is the median of three repetitions, with the minimum and maximum in brackets. Times are milliseconds; the phase columns are summed over the six refreshes. `index` is zero in native mode. `base` is reported beside `index + q3`, not added to it.

| Workload | Mode | Sequence wall | Sequence CPU | `base` | `forward` | `index` | `q3` | `index + q3` |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| large-group | native | 187.7 [185.3, 190.8] | 189.6 [187.4, 192.9] | 6.6 [6.5, 7.0] | 100.5 [98.5, 102.5] | 0 | 13.7 [13.3, 14.3] | 13.7 [13.3, 14.3] |
| large-group | reconstructed | 186.7 [185.3, 197.6] | 188.9 [188.0, 199.5] | 6.6 [6.5, 8.5] | 97.8 [96.5, 101.2] | 8.2 [8.1, 9.2] | 16.1 [16.0, 17.4] | 24.3 [24.1, 26.7] |
| clustered | native | 3.30 [3.12, 3.93] | 3.75 [3.29, 4.22] | 0.56 [0.52, 0.74] | 0.81 [0.81, 0.83] | 0 | 1.01 [0.91, 1.36] | 1.01 [0.91, 1.36] |
| clustered | reconstructed | 4.12 [3.76, 4.63] | 4.37 [3.86, 4.65] | 0.60 [0.55, 0.80] | 0.81 [0.81, 0.83] | 0.22 [0.21, 0.40] | 1.34 [1.23, 1.41] | 1.55 [1.44, 1.81] |
| scattered | native | 4.71 [3.73, 4.74] | 5.09 [4.17, 5.22] | 0.84 [0.65, 1.06] | 0.94 [0.86, 0.98] | 0 | 1.52 [1.23, 1.77] | 1.52 [1.23, 1.77] |
| scattered | reconstructed | 4.67 [4.17, 7.22] | 5.10 [4.39, 7.40] | 0.72 [0.62, 1.17] | 0.85 [0.83, 1.06] | 0.37 [0.30, 0.49] | 1.61 [1.33, 2.44] | 1.98 [1.63, 2.94] |

| Workload | Mode | Writes in the window | Storage at window end | Peak RSS (process lifetime) |
| --- | --- | --- | --- | --- |
| large-group | native | 0 | 678,400 B | 45,465,600 B |
| large-group | reconstructed | 0 | 600,576 B | 54,591,488 B |
| clustered | native | 0 | 678,400 B | 26,656,768 B |
| clustered | reconstructed | 0 | 600,576 B | 30,654,464 B |
| scattered | native | 0 | 678,400 B | 26,705,920 B |
| scattered | reconstructed | 0 | 600,576 B | 30,621,696 B |

- **Forwarding dominates large-group.** Moving the base circuit's net delta into the view circuit as JSON (`forward`) took about 100 ms of the roughly 187 ms sequence in both modes, more than `base` and `index + q3` together. It is harness overhead: a single DBSP circuit passes deltas between views as in-memory streams, so the JSON hop has no counterpart in a deployed pipeline. It is excluded from the base and IVM figures but sits inside the sequence wall and CPU columns. Compare sequence totals across versions only with this in mind.
- **Base maintenance is now apart.** `base` is about equal in the two modes, as expected, since both run the same base circuit. `q3` no longer includes it: large-group native `q3` was 23.1 ms in version 2 and is 13.7 ms here. The two versions differ in more than this split (pre-read, eviction, two runtimes), so the difference is not attributed to base maintenance alone.
- **Storage** matches the 2026-10-22 runs byte for byte in every repetition: 600,576 B reconstructed and 678,400 B native, summed over both circuits' directories. No writes fall inside the window.
- **Peak RSS** is higher than on 2026-10-22 in every row. It is the process-lifetime high-water mark, and version 3 adds a second runtime and holds the pre-read refresh rows. The window-only peak exists only on Linux.
- **Timing.** The reconstructed mode's `index + q3` was higher than native mode's in every workload, as in version 2. The runs are too short and too small to attribute that difference. Make no performance claim from this table.

### R5 results, measurement version 2 (macOS smoke, 2026-10-22)

These runs follow roadmap Step 5, at revision `3b9948017` with a clean tree. They are smoke tests, not qualifying results: macOS enforces no memory limit, and each refresh sequence lasts milliseconds, so timing differences are within the range that scheduling noise can produce. They show what the current code does on these inputs, not how the two modes compare at scale.

- **Command:** `measure.py --cache-bytes 33554432 --memory-target-bytes 536870912 --batch 512 --runs 3 --no-verify --setup-full-compaction --front-code-byte-keys --elide-value-headers`, once per workload, without `--diagnostics`.
- **Inputs:** the three smoke workloads in `target/r5-smoke-inputs/` (see "Workload definitions").
- **Artifacts:** `target/r5-step5-timing/<workload>/manifest.json` and the per-run directories beside it; `target/r5-step5-timing/summary/summary.{json,md}`, produced by `summarize.py`.

Each cell is the median of three repetitions, with the minimum and maximum in brackets. Times are milliseconds and are summed over the six refreshes where the column is per refresh. `index` is zero in native mode, which has no merged index.

| Workload | Mode | Sequence wall | Sequence CPU | `index` | `q3` | `index + q3` |
| --- | --- | --- | --- | --- | --- | --- |
| large-group | native | 127.5 [100.2, 137.6] | 121.0 [100.3, 128.5] | 0 | 23.1 [19.0, 23.6] | 23.1 [19.0, 23.6] |
| large-group | reconstructed | 129.4 [107.4, 130.4] | 129.9 [103.8, 131.7] | 11.0 [9.5, 11.4] | 28.1 [22.8, 28.1] | 39.1 [32.3, 39.5] |
| clustered | native | 2.39 [2.25, 3.23] | 2.56 [2.39, 3.04] | 0 | 1.21 [1.13, 1.38] | 1.21 [1.13, 1.38] |
| clustered | reconstructed | 3.07 [2.82, 3.09] | 3.22 [3.00, 3.27] | 0.24 [0.17, 0.25] | 1.54 [1.44, 1.57] | 1.78 [1.61, 1.82] |
| scattered | native | 2.43 [2.29, 2.77] | 2.63 [2.41, 2.65] | 0 | 1.22 [1.17, 1.23] | 1.22 [1.17, 1.23] |
| scattered | reconstructed | 3.31 [3.28, 3.53] | 3.56 [3.43, 3.68] | 0.25 [0.25, 0.31] | 1.70 [1.68, 1.78] | 2.01 [1.93, 2.03] |

| Workload | Mode | Writes in the window | Storage at window end | Peak RSS |
| --- | --- | --- | --- | --- |
| large-group | native | 0 | 678,400 B | 37,502,976 B |
| large-group | reconstructed | 0 | 600,576 B | 47,005,696 B |
| clustered | native | 0 | 678,400 B | 21,282,816 B |
| clustered | reconstructed | 0 | 600,576 B | 25,280,512 B |
| scattered | native | 0 | 678,400 B | 21,299,200 B |
| scattered | reconstructed | 0 | 600,576 B | 25,280,512 B |

- **Storage matches the earlier packed, elided campaign.** Every repetition gave the bytes of `target/r5-packed-elided/` (600,576 B reconstructed, 678,400 B native), so Step 5 left the stored state unchanged.
- **No writes inside the window.** The six refreshes stay in memory in both modes, as before. No disk-traffic comparison follows from these runs.
- **Peak RSS** is the process's high-water mark since start, so it includes loading and setup compaction. It is not a maintenance-only figure.
- **Timing.** In these runs the reconstructed mode's `index + q3` was higher than native mode's in every workload. The runs are too short and too small to attribute that difference: the state fits in memory, no repetition lasted more than about 140 ms, and decoding CPU is not measured separately. Make no performance claim from this table. The `q3` column includes base-relation maintenance, which both modes share (see "Timing definitions"). These runs remain measurement version 2.

The earlier macOS smoke tables (version 1 and version 2) are in [history.md](../../../docs/design/merged-index/history.md). They were measured before POSIX flush fix `f7fec2391`, with byte-string values, no setup compaction, and no prefix truncation. They are not results for the current configuration.

The Linux campaign is a separate pending milestone. Its plan is in [MEASUREMENT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/MEASUREMENT.md), and its commands are in the [Linux handoff](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/LINUX-HANDOFF.md).

## 2026-11-20: The memtable's dropped phase of cheaper inserts

The [memtable](memtable.md) was planned on 2026-11-19 in three phases. The user dropped the second, cheaper inserts, entirely on 2026-11-20; the measurement on Linux became phase 2. None of these changed what the tree can do, only what an insert costs:

- **`Spine::insert_tuples`**, for unsorted input: skip the per-transaction sort and batch build where the trace is the sole consumer of its input. Q3's join traces are not sole consumers, since each delta also drives a join, so it would save nothing there.
- **A per-leaf merge** of each sorted batch, in place of the tuple-by-tuple insert: merge the batch's slice for a leaf with the leaf's items in one pass, so the leaf shifts once per batch. It pays only when several tuples of one batch land in one leaf.
- **A byte leaf** for merged indexes: one contiguous byte area plus offsets, compared with `memcmp`, to save the heap allocation of each `FoldedKey` and payload. It applies only where byte order equals the spine's order.

