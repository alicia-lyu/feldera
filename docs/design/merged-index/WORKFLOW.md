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
cargo +1.96.1 run --release --offline --locked -- \
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

A macOS smoke run with the current configuration. `$SMOKE_INPUTS` holds `customer.tbl`,
`orders.tbl`, `lineitem.tbl`, and `refreshes.json` of one small workload (how the earlier smoke
inputs in `target/r5-smoke-inputs/` were generated is not recorded in the repository).
Both destinations must be new:

```bash
# From sql-to-dbsp-compiler/experiments/q3-ivm-baseline, compiler built
python3 measure.py \
  --customer "$SMOKE_INPUTS/customer.tbl" \
  --orders "$SMOKE_INPUTS/orders.tbl" \
  --lineitem "$SMOKE_INPUTS/lineitem.tbl" \
  --refresh-list "$SMOKE_INPUTS/refreshes.json" \
  --report target/r5-measurements/smoke-report.json \
  --storage-path target/r5-measurements/smoke-runs \
  --batch 512 \
  --cache-bytes 33554432 \
  --memory-target-bytes 536870912 \
  --runs 3 \
  --no-verify \
  --setup-full-compaction \
  --front-code-byte-keys \
  --elide-value-headers
```

`--storage-path` is the campaign root. Each run gets a new `run-<repetition>-<mode>` directory
under it, holding `storage/`, `driver.json`, and the logs. The Linux campaign adds `--cgroup-root`
and `--total-memory-bytes`; see the [Linux handoff](#linux-handoff).

The runner always starts the driver with `--r5`. That flag (`Config::from_args` in `src/main.rs`)
selects the measured configuration: it turns on DBSP's native join fetch and records it as
`native_join_fetch` in the report. It is accepted only with one worker, one run per process
(`--runs 1`), `--storage-path`, `--cache-bytes`, `--memory-target-bytes`, and `--report`;
otherwise the driver exits with an error. Running the driver by hand with `--r5` therefore needs
all of those. `--cache-bytes` must be a whole number of MiB.

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

## Linux handoff

This section is for a fresh agent on a fresh Linux machine that will run the R5 pilot. It is the
only place for these commands; [measurement.md](measurement.md) defines what is measured and why.
Anything marked "unverified" could not be checked from the repository.

### Pre-flight checks

These depend on the Linux machine and were not testable on macOS. Each fails loudly at startup
rather than corrupting results. Run them before building anything.

| Check | Command | If it fails |
| --- | --- | --- |
| Unified cgroup v2 with the memory controller | `stat -fc %T /sys/fs/cgroup` prints `cgroup2fs`; `cat /sys/fs/cgroup/cgroup.controllers` lists `memory` | The machine cannot qualify a run. Stop and report. |
| Kernel 5.19 or later, for `memory.peak` | `uname -r` | Runs complete, but `memory.peak` is `null`. Report the version. |
| Swap accounting on | After the recipe below, `ls /sys/fs/cgroup/r5/driver/memory.swap.max` | `configure_cgroup` fails on the first run. Enable swap accounting or report. |
| Delegation works | The recipe below, then `cat /proc/self/cgroup` prints `0::/r5/driver` | Adapt the recipe; do not run without `--cgroup-root`. |
| Storage path accepts 512-byte `O_DIRECT` | `df -T <storage dir>` shows ext4 or xfs; `sudo blockdev --getss <device>` prints 512 | Use another filesystem or disk. A 4 KiB logical sector, tmpfs, or overlay may fail with "Invalid argument" on the first state write (untested). |
| Disk room | `df -h <storage dir>` against the budget below | Free space or lower `--runs`. |

### State a clone needs

- Branch `docs/merged-index-tidy-up`, or any commit that contains this section. The branch is not
  pushed: the user must push it or supply it (for example as a `git bundle`).
- `q3-ivm-baseline/src/lib.rs`, `src/stubs.rs`, and `plan.json` are generated and git-ignored.
  `measure.py` runs `generate.sh` itself.
- Raw artifacts (reports, run directories) are git-ignored and local to the machine.

### Toolchain

- Rust: install rustup, then `rustup toolchain install 1.96.1 --component rustfmt clippy`. Every
  cargo command passes `+1.96.1`.
- Fetch dependencies online once. The runner builds with `--offline --locked`, which fails without
  a populated registry cache. `q3-ivm-baseline` and `merged-index-col` are separate workspaces with
  their own `Cargo.lock`, so fetch in each directory you will build in. Fetch at the repository root
  too if you will run the DBSP tests.

  ```bash
  # From sql-to-dbsp-compiler/experiments/q3-ivm-baseline, then from merged-index-col
  cargo +1.96.1 fetch --locked
  ```

- Java: JDK 21 (19 to 21 work, per the repository CLAUDE.md) and Maven. Set `JAVA_HOME` to the
  Linux JDK path (for example `/usr/lib/jvm/java-21-openjdk-amd64` on Debian-family systems;
  unverified) and put its `bin` first on `PATH`.
- SQL compiler: from `sql-to-dbsp-compiler`, run `./build.sh`. It needs network access, because
  it fetches or builds Calcite (`calcite-build`, pinned by `calcite_version.env`) and Maven
  dependencies, and then runs `mvn package -DskipTests`. It uses GNU `sed -i`, which Linux has.
- Python: 3.9 or later. `measure.py` calls `Path.is_relative_to`, added in Python 3.9. It has no
  third-party imports.
- `git`, `make`, and `gcc` to build dbgen.

### Generate TPC-H inputs

`dbgen` comes from tpch-kit (`https://github.com/gregrahn/tpch-kit.git` per its README). Build it
on Linux:

```bash
cd tpch-kit/dbgen
make MACHINE=LINUX DATABASE=POSTGRESQL
```

`DSS_CONFIG` names the directory that holds `dists.dss`, and `DSS_PATH` the output directory.
Run from inside a fresh `$DBGEN_OUT` with room for the SF 10 files:

```bash
export DSS_CONFIG=/path/to/tpch-kit/dbgen
cd "$DBGEN_OUT"
DSS_PATH=. "$DSS_CONFIG/dbgen" -s 10 -T c -f   # customer.tbl
DSS_PATH=. "$DSS_CONFIG/dbgen" -s 10 -T o -f   # orders.tbl and lineitem.tbl
DSS_PATH=. "$DSS_CONFIG/dbgen" -s 10 -U 3      # orders.tbl.u1..u3, lineitem.tbl.u1..u3, delete.1..delete.3
```

These were run on macOS at `-s 0.01`, which produced exactly the files named above.

- `-s 10` must be on every call: a bare `-U 3` uses scale factor 1 (the default in `driver.c`).
- Do not pass `-f` with `-U`: dbgen then ignores `-U` and regenerates the base tables.
- Keep `DSS_PATH` short. Update file paths are formatted into a 128-byte buffer (`upath` in
  `print.c`), and a long `DSS_PATH` crashed dbgen on macOS. Hence `DSS_PATH=.` from inside the
  output directory.
- Each update set has `0.1%` of the base Orders by the dbgen formula: 15,000 at SF 10 (unchecked).
  At SF 1 a run gave 1,500 Orders and 5,822 Lines; at SF 0.01 it gave 10 Orders per set.
- Base sizes are dbgen's scale-factor figures (about 1.5 million Customers, 15 million Orders, 60
  million Lines) and are unchecked here. `prepare-manifest.json` records the real counts.

### Prepare the workloads

The campaign needs one `refreshes.json` per workload (clustered and scattered; format in "Measure R5"),
next to the refresh files it names. Both workloads share the base files in `$DBGEN_OUT`. The helper is `prepare_inputs.py` in
`q3-ivm-baseline`:

```bash
python3 prepare_inputs.py --base "$DBGEN_OUT" --updates "$DBGEN_OUT" --out "$R5_INPUTS" --seed 1 --pairs 3
```

It writes, for each of `clustered/` and `scattered/`, the files `rf1-orders-K.tbl`,
`rf1-lines-K.tbl`, and `rf2-delete-K.tbl` for each pair K, and a `refreshes.json` ordered RF1 1,
RF2 1, RF1 2, and so on. `$R5_INPUTS/prepare-manifest.json` records the seed, the base and per-pair
counts, each pair's Customer range, and the SHA-256 of every input and output. The script refuses
an `--out` that already holds either workload. The selection rules are in its module docstring and
summarized in [measurement.md](measurement.md#pending-linux-campaign). The whole path (dbgen,
`prepare_inputs.py`, `measure.py`, `summarize.py`) ran end to end on macOS at SF 0.01.

### cgroup v2 root

`measure.py` creates one child cgroup per run under `--cgroup-root` and moves the run into it. It
checks the following (`cgroup_available`, `configure_cgroup`, `enter_cgroup`, `verify_membership`):

- The machine runs the unified cgroup v2 hierarchy: the root is under `/sys/fs/cgroup`, has
  `cgroup.controllers` listing `memory`, and has `memory` in `cgroup.subtree_control`. The run's
  `/proc/self/cgroup` must read `0::<path>`.
- The root is writable by the user running `measure.py`, and no ancestor's `memory.max` is below
  `--total-memory-bytes`.
- Each child's `memory.max` and `memory.swap.max` (set to `0`) must be written and read back. If
  swap accounting is off, `memory.swap.max` is missing or not writable and the run fails.
- The child moves itself in by writing its PID to the child's `cgroup.procs` in a `preexec_fn`.
  The kernel requires the writer to have write access to the `cgroup.procs` of the common ancestor
  of the source and the destination cgroup. So the process that runs `measure.py` should sit in a
  cgroup that shares a user-writable ancestor with every child. The recipe below puts it in a leaf
  next to the run cgroups, because the kernel's no-internal-processes rule forbids a process in
  the root once `memory` is enabled for its children (kernel rule, unverified here).

A recipe that satisfies these (unverified on any machine; adapt as needed, and see the
[kernel's cgroup v2 documentation](https://docs.kernel.org/admin-guide/cgroup-v2.html)):

```bash
sudo mkdir -p /sys/fs/cgroup/r5/driver
echo +memory | sudo tee /sys/fs/cgroup/cgroup.subtree_control
echo +memory | sudo tee /sys/fs/cgroup/r5/cgroup.subtree_control
sudo chown -R "$USER" /sys/fs/cgroup/r5
echo $$ | sudo tee /sys/fs/cgroup/r5/driver/cgroup.procs   # this shell now lives in r5/driver
```

Then pass `--cgroup-root /sys/fs/cgroup/r5`. The first move needs `sudo` because the source and
destination share only the real root. Later moves, from `r5/driver` to `r5/r5-<id>-run-<n>-<mode>`,
share `r5`, which the user owns. `r5` itself holds no process, as the "no internal processes" rule
requires. An alternative is `systemd-run` with `-p Delegate=yes`; it is untested here.

Also:

- `memory.peak` needs kernel 5.19 or later (per the kernel documentation, unverified here). On an
  older kernel `cgroup_evidence` records it as `null`, and the run still completes.
- The runner never removes the child cgroups. After the runs have exited, remove them by hand:
  `find /sys/fs/cgroup/r5 -mindepth 1 -maxdepth 1 -name 'r5-*' -type d -exec rmdir {} +`.

### Storage path and disk budget

- Put `--storage-path` on a local ext4 or xfs filesystem, not tmpfs or an overlay filesystem. Storage
  files open with `O_DIRECT`, and `FBuf::ALIGNMENT` is 512 bytes (see
  [measurement.md](measurement.md#pending-linux-campaign)).
- With `--cgroup-root`, every run copies all of its inputs into `<run dir>/input` (`private_inputs`
  in `measure.py`: the three base files and every refresh file) and hashes the copy. The copies are
  never deleted. Per-run input size is the size of the SF 10 `.tbl` files plus the refresh files:
  measure it with `du -shc "$DBGEN_OUT"/*.tbl "$DBGEN_OUT"/*.u* "$DBGEN_OUT"/delete.*` and
  adjust for the refresh files that `prepare_inputs.py` actually writes. Budget that figure times the run count, plus each
  run's maintained storage (`<run dir>/storage`). One campaign of `--runs N` makes `2N` runs
  (native and reconstructed), so one pilot command (`--runs 1`) makes two, and the four pilots make eight.
- `measure.py` also hashes the inputs once for the manifest, and builds both release executables
  under `q3-ivm-baseline/target/`; keep room for the build.

### Pilot command

Run once per workload, from `sql-to-dbsp-compiler/experiments/q3-ivm-baseline`, with the
compiler built and `JAVA_HOME` set, from a shell inside the cgroup tree above. Use fresh
`--report` and `--storage-path` values each time.

```bash
python3 measure.py \
  --customer "$DBGEN_OUT/customer.tbl" \
  --orders "$DBGEN_OUT/orders.tbl" \
  --lineitem "$DBGEN_OUT/lineitem.tbl" \
  --refresh-list "$R5_INPUTS/clustered/refreshes.json" \
  --report "$R5_OUT/pilot-clustered-report.json" \
  --storage-path "$R5_OUT/pilot-clustered-runs" \
  --cgroup-root /sys/fs/cgroup/r5 \
  --total-memory-bytes 1073741824 \
  --cache-bytes 134217728 \
  --memory-target-bytes 536870912 \
  --runs 1 \
  --batch 10000 \
  --no-verify \
  --setup-full-compaction \
  --front-code-byte-keys \
  --elide-value-headers
```

This is arm A. Run arm B with the same command minus `--front-code-byte-keys`, and give each run its own `--report` and `--storage-path` (see "Campaign and front-coding A/B" for naming). Run the four pilots in the order clustered A, clustered B, scattered A, scattered B. Why each flag:

- `--cgroup-root`, `--total-memory-bytes`: enforce a 1 GiB `memory.max` with no swap, per run.
- `--cache-bytes 134217728`: the 128 MiB worker cache (a whole number of MiB).
- `--memory-target-bytes 536870912`: the 512 MiB runtime memory-pressure target.
- `--runs 1`: one repetition per mode for the pilot. The campaign uses `--runs 3`.
- `--batch 10000`: the loading target of 10,000 rows per transaction.
- `--no-verify`: required for qualification (`qualification` rejects a verified run).
- `--setup-full-compaction`: both modes start the refreshes from one run per Spine.
- `--front-code-byte-keys`, `--elide-value-headers`: the current disk format of the merged index.
  Native runs write the same files with or without them.

**Pilot pass criterion.** For each of the four pilots, and for both runs in its report (`runs[].qualification`), `status` must be
`qualifying`. `qualification` in `measure.py` returns that only when the run was on Linux with a
limit, the cgroup proof matched (`cgroup.enforced`), verification was off, and
`load.observed_storage_bytes` exceeded the limit. Also check that the top-level `status` is
`complete` and `errors` is empty. A qualifying pilot shows only that the loaded state exceeds the
limit. It does not show memory pressure.

### Campaign and front-coding A/B

Run the campaign only after all four pilots pass and the pressure instrumentation exists
([measurement.md](measurement.md#pending-linux-campaign)). It has two arms, each the pilot
command with `--runs 3` and fresh `--report` and `--storage-path` values:

- **Arm A:** the pilot flags as above, with `--front-code-byte-keys`.
- **Arm B:** the same command without `--front-code-byte-keys`. Keep every other flag.

Run them in the order clustered A, clustered B, scattered A, scattered B. Give each run its own
directory named by arm and workload, with `--report "$R5_OUT/campaign-A-clustered/manifest.json"`
and `--storage-path "$R5_OUT/campaign-A-clustered/runs"`, and so on. Check
each manifest's `configuration.front_code_byte_keys` against its arm, and check that the input
hashes match across all four reports. Native storage must be equal across arms. Then summarize the
four manifests together:

```bash
python3 summarize.py "$R5_OUT"/campaign-*/manifest.json --out "$R5_OUT/summary"
```

`summarize.py` labels each manifest by its parent directory, which is why each needs its own. The
two arms double both stages: the pilot is 2 arms × 2 workloads × 1 repetition × 2 modes = 8
runs, and the campaign 2 × 2 × 3 × 2 = 24. Each run keeps its own input copy, so budget the disk
for all 32, or for 24 if the pilot's run directories are moved off the disk before the campaign.

### Known limits

- Without `--cgroup-root`, a Linux run is `nonqualifying` (reason "no cgroup v2 total memory
  limit"), yet the campaign still reports `status: complete`. Read each run's `qualification`.
- `memory.peak`, `memory.events`, and `io.stat` are read once, after the run exits. They cover the
  whole run, including the input-copy phase, loading, and setup compaction, not only maintenance.
  The driver's `/proc/self/io` counters are per interval, but they are process-wide.
- Pressure sampling and the before and after maintenance snapshots are not implemented
  ([measurement.md](measurement.md#pending-pressure-evidence)).
- Until those exist, make none of these claims: runtime memory pressure, reclaim or refault
  behavior, beyond-memory behavior, equal-memory performance, a disk saving at SF 10, or a
  maintenance-only I/O figure. No SF 10 run has been measured.

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
