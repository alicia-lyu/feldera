# Workflow: cross-directory commands

Commands live with their directory: each directory's `CLAUDE.md` lists the commands and test
filters for its code. This file holds the commands that cross directories: the prerequisites, the
DBSP and Java test filters, regenerating an index module, and inspecting the compiler pipeline. See
[README.md](README.md) for the architecture and the code map.

The Q3 harness commands (generate, test, `verify.sh`, the driver, `measure.py`) are in
[q3-ivm-baseline/CLAUDE.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CLAUDE.md). The Linux measurement handoff is in
[LINUX-HANDOFF.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/LINUX-HANDOFF.md). Rules for any index and harness are in
[experiments/CLAUDE.md](../../../sql-to-dbsp-compiler/experiments/CLAUDE.md).

Raw artifacts (logs, graph exports, reports) go in ignored `target/` directories. They are
local only and not tracked documentation.

## Worker policy

Maintenance and initial loading both use one DBSP worker
([decisions.md](decisions.md#2026-10-12-one-dbsp-worker)). The driver defaults to `--workers 1`.
Reconstructed mode and R5 runs reject any other count. Record the worker count in every run
configuration. No multiworker extension is planned.

## Prerequisites

- JDK 21 (`export JAVA_HOME=/opt/homebrew/opt/openjdk@21`; the machine default is 17).
- Rust 1.96.1: pass `+1.96.1` to cargo.
- The SQL compiler built once from `sql-to-dbsp-compiler`:

  ```bash
  ./build.sh
  ```

  [`build.sh`](../../../sql-to-dbsp-compiler/build.sh) loads
  [`calcite_version.env`](../../../sql-to-dbsp-compiler/calcite_version.env), builds the pinned
  Calcite checkout (`calcite-build`, set by `CALCITE_BUILD_DIR`), then runs
  `mvn package -DskipTests`. The wrapper `SQL-compiler/sql-to-dbsp` runs the resulting JAR.
- Graphviz, only for SVG output.

## Test filters

The DBSP side tests from the repository root:

```bash
cargo +1.96.1 test --offline -p dbsp --lib merged_index
cargo +1.96.1 test --offline -p dbsp --lib supplied_snapshot_join
cargo +1.96.1 test --offline -p dbsp --lib run_reads
cargo +1.96.1 test --offline -p dbsp --lib merged_index::run_merge
cargo +1.96.1 test --offline -p dbsp --lib merged_index::session
cargo +1.96.1 test --offline -p dbsp --lib merged_index::reader
cargo +1.96.1 test --offline -p dbsp --lib read_owner_cursor
```

The `run_reads` filter covers the run counters (`RunReads`). The `run_merge` and `session` filters
cover the merging cursor and the scan session, and the `reader` filter the readers, their capture of
`Dlt`, and the routing. The `read_owner_cursor` filter covers the adapter that charges a join's
partner cursor to the `requested_slice` read owner. The `supplied_snapshot_join` filter also covers
a delta that a source emits at the flush. The COL library tests in its own directory
(`merged-index-col`, see its [CLAUDE.md](../../../sql-to-dbsp-compiler/experiments/merged-index-col/CLAUDE.md)):

```bash
# From sql-to-dbsp-compiler/experiments/merged-index-col
cargo +1.96.1 test --offline --locked
```

The merged-index spec parser, validator, and emitter test in the SQL compiler, with Java 21 configured:

```bash
# From sql-to-dbsp-compiler
mvn test -pl SQL-compiler -Dtest='MergedIndex*Test' -Dsurefire.failIfNoSpecifiedTests=false
```

## Regenerate an index module

An index's generated module (for COL, `merged-index-col/src/generated.rs`) comes from its spec
(`col.mi`) and the SQL schema it names (`q3-ivm-baseline/query.sql`), and is committed. Regenerate it
after changing the spec, the schema, or the emitter:

```bash
# From sql-to-dbsp-compiler/SQL-compiler: the wrapper runs this JAR
mvn package -DskipTests
# From the index crate's directory (COL: sql-to-dbsp-compiler/experiments/merged-index-col)
./generate.sh                 # or ./generate.sh <output.rs>
cargo +1.96.1 test --offline --locked
```

A new index crate needs a `generate.sh` of its own. COL's runs the compiler with
`--noRust --merged-index col.mi --merged-index-output`, then formats the result with rustfmt 1.96.1.
The compiler wrapper always exits 0, so the script fails when the compiler writes no file; the
compiler's errors name `col.mi:line:column`. The harness's `verify.sh` regenerates the module and
fails if the committed file differs (`col-generated-fresh`).

## Inspect the compiler pipeline

These commands produce compiler artifacts only; none runs a benchmark. Set paths first and use a
fresh run directory under `target/`.

```bash
set -euo pipefail
REPO=/absolute/path/to/feldera
export JAVA_HOME=/absolute/path/to/java-21
export PATH="$JAVA_HOME/bin:$PATH"
RUN_DIR="$REPO/target/q3-ivm-runs/$(date -u +%Y%m%dT%H%M%SZ)"
COMPILER="$REPO/sql-to-dbsp-compiler/SQL-compiler/sql-to-dbsp"
mkdir -p "$RUN_DIR/graphs" "$RUN_DIR/incremental"
cp "$REPO/sql-to-dbsp-compiler/experiments/q3-ivm-baseline/query.sql" "$RUN_DIR/query.sql"
cd "$RUN_DIR/incremental"
```

Compile with `-i`. Omit `-O` to keep optimization level 2 and the default Catalog mode.
Option definitions are in the [compiler usage guide](../../../sql-to-dbsp-compiler/using.md).

```bash
"$COMPILER" "$RUN_DIR/query.sql" -i \
  -TCalciteOptimizer=2 -TCircuitOptimizer=2 \
  --plan relational-plan.json --dataflow dataflow.json --js schema.json \
  -o circuit.rs > "$RUN_DIR/incremental-compile.log" 2>&1
"$COMPILER" "$RUN_DIR/query.sql" -i --svg -v 2 -o circuit.svg \
  > "$RUN_DIR/incremental-diagram.log" 2>&1
"$COMPILER" "$RUN_DIR/query.sql" -i --jit -o circuit.json \
  > "$RUN_DIR/incremental-circuit-ir.log" 2>&1
```

`-TCalciteOptimizer=2` logs the initial relational plan and each pass that changes it (the
`CalciteOptimizer` logging loop prints `After <step>` only for changing steps). `--plan` writes the
optimized plan, `--dataflow` the dataflow description, `--js` the input and output schema,
`--svg` a Graphviz diagram, and `--jit` the circuit IR as JSON.

To export the compiler's expanded logical graph, captured by `MonotoneAnalyzer` after
`DeltaExpandOperators` and monotonicity analysis and before `InsertLimiters`:

```bash
"$COMPILER" "$RUN_DIR/query.sql" -i \
  --dump-expanded-circuit "$RUN_DIR/graphs/compiler" \
  -o circuit-expanded.rs > "$RUN_DIR/graphs/compiler.log" 2>&1
```

Runtime graphs come from the driver's `--dump-runtime-circuit` (see [q3-ivm-baseline/CLAUDE.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CLAUDE.md)). State
colors, text markers, and layout in the styled SVGs are presentation only; they add no nodes or
edges. The generated operators and their state reads are explained in
[CIRCUIT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md).
The hooks that render these graphs are `ToDot.exportArtifacts` and `StateDotNodesVisitor` in the
compiler and `dump_runtime_circuit` and `runtime_node_styles` in `src/main.rs`.
