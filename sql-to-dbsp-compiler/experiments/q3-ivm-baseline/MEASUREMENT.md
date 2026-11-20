# Maintenance measurement (R5)

R5 compares two ways of maintaining `V = Customer ⋈ Orders ⋈ Lineitem` under the same refreshes, with file-backed state:

- **Native:** Feldera's own traces.
- **Merged-index** (reconstructed mode): the joins read the Customer–Orders–Lineitem (COL) merged index instead of four traces ([reconstruction.md](../../../docs/design/merged-index/reconstruction.md)).

This file says what is measured, which dimensions are decided and which are still measured, and what the results are.

- **The commands** are in [LINUX-HANDOFF.md](LINUX-HANDOFF.md).
- **Harness details** are in [CLAUDE.md](CLAUDE.md).
- **Decisions** are in [DECISIONS.md](DECISIONS.md), with generic ones in [decisions.md](../../../docs/design/merged-index/decisions.md).
- **History.** macOS smoke results (versions 2 to 11) and the record-size investigation are in [history.md](../../../docs/design/merged-index/history.md).
- **Raw artifacts** stay on the machine that ran them.

Nothing here infers an unmeasured quantity.

**Contents:**
1. [Synopsis](#synopsis): every result in one table.
2. [Dimensions](#dimensions): active and decided.
3. The dated results.
4. [Pending: Linux campaign](#pending-linux-campaign).
5. Definitions and report fields.
6. [Appendix: measurement versions](#appendix-measurement-versions).

## Synopsis

**How to read the table:**
- Every figure is from one-repetition pilots: SF 10, A-seek, K = 10, clustered unless noted, on the SSD.
- The view column is per-transaction `ivm_ms` p50 in ms, RF1 / RF2, over refreshes 2 to 6. The pilots of 2026-11-18 pool every refresh.
- Base is amortized over the views an index would serve, so it comes last ([decision](DECISIONS.md#2026-11-18-base-table-blocks-leave-the-cache-after-each-base-transaction)).
- Rows differ in measurement version and settings: compare within a row, not across rows.

| Dimension | Arms measured (version) | View: `ivm_ms` p50 | Base: `base_ms` p50 | Status |
| --- | --- | --- | --- | --- |
| Prefix truncation | A (on), B (off) (v14) | merged-index A 4.51 / 3.07, B 5.55 / 3.61; native the same in both (3.6–3.8 RF1) | merged-index A 1.44 / 1.94, B 1.62 / 2.26 | **Decided 2026-11-18:** on (A only) |
| Session pass | seek, step (v14) | step: the merged-index run was stopped after 49 min (seek: about 7 min) | — | **Decided 2026-11-18:** seek |
| Setup compaction | on, off (v4–6, macOS) | — | — | **Decided 2026-11-09:** off; the flag stays to isolate fragmentation |
| Filter residency | memory, storage (v14–15) | — (the motive was memory: 305 MiB native and 461 MiB merged-index of resident filters after restore) | — | **Decided 2026-11-18:** storage-resident |
| Memory budget | cache 128 (v14), 256 (v15), 512 (v16); target 512 | 512/512: native 3.88 / 4.46, merged-index 4.31 / 3.38, both `critical` from refresh 3 | native 3.4 / 8.0, merged-index 3.8 / 15.5 | **Decided 2026-11-18:** one budget, the cache about equal to the target; 512/512 now |
| Base eviction | on (v16) | not measured in isolation (it came with the 512 MiB cache) | eviction 5.5–6 ms, in neither phase | **Decided 2026-11-18:** on, always |
| Prefix filter | off, on (v17) | merged-index off 4.56 / 3.36, on 3.98 / 3.06; index seeks 9.8 → 2.5 and 26.6 → 7.0 | off 3.77 / 15.57, on 3.56 / 14.68 | **Active:** the default awaits the user |
| Order keys | dbgen (v16–17), uuid (v17), sequence (stopped) | uuid: native 3.88 → 5.76 / 4.46 → 9.28; merged-index 4.56 → 4.15 / 3.36 → 3.49 | uuid: native 13.86 / 11.44, merged-index 11.92 / 20.20 | **Decided:** dbgen is the default, and `sequence` and timestamp IDs are out. **Active:** uuid |
| Memtable | 512/512; 448 cache; 448 + 64 MiB memtable (v18) | running | running | **Active:** [running](#memtable-at-512-mib-2026-11-20) |
| Workload | clustered, scattered (v14) | clustered: native 3.77 / 4.63, merged-index 4.51 / 3.07; scattered: native 4.30 / 4.44, merged-index 8.06 / 3.14 | 1.0–1.4 / 1.1–1.9 | **Active:** scattered not run since v14 |
| Transaction size | K = 10 | as above | — | **Active:** K = 100 and 1,000 next |
| Device | SSD | as above | — | **Active:** the hard-disk probe has not run |

What the rows add up to, so far:
- **Where the merged index helps.** Its view is faster on deletes (RF2) in every measured setting, and it holds its latency when Order keys are random, while native's doubles.
- **Where it pays.** With dbgen keys its inserts (RF1) are slower, since each transaction lands on a new customer's group (first-touch reads), and most of all with scattered customers. With uuid keys its RF1 is the faster one.
- **The prefix filter** cut the index's seeks by three quarters and its RF1 median by 13%.
- **Pressure.** Every run since the 512 MiB cache is at `critical` pressure, which is the target regime ([one budget](#one-budget-at-512-mib-base-tables-evicted-2026-11-19)).

## Dimensions

### Active

| Dimension | Arms | Selected by | Own images? | Now | Next | Decision |
| --- | --- | --- | --- | --- | --- | --- |
| Memtable | off; `--memtable-mib N` | the campaign's `--memtable-mib` | no | the three arms are running | report, view first | [split](DECISIONS.md#2026-11-20-the-memtable-arm-splits-one-512-mib-budget-448-mib-cache-64-mib-memtable), [design](../../../docs/design/merged-index/decisions.md#2026-11-19-the-memtable-counts-in-rss-soft-capped-by-its-budget) |
| Prefix filter | off, on | `--index-prefix-filter` on `--build-images` and on the campaign | yes | on in the memtable arms | the user sets the default | [decision](../../../docs/design/merged-index/decisions.md#2026-11-19-group-reads-skip-runs-by-a-prefix-filter) |
| Order keys | `dbgen` (default), `uuid` | `prepare_inputs.py --order-keys`; images from `<out>/base` | yes | dbgen | uuid as an arm of later runs | [decision](DECISIONS.md#2026-11-19-order-keys-are-a-dimension-dbgen-sequence-uuid) |
| Workload | clustered, scattered | the refresh list (`clustered/`, `scattered/`) | no | clustered | scattered at the current version | [decision](DECISIONS.md#2026-11-17-clustered-rf1-draws-tpc-cs-nurand-in-sessions-only-eligible-customers-order) |
| Transaction size | K = 10, 100, 1,000; the TPC-H batch later | `prepare_inputs.py --transaction-rows`, one `--out` per K | no | K = 10 | K = 100, then 1,000 | [decision](DECISIONS.md#2026-11-17-transaction-size-is-a-dimension-small-transactions-first) |
| Device | SSD; hard disk as a probe | `--storage-path`, `--device-label` | no | SSD | the hard-disk probe, pilots only | [decision](DECISIONS.md#2026-11-16-the-ssd-runs-the-campaign-the-hard-disk-only-a-pilot-probe) |
| Repetitions | 1 (pilot), 3 (campaign) | `--runs` | no | 1 | `--runs 3` once the arms are chosen | — |

### Decided

Each row is a fixed setting of every run. The reasoning is as the decision entry records it.

| Dimension | Decided setting | Dropped | Date | Reasoning | Last result |
| --- | --- | --- | --- | --- | --- |
| Prefix truncation | on (arm A), with value-header elision on | arm B (prefix truncation off) | [2026-11-18](DECISIONS.md#2026-11-18-arm-b-and-the-step-pass-are-retired) | The entry records the user's decision and no further reason. | v14: B was slower in merged-index mode (RF1 5.55 against 4.51 clustered, 8.82 against 8.06 scattered); native was the same |
| Session pass | `seek` | `step` | [2026-11-18](DECISIONS.md#2026-11-18-the-step-arms-are-dropped) | Under `step` a small transaction walks about half the index from the session's start. | A-step's merged-index run read 15 GB in 49 min and was stopped |
| Setup compaction | off | on (kept as `--setup-full-compaction`) | [2026-11-09](DECISIONS.md#2026-11-09-no-setup-compaction-is-the-default-workload) | No compaction before the refreshes is the more realistic workload; the flag isolates fragmentation. | v4–6 macOS smoke ran with it on |
| Filter residency | storage-resident Bloom filters, read through the buffer cache | memory-resident | [2026-11-18](DECISIONS.md#2026-11-18-bloom-filters-are-storage-resident-read-through-the-buffer-cache) | Resident filters took 305 MiB native and 461 MiB merged-index; two were never probed. A budget beats a per-spine opt-out (RocksDB's partitioned filters). | v14 window start: 361–371 MiB native and 527–528 MiB merged-index anonymous memory |
| Memory budget | one budget: the cache about equals the target; 512/512 now, halved only if pressure stays low | cache 128 MiB, then 256 MiB, separate from the target | [2026-11-18](DECISIONS.md#2026-11-18-one-memory-budget-the-cache-about-equals-the-memory-target) | With filters on storage, two layers of memory management have no point. | 512/512 reached `critical`, so it is not halved |
| Base eviction | the base tables' blocks are evicted after each base transaction | — | [2026-11-18](DECISIONS.md#2026-11-18-base-table-blocks-leave-the-cache-after-each-base-transaction) | Report the view; the base cost is amortized, and its side effects on the view are mitigated in both modes. | v15: base and index blocks evicted each other from the one cache |
| Order keys | dbgen as the default | `sequence` (stopped); timestamp IDs | [2026-11-19](DECISIONS.md#2026-11-19-the-sequence-arm-is-stopped-cancelled-rows-are-a-watch-item) | `sequence`'s key space is close to dbgen's, and its run was slowed by cancelled rows at the key range's front ([watch item](../../../docs/design/merged-index/compaction.md#watch-for-cancelled-rows-at-the-front-of-the-key-range)). Timestamp IDs order like a sequence with one writer. | `sequence`: 3 h 37 min against 23 min, and 85 of 86 GB read by the driver's lookups |
| RF1 selection | clustered: TPC-C NURand in sessions of 4; scattered: uniform; only Customers whose key is not a multiple of 3 | Zipf over a permutation | [2026-11-17](DECISIONS.md#2026-11-17-clustered-rf1-draws-tpc-cs-nurand-in-sessions-only-eligible-customers-order) | Independent draws tie locality to popularity; sessions make locality its own parameter. | — |
| RF2 selection | the oldest Orders, the same in both workloads | per-workload rules | [2026-11-04](DECISIONS.md#2026-11-04-rf2-deletes-the-oldest-orders) | An analytical database ages out its oldest history. | — |
| Loading | once, into an image per mode; each run starts a new process from a copy | loading inside each run | [2026-11-17](DECISIONS.md#2026-11-17-load-once-into-an-image-each-refresh-run-is-a-new-process-from-a-copy-of-it) | Loading SF 10 under 1 GiB took 96 min and timed out before any refresh. | — |
| Cold start | inputs evicted; the cache starts empty (`--warm-up` optional, unused) | warm cache from loading | [2026-11-11](DECISIONS.md#2026-11-11-one-set-of-inputs-read-in-place-every-run-starts-cold) | Every run starts the same way from the same inputs. | — |
| Run placement | DBSP's default in both modes | `min_storage_bytes: Some(0)` | [2026-11-09](../../../docs/design/merged-index/decisions.md#2026-11-09-dbsps-default-placement-in-both-modes) | Let memory pressure decide what reaches disk. | — |
| Workers | one | — | root [CLAUDE.md](../../../CLAUDE.md) | Maintenance and loading both use one worker. | — |

### Adding a dimension, an arm, or a version

- **A decision comes first:** an entry in `DECISIONS.md` (Q3) or `decisions.md` (generic), then a row here.
- **A new arm:** a row in Active (or a new arm in a row), its flag in the [handoff's dimension table](LINUX-HANDOFF.md#measurement-dimensions), a dated results section below, and a Synopsis row once measured.
- **A dimension that becomes decided:** move its row to Decided, with the fixed setting stated explicitly, and update its Synopsis status.
- **A new measurement version:** a bullet in the [appendix](#appendix-measurement-versions), `MEASUREMENT_VERSION` in `measure.py` (the tests and `summarize.py` follow), and the version list in [CLAUDE.md](CLAUDE.md). New images are needed only if the image sources changed (`IMAGE_SOURCE_PATHS` in `measure.py`).

## Linux pilots at K = 10 (2026-11-18)

The first SSD pilots, one repetition each, from the SF 10 images of revision `da074ff7e` (measurement version 14).

**Settings:**
- **Host.** Xeon Silver 4114, kernel 6.8, turbo off.
- **Workloads.** `prepare_inputs.py --transaction-rows 10 --seed 1 --pairs 3`: about 12.5 rows per transaction, and 18,041 RF1 and 18,129 RF2 transactions per window. Clustered draws one Customer per transaction.
- **Memory.** cgroup 1 GiB, cache 128 MiB, memory target 512 MiB.
- **Start.** Cold, with no warm-up.

Per-transaction p50 in ms, over every transaction of each phase:

| Pilot | Mode | RF1 base | RF1 IVM | RF2 base | RF2 IVM |
| --- | --- | ---: | ---: | ---: | ---: |
| A-seek clustered | native | 1.11 | 3.77 | 1.20 | 4.63 |
| A-seek clustered | merged-index | 1.44 | 4.51 | 1.94 | 3.07 |
| B-seek clustered | native | 1.06 | 3.59 | 1.16 | 4.65 |
| B-seek clustered | merged-index | 1.62 | 5.55 | 2.26 | 3.61 |
| A-seek scattered | native | 1.03 | 4.30 | 1.13 | 4.44 |
| A-seek scattered | merged-index | 1.37 | 8.06 | 1.93 | 3.14 |
| B-seek scattered | native | 1.11 | 4.51 | 1.18 | 4.56 |
| B-seek scattered | merged-index | 1.33 | 8.82 | 1.96 | 3.27 |

- **Qualification.** Every completed run qualifies: the loaded state (17 to 20 GB) exceeds the limit, the copy was evicted, and the cgroup was verified.
- **Which limit binds: DBSP's, not the kernel's.**
  - **The kernel's limit never bound.** `memory.events` shows `max 0` and `oom 0` in every run. `memory.peak` was 605 to 652 MiB native and 777 to 784 MiB merged-index.
  - **DBSP's target did.** Its pressure was `critical` in every refresh, since the window's peak RSS (521 to 563 MiB native, 689 to 693 MiB merged-index) exceeds the 512 MiB target.
- **`total_ms` is not a latency figure at K = 10.** Besides `base_ms` and `ivm_ms`, it holds:
  - input checks and preparation: 0.9 to 2.4 ms in RF1, under 0.1 ms in RF2;
  - `forward_ms`: about 0.2 ms;
  - cleanup: about 0.1 ms;
  - an unmeasured residual: 3 to 5 ms native, 2 to 3 ms merged-index RF1, 6 to 9 ms merged-index RF2.

  Compare `base_ms` and `ivm_ms` instead. *Corrected on 2026-11-18; the first version called the residual about 4 ms of instrumentation.*
- **Memory at the window's start.** Before any refresh, anonymous memory was 361 to 371 MiB native and 527 to 528 MiB merged-index.
  - **The restored runs' filters.** Most of it is these filters, which stay resident: 305 MiB native and 461 MiB merged-index, from the file trailers.
  - **The merged index's own Bloom filter** is 175 MiB, for 76.5 million keys, and the index never probes it.
  - **Native's first refresh.** Merged-index mode starts at `critical`. Native starts at `low` and reaches `critical` during its first refresh, so in clustered its RF1 IVM there was 2.7 to 2.8 ms against 4.0 to 4.6 ms later. Its pooled RF1 percentiles include that refresh.

  This led to [storage-resident filters](DECISIONS.md#2026-11-18-bloom-filters-are-storage-resident-read-through-the-buffer-cache).
- **Spilling at `critical`.** Every refresh after the first writes 0.9 to 1.4 GB in both modes: at `critical`, DBSP writes every inserted run to a file.
- **No storage read reaches the page cache.** Files use `O_DIRECT`, so every buffer-cache miss reads the SSD. Accordingly:
  - file memory stays below 1 MiB;
  - `pgmajfault` is 0;
  - `/proc/self/io` read bytes equal the backend's.
- **Merged-index RF1's extra time is outside the view circuit's steps.** The profiler's view-circuit CPU is lower in merged-index mode (1.7 to 2.2 ms against 2.8 to 3.4 ms), so the extra `ivm_ms` is in the scan session's collection, which the profiler does not cover. Scattered merged-index RF1 read 42 to 49 storage operations per transaction, against 24 to 28 clustered.
- **The step arms did not finish.** A-step clustered's merged-index run was stopped after 49 minutes ([known limits](LINUX-HANDOFF.md#known-limits)).

Artifacts: `/mnt/ssd/r5-out/pilot-<arm>-<workload>-k10/manifest.json`.

## One budget at 512 MiB, base tables evicted (2026-11-19)

**Settings.** Pilot A-seek clustered, K = 10, version 16 images (revision `727c76c52`), one repetition:
- target 512 MiB, cache 512 MiB ([one budget](DECISIONS.md#2026-11-18-one-memory-budget-the-cache-about-equals-the-memory-target));
- cgroup 1 GiB;
- base-table blocks evicted after each base transaction.

Both runs qualify, and `memory.events max` is 0. Refreshes 2 to 6, per transaction:

| | RF1 native | RF1 merged-index | RF2 native | RF2 merged-index |
| --- | ---: | ---: | ---: | ---: |
| `ivm_ms` p50 / p95 / p99 | 3.88 / 7.00 / 8.93 | 4.31 / 7.24 / 9.23 | 4.46 / 7.84 / 10.04 | 3.38 / 5.67 / 7.23 |
| view circuit CPU (profiler, ms) | 3.05 | 2.05 | 3.13 | 1.97 |
| partner reads | 3.5 seeks, plus 48.0 probes the filters answer | 9.8 index seeks | 11.1 seeks, plus 39.8 filter-answered | 26.7 index seeks |
| view-side device reads | 1.52 | 6.40 | 3.07 | 1.34 |
| view-state writes (`query`, `merged_index`) | 10.6 | 9.4 | 10.3 | 8.9 |

- **Pressure.** `critical` in both modes from refresh 3, once the cache filled. Peak RSS was about 720 MiB. The cache counts in RSS, and the program, the refresh inputs and the in-memory runs add about 200 MiB, so a cache as large as the target reaches `critical` once full.
- **RF2 (deletes).** Merged-index IVM is 24% lower at the median and 28% lower at p99. Its seeks are more numerous but stay in the cache: 1.34 device reads against 3.07.
- **RF1 (inserts).** Merged-index IVM is 11% higher at the median. Each transaction lands on a new customer's group, so its reads are first touches: 6.40 device reads against 1.52. Its view circuit takes less CPU.
- **Against version 15** (cache 256 MiB, pressure `low`): both modes are slower at `critical`, and both write about 10 spilled runs per transaction.
- **Base (amortized).** `base_ms` p50 was 3.4 (RF1) and 8.0 (RF2) native, and 3.8 and 15.5 merged-index. The eviction took 5.5 to 6 ms per transaction, which is in `total_ms` but in neither phase.

**What spills at `critical` pressure** (`circuit/runtime.rs`):
- **An inserted run.** From `high`, `min_insert_storage_bytes` is 0. This covers the base tables, native's traces and `V`, and merged-index's `Dlt` run and `V`.
- **A merge output.** From `moderate`, `min_merge_storage_bytes` is 0.
- **A run passed between operators within a step.** At `critical`, `min_step_storage_bytes` is 0. The run is built straight into a file, and that file is dropped at the end of the step.

**Why they do not wait in the buffer cache:**
- **The cache holds only blocks of files that exist.** It has no dirty or write-back state, so a run must be written before any of its blocks can be cached. A run not yet written is a heap batch, in RSS, under neither the cache's budget nor its eviction.
- **Spilled runs are cached anyway.** The writer caches every block it writes (`from_raw_with_cache` in `storage/file/writer.rs`). The cost is therefore the write, mostly on the transaction's thread.
- **The alternative.** A write-back design would keep small runs unwritten under the cache's budget and write a run only on eviction, so short-lived runs never reach the disk. DBSP does not do this; the memtable arm tests a form of it ([memtable.md](../../../docs/design/merged-index/memtable.md)).

**Which writes they are** (a `--diagnostics` rerun of the same pilot):
- **Setup.** The cgroup was 2 GiB, because the diagnostics logs are held in memory and a 1 GiB run was OOM-killed. Pressure was `critical` from refresh 3 (native) and refresh 2 (merged-index). Its latencies are not reported.
- **Method.** `spill_kinds.py` classes every file flushed in the window by its `compaction-log.jsonl` record:
  - a merge output;
  - an insert spill;
  - otherwise, a file built during a step. The logs cannot tell an inserted run that was a file from the start from a temporary in-step run.

  The window is all six refreshes: 36,170 transactions.

Per transaction, view-state owners (`query`, `merged_index`):

| | native files | native KiB | merged-index files | merged-index KiB |
| --- | ---: | ---: | ---: | ---: |
| merge outputs | 0.47 | 287.4 | 0.24 | 165.2 |
| insert spills | 1.65 | 3.8 | 3.36 | 7.6 |
| built during a step | 6.59 | 14.0 | 4.20 | 9.3 |
| total | 8.71 | 305.2 | 7.80 | 182.1 |

- **Most writes are tiny files.** Insert spills and step-built files average 2.1 to 2.3 KiB. They are 95% of native's view files and 97% of merged-index's, but only 6% and 9% of the bytes.
- **What a lazy spill could save.** About 8 of native's 9 view writes per transaction, and 7.6 of merged-index's 7.8: 18 and 17 KiB. This counts files, not time.
- **Base (amortized).** 0.23 to 0.24 merge outputs per transaction (about 115 KiB), and almost no spills.
- `spill_kinds.py` counts every owner but the base tables as the view, which gives 165.3 KiB for merged-index merge outputs.

Artifacts: `/mnt/ssd/r5-out/pilot-A-seek-clustered-k10-v16-t512-c512{,-diag}/`.

## Prefix-filter A/B and the uuid order keys (2026-11-20)

**Settings.** Pilot A-seek clustered, K = 10, version 17 (revision `d4010417d`), one repetition per arm. The settings are those of the 512/512 pilot above. There are three image sets:
- `images-v17`: dbgen keys, filter off;
- `images-v17-pf`: dbgen keys, filter on;
- `images-v17-uuid`: uuid keys, filter off.

All six runs qualify. Every merged-index run was `low` in refresh 1 and `critical` from refresh 2, so the merged-index rows compare like with like. Refreshes 2 to 6, per transaction.

**The prefix filter (merged index, dbgen keys):**

| | RF1 off | RF1 on | RF2 off | RF2 on |
| --- | ---: | ---: | ---: | ---: |
| `ivm_ms` p50 / p95 / p99 | 4.56 / 7.42 / 9.14 | 3.98 / 6.61 / 8.36 | 3.36 / 5.76 / 7.20 | 3.06 / 4.82 / 6.11 |
| index per-run seeks | 9.8 | 2.5 | 26.6 | 7.0 |
| runs skipped by the filter (`filtered_prefix_probes`) | 0 | 7.3 | 0 | 21.6 |
| index device reads (`other/merged_index`, the driver's thread) | 6.41 | 5.59 | 1.34 | 1.50 |
| `base_ms` p50 (amortized) | 3.77 | 3.56 | 15.57 | 14.68 |

- **Seeks.** The filter skips three of every four runs a group read used to seek: about 2.4 seeks per touched group instead of 9.3 to 11.6.
- **Latency.** The median falls 13% (RF1) and 9% (RF2), and p99 falls 9% and 15%. Each of refreshes 2 to 6 is lower, by 5 to 15%.
- **Device reads.** Down 13% in RF1 (0.82 per transaction, for 7.3 skipped runs), and up 0.16 in RF2. So about nine in ten skipped seeks were cache hits, and the saving is mostly CPU.
  - **Inferred, not counted:** the skipped runs are mostly small recent ones, written through the cache, while the bottom runs that take the cold reads are rarely skipped.
  - **RF2's rise** may be cold loads of stored filter partitions, which the report does not count.
  - **The worker's reads** were unchanged.
- **Size.** The index's files were 3.05 GB at the end, against 3.23 GB: the dropped whole-key filter, less the prefix filters.
- **Noise.** Merged-index at versions 16 and 17, same code path, differs by at most 6% at the median. Native varied far more between identical configurations, so its rows are not reported:
  - its onset of `critical` moved between refresh 2 and refresh 5;
  - its median ranged from 1.7 to 4.5 ms.

**uuid order keys (filter off), against dbgen.** Native's dbgen run is from version 16, which went `critical` from refresh 2 as the uuid run did; merged-index's is from version 17 off.

| | RF1 native | RF1 merged-index | RF2 native | RF2 merged-index |
| --- | ---: | ---: | ---: | ---: |
| `ivm_ms` p50 / p99, dbgen | 3.88 / 8.93 | 4.56 / 9.14 | 4.46 / 10.04 | 3.36 / 7.20 |
| `ivm_ms` p50 / p99, uuid | 5.76 / 14.10 | 4.15 / 8.10 | 9.28 / 19.86 | 3.49 / 6.53 |
| worker-thread device reads, dbgen | 11.5 | 10.0 | 33.5 | 58.9 |
| worker-thread device reads, uuid | 61.1 | 50.2 | 62.0 | 85.0 |
| `base_ms` p50, dbgen / uuid (amortized) | 3.39 / 13.86 | 3.77 / 11.92 | 7.96 / 11.44 | 15.57 / 20.20 |

- **Random keys slow native's view, whose traces are keyed by order.** Its median rises 48% (RF1) and 108% (RF2). The merged index, clustered by customer, keeps its view latency: with uuid keys its median is 28% and 62% below native's.
- **Base pays in both modes.** Each transaction's orders land in scattered base blocks, which are evicted after every base transaction, so the worker reads 50 to 85 blocks per transaction. The worker-thread reads cover both circuits; the index's own reads on the driver's thread are not in this table.
- **No cancelled-row stretch.** The driver's reads grow from refresh to refresh as with dbgen (RF1 64 to 126 per transaction, against 49 to 90), with no run-away like `sequence`'s, so uuid needs no compaction fix.

Artifacts: `/mnt/ssd/r5-out/pilot-A-seek-clustered-k10-v17-t512-c512{,-pf,-uuid}/manifest.json`.

## Memtable at 512 MiB (2026-11-20)

Running.

**Settings.** Measurement version 18, revision `2275c7446`, images `images-v18-pf`. The pilot is A-seek clustered, K = 10, dbgen keys, prefix filters on, target 512 MiB, cgroup 1 GiB. The arms, from the [decision](DECISIONS.md#2026-11-20-the-memtable-arm-splits-one-512-mib-budget-448-mib-cache-64-mib-memtable):
- 512 MiB cache with the memtable off;
- 448 MiB cache with the memtable off;
- 448 MiB cache with `--memtable-mib 64`.

Each is followed by a `--diagnostics` rerun in 2 GiB, for writes by kind (`spill_kinds.py`).

Artifacts: `/mnt/ssd/r5-out/pilot-A-seek-clustered-k10-v18-pf-t512-c{512,448,448-m64}{,-diag}/`.

## Pending: Linux campaign

**Settings of every run.** These are the decided settings above, on the SSD:
- arm A-seek, with prefix truncation and value-header elision on;
- cgroup `memory.max` 1 GiB with swap 0;
- memory target 512 MiB and cache 512 MiB (one budget);
- base eviction on;
- no setup compaction;
- one worker;
- each run a fresh process from a copy of its image, with the inputs evicted.

**Remaining, by active dimension:**
1. **The memtable.** Report the three arms, then the user decides.
2. **The prefix filter's default.** The user's decision. Every later arm records `configuration.index_prefix_filter`.
3. **Scattered, uuid, and K = 100 and 1,000.** Pilots at the current version. Every arm needs its own inputs, and uuid its own images.
4. **The campaign.** `--runs 3` per chosen arm, after its pilot passes. `summarize.py` pools the manifests.
5. **The hard-disk probe.** The pilots, repeated with `--storage-path` on the hard disk. This is evidence only.

**Inputs** ([handoff](LINUX-HANDOFF.md#generate-tpc-h-inputs)):
- **Base tables.** SF 10 dbgen gives 1.5 M Customers, 15 M Orders and 60 M Lines.
- **Refreshes.** Three RF1/RF2 pairs of 15,000 Orders each, plus an optional warm-up pair.
- **Requirement.** Each image's loaded size must exceed `memory.max`: 17 to 20 GB against 1 GiB. That checks scale, not pressure.

**Storage files bypass the page cache, and inputs do not** (checked against the code):
- `StorageCacheConfig::to_custom_open_flags` (`crates/feldera-types/src/config.rs`) returns `O_DIRECT` for `FelderaCache` on Linux. `cache_flags` (`crates/dbsp/src/storage/backend.rs`) applies it when the POSIX backend opens a file. The driver selects `FelderaCache`.
- The `.tbl` inputs are read with `BufReader` over `File`, so they use the page cache. The runner evicts them inside the run's cgroup, so their reads are charged to it.
- `FBuf::ALIGNMENT` is 512 (`crates/storage/src/fbuf.rs`). A 4Kn disk, tmpfs or an overlay filesystem may fail. Use local ext4 or xfs.

## Definitions

- **The window.** Loading is setup and is excluded (it is a separate program since version 13). The measured window is the refreshes, from the first to the last refresh's cleanup.
  - **No forced compaction.** There is no final compaction wait and no extra publication (`completion_boundary = last_refresh_complete`).
  - **Since version 13** a run starts from a restored image. Every run is then a file, and the cache starts empty unless `--warm-up` ran.
- **Two circuits, one runtime.** The base circuit (primary-key tables) and the view circuit are chained by the base circuit's net delta, in one runtime with one buffer cache. The merged index is secondary storage of the base tables: `ManualIndex` runs on an auxiliary thread of that runtime, and its upkeep is base maintenance.
- **Phases.** [Timing definitions](#timing-definitions) define them. Base and IVM are always reported side by side, and an IVM figure alone is never quoted as a saving.
- **Refreshes stay whole.** Each refresh is complete in RAM; an oversized refresh fails rather than being split to fit a budget. A refresh may be cut into small transactions by the input files (`transaction_orders`), and each transaction is timed.
- **Refresh files are pre-read** before the window: RF1 into the `Change` the loop applies, RF2 into its key set. The window times no file read. The pre-read rows are process memory and count toward DBSP's pressure. Their cost is `refresh_inputs`.
- **Inputs evicted.** On Linux the driver advises `DONTNEED` on every input it read, after the pre-read. `measure.py` does the same inside the run's cgroup before the driver starts. A run whose inputs were not evicted does not qualify.
- **Window versus lifetime.** `resources.peak_rss_bytes` is the process's lifetime peak; the window figures are in `sequence`.
- **Durability.** A refresh's completion implies none. Checkpoint costs are deferred.
- **Equal inputs.** Both modes get the same logical refreshes. The manifest records the SHA-256 of every input and executable.
- **Excluded.** Full-join validation runs only on request and outside the window (`--no-verify` omits it). Reconstruction decoding, copying, sorting, spills and cleanup stay inside their intervals.
- **Counters.** Backend read and write counters are storage-backend operations. OS I/O is `linux_physical_io` (`/proc/self/io`, the whole process), and the cgroup's `io.stat` is under `cgroup_evidence`.

## RF2 discovery

The driver keeps one `BaseState` of the base tables' snapshots, replaced after each transaction.

- **Order IDs.** RF2 Order IDs resolve against the current Orders snapshot.
- **Deletion rows.** The complete rows come from native Lineitem's primary-key range, or, in merged-index mode, from the index's `(custkey, orderkey)` line range.
- **Duplicates and absent Orders.** Requested IDs are deduplicated, and absent Orders are counted.
- **No rescans.** Discovery never rescans base files.

Each lookup opens a new cursor. That cost grew without bound under `sequence` keys ([watch item](../../../docs/design/merged-index/compaction.md#watch-for-cancelled-rows-at-the-front-of-the-key-range)).

## Buffer cache

There is one cache: the runtime's worker cache (`--cache-bytes`). The view worker, the merged index's thread, and the driver's snapshot reads (RF2 discovery, verification and the scan session) all run inside `Runtime::with_buffer_cache`. No other cache is created, so the reported capacity covers every reader.

- **Bloom filter partitions** are in it since version 15.
- **The base tables' blocks** are evicted from it after each base transaction, since version 16.

## Report fields

The driver's JSON report (`src/main.rs`). `measure.py` rejects a report that fails `report_problems`, listed under [The runner](#the-runner).

**Version and configuration:**
- `measurement_version` (18), `completion_boundary`, and `compaction_policy`.
- `configuration`:
  - `front_code_byte_keys` and `elide_value_headers`;
  - `session_pass` (`null` in native mode);
  - `min_storage_bytes`, with its source and `run_placement`;
  - `setup_full_compaction`;
  - `filter_residency`;
  - `evict_base_after_transaction`;
  - `index_prefix_filter`;
  - `memtable_mib` (`null` when off);
  - `diagnostics`;
  - `runtime` (the storage directory and the cache).
- `front_coding` and `value_header_elision`: blocks written encoded, `layout_fallbacks` (which must be 0) and `size_fallbacks` (legitimate).
- `os`, `storage_device` (from `measure.py`; device, filesystem, sector size, `rotational`, `label`), and `image` (the reopened image's settings, checkpoint, inputs and load record). `load` is `null` for a run from an image. `warm_up` is `null`, or the warm-up's list and time.

**Per refresh:**
- **Phases.** As defined in [Timing definitions](#timing-definitions). With `transactions` and `transaction_latency`: each transaction's `total_ms`, `base_ms` and `ivm_ms`, with nearest-rank p50, p95, p99, maximum and mean.
- `base_eviction_ms` and `base_evicted_bytes`.
- `memory_pressure.before` and `.after`: DBSP's level (`low`, `moderate`, `high`, `critical`), sampled at the refresh's boundaries. It is updated about once a second, so a sample can lag.
- **Run reads** (`RunReads`):
  - **Fields:** seeks, `filtered_probes` (exact probes a file run's filters answered), `filtered_prefix_probes` (runs a group seek skipped), key, value and weight decodes, and `memory_*_reads` for in-memory runs, which are never added to decodes.
  - **Decodes are not disk reads;** compare device reads for storage traffic.
  - **Where they are counted:**
    - `view_run_reads`: the view worker per read owner (`query`, `base_relation`, `merged_index`, `requested_slice`, `partner_trace`). The worker's `merged_index` part is always zero: the scan session runs on the driver's thread.
    - `index_run_reads`: the scan session, on the driver's thread.
    - `index_upkeep_run_reads`: base maintenance's reads of `Old`.
    - `driver_run_reads`: the driver thread's reads per owner, including the routing's reads of the driving deltas under `requested_slice`.
  - **In `summarize.py`:** `dbsp_operator_*` (every owner but the last two), `partner_trace_*` and `requested_slice_*`.
- `index_session_counts`: the scan session's groups, cursor opens, logical seeks and steps, positions, payload decodes from `Old` and `Dlt`, and cancelled values.
- `index_layout`: the runs and bytes, in file and memory, of the before view (`Old`) and the after view (`Old` + `Dlt`).
- `profiled_cpu_ms`: the CPU profiler's step time of the base and view circuits. It cross-checks `base_ms` and `q3_ms`, and is not a phase. It leaves out the index upkeep and the scan session.
- `block_expansions`: storage-level rebuilds of encoded data blocks on cache misses, per thread (`view_worker`, `driver`, `index_upkeep`) and owner. They are not item decodes ([storage-layout.md](../../../docs/design/merged-index/storage-layout.md#prefix-truncation-on-the-read-path)).
- `run_counts`, from `dbsp::storage::diagnostics::spine_run_counts`:
  - **Per spine:** runs after each transaction (minimum, median, maximum), runs at the start and the end, runs per level, and merges with their real and CPU time.
  - **Memtable fields, when on:** `memtable_entries_at_end`, `memtable_bytes_at_end`, `memtable_flushes`, `memtable_node_copies` and `memtable_copy_ms`.
  - **`device_reads`:** every storage read of the process, by thread (`foreground`, `background`, `other`), split once by read owner and once by write owner.
- `backend_write_by_owner` in every `resources` block: writes per owner (`query`, `base_relation`, `merged_index`; the read owners `requested_slice` and `partner_trace` write nothing). The owners sum to the totals, and a compaction runs under its spine's owner.

**Per window** (`sequence`):
- `window_peak_rss_bytes`, `window_end_rss_bytes`, `window_peak_rss_error`;
- `cgroup`: `memory.current`, `memory.events`, named `memory.stat` counters and `memory.pressure`, before, after and as a delta, with `peak`;
- `window_started_micros` and `window_finished_micros`, which place the diagnostics logs;
- the cache occupancy at the start and the end;
- `observed_storage_bytes`, taken with merges possibly still running;
- the sums of every per-refresh counter, which `measure.py` checks.

**Inputs:**
- `refresh_inputs`: row counts, and `VmRSS` before and after the pre-read.
- `inputs_evicted`: `evicted`, files, failures, and the cgroup's `memory.current` before and after.

## Workloads

**SF 10 Linux workloads.** These are prepared by `prepare_inputs.py`; see [the handoff](LINUX-HANDOFF.md#prepare-the-workloads) and the RF1 and RF2 rows under [Decided](#decided).

**macOS smoke workloads.** Each starts from 64 Customers, 512 Orders and 2,048 Lines, and runs RF1, RF2, a repeated RF2 (absent Orders), an RF1 reinsertion, RF2, and an empty RF2. They are smoke tests only. Their highest RSS (47 MiB) is far below the pressure thresholds, so they show no memory pressure.

| Workload | Orders per RF1 | Customers referenced | Lines per Order | Lines per RF1 |
| --- | ---: | ---: | ---: | ---: |
| clustered | 16 | 1 shared Customer | 4 | 64 |
| scattered | 16 | 16 distinct Customers | 4 | 64 |
| large-group | 1 | 1 Customer | 10,000 | 10,000 |

## Column definitions

### Timing definitions

These are current since version 11. The history of each phase is in the [appendix](#appendix-measurement-versions).

| Field | Definition |
| --- | --- |
| `discovery_ms` | RF2 lookups and the assembly of complete deletion rows, with their decoding. |
| `checks_input_preparation_ms` | Operational checks, workload statistics, logical-row conversion, and circuit input serialization. |
| `base_ms` | Base maintenance: the base circuit's transaction plus, in merged-index mode, `index_ms`. |
| `index_ms` | A part of `base_ms`, never added again: completing and appending `Dlt` to the merged index, and taking its snapshot, on the index's thread. Zero in native mode. |
| `forward_ms` | Reading the base circuit's net delta (`workload::net_delta`), the same in both modes. This is harness overhead, since one deployed circuit passes deltas as streams; it is in neither phase. |
| `delta_ms` | A part of `ivm_ms`: preparing the view circuit's transaction delta. Native converts the net delta into input rows; merged-index builds `ΔC`, `ΔO` and `ΔL` from `Dlt` and hands them over. The two operations are not identical, so compare with and without it. |
| `q3_ms` | The view circuit's transaction, plus the scan session's collection in merged-index mode. |
| `ivm_ms` | `delta_ms + q3_ms`. A transaction's latency figure. |
| `base_eviction_ms` | Evicting the base tables' blocks after the base transaction: in `total_ms`, in neither phase. |
| `publication_cleanup_ms` | Snapshot publication and release, and driver cleanup. |
| Foreground ms | Sum of the refreshes' wall times, every phase above included. |
| Sequence ms and CPU ms | Wall time and process CPU (every thread, background merges included) from the first refresh to the last's cleanup. |

### Resource definitions

| Column | Definition |
| --- | --- |
| Read and write ops and bytes | Storage-backend operations submitted during the window; not OS I/O. |
| Observed storage | File lengths in the storage directory after the last refresh, with merges possibly running. |
| Sampled peak storage | The largest directory size, sampled every 10 ms and at the end. Shorter peaks may be missed. |
| Peak RSS | The process's lifetime peak (`getrusage`), including restore and the pre-read. Not a window figure. |
| Window peak RSS | `VmHWM` at the window's end, after `clear_refs` reset it at the start (Linux). |
| Window end RSS | `VmRSS` at the window's end (Linux). |
| Cgroup window block | `sequence.cgroup`: evidence of memory use, not proof of pressure. `peak` falls back to the sampled maximum of `memory.current` where `memory.peak` cannot be reset. |
| Memory pressure | DBSP's level for the runtime, from process RSS against the target, not from cgroup memory. |
| Cache occupancy | The worker cache's occupancy at the window's start and end; the capacity is `--cache-bytes`. |
| Memory and storage batches | Nonempty in-memory and file runs in the retained snapshots; the index reports its own. |

## Pending: pressure evidence

What the instrumentation showed on the first Linux host (kernel 6.8; for example the version 17 reports):
- **The `clear_refs` reset works:** `window_peak_rss_error` is `null`.
- **`memory.peak` cannot be reset on 6.8.** It is read-only, so `window_peak_supported` is false, and `peak` falls back to `max_sampled_memory_current_bytes`.
- **Per-refresh pressure levels** show `critical` from refresh 2 or 3 at 512/512. The cgroup's `memory.events max` stays 0, so DBSP's target binds, not the kernel's limit.

Still pending:
- **A pressure probe.** A run at a lower target (for example 192 MiB) whose samples show the intended levels.
- **An optional stress case.** Eight complete RF1 sets in RAM; a refresh that exceeds its budget must fail whole.
- **Any claim of reclaim or beyond-memory behavior.** The fields are evidence, not proof.

### Two pressure signals

DBSP's pressure level and the cgroup limit measure different things:

- **DBSP's signal.** The `rss-monitor` thread (`crates/dbsp/src/circuit/runtime.rs`) reads process RSS once a second (`process_rss_bytes`).
  - **Levels.** Moderate, High and Critical are 0.85, 0.90 and 0.95 of `max_rss`, which is set from `--memory-target-bytes` (`crates/feldera-types/src/memory_pressure.rs`). With no target, the level stays Low.
  - **Hysteresis.** After High, the level falls back only at Low. After Critical, only at Moderate or Low.
- **The cgroup limit.** `memory.max` charges anonymous memory and page cache together. Page cache here is input reads only, since state files use `O_DIRECT`.
- **Consequence.** The cgroup can near its limit, and the kernel can reclaim, while DBSP reports Low. Equally, DBSP's level says nothing about cgroup headroom. Report the two apart.
- **Mitigations.** Evicting the inputs and pre-reading the refreshes narrow the gap but do not close it.

## Setup compaction and the compaction threshold

A spine starts a background compaction when a level reaches 8 runs (`MIN_LEVEL0_MERGE_BATCHES`), so the window's writes depend on where each spine stands against that threshold when the window opens.

- **What the flag does.** `--setup-full-compaction` removes that dependence. Both modes start from one run per spine, which isolates the effect of fragmentation.
- **Off by default.** The flag is off ([decided](#decided)); results of versions 4 to 6 ran with it on.
- **The merged index reaches the threshold first.** It takes a run from every transaction that touches any of its tables, so it reaches the threshold a refresh earlier than each native trace. The evidence is in [history.md](../../../docs/design/merged-index/history.md#2026-10-14-write-and-storage-investigation).

### Terms

- **Run:** an immutable sorted collection in a spine, in memory or in a file. Feldera's code calls it a batch, and fields like `memory_batch_count` count runs.
- **Compaction:** LSM compaction. A requested full compaction is `DBSPHandle::start_compaction`, or `initiate_compaction` on the index's spine.
- **Base-relation state:** the base tables stored by primary key (Feldera's input map).

## Driver and runner options

The command lines are in [CLAUDE.md](CLAUDE.md) and [LINUX-HANDOFF.md](LINUX-HANDOFF.md).

### The driver

The driver takes complete dbgen base files and the refresh list.

- **Running it.** Build the release binary (native, or with `--features merged-index`) and run it from a fresh run directory.
  - `--storage-path DIR` puts file-backed state in a new `run-N` directory.
  - `--dump-runtime-circuit DIR` writes the runtime graphs after the last refresh.
- `--write-image DIR` loads the base files, writes one DBSP checkpoint of both circuits (plus, in merged-index mode, the index and its orphan lines) to `DIR/storage`, with `DIR/image.json`, and stops.
- `--from-image DIR` reopens a copy of an image and runs only the refreshes. It refuses an image written with other storage settings (`ImageSettings`: mode, workers, the two encodings, the prefix filter).
- **Deltas.** The CLI applies RF1 inserts and RF2 deletes under referential integrity. Merged-index mode accepts any base-table delta, but only the tests exercise that.

### The runner

`measure.py` is the runner.

**Layout.** `--storage-path` is the campaign root; each run gets `run-<repetition>-<mode>` under it, with `storage/`, `driver.json` and the logs.

**`--r5`.** The runner always passes it, which selects the measured configuration:
- native joins probe their traces through run cursors (`native_join_fetch: false`);
- one worker and one run per process;
- `--storage-path`, `--cache-bytes` (a whole number of MiB), `--memory-target-bytes` and `--report` are required.

**Options** (`python3 measure.py --help` lists them all):
- `--images DIR`: start each run from a copy of its mode's image (`--build-images` and `--complete-images` make them).
- `--front-code-byte-keys`, `--elide-value-headers`: the merged index's key and value encodings, with prefix truncation for keys and header elision for values. Native writes the same files either way.
- `--index-prefix-filter`: the prefix-filter arm. Images record it, so each arm needs its own.
- `--memtable-mib N`: the memtable arm. It must stay below half of the memory target, and is refused when building images.
- `--session-pass seek`: the only pass offered.
- `--setup-full-compaction`: compact fully after loading.
- `--warm-up LIST`: run one pair before the window, untimed.
- `--diagnostics`: write `flush-log.jsonl`, `compaction-log.jsonl`, `run-contents.json` and `summary.json` per run. The logs are held in memory, so allow a 2 GiB cgroup. `spill_kinds.py` classes their files by write kind.
- `--cgroup-root`, `--total-memory-bytes`: the per-run cgroup.
- `--device-label`: names the device.
- `--keep-run-storage`: keep each run's image copy.

**The refresh list** is a JSON array in execution order, of `{"rf1_orders": ..., "rf1_lines": ...}` or `{"rf2": ...}` items. A cut refresh also carries `transaction_orders`. Relative paths resolve against the list file.

**`report_problems` rejects a report unless:**
- the per-owner writes sum to the totals, and read owners write nothing;
- `delta_ms` is finite and non-negative, and `ivm_ms >= delta_ms`;
- the run reads follow the mode: native has `partner_trace` reads and no `requested_slice` reads, merged-index the reverse, and the worker never reads `merged_index`;
- `driver_run_reads`, the session counts, `index_upkeep_run_reads`, `profiled_cpu_ms` and `block_expansions` are well formed and sum over the refreshes;
- the transaction latencies are well formed;
- `session_pass`, `index_prefix_filter` and `memtable_mib` are what the runner asked for;
- the layout fallbacks are 0 for each encoding that is on;
- `measurement_version` is `MEASUREMENT_VERSION` (18).

The runner builds and keeps separate native and merged-index executables. It starts a fresh process and storage directory per mode and repetition.

## Deferred

These are deferred:
- broader updates;
- residual Q3 ([deferred.md](../../../docs/design/merged-index/deferred.md));
- periodic checkpoint measurements;
- Q5 and Q10.

## Appendix: measurement versions

Each version changed what the report means. `measure.py` and `summarize.py` accept only the current version.

- **2.** No forced final compaction (`last_refresh_complete`). Version 1 forced one; its results are historical ([history.md](../../../docs/design/merged-index/history.md#2026-10-08-r5-version-1-smoke-runs)). `q3_ms` covered the base tables as well.
- **3** (2026-10-25). Two chained circuits, the refresh pre-read, and input eviction. The merged index was booked in `combined_maintenance_ms` (`index_ms + q3_ms`), not in `base_ms`.
- **4** (2026-10-26). The merged index moved into the base runtime and phase: `base_ms` includes `index_ms`, and `ivm_ms = q3_ms`.
- **5** (2026-11-04). Run reads: per reader for the index, per owner for the view worker, and the index's layout.
- **6** (2026-11-05). One runtime with one buffer cache for both circuits, and the profiler's per-circuit CPU time.
- **7** (2026-11-10). DBSP's default run placement, with no `min_storage_bytes: Some(0)`. Results of version 6 and earlier differ in file placement. In-memory runs' access counters. `Old` and `Dlt` are published.
- **8** (2026-11-10). One scan session per transaction instead of four readers' scans. `index_run_reads` becomes `{session}`; `index_session_counts` and `index_upkeep_run_reads` are new.
- **9** (2026-11-11). The driving deltas' boundary copy is booked in `forward_ms`.
- **10** (2026-11-11). The driving deltas reach the view circuit as whole batches through DBSP's `Input` operator. `session_pass` is reported.
- **11** (2026-11-12).
  - **Readers.** Each reader holds its requests and keeps only its rows, and the `requested_slice` and `partner_trace` owners are new.
  - **`delta_ms`.** It is inside `ivm_ms` (`ivm_ms = delta_ms + q3_ms`), and `forward_ms` keeps only the net-delta read.
  - **A rename.** `filtered_probes` was `filter_rejections` until 2026-11-16, without a version change.
- **12** (2026-11-16). `block_expansions` and `storage_device`.
- **13** (2026-11-17). Loading separated: runs start from images (`--images`, the driver's `--from-image`), `load` is `null` and `image` is recorded.
- **14** (2026-11-17). Every transaction is timed (`transactions`, `transaction_latency`), and refreshes are cut by `transaction_orders`.
- **15** (2026-11-18). Bloom filters storage-resident (`filter_residency`), the cache 256 MiB, and `run_counts` with `device_reads`. New images.
- **16** (2026-11-18). Base tables evicted after each base transaction (`base_eviction_ms`, `base_evicted_bytes`). `BaseState` moved out of the image sources. New images.
- **17** (2026-11-19). The prefix-filter arm (`--index-prefix-filter`, `filtered_prefix_probes`), with its own images. The file format changed. New images.
- **18** (2026-11-20). The memtable arm (`--memtable-mib`, `configuration.memtable_mib`, `run_counts` memtable fields). `spine_async.rs` and `trace/ord/**` changed, so new images. `measure.py` accepts 18 from `2275c7446`.
