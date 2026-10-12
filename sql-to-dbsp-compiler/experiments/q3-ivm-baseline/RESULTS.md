# Q3 experiment results

This file keeps only results that have a raw artifact. The artifacts are local-only: they sit in git-ignored `target/` directories (this experiment's `target/` and the repository-root `target/`) and are not in git. On a fresh checkout, rerun the commands in [WORKFLOW.md](../../../docs/design/merged-index/WORKFLOW.md) to reproduce them.

The R1 native-baseline measurement table was removed on 2026-10-12: no raw artifact backed it, and its results were not actionable (see [decisions.md](../../../docs/design/merged-index/decisions.md)). The loading-hash table and the three-run dbgen correctness table went with it, for the same reason. Definitions of the R5 measures and the pending Linux campaign are in [measurement.md](../../../docs/design/merged-index/measurement.md).

## R4 shared RF1/RF2 verification

The shared tests in [src/verification.rs](src/verification.rs) verify both
generated modes with the same initial loading of Customers followed by complete
Order/Lineitem groups, and three RF1/RF2 cycles. They use one worker and a native
splitter chunk size of eight records; groups with 13, 11, and nine lines cross
batch boundaries. Three transactions use explicit start/step/commit, submitting
Orders and two Lineitem chunks in separate steps; one of these transactions is
empty. Other transactions use the normal transaction call.

The independent evaluator uses logical primary-key maps, enumerates Lines, and
looks up their Orders and Customers. Its before/after difference supplies
expected signed `ΔV`; its after result supplies expected materialized `V`.
Expected reader rows are computed directly from the logical input records
for the requested keys, including independently joining Customers and Orders.
Neither expected result uses merged-index reads or incremental join logic.
Customer-first loading makes the requested `orders_before` relation empty in
this workload; existing COL weighted-reader tests retain its nonempty coverage.

Each run's JSON result file records the transaction names, actual emitted rows and
materialized rows, sorted with their weights. It contains no timings or
mode-specific fields. The verified cardinalities are:

| Transaction | Emitted `ΔV` | Materialized `V` rows |
| --- | ---: | ---: |
| Customers | 0 | 0 |
| Load A | +15 | 15 |
| Load B | +4 | 19 |
| Empty after load | 0 | 19 |
| Empty multi-step | 0 | 19 |
| RF1 A, multi-step | +13 | 32 |
| RF2 A | −14 | 18 |
| RF1 B | +9 | 27 |
| RF2 B, multi-step | −13 | 14 |
| Empty between cycles | 0 | 14 |
| RF1 C | +3 | 17 |
| RF2 C | −11 | 6 |
| Empty final | 0 | 6 |

The test data includes three customers, four initially loaded orders, five RF1
orders, 44 total inserted lines, and six RF2 orders with 38 deleted lines.
Distinct line identities have identical payloads. Broader replacements, key
moves, independent line updates, customer updates, and orders with several
candidate customers are outside R4 (see
[reconstruction.md](../../../docs/design/merged-index/reconstruction.md)).
R4 transactions supply one candidate customer per order and do not establish
broader maintenance support. Residual Q3 and performance measurements remain
deferred.

Run [verify.sh](verify.sh) with Java 21 and the compiler already built. It runs
both experiment suites and COL with Rust 1.96.1 and offline locked dependencies,
compares the native and reconstructed JSON result files byte for byte, checks
formatting, strict Clippy, shell syntax, and whitespace, and restores default
native generation on exit. Separate checks confirmed restoration after success,
a failed reconstructed test, and a difference between the two result files.

The complete run on 2026-10-06 passed all 10 native tests, all seven reconstructed
tests, and all 21 COL tests. Existing storage, compaction, checkpoint, and
unsupported-workload regressions passed. Formatting, Clippy with warnings denied,
shell syntax, and whitespace checks passed, and default native generation was
restored. The JSON result files covering 13 transactions were identical byte for
byte: each is 97,718 bytes with SHA-256
`826d6d3fe047f95752e42cdc88f3633def2ee83033fd24a548a08fb3aa3d67bc`.
Logs and JSON result files are stored in the experiment's ignored
`target/r4-verification/20261006T222308Z-60261/` directory. The COL count includes
the separately committed candidate-location tests; R4 did not modify placement
or publication behavior.

## R5 v2 verification and macOS smoke evidence

**Maintenance ends at the last refresh's publication and cleanup. No forced
compaction or final compaction wait is performed.** Background merges can run
across refresh transactions and continue after the measured interval. Loading
waits only for normally eligible merges, preserves below-threshold runs, and
publishes its snapshots once with an empty transaction. Loading is excluded
from maintenance measurements.

On 2026-10-08, the revised `verify.sh` passed **18 native, 15 reconstructed,
21 COL, and 12 Python runner tests**, with exact R4 transcript agreement.
Formatting, strict experiment/COL Clippy, shell syntax, and experiment-scoped
whitespace checks passed. Artifacts are in
`target/r4-verification/20261008T215445Z-46398/` within this experiment.
Five focused DBSP tests passed, covering background-idle versus full-compaction
status and existing single-batch compaction cases. Strict production DBSP
library Clippy passed. Default native generation was restored. Regression
coverage includes RAM/disk runs surviving refresh completion and an empty
sequence recording zero sequence time and CPU.

### Workload definitions

All three smoke workloads start with **64 Customers, 512 Orders, and 2,048
Lines**. The label describes each RF1's inserted groups:

| Workload type | Orders per RF1 | Customers referenced | Lines per Order | Total Lines per RF1 |
| --- | ---: | ---: | ---: | ---: |
| **clustered** | 16 | 1 shared Customer | 4 | 64 |
| **scattered** | 16 | 16 distinct Customers | 4 | 64 |
| **large-group** | 1 | 1 Customer | 10,000 | 10,000 |

Each sequence is **RF1, RF2, repeated RF2 for absent Orders, RF1 reinsertion,
RF2, and empty RF2**. RF1 inserts complete groups; RF2 deletes each requested
Order and every current Line belonging to it. Requested duplicate IDs are
removed and absent Orders counted. Both modes receive the same inputs.

**Native** uses the generated native Q3 circuit and its traces.
**Reconstructed** maintains the merged index in an auxiliary thread and uses
it alongside the remaining circuit traces. The index uses the ordinary DBSP
Spine background merger; its compaction can continue across transactions.

### Timing definitions

**All columns below cover the six refreshes; loading and independent correctness
checking are excluded.** The performance processes use `--no-verify` and retain
zero `Reference` map bytes.

| Column | Definition |
| --- | --- |
| **Foreground ms** | Sum of the six refresh wall times, including input reads/discovery, operational checks, input preparation, index maintenance, Q3, publication, and cleanup. |
| **Sequence ms** | Wall time from the start of the refresh sequence through the last refresh's publication/cleanup. Includes between-refresh measurement/logging overhead; excludes final reporting and any subsequent compaction. |
| **Sequence CPU ms** | Process user + system CPU over the sequence, summed across driver, runtime, and auxiliary threads. Includes naturally scheduled background merge work executed inside that interval. |

**Per-refresh JSON phases:**

| Field | Definition |
| --- | --- |
| **discovery_ms** | RF2 delete-ID reads, deduplication, maintained-state lookups, and assembly of complete current deletion rows, including decoding. |
| **checks_input_preparation_ms** | RF1 input reads, operational checks, workload statistics, logical-row conversion, and circuit input serialization. |
| **index_ms** | Reconstructed index batch construction and append, including folded-key/payload encoding, copying, sorting, and snapshot acquisition. Zero in native mode. |
| **q3_ms** | Circuit transaction, including materialized `V`; reconstructed execution includes its temporary reconstruction, decoding, copying, and sorting. |
| **publication_cleanup_ms** | Snapshot publication/release, refreshed Catalog snapshots, and foreground output/driver cleanup. |
| **combined_maintenance_ms** | `index_ms + q3_ms`; excludes discovery, preparation, and publication/cleanup. |

**Final drainage ms is absent in v2.** The report records
`measurement_version=2`, `completion_boundary=last_refresh_complete`, and zero
sequence `snapshot_publication_transactions`.

### Resource definitions

**Backend counters cover the sequence interval. Storage, RSS, and cache columns
are bytes.** Storage observations do not require compaction to finish.

| Column | Definition |
| --- | --- |
| **Read/write ops and bytes** | Storage-backend operation counts and bytes submitted during the sequence; distinct from OS physical I/O. |
| **Observed storage** | Regular-file lengths in the storage directory observed after the last refresh without a compaction wait; background merges can still be active. |
| **Sampled peak storage** | Largest directory size sampled during the sequence and its final storage observation, every 10 ms and once at the end. Shorter peaks may be missed. |
| **Peak RSS** | Process resident-memory high-water mark since process start, including loading, driver, runtime, and auxiliary threads. |
| **Cache occupancy** | Configured worker-cache occupancy observed after the last refresh; capacity is 32 MiB. |
| **Memory/storage batches** | Counts of nonempty RAM and disk batches in the retained Catalog snapshots; the auxiliary index reports its current batches separately. File-backed configuration permits RAM batches. |

`load.background_merge_wait_ms` includes the loading wait, its snapshot
publication, and associated cleanup. `load.observed_storage_bytes` is the
loaded storage observation used by the Linux size qualification check. Neither
loading field is part of the refresh sequence.

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
[campaign description](../../../docs/design/merged-index/measurement.md)
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

## Historical

Superseded by R5 v2 above. Kept because the artifacts exist under the repository-root `target/r5-smoke/` (local-only). Do not compare v1 and v2 timings: their completion boundaries differ.

## R5 v1 harness verification and macOS smoke evidence

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

### Workload definitions

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

### Timing definitions and results

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

### Resource definitions and results

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
