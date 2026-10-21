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

## R5 results in the current configuration (pending)

The macOS rerun in the current configuration is pending. It will be filled in after the Step 5 code lands, with raw artifacts. Its flags are `--setup-full-compaction --front-code-byte-keys --elide-value-headers`; see [WORKFLOW.md](../../../docs/design/merged-index/WORKFLOW.md#measure-r5) for the command.

The earlier macOS smoke tables (version 1 and version 2) are in [history.md](../../../docs/design/merged-index/history.md). They were measured before POSIX flush fix `f7fec2391`, with byte-string values, no setup compaction, and no front coding. They are not results for the current configuration.

The Linux campaign is a separate pending milestone. Its plan is in [measurement.md](../../../docs/design/merged-index/measurement.md), and its commands are in the [Linux handoff](../../../docs/design/merged-index/WORKFLOW.md#linux-handoff).
