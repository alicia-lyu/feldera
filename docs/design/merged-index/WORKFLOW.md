# Workflow: compile, test, verify, measure

This file is the only home for commands. It covers the Q3 experiment
[`q3-ivm-baseline`](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/README.md), which
maintains `V = Customer ⋈ Orders ⋈ Lineitem` in two modes: native, and reconstructed from the
Customer–Orders–Lineitem (COL) merged index. See [README.md](README.md) for the architecture.

Raw artifacts (logs, graph exports, reports) go in git-ignored `target/` directories. They are
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

## Generate and test

Run from `sql-to-dbsp-compiler/experiments/q3-ivm-baseline`. `generate.sh` writes the git-ignored
`src/lib.rs`, `src/stubs.rs`, and `plan.json` from `query.sql` (Catalog mode, default optimization
level 2). Run it before building or testing a fresh checkout.

```bash
# Native circuit (the default)
./generate.sh
cargo +1.96.1 test --offline --locked

# Reconstructed circuit: the compiler option and the Cargo feature must be enabled together
./generate.sh --q3-merged-index
cargo +1.96.1 build --release --offline -j2 --features merged-index
cargo +1.96.1 test --offline --locked --features merged-index
```

The COL library tests in its own directory; the DBSP side tests from the repository root:

```bash
# From sql-to-dbsp-compiler/experiments/merged-index-col
cargo +1.96.1 test --offline --locked

# From the repository root
cargo +1.96.1 test --offline -p dbsp --lib merged_index
cargo +1.96.1 test --offline -p dbsp --lib supplied_snapshot_join
```

The merged-index spec parser, validator, and emitter test in the SQL compiler, with Java 21 configured:

```bash
# From sql-to-dbsp-compiler
mvn test -pl SQL-compiler -Dtest='MergedIndex*Test' -Dsurefire.failIfNoSpecifiedTests=false
```

## Regenerate the COL module

`merged-index-col/src/generated.rs` is generated from `merged-index-col/col.mi` and the Q3 schema,
and committed. Regenerate it after changing the spec, `query.sql`, or the emitter:

```bash
# From sql-to-dbsp-compiler/SQL-compiler: the wrapper runs this JAR
mvn package -DskipTests
# From sql-to-dbsp-compiler/experiments/merged-index-col
./generate.sh                 # or ./generate.sh <output.rs>
cargo +1.96.1 test --offline --locked
```

`generate.sh` runs the compiler with `--noRust --merged-index col.mi --merged-index-output`, then
formats the result with rustfmt 1.96.1. The compiler wrapper always exits 0, so the script fails
when the compiler writes no file; the compiler's errors name `col.mi:line:column`.

## Verify R3 and R4

```bash
# From sql-to-dbsp-compiler/experiments/q3-ivm-baseline, compiler built, Java 21 configured
./verify.sh
```

`verify.sh` generates and tests both modes, then regenerates the COL module and fails if the
committed `generated.rs` differs (`col-generated-fresh`). It then runs formatting checks, strict
Clippy, the COL suite, the Python runner tests, a syntax check of each shell script, and a
whitespace check, all with `--offline --locked`. It compares the native and reconstructed transaction transcripts with `cmp`.
It restores native generation on exit, even after a failure. Logs and transcripts go to a fresh
`target/r4-verification/` directory; `CARGO_TARGET_DIR` can point at an existing build cache.
What the tests cover is in [reconstruction.md](reconstruction.md).

## Run the driver

The driver takes complete dbgen base files and one refresh batch. Build the release binary
first (native or reconstructed, per the previous section), then run it from a fresh run
directory. `$TPCH_DATA` is the dbgen output directory.

```bash
cargo run --release -- \
  --customer "$TPCH_DATA/customer.tbl" \
  --orders "$TPCH_DATA/orders.tbl" \
  --lineitem "$TPCH_DATA/lineitem.tbl" \
  --rf1-orders "$TPCH_DATA/orders.tbl.u1" \
  --rf1-lines "$TPCH_DATA/lineitem.tbl.u1" \
  --rf2 "$TPCH_DATA/delete.1" \
  --workers 1
```

Defaults: one worker, 10,000 input records per load transaction (`--batch`), three independent
runs (`--runs`), full verification, and memory-backed state. `--storage-path DIR` selects
file-backed state in a distinct `run-N` subdirectory per run. `--no-verify` skips the
independent full-join checks and leaves `delta_plus`, `delta_minus`, and `view_rows` unavailable
(`verified=false`). `--dump-runtime-circuit DIR` writes each run's per-worker runtime graphs
after its final refresh, outside the phase timers; the directory must not already hold a run.

Loading takes complete Order and Lineitem groups in ascending Order key and reports any other
order as unsupported. The supported workload is RF1 insertion and RF2 deletion under referential
integrity.

## Measure R5

```bash
# From sql-to-dbsp-compiler/experiments/q3-ivm-baseline, compiler built
python3 measure.py \
  --customer "$TPCH_DATA/customer.tbl" \
  --orders "$TPCH_DATA/orders.tbl" \
  --lineitem "$TPCH_DATA/lineitem.tbl" \
  --refresh-list "$TPCH_DATA/refreshes.json" \
  --report target/r5-measurements/report.json \
  --cache-bytes 1073741824 \
  --memory-target-bytes 4294967296 \
  --no-verify
```

`python3 measure.py --help` lists the remaining options (`--compaction-timeout-secs`, `--batch`,
`--runs`, `--storage-path`, `--diagnostics`, `--setup-full-compaction`, `--front-code-byte-keys`,
`--elide-value-headers`, `--cgroup-root`, `--total-memory-bytes`).

`--setup-full-compaction` (also a driver flag) changes only the setup. After loading, it requests
full compaction for the circuit and the merged index and waits for compaction to finish, then
publishes and starts the measured refreshes. Nothing is compacted on request after any refresh.
The report's `load.setup_full_compaction` holds the setup's cost.

`--front-code-byte-keys` (also a driver flag) sets DBSP's `front_code_byte_keys` dev tweak. Every
storage file writer then front-codes data blocks whose keys are raw byte strings with no auxiliary
data, which in this experiment is only the merged index's key level; native runs write the same
files as without it. The report's `front_coding` holds the writer's counters: blocks written
front-coded, `layout_fallbacks` (eligible blocks the encoder did not understand, which should be 0),
and `size_fallbacks` (blocks that front coding would not have shrunk by a 512-byte sector).

`--elide-value-headers` (also a driver flag) sets DBSP's `elide_value_headers` dev tweak. It is
independent of `--front-code-byte-keys`, so each is measured on its own. Every storage file writer
then drops the redundant 16-byte `ArchivedVec` header of each byte-string value in data blocks whose
items are a `Vec<u8>` with an `i64` weight, which in this experiment is only the merged index's value
level (its packed payloads), and rebuilds the header when the block is read. Native runs write the
same files as without it. The report's `value_header_elision` holds the counters, with the same
meaning as `front_coding`'s: `blocks`, `layout_fallbacks` (should be 0), and `size_fallbacks`.

`--diagnostics` turns on the opt-in storage diagnostics. Each run directory then gains a
`diagnostics/` directory (the driver's own `--diagnostics DIR`, which needs file storage and one run
per process) with four files:

- `flush-log.jsonl`: one line per backend flush, with file, bytes, owner and times.
- `compaction-log.jsonl`: one line per run that a spine wrote outside the circuit's steps, with
  kind (background compaction, requested compaction, or insert spill), owner, spine, input runs, and
  output file and size.
- `run-contents.json`: for every spine alive at the end of the window, each run's location,
  records, negative-weight records, file size and bytes per block type, plus the consolidated row
  count, labelled with the run's mode.
- `summary.json`: the window, the flush log split before, in and after the window, and whether the
  flush log matches the backend's counters.

The scans run outside the timed window. The runner also rejects any driver report whose per-owner
writes do not add up to its totals.

The refresh list is a JSON array in execution order. Each item is
`{"rf1_orders": "orders.tbl.u1", "rf1_lines": "lineitem.tbl.u1"}` or `{"rf2": "delete.1"}`;
relative paths resolve against the list file. `--cache-bytes` must be a whole number of MiB.

The runner builds and preserves separate native and reconstructed release executables, restores
native generation, and starts a fresh process and storage directory for each mode and repetition
(three by default). Definitions of the reported intervals, the compaction policy, and the pending
Linux qualification are in [measurement.md](measurement.md).

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

Runtime graphs come from the driver's `--dump-runtime-circuit` (see Run the driver). State
colors, text markers, and layout in the styled SVGs are presentation only; they add no nodes or
edges. The generated operators and their state reads are explained in
[CIRCUIT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md).
The hooks that render these graphs are `ToDot.exportArtifacts` and `StateDotNodesVisitor` in the
compiler and `dump_runtime_circuit` and `runtime_node_styles` in `src/main.rs`.
