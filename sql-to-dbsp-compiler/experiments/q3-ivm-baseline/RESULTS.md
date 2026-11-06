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
**Reconstructed** maintains the merged index in an auxiliary thread of the base
runtime, beside the base tables, and the view circuit reads published snapshots of it. The index uses the ordinary DBSP
Spine background merger; its compaction can continue across transactions.

### Timing definitions

**All columns below cover the six refreshes; loading and independent correctness
checking are excluded.** The performance processes use `--no-verify` and retain
zero `Reference` map bytes.

| Column | Definition |
| --- | --- |
| **Foreground ms** | Sum of the six refresh wall times, including discovery (the lookup; the refresh files are read before the window), operational checks, input preparation, the base circuit, forwarding, index maintenance, the view circuit, publication, and cleanup. |
| **Sequence ms** | Wall time from the start of the refresh sequence through the last refresh's publication/cleanup. Includes between-refresh measurement/logging overhead; excludes final reporting and any subsequent compaction. |
| **Sequence CPU ms** | Process user + system CPU over the sequence, summed across driver, runtime, and auxiliary threads. Includes naturally scheduled background merge work executed inside that interval. |

**Per-refresh JSON phases:**

| Field | Definition |
| --- | --- |
| **discovery_ms** | RF2 maintained-state lookups and assembly of complete current deletion rows, including decoding. The delete-ID file is parsed into a key set before the window opens, so its read is not included (version 2 included it). |
| **checks_input_preparation_ms** | Operational checks, workload statistics, logical-row conversion, and circuit input serialization. RF1 input files are parsed before the window and are not included. |
| **index_ms** | A sub-phase of `base_ms`; never add it to `base_ms` again. Reconstructed index append and snapshot acquisition. The index rows come from the base circuit's net delta; the conversion to rows (`reconstruction::prepare`) stays in `checks_input_preparation_ms`, and the append includes folded-key/payload encoding, copying, and sorting. The index runs on an auxiliary thread of the runtime that hosts both circuits (measurement version 6; through version 5 it ran in the base circuit's own runtime, storage directory `run-N/base`). Zero in native mode. |
| **base_ms** | Base maintenance, in version 4: the base circuit's transaction (the primary-key input maps and their delta accumulators) plus, in reconstructed mode, the merged index's upkeep (`index_ms`). The merged index is secondary storage of the base relations, so its upkeep is base maintenance and not IVM. The two parts are separate intervals of the transaction: the forward read and the row conversion fall between them. |
| **forward_ms** | Reading the base circuit's net delta (`workload::net_delta`) and feeding it to the view circuit's inputs. Harness overhead: a single DBSP circuit passes deltas between views as in-memory streams, so the JSON hop has no counterpart in a deployed pipeline. Excluded from the base and IVM figures, but inside the sequence totals. |
| **q3_ms** | The view circuit's transaction alone: delta accumulation, the joins (native traces or reconstructed readers), and materialized `V`. Reconstructed execution includes its temporary reconstruction, decoding, copying, and sorting. In version 2 it was the single `circuit.transaction()` and also covered base-relation maintenance (`add_input_map_persistent`, owner `base_relation`), common to both modes, which dilutes any version 2 native-to-reconstructed ratio. |
| **publication_cleanup_ms** | Snapshot publication/release, refreshed Catalog snapshots, and foreground output/driver cleanup. |
| **ivm_ms** | Incremental view maintenance, in version 4: equal to `q3_ms`, the view circuit's transaction. Native mode counts its join traces here; reconstructed mode counts its snapshot readers here, and not the merged index's upkeep. It replaces version 3's `combined_maintenance_ms` (`index_ms + q3_ms`). Excludes `base_ms`, `forward_ms`, discovery, preparation, and publication/cleanup. |

**Reporting rule: base and IVM are always reported side by side.** Native against
reconstructed is compared in two parts: base maintenance (`base_ms`: native base tables, against
base tables plus the merged index) and IVM (`ivm_ms`: native join traces, against snapshot
readers). An IVM figure alone is never quoted as a saving, because the merged index's upkeep is
booked in the base phase. `index_ms` is shown only as a part of `base_ms`.

**Final drainage ms is absent in v2, v3 and v4.** The report records
`measurement_version=4`, `completion_boundary=last_refresh_complete`, and zero
sequence `snapshot_publication_transactions`. Version 3 splits the transaction
into the base circuit and the view circuit, each in its own runtime with its own
storage directory and buffer cache. Version 4 moves the merged index into the base
runtime and phase: `base_ms` includes `index_ms`, and `combined_maintenance_ms` is renamed
`ivm_ms` and equals `q3_ms`. Version 2 results, such as the macOS rerun of
2026-10-22, keep their label: their `q3_ms` includes base-relation maintenance, so
they are not comparable with version 3 or 4. Version 3 results keep their label too: their
`base_ms` leaves out the merged index and their `combined_maintenance_ms` includes it, so
their phase figures are not comparable with version 4.

### Resource definitions

**Backend counters cover the sequence interval. Storage, RSS, and cache columns
are bytes.** Storage observations do not require compaction to finish.

| Column | Definition |
| --- | --- |
| **Read/write ops and bytes** | Storage-backend operation counts and bytes submitted during the sequence; distinct from OS physical I/O. |
| **Observed storage** | Regular-file lengths in the storage directory observed after the last refresh without a compaction wait; background merges can still be active. |
| **Sampled peak storage** | Largest directory size sampled during the sequence and its final storage observation, every 10 ms and once at the end. Shorter peaks may be missed. |
| **Peak RSS** | Process-lifetime resident-memory high-water mark since process start (`getrusage`), including loading, setup compaction, the refresh pre-read, driver, runtime, and auxiliary threads. Not a maintenance-window figure. |
| **Window peak RSS** | `sequence.window_peak_rss_bytes`. Linux only (`null` elsewhere): `VmHWM` at the end of the window, after `echo 5 > /proc/self/clear_refs` reset it at the window's start. It includes the pre-read refresh rows still held. `window_peak_rss_error` explains a failed reset (the peak is then `null`). |
| **Window end RSS** | `sequence.window_end_rss_bytes`. Linux only: `VmRSS` at the end of the window. |
| **Cgroup window block** | `sequence.cgroup`. Linux with cgroup v2 only (`null` elsewhere). `before`, `after` and `delta` of `memory.current`, `memory.events`, a named subset of `memory.stat` (`pgscan`, `pgsteal`, `workingset_refault*` and others the kernel has) and `memory.pressure`, taken before the first refresh and after the last. `peak`: `window_peak_bytes` from resetting and re-reading `memory.peak` (kernel 6.12 or later; unverified, null with `window_peak_supported: false` if it fails), `lifetime_peak_at_end_bytes`, and `max_sampled_memory_current_bytes` (the fallback, sampled at the window start, each refresh end and the window end). Evidence of memory use, not proof of pressure. |
| **Memory pressure (per refresh)** | `memory_pressure.before` and `.after`: DBSP's level for the one runtime that hosts both circuits (`runtime`; through version 5, `base_runtime` and `q3_runtime`), sampled at the refresh's boundaries. The level comes from process RSS against the memory target, not from cgroup memory. |
| **Refresh inputs** | `refresh_inputs`: per-refresh row counts of the pre-read refresh files, and `VmRSS` before and after the pre-read (Linux), which measure its fixed overhead. |
| **Inputs evicted** | `inputs_evicted`: whether `posix_fadvise(DONTNEED)` was applied to every base and refresh input file after the pre-read (Linux only; false on macOS), with the cgroup's `memory.current` before and after. |
| **Cache occupancy** | Configured worker-cache occupancy observed after the last refresh (`cache_occupancy_bytes`) and at the start of the window (`cache_occupancy_at_window_start_bytes`), per runtime under `circuits`; total capacity is 32 MiB. |
| **Memory/storage batches** | Counts of nonempty RAM and disk batches in the retained Catalog snapshots; the auxiliary index reports its current batches separately. File-backed configuration permits RAM batches. |

`load.background_merge_wait_ms` includes the loading wait, its snapshot
publication, and associated cleanup. `load.observed_storage_bytes` is the
loaded storage observation used by the Linux size qualification check. Neither
loading field is part of the refresh sequence.

## R5 results, measurement version 4 (macOS smoke, 2026-10-26)

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

## R5 results, measurement version 3 (macOS smoke, 2026-10-25)

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

## R5 results, measurement version 2 (macOS smoke, 2026-10-22)

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

The earlier macOS smoke tables (version 1 and version 2) are in [history.md](../../../docs/design/merged-index/history.md). They were measured before POSIX flush fix `f7fec2391`, with byte-string values, no setup compaction, and no front coding. They are not results for the current configuration.

The Linux campaign is a separate pending milestone. Its plan is in [measurement.md](../../../docs/design/merged-index/measurement.md), and its commands are in the [Linux handoff](../../../docs/design/merged-index/WORKFLOW.md#linux-handoff).
