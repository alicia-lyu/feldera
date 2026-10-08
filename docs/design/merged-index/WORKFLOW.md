# Native Q3 compilation and run workflow

The [native Q3 experiment](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/README.md)
maintains the predicate-free materialized join `V = Customer ⋈ Orders ⋈ Lineitem`
through Feldera's incremental circuit. This page explains compilation, execution,
raw artifacts, and the agent-added presentation steps. The
[circuit note](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md)
owns the generated operator and state-read explanation; the
[results note](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/RESULTS.md)
owns the original dataset, correctness tests, and historical R1 measurements.
The experiment README documents the implemented R5 measurement harness; its
qualifying Linux campaign is pending.

The local graph captures and raw logs are in the repository’s
[`target/q3-circuit-exports/`](../../../target/q3-circuit-exports/) directory. The local native benchmark reports in
`target/q3-ivm-runs/20261002T183104Z/benchmark.log` remain separate from the
original measurements in the results note. Commands below use configurable
paths and fresh output directories.

## Reconstructed RF1/RF2 prototype

The experiment also has an opt-in reconstruction path. It is limited to
subsequent RF1/RF2 refreshes after initial state setup. The native path remains
the default and is generated and run with `./generate.sh` and default Cargo
features. The reconstructed path requires both the compiler option and Cargo
feature:

```bash
# From sql-to-dbsp-compiler/experiments/q3-ivm-baseline
./generate.sh --q3-merged-index
cargo +1.96.1 build --release --offline -j2 --features merged-index
```

Run the resulting executable with the same base and refresh file arguments
shown below. Initial loading builds maintained state as setup for maintenance
measurements. The R5 driver reads bounded batches, loading Customers first and
then complete Order/Lineitem groups in identical transactions in both modes. It
requires ascending Order keys and Lineitem rows grouped by Order, and reports
unsupported ordering. Independent `Reference` maps are allocated only for
correctness verification; `--no-verify` retains none. The supported workload
is RF1 insertion and RF2 deletion under referential integrity. Replacements,
key moves, orphan rows, independent Lineitem changes, and Customer updates are
outside the prototype workload.

The manual merged-index owner runs on a DBSP auxiliary thread outside the Q3
circuit graph. For each refresh, it captures the previous snapshot, applies
physical signed index rows, waits for insertion, then publishes previous and
current snapshots to Q3 before logical deltas are submitted. Customer placement
for changed orders comes from complete Orders records in that same delta batch,
supplying one candidate customer per order to the placement map. The
[separate placement correction](placement-lookup-multimap-plan.md) adds support for
multiple candidates in readers without extending the RF1/RF2 workload. No persistent
order-to-customer lookup is maintained. Snapshot views stay
published through the Q3 transaction and are released after its consumers
finish.

Reconstruction decodes and copies only requested matching key ranges into
temporary native batches, which the existing joins consume. This materializing
copy path is suboptimal: it adds decoding, copying, and possible sorting to Q3
maintenance. Those costs are included in `q3_ms`. A direct reconstructed cursor
could avoid the temporary batches and is a future optimization. The temporary
placement map uses `BTreeMap` and `BTreeSet`; container selection is another future
optimization that preserves its transaction-local contents. The current
prototype makes no performance-gain or hard-memory-cap claim.

For the R1 measurements, `index_ms`, `q3_ms`, and
`combined_maintenance_ms` were reported from the reconstructed run. R5 reports
discovery, preparation and checks, index maintenance, the Q3 transaction,
publication/cleanup, and complete foreground latency separately. The
`index_ms + q3_ms` subtotal excludes discovery and preparation. Initial load
remains setup rather than comparison data. Refresh completion carries no
durability guarantee; periodic checkpoint measurement and cross-circuit
checkpoint coordination are deferred.

## R5 measurement runner

See the [experiment README](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/README.md#r5-maintenance-measurements)
for the runner command and ordered refresh-list format. The list contains
`{"rf1_orders":"...","rf1_lines":"..."}` entries or `{"rf2":"..."}`
entries; relative paths resolve against the list file. The runner builds and
preserves separate native and reconstructed release executables, restores
default native generation, and uses fresh processes and storage directories for
each run, with three repetitions by default. It runs with Java 21 and Rust
1.96.1. An optional `CARGO_TARGET_DIR` can point to a reusable root cache.

Each run requires file-backed base state, materialized `V`, and manual merged
index; explicit cache capacity, memory-pressure target, and compaction timeout;
one worker; native join fetch; no compression; and `min_storage_bytes=0`.
Driver and manual-index storage lookups are scoped with
`Runtime::with_buffer_cache` to the existing configured worker cache, without
allocating an additional cache. An unbound lookup uses the runtime's global
256 MiB cache; the scoped binding is restored during unwind.
One shared `BaseState` uses Catalog batch snapshots and is refreshed after each
transaction for RF2 discovery and workload checks. File backing is checked from
`SerBatch::location`. Since `min_storage_bytes=0` may leave inserts in memory at
low pressure, explicit compaction rewrites an eligible lone memory batch when
the existing merge policy selects storage. Loading compaction finishes before
measurement. Loading and final drainage each include an empty logical
transaction that publishes compacted snapshots and releases old output-mailbox
references. Circuit and manual-index compaction then drain with a timeout; these
publication transactions are included in drainage and counted as
`snapshot_publication_transactions`. Drainage is included in sequence totals.
The JSON report includes hashes, revision, configuration,
interval measurements, and qualification status. Backend read/write operations
and bytes are distinct from OS physical I/O. CPU time, process RSS high-water,
cache occupancy, settled storage, sampled peak storage, affected customers and
orders, and line fan-out are reported; unavailable counters are marked
explicitly. On macOS OS physical byte counters are null. Independent checking
is excluded from measurement.

The old execution notes below describe the original R1 driver and remain to
interpret its saved logs and results. That driver buffered complete base files,
used retained source maps for RF2 discovery, and still retained those maps with
`--no-verify`. These statements do not describe the current R5 driver.

macOS runs are smoke tests only and do not establish equal enforced total
memory. Linux qualification requires matching cgroup v2 total-memory limits
with swap disabled and private inputs copied inside each run's cgroup during
loading. R5's harness is implemented, while the qualifying campaign is pending.
Broader updates, residual Q3, checkpoint measurement, and Q5/Q10 remain
deferred.

## Verify supported loading and RF1/RF2

With the compiler built and Java 21 configured, run:

```bash
# From sql-to-dbsp-compiler/experiments/q3-ivm-baseline
./verify.sh
```

This runs the native and reconstructed experiment suites, the COL suite, and
Python runner tests with Rust 1.96.1, `--offline --locked`, formatting checks,
strict Clippy, shell syntax, and whitespace checks. Both generated modes run the
same loading and RF1/RF2 test transactions with one worker. Initial loading is
included in correctness verification; it remains setup for maintenance
measurements. Each committed transaction compares emitted
`ΔV` and materialized `V` with a full join recomputed from the logical input records.
The reconstructed run also checks each reader's rows against expected rows computed
directly from those records for the requested keys. Each run writes a JSON file with
sorted emitted rows and materialized rows, including their weights, for every
transaction. The script uses `cmp` to check that the native and reconstructed files
are identical byte for byte.

The RF1/RF2 test transactions insert and delete complete Order/Lineitem groups
for multiple customers and orders. The tests also cover distinct lines with
identical payloads, oversized groups crossing batch boundaries, repeated refresh
cycles, empty transactions, and multiple runtime steps. Existing native broader-update,
storage, checkpoint, and unsupported-workload checks remain separate regressions.
This completes RF1/RF2 R4 verification; broader updates and residual Q3 remain
deferred, and no performance measurement is produced.

Logs and JSON result files go to a fresh ignored experiment-local
`target/r4-verification/` directory. Set `CARGO_TARGET_DIR` to reuse a different
compilation cache if needed. The script restores default native generation on
exit even after a failed check; restoration failures are reported explicitly.

## Worker policy

All future IVM maintenance runs use one DBSP worker (`--workers 1`), including
RF1 and RF2. Initial loading may use multiple workers.
Record the worker count in each run's configuration. The existing two-worker
graphs are retained as prior runtime inspection artifacts.

The IVM maintenance implementation targets single-worker execution. Partitioning,
cross-worker exchange, distributed state placement, and migration are outside
its current scope. No multiworker maintenance extension is planned.

## Configure paths

Use absolute paths and a fresh run directory. Set `REPO`, `TPCH_DATA`, and
`JAVA_HOME` for the local repository, complete dbgen base/refresh files, and
Java 21 installation. Graphviz is needed for SVG generation; an offline Cargo
build requires already-cached dependencies and Rust 1.96.1.

```bash
set -euo pipefail
REPO=/absolute/path/to/feldera
TPCH_DATA=/absolute/path/to/dbgen-output
JAVA_HOME=/absolute/path/to/java-21
export JAVA_HOME
export PATH="$JAVA_HOME/bin:$PATH"
export CARGO_TARGET_DIR="$REPO/target"
RUN_DIR="$REPO/target/q3-ivm-runs/$(date -u +%Y%m%dT%H%M%SZ)"
EXPERIMENT="$REPO/sql-to-dbsp-compiler/experiments/q3-ivm-baseline"
COMPILER="$REPO/sql-to-dbsp-compiler/SQL-compiler/sql-to-dbsp"
mkdir -p "$RUN_DIR/graphs"
cp "$EXPERIMENT/query.sql" "$RUN_DIR/query.sql"
```

The repository's `**/target` ignore rule covers this run directory, including
logs, manifests, and local helper scripts. Keep each run's artifacts together
there; they are not tracked documentation. Dataset generation and input hashes
are documented in the results note.

## 1. Build the SQL compiler

From the compiler directory, Java 21 runs the repository build script:

```bash
cd "$REPO/sql-to-dbsp-compiler"
./build.sh > "$RUN_DIR/compiler-build.log" 2>&1
```

[`build.sh`](../../../sql-to-dbsp-compiler/build.sh) loads
[`calcite_version.env`](../../../sql-to-dbsp-compiler/calcite_version.env), which
selects Apache Calcite commit `f840e7ab3f8121e71035a7a1ed3fc1372f0139d5` and
version `1.43.0`. The script builds Calcite with Gradle, installs its core and
linq4j JARs into the local Maven repository, checks keyword drift, and runs
`mvn package -DskipTests` to package Feldera's compiler. The
[`sql-to-dbsp` wrapper](../../../sql-to-dbsp-compiler/SQL-compiler/sql-to-dbsp)
executes `SQL-compiler/target/sql2dbsp-jar-with-dependencies.jar` with Java.

The default Calcite checkout is `sql-to-dbsp-compiler/calcite-build`, controlled
by `CALCITE_BUILD_DIR`. The sibling `../calcite` checkout is not automatically
selected. The local graph capture uses the rebuilt compiler JAR, with its hash
recorded in `target/q3-circuit-exports/provenance.txt`.

## 2. Translate SQL into a relational plan

The compiler parses and validates the copied `query.sql`, converts the view
query into Calcite relational nodes, and applies Feldera's optimizer rules.
See [`SqlToRelCompiler`](../../../sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/compiler/frontend/calciteCompiler/SqlToRelCompiler.java#L2037)
for conversion and the
[view optimization call](../../../sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/compiler/frontend/calciteCompiler/SqlToRelCompiler.java#L2401).
This happens inside each compiler invocation in the next stage.

`-TCalciteOptimizer=2` enables logging of the initial plan and changed
intermediate plans. The
[logging loop](../../../sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/compiler/frontend/calciteCompiler/optimizer/CalciteOptimizer.java#L167)
prints `After <step>` when a step changes the plan; it does not print every
unchanged pass. In this rerun, `After Hypergraph` records both operand swaps:
Orders becomes left of Customer, and Lineitem becomes left of their joined
result. Orders–Customer grouping remains intact. This observation comes from
the logged before/after plans, beyond the rule's eligibility alone.

## 3. Generate the incremental circuit

Compile with `-i`. Omit `-O` to retain default optimization level 2 and leave
Catalog mode at its default. The inspection uses three separate compiler
invocations:

```bash
DESTINATION="$RUN_DIR/incremental"
mkdir "$DESTINATION"
cd "$DESTINATION"
"$COMPILER" "$RUN_DIR/query.sql" -i \
  -TCalciteOptimizer=2 -TCircuitOptimizer=2 \
  --plan "$DESTINATION/relational-plan.json" \
  --dataflow "$DESTINATION/dataflow.json" \
  --js "$DESTINATION/schema.json" \
  -o "$DESTINATION/circuit.rs" \
  > "$RUN_DIR/incremental-compile.log" 2>&1
"$COMPILER" "$RUN_DIR/query.sql" -i \
  --svg -v 2 -o "$DESTINATION/circuit.svg" \
  > "$RUN_DIR/incremental-diagram.log" 2>&1
"$COMPILER" "$RUN_DIR/query.sql" -i \
  --jit -o "$DESTINATION/circuit.json" \
  > "$RUN_DIR/incremental-circuit-ir.log" 2>&1
```

To export the expanded logical graph for the incremental compilation, run an
additional compiler invocation with `-i` and
`--dump-expanded-circuit "$RUN_DIR/graphs/compiler"`. The capture occurs in
`MonotoneAnalyzer` after `DeltaExpandOperators` and monotonicity analysis,
before `InsertLimiters`:

```bash
cd "$RUN_DIR/incremental"
"$COMPILER" "$RUN_DIR/query.sql" -i \
  --dump-expanded-circuit "$RUN_DIR/graphs/compiler" \
  -o "$RUN_DIR/incremental/circuit-expanded.rs" \
  > "$RUN_DIR/graphs/compiler.log" 2>&1
```

`--plan` writes the optimized relational plan, `--dataflow` the dataflow
description, and `--js` the input/output schema. `--svg -v 2` produces a circuit
diagram with layout through Graphviz. `--jit` serializes circuit IR as JSON.
These are compiler artifacts; none of these commands executes a benchmark.
Working in the incremental directory also keeps auxiliary generated Rust files
(`stubs.rs` and `udf.rs`) separate. Option definitions are in the
[compiler usage guide](../../../sql-to-dbsp-compiler/using.md#command-line-options).

The incremental Rust calls `join_index_balanced` and `join_balanced`. Their
retained traces and before/after reads are implemented inside the native
runtime, so the top-level compiler graph does not expand them into separate
trace operators. The circuit note maps those calls to
[`dyn_join_generic`](../../../crates/dbsp/src/operator/dynamic/join.rs#L700)
and explains their signed-delta semantics.

## 4. Build the Rust executable

```bash
cd "$EXPERIMENT"
./generate.sh
cargo +1.96.1 build --release --offline -j2 \
  > "$RUN_DIR/release-build.log" 2>&1
```

`generate.sh` recreates Git-ignored `src/lib.rs`, `src/stubs.rs`, and `plan.json`
from the tracked SQL using the compiler built above. Cargo builds that generated
`src/lib.rs`, the handwritten `src/main.rs` driver and `src/data.rs` parser,
the handwritten `src/udf.rs` scaffolding, and native runtime dependencies declared in
[`Cargo.toml`](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/Cargo.toml).
`CARGO_TARGET_DIR` selects the repository target directory. Cargo does not
build run-directory Rust copies.

## 5. Load and refresh through the incremental circuit

```bash
cd "$RUN_DIR"
"$CARGO_TARGET_DIR/release/q3-ivm-baseline" \
  --customer "$TPCH_DATA/customer.tbl" \
  --orders "$TPCH_DATA/orders.tbl" \
  --lineitem "$TPCH_DATA/lineitem.tbl" \
  --rf1-orders "$TPCH_DATA/orders.tbl.u1" \
  --rf1-lines "$TPCH_DATA/lineitem.tbl.u1" \
  --rf2 "$TPCH_DATA/delete.1" \
  --workers 1 --batch 10000 --runs 3 \
  --dump-runtime-circuit "$RUN_DIR/graphs/runtime-1" \
  > "$RUN_DIR/benchmark.log" 2>&1
```

The driver writes `run-N/worker-ID.dot`, `.json`, `.svg`,
`worker-ID-styled.dot`, and `worker-ID-styled.svg`, plus `manifest.json`.
Runtime capture happens after the final refresh report and outside phase
timers. Supply a fresh output root: existing per-run directories are rejected
so captures are not overwritten. Preserve the output in the run directory.

The process constructs three fresh incremental circuits with one worker and
memory-backed state; omission of `--storage-path` selects that configuration.
Each circuit handles its initial load and then RF1/RF2 refreshes, so bootstrap
and refresh use the same incremental program. The
[parser](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/data.rs#L30)
validates exact dates and represents decimals as integer hundredths before
serializing them for Catalog inputs. The driver submits inputs and calls
[`circuit.transaction()`](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/main.rs#L298).
Load batches contain at most 10,000 total input records, taking Customer,
Orders, then Lineitem records. Loading bootstraps through the same incremental
circuit used for updates. RF1 submits related order/line inserts together;
RF2 discovers matching lines in the retained logical source map and submits
related deletes together. Each refresh phase is one transaction per run.

Native time surrounds only `circuit.transaction()`, including maintenance of
materialized `V`. Parsing, refresh discovery, batch assembly, input
serialization, independent full joins, and output inspection occur outside
that timer. Verification compares signed deltas and materialized snapshots
after every transaction. End-to-end time includes preparation and checking;
load also includes circuit initialization. The driver retains source maps for
verification and RF2 discovery, and buffers the base files during loading.
Its reference-byte measurement covers retained source maps. Peak process RSS
covers driver and runtime and is cumulative across runs in this process.

The successful log has nine phase reports: load, RF1, and RF2 for each run.
`delta_plus` and `delta_minus` sum positive and absolute negative output
multiplicities; `view_rows` sums materialized weights. They do not dump all `V`
tuples. The saved rerun has `verified=true` in every report, with eight load
transactions and one transaction per refresh. Counts match the correctness
table in the results note. Omitting verification with `--no-verify` makes those
counters unavailable and still retains source maps for RF2 discovery.

## Raw artifacts and presentation additions

Names below are relative to a run directory. `incremental/` contains direct
compiler artifacts. The incremental compile also
captures the compiler-expanded logical graph. The executable can capture
per-worker runtime graphs after each run's final refresh report with
`--dump-runtime-circuit "$RUN_DIR/graphs/runtime-1"` with `--workers 1`. Each run
uses a new directory; capture fails if a run directory already exists. Capture
is outside phase timers.

For the local example exports, the graph directories are
`target/q3-circuit-exports/compiler/`,
`target/q3-circuit-exports/runtime-1/run-1/`, and
`target/q3-circuit-exports/runtime-2/run-1/`. Compiler output is captured after
`DeltaExpandOperators` and monotonicity analysis, before `InsertLimiters`.
Runtime output comes from `graph_profile()` and preserves each worker's raw
DOT and JSON, plus raw and styled SVG. Both captures record revision,
command, configuration, and capture stage. See the experiment's
[CIRCUIT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md)
for generated graph links, styling labels, and interpretation.

| Artifact | Origin and contents |
| --- | --- |
| `incremental-compile.log`, `incremental-diagram.log`, `incremental-circuit-ir.log` | Captured output from incremental compiler invocations; a successful log may be empty. |
| `release-build.log` | Captured Cargo build output. |
| `benchmark.log` | Nine driver reports with timings, throughput, signed multiplicities, cardinalities, reference bytes, and peak RSS. |
| `git-provenance.log`, `git-status-before.log`, `java-version.log`, `rust-version.log`, `compiler-version.log`, `compiler-help.log`, `host.log` | Captured revision/status, Java/Rust/compiler versions, compiler help, and host information. |
| `relational-plan.json`, `dataflow.json`, `schema.json`, `circuit.json`, `circuit.rs`, `stubs.rs`, `udf.rs`, `circuit.svg` | Direct compiler artifacts in the incremental directory; Graphviz supplies SVG layout. |
| `relational-optimization.txt`, `operators.tsv` | Agent-extracted plan text from compile logs and operator listings from circuit JSON. |
| `graphs/compiler/expanded.dot`, `expanded.svg`, `expanded.metadata.txt` | Compiler-expanded logical graph and provenance, captured by `--dump-expanded-circuit`; the SVG is rendered from the preserved DOT. |
| `graphs/runtime-1/run-1/`, `graphs/runtime-2/run-1/` | Per-worker runtime graph captures: raw DOT/JSON, raw SVG, styled DOT/SVG, and a manifest with provenance. |
| `graphs/compiler.log`, `graphs/runtime-1.log`, `graphs/runtime-2.log`, `graphs/verification.log`, `graphs/provenance.txt` | Capture commands, their outputs, correctness verification with capture enabled, and provenance for the local graph artifacts. |
| `CIRCUIT.md` | Generated graph links and source-grounded explanation of the compiler and runtime captures. |
| `metrics.csv`, `validation.log` | Parsed benchmark fields, calculated rates, and validation/median output. |

Each capture preserves compiler or driver stdout/stderr in its raw log.
Compiler and runtime exports supply topology; state colours, textual markers,
Graphviz layout, and the accompanying legend are presentation steps. They do
not add nodes or edges.

### Export and styling hooks

The compiler's [`ToDot.exportArtifacts`](../../../sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/compiler/backend/dot/ToDot.java)
shares DOT writing with diagnostic dumps and renders SVG through Graphviz.
[`MonotoneAnalyzer`](../../../sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/compiler/visitors/outer/monotonicity/MonotoneAnalyzer.java)
exports its existing analysis circuit; it does not rebuild maintenance topology.
[`StateDotNodesVisitor`](../../../sql-to-dbsp-compiler/SQL-compiler/src/main/java/org/dbsp/sqlCompiler/compiler/backend/dot/StateDotNodesVisitor.java)
uses `DBSPIntegrateOperator` and `DBSPDelayedIntegralOperator` for the blue
integrator-state marker, and `IStateful` for other compiler-declared state; the Q3 analysis export contains these prior-integral placeholders
rather than explicit runtime integration regions. Expanded stream-join
products are neutral despite inherited `IStateful`: their supplied integrator
operands already represent the retained histories.

The experiment's [`dump_runtime_circuit` and `runtime_node_styles`](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/main.rs)
capture `GraphProfile` and map source-referenced operator names to existing IDs.
The monitor's [`Graph::to_dot_with_styles`](../../../crates/dbsp/src/monitor/visual_graph.rs)
shares topology traversal with `to_dot()`, whose profiling colours remain
unchanged. Existing integration-region IDs receive blue `[I]`; the renderer
adds no integrator nodes. `AccumulateDelayTrace` snapshots also receive integrator-state styling;
transaction delta collectors receive other-state styling. Unknown names receive `[U]`; this does not assert they are stateless.
Raw files survive Graphviz failures, including every worker's DOT and JSON.

### Metric formulas and precision

Let `R` be input records in a phase, `T` its transaction count, `N` native seconds,
`E` end-to-end seconds, and `P` the printed `native_rows_per_s` field.

| Metric | Denominator and calculation |
| --- | --- |
| Native input records/s | Driver computes `R / N` from the full-resolution duration, then prints one decimal place. |
| Native orders/s for RF1/RF2 | Helper computes `10 * P / R`: ten orders per refresh, with `R = 47` for RF1 and `R = 48` for RF2. |
| Native transactions/s | Helper computes `T * P / R`; RF1/RF2 each have `T = 1`. |
| End-to-end input records/s | Helper computes `1000 * R / end_to_end_ms`, using printed milliseconds. |
| Cross-run median | Take the median of the same phase's per-run metric across the three independent runs; do not pool load and refresh phases or invert a median time. |

The driver prints milliseconds to three decimal places and native records/s to
one. The helper prints derived rates to one decimal place. It uses `P` for
native derived rates because dividing by the already-rounded `native_ms` can
give a different answer. For example, RF1 run 1 prints `P = 122875.8` and
`native_ms = 0.382`; its derived orders/s is `10 * 122875.8 / 47 = 26143.8`
to one decimal place. No extra timing precision is recovered by the report.
The saved validation output records cross-run medians for native RF records/s;
the same median rule applies to other reported fields. Refresh rates describe
one small batch per fresh circuit, not sustained repeated refresh execution.

### Final checks

The captured verification log records correctness checks with graph capture
enabled. The compiler and runtime SVGs are rendered from their corresponding
DOT files. Provenance records revisions, commands, configurations, and capture
stages. Markdown and local artifact links should be checked when updating the
local capture set.
