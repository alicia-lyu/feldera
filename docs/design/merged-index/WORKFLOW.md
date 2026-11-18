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
`q3-base-circuit/src/{lib,stubs}.rs` from `base.sql`, and `q3-view-circuit/src/{lib,stubs}.rs` and
`plan.json` from `q3.sql` (Catalog mode, default optimization level 2). The two circuits are the
base tables and the view; `query.sql` stays the logical definition. The driver builds both in one
DBSP runtime, with one storage directory and one buffer cache (`--cache-bytes`). Run it before
building or testing a fresh checkout. It generates only the native pair: the reconstructed build's
view circuit, `q3-trie-view-circuit/`, is hand-written and tracked.

`generate.sh` stops with a message if `src/lib.rs` or `src/stubs.rs` exists in the experiment.
Those are stale files from before the base/view split. They are untracked and no longer
git-ignored, and Cargo would take `src/lib.rs` for a library target, so `verify.sh` would fail in
`native-format` with "failed to resolve mod `udf`". Move them to `TRASH/` and rerun.

```bash
# Generate the native pair (base and view circuits); takes no option
./generate.sh

# Native build (the default)
cargo +1.96.1 test --offline --locked

# Reconstructed build: the view circuit is hand-written (q3-trie-view-circuit), so only the Cargo feature is needed
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
cargo +1.96.1 test --offline -p dbsp --lib run_reads
cargo +1.96.1 test --offline -p dbsp --lib merged_index::run_merge
cargo +1.96.1 test --offline -p dbsp --lib merged_index::session
cargo +1.96.1 test --offline -p dbsp --lib merged_index::reader
cargo +1.96.1 test --offline -p dbsp --lib read_owner_cursor
```

The `run_reads` filter covers the run counters (`RunReads`). The baseline of the four readers on the worked transaction, `todays_readers_run_reads_on_the_worked_transaction`, was retired in step 4. The `run_merge` and `session` filters cover the merging cursor and the scan session, and the `reader` filter the readers, their capture of `Dlt`, and the routing (roadmap step 5). The `read_owner_cursor` filter covers the adapter that charges a join's partner cursor to the `requested_slice` read owner. The `supplied_snapshot_join` filter also covers a delta that a source emits at the flush. The session's counters on the COL data are `session_run_reads_on_the_worked_transaction` and `the_passes_pass_untouched_groups_and_rejected_subtrees_in_different_ways` (filter `session_run_reads` and `the_passes`) in the COL suite. The same rows under one memory-pressure mechanism for both sides are the harness's `native_worked_transaction_tests` (`worked_transaction_default_arm` and `worked_transaction_pressured_arm`, with the native build, which is what the default features give) and `session_worked_transaction_tests` (`session_worked_transaction_default_arm` and `session_worked_transaction_pressured_arm`, with `--features merged-index`). Run each with `cargo +1.96.1 test --offline --locked worked_transaction` from `q3-ivm-baseline`, adding `-- --nocapture` to print each run's pressure level, file counts, layout, and reads. The pressure is sampled, so repeat the run when changing them.

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
It starts by generating the native pair. Logs and transcripts go to a fresh
`target/r4-verification/` directory; `CARGO_TARGET_DIR` can point at an existing build cache.
What the tests cover is in [reconstruction.md](reconstruction.md).

`verify.sh` exits nonzero on the first failing stage and names its log in
`target/r4-verification/<run>/`. Wrapping it as `./verify.sh > log; echo $?` shows the real exit
code; a background task's own exit status may report only that of the wrapper. Code under
`#[cfg(target_os = "linux")]` is first compiled, linted, and tested on Linux, so a gate that passes
on macOS can fail there. On the first Linux host, two such failures were fixed (2026-11-17):

- Strict Clippy rejected `evict_inputs(&[path.clone()])` in a Linux-only test of
  `src/window_memory.rs` (`cloned_ref_to_slice_refs`).
- `native_spine_compacts_full_col_runs_to_file_and_preserves_snapshots` in the COL suite timed out:
  "no file batch after 30s". The background merger was running, but the suite's unoptimized build
  on a 2.2 GHz Xeon took 198 s for the whole test (24 s in release). `MERGE_DEADLINE` in
  `merged-index-col/src/storage_tests.rs` is now 300 s. A timeout there means a slow debug build,
  not a stuck merger, if `merger-0` shows state `R` in `/proc/<pid>/task/*/stat`.

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
(`verified=false`). It also keeps the partner-read contract check off, which the driver enables only
when verifying (`enable_contract_check` in `src/main.rs`). `--dump-runtime-circuit DIR` writes each run's per-worker runtime graphs
after its final refresh, outside the phase timers; the directory must not already hold a run.

The driver also runs as two programs (measurement version 13), which `measure.py --build-images`
and `--images` start:

- `--write-image DIR` loads the base files and writes `DIR/storage` (one DBSP checkpoint of both
  circuits, and in reconstructed mode the merged index and its orphan lines) and `DIR/image.json`,
  then stops. It takes `--runs 1` and no refresh, report, or `--storage-path`.
- `--from-image DIR` reopens the image in `DIR` (a copy: its `storage/` changes as the refreshes
  run) and runs only the refreshes, with `--no-verify` and `--runs 1` and no base files. It
  refuses an image written with other storage settings. `--warm-up LIST` applies a refresh list,
  untimed, before the window.

Loading takes complete Order and Lineitem groups in ascending Order key and reports any other
order as unsupported. The CLI accepts RF1 insertion and RF2 deletion under referential integrity
only. Reconstructed mode itself accepts general deltas of the base tables
([decisions.md](decisions.md#2026-11-10-general-deltas-with-an-orphan-side-store)); only the tests
exercise them.

## Measure R5

A macOS smoke run with the current configuration. `$SMOKE_INPUTS` holds `customer.tbl`,
`orders.tbl`, `lineitem.tbl`, and `refreshes.json` of one small workload. Do not use the earlier
smoke inputs in `target/r5-smoke-inputs/`: their RF2 deletes the Orders that their RF1 inserted,
which violates the refresh selection (RF2 deletes the oldest base Orders, chosen independently
of RF1). Prepare smoke inputs from dbgen output at `-s 0.01` with
`prepare_inputs.py` (see "Prepare the workloads").
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
  --front-code-byte-keys \
  --elide-value-headers
```

`--storage-path` is the campaign root. Each run gets a new `run-<repetition>-<mode>` directory
under it, holding `storage/`, `driver.json`, and the logs. The Linux campaign adds `--cgroup-root`
and `--total-memory-bytes`; see the [Linux handoff](#linux-handoff).

The runner always starts the driver with `--r5`. That flag (`Config::from_args` in `src/main.rs`)
selects the measured configuration. Native joins probe their traces through run cursors, with
DBSP's asynchronous join fetch off (`native_join_fetch: false` in the report; it was on until
2026-11-04). It is accepted only with one worker, one run per process
(`--runs 1`), `--storage-path`, `--cache-bytes`, `--memory-target-bytes`, and `--report`;
otherwise the driver exits with an error. Running the driver by hand with `--r5` therefore needs
all of those. `--cache-bytes` must be a whole number of MiB.

`python3 measure.py --help` lists the remaining options (`--compaction-timeout-secs`, `--batch`,
`--runs`, `--storage-path`, `--diagnostics`, `--setup-full-compaction`, `--front-code-byte-keys`,
`--elide-value-headers`, `--session-pass`, `--cgroup-root`, `--total-memory-bytes`,
`--device-label`). `--session-pass seek|step`
(default `seek`, also a driver flag) selects how reconstructed mode's scan session passes the
untouched groups; native ignores it and reports `configuration.session_pass` as `null`.

`--setup-full-compaction` (also a driver flag) changes only the setup. After loading, it requests
full compaction for the circuit and the merged index and waits for compaction to finish, then
publishes and starts the measured refreshes. Nothing is compacted on request after any refresh.
The report's `load.setup_full_compaction` holds the setup's cost. It is off by default, since no
compaction before the refreshes is the more realistic workload (decision of 2026-11-09). Add it
to isolate the fragmentation effect: both modes then start from one run per Spine.

`--front-code-byte-keys` (also a driver flag) sets DBSP's `front_code_byte_keys` dev tweak. Every
storage file writer then prefix-truncates data blocks whose keys are raw byte strings with no auxiliary
data, which in this experiment is only the merged index's key level; native runs write the same
files as without it. The report's `front_coding` holds the writer's counters: blocks written
prefix-truncated, `layout_fallbacks` (eligible blocks the encoder did not understand, which should be 0),
and `size_fallbacks` (blocks that prefix truncation would not have shrunk by a 512-byte sector).

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

The scans run outside the timed window. The runner rejects any driver report that fails
`report_problems` in `measure.py`:

- per-owner writes sum to the totals;
- `delta_ms` is present, finite, and non-negative, and `ivm_ms >= delta_ms`;
- `view_run_reads` of `requested_slice` and `partner_trace` follow the mode: a native run has no
  `requested_slice` reads and some `partner_trace` reads, a reconstructed run the reverse, and
  nothing is read under `merged_index` on the view worker;
- `driver_run_reads` is well formed (`null` in native mode), and no writes are charged to
  `requested_slice`;
- the scan session's counts, `index_upkeep_run_reads`, and `profiled_cpu_ms` are well formed and
  their sequence totals are the sums of the refreshes;
- `configuration.session_pass` is the pass that the runner asked for;
- `front_coding.layout_fallbacks` is 0 when prefix truncation is on, and
  `value_header_elision.layout_fallbacks` is 0 when elision is on (decision of 2026-11-16);
- the report is measurement version 12, and `block_expansions` are well formed, sum over the
  refreshes to the window's, and are 0 for an encoding that is off.

The refresh list is a JSON array in execution order. Each item is
`{"rf1_orders": "orders.tbl.u1", "rf1_lines": "lineitem.tbl.u1"}` or `{"rf2": "delete.1"}`;
relative paths resolve against the list file. `--cache-bytes` must be a whole number of MiB.

The runner builds and preserves separate native and reconstructed release executables,
regenerates the native circuits at the end, and starts a fresh process and storage directory for each mode and repetition
(three by default). Definitions of the reported intervals, the compaction policy, and the pending
Linux qualification are in [measurement.md](measurement.md).

## Linux handoff

This section is for a fresh agent on a fresh Linux machine that will run the R5 pilot. It is the
only place for these commands; [measurement.md](measurement.md) defines what is measured and why.
Anything marked "unverified" could not be checked from the repository.

### Machine and OS

Every figure here that the repository cannot check is unverified unless marked "verified on the
first Linux host". That host (2026-11-17) was an Emulab/CloudLab-style bare-metal node: Ubuntu
24.04.5 with the GA kernel 6.8.0, two Intel Xeon Silver 4114 (40 logical cores), 187 GiB RAM, an
Intel SSDSC2BB48 SSD (480 GB, 512-byte logical sectors), and a Seagate ST1200MM0088 hard disk (1.2
TB, 512-byte logical sectors).

- **Architecture:** x86_64. The dbgen `MACHINE=LINUX` build and the JDK path example below assume
  it; aarch64 is untested.
- **OS:** an Ubuntu LTS whose kernel is 6.12 or later (`uname -r`), for example Ubuntu 24.04 with
  its hardware-enablement kernel, or a later LTS. Kernel 5.19 is the floor for `memory.peak`
  itself, and 6.12 for resetting it by writing to it (unverified). The 24.04 GA kernel 6.8 also
  works (verified on the first Linux host). There `memory.peak` exists but is read-only (mode
  0444, so even its owner gets "Permission denied" on open for writing), so the window peak falls
  back as the pre-flight table says. Use bare metal or a full virtual machine with root, not a
  container (unverified).
- **CPU:** at least 8 cores, for the worker, the index thread, background compaction, and the build
  (unverified). More cores are fine: they speed the build and do not change the runs, which use one
  DBSP worker. Record the CPU model, and keep frequency scaling fixed if possible. With the
  `intel_pstate` driver (check `cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_driver`), set
  the governor with `echo performance | sudo tee
  /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor` and turn turbo off with `echo 1 | sudo
  tee /sys/devices/system/cpu/intel_pstate/no_turbo`, after the build. Neither survives a reboot.
  Testbed images often default to `powersave` (verified on the first Linux host).
- **RAM:** at least 32 GiB, for the Rust release builds (unverified). Runs are capped at 1 GiB by
  the cgroup, so host RAM does not change them.
- **Disks:** one SSD and, for the probe, one hard disk. Both need 512-byte logical sectors
  (`O_DIRECT`, `FBuf::ALIGNMENT`; a 4Kn drive may fail) and ext4 or xfs. The SSD holds the inputs,
  the build, and the campaign's storage. The hard disk holds only the probe's 16 runs
  ("Hard-disk probe" below). `measure.py` records the device of `--storage-path` in the manifest
  (`storage_device`: `path`, `device`, `filesystem`, `logical_sector_bytes`, `rotational`, `model`,
  `label`). Every field is `null` on macOS or when it cannot be resolved; a device-mapper device is
  reported unresolved, not guessed. `--device-label ssd|hdd` names the device, and the runner fails
  if the label contradicts the device's `rotational` flag. Both arrived with measurement version 12.
  List the disks with `lsblk -d -o NAME,ROTA,MODEL,SIZE,LOG-SEC,PHY-SEC` (`ROTA` 0 is the SSD).
  The root filesystem of a testbed image may be ext3 and small (64 GB on the first Linux host),
  with the rest of the SSD unpartitioned and the hard disk blank. See "Prepare the disks" below.

Disk budget per device, with the run counts of "Storage path and disk budget": the SSD takes the
inputs once (about 10 GB at SF 10, unchecked), the four images (see "Build the images"), one run's
copy at a time, the loads' peak while the images are built (all four at once; a native SF 10 load
with 10,000-row transactions peaked at about 41 GB), and the build.

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
| `clear_refs` resets the peak (Linux 4.0 or later) | `grep VmHWM /proc/self/status; echo 5 > /proc/self/clear_refs; grep VmHWM /proc/self/status` | `sequence.window_peak_rss_bytes` is `null` with `window_peak_rss_error` set. Report the kernel. (Worked on 6.8, verified on the first Linux host. The second `VmHWM` is the current RSS, so on an idle shell it barely drops.) |
| `memory.peak` resets by writing to it (kernel 6.12 or later; unverified) | In the cgroup: `exec 3<>/sys/fs/cgroup/r5/driver/memory.peak; echo reset >&3; cat /proc/self/fd/3` | `peak.window_peak_supported` is false and the report falls back to the sampled `memory.current` maximum. Not a failure. On 6.8 the `exec` fails with "Permission denied", because the file is read-only (verified on the first Linux host). |
| Disk room | `df -h <storage dir>` against the budget below | Free space or lower `--runs`. |

### State a clone needs

- Branch `docs/merged-index-tidy-up`, or any commit that contains this section. A plain `git
  clone` checks out `main`, which lacks most of this work (it was 114 commits behind on
  2026-11-17). Run `git checkout docs/merged-index-tidy-up` right after cloning. Before the Linux
  clone, check that `origin` has the commit: `git rev-list --count
  origin/docs/merged-index-tidy-up..HEAD` must print 0. Otherwise the user must push the verified
  commit, or supply it (for example as a `git bundle`).
- `q3-ivm-baseline/q3-base-circuit/src/{lib,stubs}.rs`, `q3-view-circuit/src/{lib,stubs}.rs`, and
  `plan.json` are generated and git-ignored. `measure.py` runs `generate.sh` itself, which
  generates only the native pair. `q3-trie-view-circuit/` is hand-written and tracked.
- Raw artifacts (reports, run directories) are git-ignored and local to the machine.

### Toolchain

On Ubuntu 24.04 the system packages are (verified on the first Linux host, which had only Python,
gcc, make, and git):

```bash
sudo apt-get update
sudo apt-get install -y openjdk-21-jdk-headless maven graphviz build-essential pkg-config \
  libssl-dev cmake clang
curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none --profile minimal
. "$HOME/.cargo/env"
rustup toolchain install 1.96.1 --profile minimal -c rustfmt -c clippy
```

- Rust: install rustup, then the 1.96.1 toolchain with rustfmt and clippy as above. Write each
  component as its own `-c` (or `--component rustfmt,clippy`). rustup 1.29 rejects
  `--component rustfmt clippy`, printing only "For more information, try '--help'" (verified on the
  first Linux host). Every cargo command passes `+1.96.1`. Export `RUSTUP_TOOLCHAIN=1.96.1` too,
  because the compiler's tests and some scripts call plain `cargo`.
- Java: JDK 21 (19 to 21 work, per the repository CLAUDE.md) and Maven. Set
  `JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64` (the Ubuntu path, verified on the first Linux
  host) and put its `bin` first on `PATH`.
- SQL compiler: from `sql-to-dbsp-compiler`, run `./build.sh`. It needs network access, because
  it fetches or builds Calcite (`calcite-build`, pinned by `calcite_version.env`) and Maven
  dependencies, and then runs `mvn package -DskipTests`. It uses GNU `sed -i`, which Linux has.
  It took about two minutes on the first Linux host and ends with "Parser generated successfully";
  check that `SQL-compiler/target/sql2dbsp-jar-with-dependencies.jar` exists.
- Fetch dependencies online once, **after** the compiler is built. The runner builds with
  `--offline --locked`, which fails without a populated registry cache. `q3-ivm-baseline` and
  `merged-index-col` are separate workspaces with their own `Cargo.lock`, so fetch in each
  directory you will build in. Fetch at the repository root too if you will run the DBSP tests. In
  `q3-ivm-baseline`, run `./generate.sh` first. Its workspace members `q3-base-circuit` and
  `q3-view-circuit` have no `src/lib.rs` until it does, and on a fresh clone `cargo fetch` fails with
  "no targets specified in the manifest" (verified on the first Linux host).

  ```bash
  # From sql-to-dbsp-compiler/experiments/q3-ivm-baseline (compiler built)
  ./generate.sh
  cargo +1.96.1 fetch --locked
  # From sql-to-dbsp-compiler/experiments/merged-index-col, and from the repository root
  cargo +1.96.1 fetch --locked
  ```

A shell setup file saves retyping in every new shell (agent harnesses start a fresh shell for each
command):

```bash
cat > ~/r5-env.sh <<'EOF'
. "$HOME/.cargo/env"
export JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64
export PATH="$JAVA_HOME/bin:$PATH"
export RUSTUP_TOOLCHAIN=1.96.1
export DBGEN_OUT=/mnt/ssd/dbgen-sf10 R5_INPUTS=/mnt/ssd/r5-inputs R5_OUT=/mnt/ssd/r5-out
export HDD_MOUNT=/mnt/hdd
EOF
```
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
DSS_PATH=. "$DSS_CONFIG/dbgen" -s 10 -U 4      # orders.tbl.u1..u4, lineitem.tbl.u1..u4, delete.1..delete.4
```

Set 4 is the optional warm-up pair (`prepare_inputs.py --warm-up`); sets 1 to 3 are the measured
pairs, the same files that `-U 3` writes.

These were run on macOS at `-s 0.01`, which produced exactly the files named above.

- `-s 10` must be on every call: a bare `-U 3` uses scale factor 1 (the default in `driver.c`).
- Do not pass `-f` with `-U`: dbgen then ignores `-U` and regenerates the base tables.
- Keep `DSS_PATH` short. Update file paths are formatted into a 128-byte buffer (`upath` in
  `print.c`), and a long `DSS_PATH` crashed dbgen on macOS. Hence `DSS_PATH=.` from inside the
  output directory.
- Each update set has `0.1%` of the base Orders by the dbgen formula: 15,000 Orders and 15,000
  deletes per set at SF 10, with 60,092, 59,617, and 60,274 Lines in sets 1 to 3. At SF 1 a run gave
  1,500 Orders and 5,822 Lines; at SF 0.01 it gave 10 Orders per set.
- Base sizes at SF 10: 1,500,000 Customers, 15,000,000 Orders, and 59,986,052 Lines, about 9.7 GB in
  all (7.7 GB of it `lineitem.tbl`). The three calls took about ten minutes, single-threaded
  (verified on the first Linux host with tpch-kit commit `852ad0a`). `prepare-manifest.json`
  records the counts.

### Prepare the workloads

The campaign needs one `refreshes.json` per workload (clustered and scattered; format in "Measure R5"),
next to the refresh files it names. Both workloads share the base files in `$DBGEN_OUT`. The helper is `prepare_inputs.py` in
`q3-ivm-baseline`:

```bash
python3 prepare_inputs.py --base "$DBGEN_OUT" --updates "$DBGEN_OUT" --out "$R5_INPUTS" --seed 1 --pairs 3 --warm-up
```

`--warm-up` also writes `<workload>/warm-up/` (pair 4, from update set 4, with its own
`refreshes.json`) for the refresh program's optional warm-up. The measured pairs are the same with
or without it.

It writes, for each of `clustered/` and `scattered/`, the files `rf1-orders-K.tbl`,
`rf1-lines-K.tbl`, and `rf2-delete-K.tbl` for each pair K, and a `refreshes.json` ordered RF1 1,
RF2 1, RF1 2, and so on. `$R5_INPUTS/prepare-manifest.json` records the seed, the base and per-pair
counts, each pair's distinct Customers and the Order counts of its top 1, 10, and 100 Customers, and the SHA-256 of every input and output. The script refuses
an `--out` that already holds either workload. Clustered RF1 draws Customers from a Zipf distribution (`--zipf-exponent`, default 1.0) independent of the RF2 sets. The selection rules are in its module docstring and
summarized in [measurement.md](measurement.md#pending-linux-campaign). The whole path (dbgen,
`prepare_inputs.py`, `measure.py`, `summarize.py`) ran end to end on macOS at SF 0.01. At SF 10 the
command above took about 100 seconds and wrote 15,000 RF1 Orders and 15,000 RF2 deletes per pair in
both workloads (verified on the first Linux host).

### Build the images

Loading is not measured (user decision of 2026-11-17), so it runs once, before the pilots, with no
cgroup limit and no DBSP memory target. The first Linux pilot loaded SF 10 under the 1 GiB limit for
96 minutes of direct reclaim, and its background merges were still running when the 300 s
compaction timeout expired, so no refresh ran. Now every measured run starts a new process from its
own copy of a loaded image.

```bash
# From sql-to-dbsp-compiler/experiments/q3-ivm-baseline; a new directory on the SSD
python3 measure.py --build-images "$R5_OUT/images" \
  --customer "$DBGEN_OUT/customer.tbl" \
  --orders "$DBGEN_OUT/orders.tbl" \
  --lineitem "$DBGEN_OUT/lineitem.tbl" \
  --elide-value-headers \
  --device-label ssd
```

It builds both release executables, then runs the load program (the driver's `--write-image`)
four times at once, writing `native-A`, `native-B`, `merged-index-A`, and `merged-index-B`. A is with
`--front-code-byte-keys`, B without; the refresh program reopens only an image with its own storage
settings (`ImageSettings` in `src/main.rs`). Every load finishes before a failure is reported, and
each image that loaded is kept.

**Load settings.** Loading has its own defaults in `measure.py` (`LOAD_CACHE_BYTES`, `LOAD_BATCH`,
`LOAD_COMPACTION_TIMEOUT_SECS`; decision of 2026-11-17): a 16 GiB buffer cache per load,
1,000,000-row load transactions, and a 3600 s wait for the background merges. Flags override
them, but do not pass the campaign's `--cache-bytes 134217728` or `--batch 10000` here. None of
them reaches a refresh run, which uses the campaign's flags, and neither the cache nor the batch is
part of `ImageSettings`. All four images must use the same batch, though: no full compaction runs
after loading (decision of 2026-11-09), so the batch shapes each image's runs.

DBSP has two memory controls, and loading needs the second one large:

- The memory target (`--memory-target-bytes`, DBSP's RSS pressure) decides when in-memory batches
  spill to files. Loading sets none.
- The buffer cache (`--cache-bytes`) holds storage-file blocks, written and read. Storage files are
  opened with `O_DIRECT` (`StorageCacheConfig::FelderaCache` in `circuit_config`), which bypasses
  the page cache, so a block that is not in this cache is read from the device. Files exist even
  with no memory pressure: a merge output of `min_storage_bytes` (10 MiB) or more is written to a
  file.

The view phase of every reconstructed load transaction (`Q3State::collect`) reads each touched
customer's whole group in the merged index, which clusters by customer, while the loader reads
orders in `orderkey` order: each transaction touches customers spread over the whole index. On the
first Linux host, with the campaign's 128 MiB cache and 10,000-row transactions, a reconstructed
SF 10 load read 0.35% of `lineitem.tbl` per minute, a profile put about 68% of its main thread in
`Q3State::collect`, and its time went to block reads, checksums, and block expansion. With the load
defaults it read about 2% per minute and nothing from the device in its first 15 minutes
(`read_bytes` in `/proc/<pid>/io`). Each load then holds up to its cache plus a few GiB of working
memory, about 80 GiB for the four on a 187 GiB host; on a smaller host lower `--cache-bytes`, or
watch `read_bytes` to see whether the working set still fits.

`images.json` records the settings used, the revision, the input hashes, the executables' SHA-256,
and each image's size and load time. Each image is `storage/` (one DBSP checkpoint of both
circuits, and in reconstructed mode the merged index and its orphan lines) and `image.json`, the
settings, the checkpoint, and the load record. The checkpoint writes every in-memory batch to a
file (`Spine::save`), so an image holds file runs only. A campaign reopens an image only with the
executable that wrote it, byte for byte (`image_executable_problems`): it stops before any run if
the executables it builds differ. A commit that leaves the driver's sources alone (docs,
`measure.py`) rebuilds the same executables and keeps the images usable; a change to the driver,
its circuits, or DBSP needs new images.

Pre-build the release executables with `cargo +1.96.1 build --release --offline --locked
-j"$(nproc)"`, with and without `--features merged-index`: `measure.py` builds with `-j2`, and on
the first Linux host that took 56 minutes from a cold target. Already-built executables make its
build a no-op.

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

A recipe that satisfies these (verified on the first Linux host, Ubuntu 24.04, kernel 6.8; see the
[kernel's cgroup v2 documentation](https://docs.kernel.org/admin-guide/cgroup-v2.html)):

```bash
sudo mkdir -p /sys/fs/cgroup/r5/driver
echo +memory | sudo tee /sys/fs/cgroup/cgroup.subtree_control   # harmless if already enabled
echo +memory | sudo tee /sys/fs/cgroup/r5/cgroup.subtree_control
sudo chown -R "$USER" /sys/fs/cgroup/r5
echo $$ | sudo tee /sys/fs/cgroup/r5/driver/cgroup.procs   # this shell now lives in r5/driver
```

Then pass `--cgroup-root /sys/fs/cgroup/r5`. The first move needs `sudo` because the source and
destination share only the real root. Later moves, from `r5/driver` to `r5/r5-<id>-run-<n>-<mode>`,
share `r5`, which the user owns. `r5` itself holds no process, as the "no internal processes" rule
requires. An alternative is `systemd-run` with `-p Delegate=yes`; it is untested here. On Ubuntu
24.04 the root's `cgroup.subtree_control` already lists `memory`, and systemd leaves a top-level
`r5` alone. The tree does not survive a reboot, so rerun the recipe after one.

**The move applies to one shell only.** `echo $$` moves the shell that runs it, and only its
descendants inherit the cgroup. An interactive user can run the recipe once and then type the
pilot commands in that shell. An agent's tool usually starts a fresh shell for each command, so it
must move the shell and start `measure.py` in **one** command:

```bash
bash -c 'echo $$ | sudo tee /sys/fs/cgroup/r5/driver/cgroup.procs >/dev/null &&
  grep -qx "0::/r5/driver" /proc/self/cgroup && exec python3 measure.py ...'
```

If `measure.py` instead starts in the login session's cgroup (`0::/user.slice/...`), the move of
each run into `r5/r5-*` needs write access to the root's `cgroup.procs`, and the run fails in
`enter_cgroup`.

Also:

- `memory.peak` needs kernel 5.19 or later (per the kernel documentation, unverified here). On an
  older kernel `cgroup_evidence` records it as `null`, and the run still completes.
- The runner never removes the child cgroups. After the runs have exited, remove them by hand:
  `find /sys/fs/cgroup/r5 -mindepth 1 -maxdepth 1 -name 'r5-*' -type d -exec rmdir {} +`.

### Prepare the disks

Skip this if an ext4 or xfs filesystem with enough room already sits on each device. On the first
Linux host the root (`/dev/sda3`) was 64 GB of ext3, about 375 GB of the SSD was unallocated, and
the hard disk had no partition table. The user approved partitioning both. Inspect the disks and
**get the user's approval before writing any partition table**:

```bash
lsblk -d -o NAME,ROTA,MODEL,SIZE,LOG-SEC,PHY-SEC
sudo parted -s /dev/sda unit GB print free   # look for the unallocated block
sudo parted -s /dev/sdb print                # "unrecognised disk label" means blank
```

Then, with the device names checked against that output:

```bash
sudo sgdisk -n 4:0:0 -t 4:8300 -c 4:r5-ssd /dev/sda   # new partition in the largest free block
sudo sgdisk -o -n 1:0:0 -t 1:8300 -c 1:r5-hdd /dev/sdb  # -o: new GPT, whole disk
sudo partprobe /dev/sda /dev/sdb
sudo mkfs.ext4 -q -L r5-ssd /dev/sda4
sudo mkfs.ext4 -q -L r5-hdd /dev/sdb1
sudo mkdir -p /mnt/ssd /mnt/hdd
for p in sda4:/mnt/ssd sdb1:/mnt/hdd; do
  echo "UUID=$(sudo blkid -s UUID -o value /dev/${p%%:*}) ${p#*:} ext4 defaults,noatime,nofail 0 2" |
    sudo tee -a /etc/fstab
done
sudo systemctl daemon-reload && sudo mount -a
sudo chown "$USER": /mnt/ssd /mnt/hdd
sudo blockdev --getss /dev/sda4 /dev/sdb1   # both must print 512
```

`sgdisk` and `parted` warn that "not all of the space available ... appears to be used" when the
GPT backup header leaves a few sectors unused. The warning is harmless. Put `$DBGEN_OUT`,
`$R5_INPUTS`, and `$R5_OUT` on `/mnt/ssd`. The build stays in the clone, which may stay on the
root if it has room. After `verify.sh`, the first Linux host's build used about 18 GB: 11 GB in
`q3-ivm-baseline/target`, 4.4 GB in `merged-index-col/target`, 2 GB in `~/.cargo`, and 0.6 GB for
Maven and Calcite.

### Storage path and disk budget

- Put `--storage-path` on a local ext4 or xfs filesystem, not tmpfs or an overlay filesystem. Storage
  files open with `O_DIRECT`, and `FBuf::ALIGNMENT` is 512 bytes (see
  [measurement.md](measurement.md#pending-linux-campaign)).
- The inputs are shared: every run of every campaign reads `$DBGEN_OUT` and `$R5_INPUTS` in place,
  and none copies them (decision of 2026-11-11). Budget them once: `du -shc "$DBGEN_OUT"/*.tbl
  "$R5_INPUTS"`. Do not edit, touch, or regenerate them between runs: each run checks every input's
  size and modification time against the campaign's hash record and fails if one changed.
- The four images are kept once, in `$R5_OUT/images`; `images.json` gives each one's
  `size_bytes`.
- Each run copies its image into `<run dir>/image` (`cp -a`, outside the run's cgroup) and deletes
  the copy once the run completes and its report is recorded (`storage_deleted` in the run's
  record); `--keep-run-storage` keeps it. A failed run keeps its copy for diagnosis, and the
  campaign stops. So a device needs room for the images and one run's copy at a time: the image's
  size plus what the six refreshes write (a native copy is about 20 GB at SF 10). Keeping every copy
  does not fit: one campaign of `--runs N` makes `2N` runs (native and reconstructed), so the
  eight pilots make 16 and the eight campaigns 48. In the hard-disk probe, each run copies its image
  from the SSD's `$R5_OUT/images` to its run directory on the hard disk.
- `measure.py` also hashes the inputs once for the manifest, and builds both release executables
  under `q3-ivm-baseline/target/`; keep room for the build.

### Pilot command

Run once per workload, from `sql-to-dbsp-compiler/experiments/q3-ivm-baseline`, with the
compiler built and `JAVA_HOME` set, from a shell inside the cgroup tree above. Use fresh
`--report` and `--storage-path` values each time.

```bash
python3 measure.py \
  --images "$R5_OUT/images" \
  --refresh-list "$R5_INPUTS/clustered/refreshes.json" \
  --report "$R5_OUT/pilot-A-seek-clustered/manifest.json" \
  --storage-path "$R5_OUT/pilot-A-seek-clustered/runs" \
  --cgroup-root /sys/fs/cgroup/r5 \
  --total-memory-bytes 1073741824 \
  --cache-bytes 134217728 \
  --memory-target-bytes 536870912 \
  --runs 1 \
  --no-verify \
  --front-code-byte-keys \
  --elide-value-headers \
  --session-pass seek \
  --device-label ssd
```

This is arm `A-seek` for clustered. The four arms differ in two flags ("Campaign and four arms"
below): A keeps `--front-code-byte-keys` and B drops it, and `--session-pass` is `seek` or `step`.
Give each pilot its own `--report` and `--storage-path`, named by arm and workload:
`--report "$R5_OUT/pilot-<arm>-<workload>/manifest.json"` and
`--storage-path "$R5_OUT/pilot-<arm>-<workload>/runs"`. Run the eight pilots in the order `A-seek`,
`A-step`, `B-seek`, `B-step` for clustered, then the same for scattered. Why each flag:

- `--images`: start each run from a copy of its mode's and arm's image ("Build the images"),
  in a new process that runs only the refreshes (the driver's `--from-image`). The window starts
  with the buffer cache empty; add `--warm-up "$R5_INPUTS/clustered/warm-up/refreshes.json"` (the
  workload's own warm-up pair) to run one unmeasured RF1/RF2 pair before it instead. Run the
  pilots without `--warm-up` unless the user asks for the warm arm.
- `--cgroup-root`, `--total-memory-bytes`: enforce a 1 GiB `memory.max` with no swap, per run.
- `--cache-bytes 134217728`: the 128 MiB worker cache (a whole number of MiB).
- `--memory-target-bytes 536870912`: the 512 MiB runtime memory-pressure target.
- `--runs 1`: one repetition per mode for the pilot. The campaign uses `--runs 3`.
- `--no-verify`: required for qualification (`qualification` rejects a verified run).
- `--front-code-byte-keys`, `--elide-value-headers`: the current disk format of the merged index
  (`--front-code-byte-keys` is prefix truncation). Native runs write the same files with or
  without them.
- `--session-pass`: how reconstructed mode's scan session passes the untouched groups. Native has
  no scan session; its report records `configuration.session_pass` as `null`.
- `--device-label`: names the device of `--storage-path` in the manifest and fails the run if it
  contradicts the device. Optional.

**Pilot pass criterion.** For each of the eight pilots, and for both runs in its report (`runs[].qualification`), `status` must be
`qualifying`. `qualification` in `measure.py` returns that only when the run was on Linux with a
limit, the cgroup proof matched (`cgroup.enforced`), the shared inputs were evicted before the
driver started (`cgroup.proof.inputs_evicted`, which with `--images` also covers the run's copy
of the image), verification was off, and the image's loaded size
(`driver.image.load.observed_storage_bytes`) exceeded the limit. Also check that the top-level `status` is
`complete` and `errors` is empty. A qualifying pilot shows only that the loaded state exceeds the
limit. It does not show memory pressure.

Also read the window evidence in each run's `driver` report, as evidence and not as proof of
pressure:

- `sequence.window_peak_rss_bytes` and `sequence.window_end_rss_bytes` must be numbers, and
  `sequence.window_peak_rss_error` must be `null`. Otherwise the `clear_refs` reset failed.
- `sequence.cgroup` must not be `null`. Check `peak.window_peak_supported`: if it is false, read
  `peak.window_peak_error` and use `max_sampled_memory_current_bytes` (a boundary sample, a lower
  bound on the true peak).
- `inputs_evicted.evicted` should be true, and `cgroup_memory_current_after_bytes` should be
  below `..._before_bytes` if the page cache held the inputs.
- `refresh_inputs.vm_rss_after_pre_read_bytes` minus `vm_rss_before_pre_read_bytes` is the
  pre-read's fixed overhead, equal in both modes.
- `refreshes[].memory_pressure` shows what DBSP's level was; `cgroup.delta` (`pgscan`, `pgsteal`,
  `workingset_refault_*`, `memory_events.max`) shows what the kernel did. Report the two apart.
- `measurement_version` (13) and the fallback counters:
  `front_coding.layout_fallbacks` and `value_header_elision.layout_fallbacks` must be 0; the runner
  gates on them.
- `delta_ms`, `q3_ms`, and `ivm_ms` per refresh, with `ivm_ms = delta_ms + q3_ms`.
- The run reads per owner: `dbsp_operator_*`, `partner_trace_*`, and `requested_slice_*` in the
  summary, and `driver_run_reads` and the `filtered_probes` counter in the report. (`filtered_probes`
  was `filter_rejections` before 2026-11-16, so older manifests use that name.)
- `block_expansions` per refresh and owner (`view_worker`, `driver`, `index_upkeep`): the storage-level
  rebuilds of encoded data blocks on buffer-cache misses, as `prefix_truncated_blocks`,
  `prefix_truncated_encoded_bytes`, `prefix_truncated_expanded_bytes`, `elided_blocks`,
  `elided_encoded_bytes`, and `elided_expanded_bytes`. They are not item decodes.
- `storage_device`: the device, filesystem, sector size, and `rotational` flag of `--storage-path`.

### Hard-disk probe

The user decided on 2026-11-16 that the hard disk gets only the eight pilots, as a probe of 16 runs
([decisions.md](decisions.md#2026-11-16-the-ssd-runs-the-campaign-the-hard-disk-only-a-pilot-probe)).
Run it after the SSD pilots pass. Repeat each pilot command with `--storage-path` on a directory of
the hard disk and `--device-label hdd`, and keep every other flag. The inputs (`$DBGEN_OUT`,
`$R5_INPUTS`), the images, and the build stay on the SSD; each run's copy of its image, which holds
the maintained state, goes under `--storage-path` on the hard disk. The runs read the inputs only
while pre-reading, before the measured window, and evict them and the copy from the page cache
first. Keeping them
fixed makes the probe change one thing, where the maintained state lives. Name the directories by device, arm, and workload:

```bash
--report "$R5_OUT/pilot-hdd-<arm>-<workload>/manifest.json" \
--storage-path "$HDD_MOUNT/r5/pilot-hdd-<arm>-<workload>/runs" \
--device-label hdd
```

The pilot pass criterion applies unchanged, and each manifest's `storage_device.rotational` should
be `true`. Report
the probe as evidence only: no HDD campaign exists, and nothing here supports a comparison between
devices until the user decides one. Do not summarize `pilot-hdd-*` manifests together with the SSD
campaign's.

### Campaign and four arms

Run the campaign on the SSD, only after all eight SSD pilots pass and the pressure instrumentation
exists ([measurement.md](measurement.md#pending-linux-campaign)); the hard-disk probe does not
gate it. It has four arms, prefix truncation
crossed with the session pass (decision of 2026-11-11), each the pilot command with `--runs 3` and
fresh `--report` and `--storage-path` values:

| Arm | `--front-code-byte-keys` | `--session-pass` |
| --- | --- | --- |
| `A-seek` | yes | `seek` |
| `A-step` | yes | `step` |
| `B-seek` | no | `seek` |
| `B-step` | no | `step` |

Keep every other flag. Run them in the order `A-seek`, `A-step`, `B-seek`, `B-step` for clustered,
then the same for scattered. Give each run its own directory named by arm and workload, with
`--report "$R5_OUT/campaign-A-seek-clustered/manifest.json"` and
`--storage-path "$R5_OUT/campaign-A-seek-clustered/runs"`, and so on. Then check:

- each manifest's `configuration.front_code_byte_keys` and `configuration.session_pass` against its
  arm (`measure.py` already rejects a driver report whose `session_pass` is not the one it asked
  for);
- that the input hashes match across all eight manifests;
- that native's storage and run reads are equal across the four arms of each workload.

Then summarize the eight manifests together:

```bash
python3 summarize.py "$R5_OUT"/campaign-*/manifest.json --out "$R5_OUT/summary"
```

`summarize.py` labels each manifest by its parent directory, which is why each needs its own, and
its Markdown names each workload's session pass. The four arms quadruple both stages: the pilot is
4 arms × 2 workloads × 1 repetition × 2 modes = 16 runs per device (the SSD, then the hard-disk
probe), and the campaign 4 × 2 × 3 × 2 = 48 on the SSD. The inputs are shared, so the disk holds
them once, plus each run's storage (see "Storage path and disk budget").

### Report back

When the pilots, or the campaign, are done, report:

- the pre-flight results, row by row, with the kernel version (`uname -r`) and the filesystem and
  logical sector size of the storage path;
- each manifest's `status` and `errors`, and each run's `qualification`;
- whether the `clear_refs` and `memory.peak` resets worked (`sequence.window_peak_rss_error` and
  `sequence.cgroup.peak.window_peak_supported`), and `cgroup.proof.inputs_evicted` per run;
- the revision (`manifest.revision`) and `git_status_short`, which must be empty;
- each manifest's `measurement_version`, `storage_device`, and fallback counters (`front_coding`,
  `value_header_elision`), and `block_expansions`;
- the timing split per refresh (`delta_ms`, `q3_ms`, `ivm_ms = delta_ms + q3_ms`, `base_ms`) and the
  run reads per owner, as `summarize.py` prints them (`dbsp_operator_*`, `partner_trace_*`,
  `requested_slice_*`, `driver_run_reads`, `filtered_probes`);
- the paths of the manifests and of `summary.md`. Raw artifacts stay on the machine.

### Known limits

- Without `--cgroup-root`, a Linux run is `nonqualifying` (reason "no cgroup v2 total memory
  limit"), yet the campaign still reports `status: complete`. Read each run's `qualification`.
- `memory.peak`, `memory.events`, and `io.stat` are read once, after the run exits. With
  `--images` they cover the refresh process: the eviction, the reopening of the image, the
  republish transaction, the optional warm-up, and the refreshes, but not loading, which ran in
  the load program outside any cgroup.
  The driver's `/proc/self/io` counters are per interval, but they are process-wide.
- The driver now records maintenance-window memory evidence: per-refresh pressure levels
  (`refreshes[].memory_pressure`), `sequence.window_peak_rss_bytes`,
  `sequence.window_end_rss_bytes`, and the `sequence.cgroup` before, after and delta snapshot
  ([measurement.md](measurement.md#pending-pressure-evidence)). The Linux paths have never run.
  The resets they depend on (`clear_refs`, `memory.peak`) are unverified; see the pre-flight rows.
- DBSP's pressure level follows process RSS only, while `memory.max` also charges page cache, so
  the cgroup can near its limit while DBSP reports Low pressure
  ([measurement.md](measurement.md#two-pressure-signals)). Input-cache eviction and refresh-file
  pre-reading narrow the gap and do not close it.
- The jemalloc global allocator and its `malloc_conf` (`prof:true,prof_active:true,lg_prof_sample:19`)
  are defined once, in the driver's `src/main.rs`, because a binary can link one allocator;
  `generate.sh` strips them from the generated circuits. Whether the symbol takes effect from the
  driver on Linux (it changes allocator behavior and RSS) is unverified.
- The campaign runs with Q3's routing hook (`Routing::hook` in `merged-index-col/src/q3.rs`). The
  generic-reader work on the Mac ([deferred.md](deferred.md#generic-readers-before-and-after-state-and-declared-join-readers))
  must not land on the campaign's revision while the campaign runs.
- A seek expands at most one prefix-truncated or elided block, in full, on a buffer-cache miss, and
  the cache holds the expanded bytes. The campaign pays this in every arm that encodes; it is
  documented as suboptimal ([col-storage.md](col-storage.md#prefix-truncation-on-the-read-path)).
- `FelderaCache` (Feldera's own buffer cache, `O_DIRECT` on Linux) needs every read and write to
  be whole 512-byte sectors. DBSP wrote and read whole-file metadata, the checkpoint catalog
  and the operators' state files, through it unpadded, so on Linux every checkpoint failed with
  "write failed: invalid input parameter" (`EINVAL`). macOS has no `O_DIRECT`, so it never showed
  there. The POSIX backend now reads and writes those files through the page cache
  (`PosixBackend::read` and `write`); batch files keep `O_DIRECT`
  (`checkpoint_and_restore_with_feldera_cache` in `dbsp_handle.rs`; fixed on the first Linux host,
  2026-11-17).
- Make none of these claims without raw artifacts: runtime memory pressure, reclaim or refault
  behavior, beyond-memory behavior, equal-memory performance, a disk saving at SF 10, or a
  maintenance-only I/O figure. The window fields are evidence, not proof. No SF 10 run has been
  measured.

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
