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

On 2026-10-08, the then-revised `verify.sh` passed with exact R4 transcript agreement; its test counts and artifacts are in [history.md](../../../docs/design/merged-index/history.md#2026-10-08-r5-version-2-smoke-runs). `verify.sh` still runs the Python runner tests, and `test_measure.py` now defines 19.

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
| **Foreground ms** | Sum of the six refresh wall times, including input reads/discovery, operational checks, input preparation, the base circuit, forwarding, index maintenance, the view circuit, publication, and cleanup. |
| **Sequence ms** | Wall time from the start of the refresh sequence through the last refresh's publication/cleanup. Includes between-refresh measurement/logging overhead; excludes final reporting and any subsequent compaction. |
| **Sequence CPU ms** | Process user + system CPU over the sequence, summed across driver, runtime, and auxiliary threads. Includes naturally scheduled background merge work executed inside that interval. |

**Per-refresh JSON phases:**

| Field | Definition |
| --- | --- |
| **discovery_ms** | RF2 delete-ID reads, deduplication, maintained-state lookups, and assembly of complete current deletion rows, including decoding. |
| **checks_input_preparation_ms** | RF1 input reads, operational checks, workload statistics, logical-row conversion, and circuit input serialization. |
| **index_ms** | Reconstructed index append and snapshot acquisition. In version 3 the index rows come from the base circuit's net delta; the conversion to rows (`reconstruction::prepare`) is in `checks_input_preparation_ms`, and the append includes folded-key/payload encoding, copying, and sorting. Zero in native mode. |
| **base_ms** | The base circuit's transaction: the primary-key input maps and their delta accumulators. Measurement version 3 times it apart from the view circuit. Reported beside `combined_maintenance_ms` and never added to it. |
| **forward_ms** | Reading the base circuit's net delta (`workload::net_delta`) and feeding it to the view circuit's inputs. Outside both circuits' timings. |
| **q3_ms** | The view circuit's transaction alone, in version 3: delta accumulation, the joins (native traces or reconstructed readers), and materialized `V`. Reconstructed execution includes its temporary reconstruction, decoding, copying, and sorting. In version 2 it also covered base-relation maintenance. |
| **publication_cleanup_ms** | Snapshot publication/release, refreshed Catalog snapshots, and foreground output/driver cleanup. |
| **combined_maintenance_ms** | `index_ms + q3_ms`, the incremental view maintenance; excludes `base_ms`, `forward_ms`, discovery, preparation, and publication/cleanup. |

**Final drainage ms is absent in v2 and v3.** The report records
`measurement_version=3`, `completion_boundary=last_refresh_complete`, and zero
sequence `snapshot_publication_transactions`. Version 3 splits the transaction
into the base circuit and the view circuit, each in its own runtime with its own
storage directory and buffer cache. Version 2 results, such as the macOS rerun of
2026-10-22, keep their label: their `q3_ms` includes base-relation maintenance, so
they are not comparable with version 3.

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

## R5 results in the current configuration (macOS smoke, 2026-10-22)

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
- **Timing.** In these runs the reconstructed mode's `index + q3` was higher than native mode's in every workload. The runs are too short and too small to attribute that difference: the state fits in memory, no repetition lasted more than about 140 ms, and decoding CPU is not measured separately. Make no performance claim from this table.

The earlier macOS smoke tables (version 1 and version 2) are in [history.md](../../../docs/design/merged-index/history.md). They were measured before POSIX flush fix `f7fec2391`, with byte-string values, no setup compaction, and no front coding. They are not results for the current configuration.

The Linux campaign is a separate pending milestone. Its plan is in [measurement.md](../../../docs/design/merged-index/measurement.md), and its commands are in the [Linux handoff](../../../docs/design/merged-index/WORKFLOW.md#linux-handoff).
