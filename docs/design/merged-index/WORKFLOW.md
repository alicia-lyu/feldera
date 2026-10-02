# Native Q3 compilation and run workflow

The [native Q3 experiment](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/README.md)
maintains the predicate-free materialized join `V = Customer ⋈ Orders ⋈ Lineitem`
through Feldera's incremental circuit. This page explains compilation, execution,
raw artifacts, and the agent-added presentation steps. The
[circuit note](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md)
owns the generated operator and state-read explanation; the
[results note](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/RESULTS.md)
owns the original dataset, correctness tests, and measurements.

The saved rerun `target/q3-ivm-runs/20261002T183104Z` is a local example from
2026-10-02, separate from the measurements in the results note. Its `rerun.py`
and `summarize.py` are local, gitignored helpers with host-specific paths, rather
than checked-in entry points. The directory preserves the compiler outputs,
logs, checks, and formatted report. The examples below express its commands
with configurable paths; they do not require those helpers.

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
mkdir -p "$RUN_DIR"
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
selected. The saved rerun reused the existing compiler JAR: its manifest contains
compiler version/help queries and SQL compilations, but no Calcite or Maven
build command. Thus `compiler-build.log` above belongs to a new build, not to
that rerun's artifact set.

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

## 3. Generate ordinary and incremental circuits

Compile once without `-i` and once with it. Omit `-O` to retain default
optimization level 2 and leave Catalog mode at its default. Each mode uses
three separate compiler invocations:

```bash
for mode in ordinary incremental; do
  set --
  if [[ "$mode" == incremental ]]; then
    set -- -i
  fi
  destination="$RUN_DIR/$mode"
  mkdir "$destination"
  cd "$destination"
  "$COMPILER" "$RUN_DIR/query.sql" "$@" \
    -TCalciteOptimizer=2 -TCircuitOptimizer=2 \
    --plan "$destination/relational-plan.json" \
    --dataflow "$destination/dataflow.json" \
    --js "$destination/schema.json" \
    -o "$destination/circuit.rs" \
    > "$RUN_DIR/$mode-compile.log" 2>&1
  "$COMPILER" "$RUN_DIR/query.sql" "$@" \
    --svg -v 2 -o "$destination/circuit.svg" \
    > "$RUN_DIR/$mode-diagram.log" 2>&1
  "$COMPILER" "$RUN_DIR/query.sql" "$@" \
    --jit -o "$destination/circuit.json" \
    > "$RUN_DIR/$mode-circuit-ir.log" 2>&1
done
```

`--plan` writes the optimized relational plan, `--dataflow` the dataflow
description, and `--js` the input/output schema. `--svg -v 2` produces a circuit
diagram with layout through Graphviz. `--jit` serializes circuit IR as JSON.
These are compiler artifacts; none of these commands executes a benchmark.
Working in each mode's directory also keeps auxiliary generated Rust files
(`stubs.rs` and `udf.rs`) separate. Option definitions are in the
[compiler usage guide](../../../sql-to-dbsp-compiler/using.md#command-line-options).

The optimized ordinary circuit differentiates Orders and Customer, joins their
deltas with `join_index`, integrates that result, and then uses `stream_join`
with Lineitem. Ordinary compilation can therefore retain incremental operators
internally after optimization. The incremental Rust instead calls
`join_index_balanced` and `join_balanced`. Their retained traces and before/after
reads are implemented inside the native runtime, so the top-level compiler
graph does not expand them into separate trace operators. The circuit note
maps those calls to
[`dyn_join_generic`](../../../crates/dbsp/src/operator/dynamic/join.rs#L700)
and explains their signed-delta semantics.

## 4. Check the generated baseline

Keep raw inspection output unchanged. Normalize a separate copy of incremental
Rust using the same adjustments as the checked-in
[`generate.sh`](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/generate.sh):

```bash
cp "$RUN_DIR/incremental/circuit.rs" \
  "$RUN_DIR/incremental/circuit.normalized.rs"
perl -pi -e 's/[ \t]+$//' "$RUN_DIR/incremental/circuit.normalized.rs"
perl -0pi -e 's{\A// Automatically-generated file\n}{// Automatically-generated file\n#![allow(clippy::useless_conversion, clippy::explicit_auto_deref, clippy::clone_on_copy, clippy::type_complexity)]\n}' \
  "$RUN_DIR/incremental/circuit.normalized.rs"
rustfmt +1.96.1 --edition 2021 "$RUN_DIR/incremental/circuit.normalized.rs"
{
  cmp "$RUN_DIR/incremental/circuit.normalized.rs" "$EXPERIMENT/src/lib.rs"
  cmp "$RUN_DIR/ordinary/relational-plan.json" \
    "$RUN_DIR/incremental/relational-plan.json"
  cmp "$RUN_DIR/incremental/relational-plan.json" "$EXPERIMENT/plan.json"
  echo 'Normalized Rust and all three relational plans match byte for byte.'
} > "$RUN_DIR/generation-check.log" 2>&1
```

Whitespace trimming, the four file-scoped Clippy allowances, and rustfmt do not
change query semantics. The saved rerun passed all three byte comparisons. Its
raw `circuit.rs`, normalized copy, JSON plans, and diagrams remain available.
The comparison establishes that the inspected incremental program reproduces
the checked-in baseline; it does not replace that library with the inspection
copy.

## 5. Build the Rust executable

```bash
cd "$EXPERIMENT"
cargo +1.96.1 build --release --offline -j2 \
  > "$RUN_DIR/release-build.log" 2>&1
```

Cargo builds the checked-in generated `src/lib.rs`, handwritten `src/main.rs`
driver and `src/data.rs` parser, and native runtime dependencies declared in
[`Cargo.toml`](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/Cargo.toml).
`CARGO_TARGET_DIR` selects the repository target directory. Cargo does not
build the ordinary inspection circuit or either run-directory Rust copy.

## 6. Load and refresh through the incremental circuit

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
  > "$RUN_DIR/benchmark.log" 2>&1
```

The process constructs three fresh circuits with one worker and memory-backed
state; omission of `--storage-path` selects that configuration. The
[parser](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/data.rs#L30)
validates exact dates and represents decimals as integer hundredths before
serializing them for Catalog inputs. The driver submits inputs and calls
[`circuit.transaction()`](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/main.rs#L295).
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

Names below are relative to the saved run directory. Both `ordinary/` and
`incremental/` contain direct compiler artifacts and derived inspection files.

| Artifact | Origin and contents |
| --- | --- |
| `ordinary-compile.log`, `incremental-compile.log` | Captured compiler stdout/stderr: relational optimization output and circuit-pass timings. |
| `*-diagram.log`, `*-circuit-ir.log` | Captured output from the separate SVG and `--jit` invocations; a successful log may be empty. |
| `release-build.log` | Captured Cargo build output. |
| `benchmark.log` | Nine driver reports with timings, throughput, signed multiplicities, cardinalities, reference bytes, and peak RSS. |
| `git-provenance.log`, `git-status-before.log`, `java-version.log`, `rust-version.log`, `compiler-version.log`, `compiler-help.log`, `host.log` | Captured revision/status, Java/Rust/compiler versions, compiler help, and host information. |
| `relational-plan.json`, `dataflow.json`, `schema.json`, `circuit.json`, `circuit.rs`, `stubs.rs`, `udf.rs`, `circuit.svg` | Direct compiler artifacts in each mode directory; Graphviz supplies SVG layout. |
| `run.log` | Aggregated stage output with agent-added command, directory, UTC timestamp, exit-status, and elapsed-time headers, plus a validation footer. |
| `manifest.json` | Agent-produced configuration, source/input/JAR/binary hashes, command outcomes, artifact hashes, and checks. |
| `circuit.normalized.rs`, `generation-check.log` | Agent-normalized comparison copy and byte-comparison results. |
| `relational-optimization.txt`, `operators.tsv` | Agent-extracted plan text from compile logs and operator listings from circuit JSON. |
| `CIRCUIT.md`, `incremental/expanded-native.mmd` | Copied circuit explanation and extracted explanatory Mermaid expansion of native trace reads and before/after semantics, with source references in the note. |
| `metrics.csv`, `README.md`, `validation.log` | Agent-parsed benchmark fields, calculated rates, formatted report, and validation/median output. |

The runner merges stdout and stderr into each stage log, copies those lines
into `run.log`, and records command outcomes in `manifest.json`. The summary
helper parses reports into CSV and Markdown tables. The expanded native
diagram and formatted report are agent-authored explanations, while the
compiler artifacts and underlying measurements remain identifiable.

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

The agent repaired local documentation links and checked Markdown formatting,
JSON parsing and circuit-mode flags, SVG XML, generated-file equality, and all
nine reported verification results. It also preserved provenance and artifact
hashes in the manifest. The explanatory Mermaid diagram belongs to the circuit
note; the SVG is a separate compiler-generated diagram. Checks of links and
formatting establish readability, while the independent full-join comparisons
establish the executed transactions' correctness.

When documenting an existing run, inspect its manifest commands, raw reports,
comparison results, and source references without rewriting historical logs
or rerunning its helpers. This workflow documentation requires no new benchmark
run, compiler/driver changes, or production tests.
