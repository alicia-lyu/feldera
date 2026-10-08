# Q3 baseline and RF1/RF2 verification

The checked-in [query](query.sql) was compiled with `./generate.sh` on
2026-10-01 to produce the [optimized relational plan](plan.json),
[generated circuit](src/lib.rs), and UDF stubs. These outputs are now
Git-ignored; run `./generate.sh` before inspecting their local links.
The compiler was built by
`sql-to-dbsp-compiler/build.sh` against pinned Apache Calcite commit
`f840e7ab3f8121e71035a7a1ed3fc1372f0139d5` with OpenJDK `21.0.12.1`.
The compiler invocation uses `-i`, Catalog mode, and default `-O2`.
Regenerating twice produced identical SHA-256 hashes:

| Generated file | SHA-256 |
| --- | --- |
| `src/lib.rs` | `df6c6d5fe86e2ff1b83fa9c5b92c9edc31b71a617fa491ce371977d6a45cd420` |
| `src/stubs.rs` | `771132905b7ac8c6525add6c08d2321984924ae6f9d68a9da9017f4dd4b6e209` |
| `plan.json` | `bad1e218886c20071fa567dd556246661e7f46fbd63470c3f865670576dfdcdd` |

## Local dbgen input

The local source checkout was `tpch-kit` commit
`852ad0a5ee31ebefeed884cea4188781dd9613a3`. With `DSS_PATH` set to an
empty output directory and `DSS_CONFIG` set to that checkout's `dbgen`
directory (which contains `dists.dss`), the base and refresh commands were:

```sh
TPCH_KIT=/Users/alicialyu/Local/tpch-kit
DSS_PATH=/private/tmp/q3-native-tpch
DSS_CONFIG="$TPCH_KIT/dbgen"
export DSS_PATH DSS_CONFIG
mkdir -p "$DSS_PATH"
"$TPCH_KIT/dbgen/dbgen" -s 0.01 -T c
"$TPCH_KIT/dbgen/dbgen" -s 0.01 -T o
"$TPCH_KIT/dbgen/dbgen" -s 0.01 -U 1
```

These produced complete SF 0.01 base inputs and one RF1/RF2 batch:

| File | Records | SHA-256 |
| --- | ---: | --- |
| `customer.tbl` | 1,500 | `22156f2770387f5adadbc72774f2282d353aedd5092bd8fc54871b3bf5e74cba` |
| `orders.tbl` | 15,000 | `a444603dfba6c47e902e24b517608a5eb3b117127e99dff16a40f4eaa47b812c` |
| `lineitem.tbl` | 60,175 | `517b566190fbeadc638602554d109a463631e19788936ccb97196ebd407b51f1` |
| `orders.tbl.u1` | 10 | `98b3ed77419b6b34a2fb5b69fa232e26d3f433bf2ccb0bf86946106de990a74a` |
| `lineitem.tbl.u1` | 37 | `e809aab2ce9baeb5050419100bae49ad9850c00e0556e33253234bc6ea17b266` |
| `delete.1` | 10 | `4bfa140bac8454e10c8cb7da40ed65e1edb2775fbbbaff53b62647b294b4fc23` |

## Correctness

The original `cargo +1.96.1 test --offline -j2` run passed all six tests. The native
transaction test checked emitted signed deltas and the integrated `V`
snapshot after unmatched input, duplicate line payloads with distinct line
identities, customer and line payload replacements, a stable-order customer
move, an order and line key move, simultaneous changes, deletions, and an
empty transaction. A separate test covered simultaneous Customer, Orders, and
Lineitem bootstrap and replacements with one and two workers. Parser, RF2
discovery, and the separate two-worker circuit tests passed too.

Three independent local dbgen runs, each with a new circuit, agreed with an
independent full-join evaluator after every load batch and RF1/RF2
transaction:

| Phase | Source changes | Emitted `ΔV` | Materialized `V` rows |
| --- | ---: | ---: | ---: |
| Initial load | 76,675 inserts | +60,175 | 60,175 |
| RF1 | 10 Orders + 37 Lineitem inserts | +37 | 60,212 |
| RF2 | 10 Orders + 38 Lineitem deletes | −38 | 60,174 |

The RF2 file lists ten order keys. Their 38 matching lines are discovered in
one pass over the retained logical Lineitem map. Each RF1 and RF2 set of
related order and line changes is submitted in one native transaction.

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
moves, independent line updates, customer updates, and multimap changes are
outside R4. The
[separate placement correction](../../../docs/design/merged-index/placement-lookup-multimap-plan.md)
was present in the verified tree; R4 test transactions supply one candidate customer per
order and do not establish broader maintenance support. Residual Q3 and performance
measurements remain deferred; the original native measurements below are
separate evidence.

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

## Measurement command

The host was an Apple M1 Pro with 8 logical CPUs and 16 GiB RAM, running
Darwin arm64, macOS 26.3.1, with Rust/Cargo 1.96.1. The three
independent runs used the default one worker, 10,000 input records per load
transaction, and memory-backed state. The command was:

```sh
TPCH_DATA=/private/tmp/q3-native-tpch
CARGO_TARGET_DIR=/Users/alicialyu/Local/feldera/target \
cargo +1.96.1 build --release --offline -j2
/Users/alicialyu/Local/feldera/target/release/q3-ivm-baseline \
  --customer "$TPCH_DATA/customer.tbl" \
  --orders "$TPCH_DATA/orders.tbl" \
  --lineitem "$TPCH_DATA/lineitem.tbl" \
  --rf1-orders "$TPCH_DATA/orders.tbl.u1" \
  --rf1-lines "$TPCH_DATA/lineitem.tbl.u1" \
  --rf2 "$TPCH_DATA/delete.1"
```

Native latency times only `circuit.transaction()`, including the integrated
materialized `V`. Preparation covers parsing, refresh discovery, batch
assembly, and input serialization. End-to-end includes independent full-join
verification and output inspection; the load phase also includes circuit
initialization. Driver reference bytes are the retained source maps measured
with `SizeOf`. Peak process RSS includes both driver and embedded runtime and
is cumulative within the process across runs. The numbers below are for this
input, host, and build profile; they do not establish a performance gain.

The optimized build used Cargo's `release` profile. It completed in 13 minutes
40 seconds on a cold release cache; the final source rebuild completed after
the generated Rust's scoped Clippy allowances and handwritten style fixes.
The executable performed three runs in a single process. Each run used a new
circuit and fully verified its emitted deltas and integrated `V` against an
independent full join after each transaction. Load used eight transactions;
RF1 and RF2 each used one. Each throughput is input records divided by native
transaction time, so it includes materialized `V` maintenance but excludes
preparation and checking.

| Run | Phase | Native ms | Rows/s | Transaction ms min/median/p95/max | Prep ms | End-to-end ms | Driver reference bytes | Peak process RSS bytes |
| ---: | --- | ---: | ---: | --- | ---: | ---: | ---: | ---: |
| 1 | Load | 92.977 | 824,669 | 3.045 / 12.970 / 16.144 / 16.144 | 441.027 | 2,187.702 | 12,285,383 | 114,343,936 |
| 1 | RF1 | 0.314 | 149,642 | 0.314 / 0.314 / 0.314 / 0.314 | 0.595 | 186.102 | 12,285,853 | 126,468,096 |
| 1 | RF2 | 0.313 | 153,151 | 0.313 / 0.313 / 0.313 / 0.313 | 1.248 | 332.075 | 12,285,373 | 126,500,864 |
| 2 | Load | 34.130 | 2,246,582 | 3.142 / 4.495 / 4.706 / 4.706 | 164.815 | 974.367 | 12,285,383 | 126,500,864 |
| 2 | RF1 | 0.399 | 117,659 | 0.399 / 0.399 / 0.399 / 0.399 | 0.263 | 179.285 | 12,285,853 | 132,546,560 |
| 2 | RF2 | 0.691 | 69,494 | 0.691 / 0.691 / 0.691 / 0.691 | 1.521 | 176.024 | 12,285,373 | 132,546,560 |
| 3 | Load | 33.833 | 2,266,293 | 3.128 / 4.404 / 4.997 / 4.997 | 159.248 | 915.324 | 12,285,383 | 132,546,560 |
| 3 | RF1 | 0.300 | 156,623 | 0.300 / 0.300 / 0.300 / 0.300 | 0.244 | 181.383 | 12,285,853 | 139,591,680 |
| 3 | RF2 | 0.330 | 145,583 | 0.330 / 0.330 / 0.330 / 0.330 | 1.271 | 174.727 | 12,285,373 | 139,591,680 |

`cargo +1.96.1 fmt --all -- --check` and strict
`cargo +1.96.1 clippy --offline --all-targets -j2 -- -D warnings` passed.
The generator scopes four allowances to compiler-generated `src/lib.rs` syntax;
the handwritten driver and parser remain subject to strict Clippy.

## Current R5 v2 evidence (pending)

The revised harness no longer forces compaction. Loading waits only for natural
eligible background merges, publishes once with an empty transaction, and
records `load.background_merge_wait_ms` and `load.observed_storage_bytes`. The
measured sequence ends at the last refresh's publication and cleanup, with
`completion_boundary=last_refresh_complete` and
`snapshot_publication_transactions=0`; no final wait or transaction is run.
`sequence.observed_storage_bytes` is the storage size at that completion
boundary, not a settled post-compaction size. Process CPU includes natural
background merge work that runs during the sequence.
The JSON configuration's `compaction_policy` states that there is no forced
compaction, loading waits only for normal background merges, and there is no
final wait. Catalog storage summaries report snapshot, memory, and storage
batch counts plus `file_backed` for each relation. `manual_index_storage`
reports the auxiliary index separately; memory batches can remain at completion.

New smoke measurements for this completion boundary have not yet been recorded.
The prior table below is retained as historical R5 v1 evidence and must not be
treated as results from the revised harness. The Linux pressure campaign is
also pending; see the
[campaign plan](../../../docs/design/merged-index/r5-pressure-and-linux-campaign-documentation-plan.md).

## Historical R5 v1 harness verification and macOS smoke evidence

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
| **Final drainage ms** | Elapsed wall time after the six refreshes to drain circuit/manual-index compaction, publish compacted snapshots through an empty logical transaction, release obsolete snapshots, and clean outputs. Included in Sequence ms. |
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
