# Linux handoff

The commands for running R5 on a Linux machine, in the order they are run. It is written for a fresh agent on a fresh machine.

- **What is measured and why**, which dimensions are decided, and the results: [MEASUREMENT.md](MEASUREMENT.md).
- **Decisions:** [DECISIONS.md](DECISIONS.md).

**Markers.**
- **"Verified"** means checked on the first Linux host.
- **"Unverified"** means it could not be checked there or from the repository.

**The first Linux host** (2026-11-17), an Emulab/CloudLab-style bare-metal node:
- Ubuntu 24.04.5 with the GA kernel 6.8.0;
- two Xeon Silver 4114 (40 logical cores) and 187 GiB RAM;
- an Intel SSDSC2BB48 SSD (480 GB) and a Seagate ST1200MM0088 hard disk (1.2 TB), both with 512-byte logical sectors.

## Machine and OS

- **Architecture: x86_64.** The dbgen build and the JDK path assume it; aarch64 is untested.
- **OS.** An Ubuntu LTS on bare metal or a full VM with root, not a container (unverified).
  - **Kernel 6.8 works** (verified). There, `memory.peak` is read-only, so the window peak falls back (see the pre-flight table).
  - **Kernel 6.12 or later** also lets `memory.peak` be reset (unverified).
- **CPU.** At least 8 cores, for the worker, the index thread, compaction and the build (unverified). The runs use one DBSP worker, so more cores speed only the build.
  - **Pin the frequency after the build.** With `intel_pstate` (`cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_driver`):

    ```bash
    echo performance | sudo tee /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor
    echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo
    ```

  - **Neither setting survives a reboot.** Testbed images often default to `powersave` (verified).
- **RAM.** At least 32 GiB, for the release builds (unverified). Runs are capped at 1 GiB by the cgroup.
- **Disks.** One SSD for the inputs, the build, the images and the runs, and one hard disk for the probe only.
  - **Requirements.** Both need 512-byte logical sectors (`O_DIRECT`, `FBuf::ALIGNMENT`; a 4Kn drive may fail) and ext4 or xfs.
  - **Listing them.** `lsblk -d -o NAME,ROTA,MODEL,SIZE,LOG-SEC,PHY-SEC` (`ROTA` 0 is the SSD).

## Pre-flight checks

Run these before building anything. Each failure is loud at startup rather than corrupting results.

| Check | Command | If it fails |
| --- | --- | --- |
| Unified cgroup v2 with the memory controller | `stat -fc %T /sys/fs/cgroup` prints `cgroup2fs`; `cat /sys/fs/cgroup/cgroup.controllers` lists `memory` | The machine cannot qualify a run. Stop and report. |
| Kernel 5.19 or later, for `memory.peak` | `uname -r` | Runs complete, with `memory.peak` `null`. Report the version. |
| Swap accounting on | after the [cgroup recipe](#cgroup-v2-root), `ls /sys/fs/cgroup/r5/driver/memory.swap.max` | `configure_cgroup` fails on the first run. Enable it or report. |
| Delegation works | the recipe, then `cat /proc/self/cgroup` prints `0::/r5/driver` | Adapt the recipe; never run without `--cgroup-root`. |
| Storage path takes 512-byte `O_DIRECT` | `df -T <dir>` shows ext4 or xfs; `sudo blockdev --getss <device>` prints 512 | Use another filesystem or disk. |
| `clear_refs` resets the peak | `grep VmHWM /proc/self/status; echo 5 > /proc/self/clear_refs; grep VmHWM /proc/self/status` | `window_peak_rss_bytes` is `null`, with `window_peak_rss_error` set. Works on 6.8 (verified). |
| `memory.peak` resets by writing to it (6.12 or later, unverified) | in the cgroup: `exec 3<>/sys/fs/cgroup/r5/driver/memory.peak; echo reset >&3; cat /proc/self/fd/3` | Not a failure: `peak` falls back to the sampled `memory.current` maximum. On 6.8 the `exec` fails with "Permission denied" (verified). |
| Disk room | `df -h <dir>` against [the budget](#storage-path-and-disk-budget) | Free space, or lower `--runs`. |

## Prepare the disks

Skip this if each device already has an ext4 or xfs filesystem with room. On the first host the root was 64 GB of ext3, about 375 GB of the SSD was unallocated, and the hard disk was blank.

**Get the user's approval before writing any partition table.** First inspect:

```bash
lsblk -d -o NAME,ROTA,MODEL,SIZE,LOG-SEC,PHY-SEC
sudo parted -s /dev/sda unit GB print free   # the unallocated block
sudo parted -s /dev/sdb print                # "unrecognised disk label" means blank
```

Then, with the device names checked against that output:

```bash
sudo sgdisk -n 4:0:0 -t 4:8300 -c 4:r5-ssd /dev/sda     # a partition in the largest free block
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

- **A harmless warning.** `sgdisk` and `parted` may warn that "not all of the space available ... appears to be used".
- **Where things go.** `$DBGEN_OUT`, `$R5_INPUTS` and `$R5_OUT` go on `/mnt/ssd`. The clone may stay on the root if it has room: about 18 GB after `verify.sh` (verified).

## State a clone needs

- **Branch `main`.** The fork works on `main`, so a plain clone is right. Before cloning, check that `origin` has the commit: `git rev-list --count origin/main..HEAD` prints 0.
- **Generated files.** `q3-base-circuit/src/{lib,stubs}.rs`, `q3-view-circuit/src/{lib,stubs}.rs` and `plan.json` are generated and git-ignored. `measure.py` runs `generate.sh`. `q3-trie-view-circuit/` is tracked.
- **Raw artifacts** are git-ignored and stay on the machine.

## Toolchain

On Ubuntu 24.04 (verified on a host that had only Python, gcc, make and git):

```bash
sudo apt-get update
sudo apt-get install -y openjdk-21-jdk-headless maven graphviz build-essential pkg-config \
  libssl-dev cmake clang
curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none --profile minimal
. "$HOME/.cargo/env"
rustup toolchain install 1.96.1 --profile minimal -c rustfmt -c clippy
```

- **Rust 1.96.1.** Give each component its own `-c`: rustup 1.29 rejects `--component rustfmt clippy` (verified). Every cargo command passes `+1.96.1`. Export `RUSTUP_TOOLCHAIN=1.96.1` too, since some scripts call plain `cargo`.
- **Java.** JDK 21 and Maven, with `JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64` first on `PATH` (verified).
- **The SQL compiler.** `./build.sh` in `sql-to-dbsp-compiler`. It needs the network for Calcite and Maven, took about two minutes (verified), and ends with "Parser generated successfully". Check that `SQL-compiler/target/sql2dbsp-jar-with-dependencies.jar` exists.
- **Fetch dependencies once, after the compiler is built.** The runner builds with `--offline --locked`. `q3-ivm-baseline` and `merged-index-col` are separate workspaces. Run `./generate.sh` first, since without it `cargo fetch` fails with "no targets specified in the manifest" (verified):

  ```bash
  # in sql-to-dbsp-compiler/experiments/q3-ivm-baseline (compiler built)
  ./generate.sh
  cargo +1.96.1 fetch --locked
  # in sql-to-dbsp-compiler/experiments/merged-index-col, and at the repository root
  cargo +1.96.1 fetch --locked
  ```

- **The adapters crate** (`dbsp_adapters`) needs librdkafka for its default `with-kafka` feature, which the first host lacks. Check it with every other default feature (verified 2026-11-20):

  ```bash
  cargo +1.96.1 check --offline -p dbsp_adapters --no-default-features --features \
    with-deltalake,with-iceberg,with-avro,with-nexmark,with-pubsub,with-redis,with-nats,with-postgres-cdc,with-dynamodb,with-heap-profiling,with-s2
  ```

- **Python** 3.9 or later, with no third-party imports. Also `git`, `make` and `gcc` for dbgen.

**A shell setup file.** Agent harnesses start a fresh shell for each command, so put the environment in one file:

```bash
cat > ~/r5-env.sh <<'EOF'
. "$HOME/.cargo/env"
export JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64
export PATH="$JAVA_HOME/bin:$PATH"
export RUSTUP_TOOLCHAIN=1.96.1
export DBGEN_OUT=/mnt/ssd/dbgen-sf10 R5_INPUTS=/mnt/ssd/r5-inputs R5_OUT=/mnt/ssd/r5-out
export HDD_MOUNT=/mnt/hdd REPO=$HOME/feldera
EOF
```

**Pre-build the release executables** with `-j"$(nproc)"`: `measure.py` builds with `-j2`, which took 56 minutes from a cold target (verified). Build both modes:

```bash
cargo +1.96.1 build --release --offline --locked -j"$(nproc)"
cargo +1.96.1 build --release --offline --locked -j"$(nproc)" --features merged-index
```

## Generate TPC-H inputs

**Build dbgen** from tpch-kit (`https://github.com/gregrahn/tpch-kit.git`):

```bash
cd tpch-kit/dbgen
make MACHINE=LINUX DATABASE=POSTGRESQL
```

**Generate**, from inside a fresh `$DBGEN_OUT`:

```bash
export DSS_CONFIG=/path/to/tpch-kit/dbgen
cd "$DBGEN_OUT"
DSS_PATH=. "$DSS_CONFIG/dbgen" -s 10 -T c -f   # customer.tbl
DSS_PATH=. "$DSS_CONFIG/dbgen" -s 10 -T o -f   # orders.tbl and lineitem.tbl
DSS_PATH=. "$DSS_CONFIG/dbgen" -s 10 -U 4      # update sets 1-4: orders.tbl.uN, lineitem.tbl.uN, delete.N
```

**Watch for:**
- **`-s 10` on every call.** A bare `-U` uses scale factor 1.
- **No `-f` with `-U`.** dbgen then ignores `-U` and regenerates the base tables.
- **A short `DSS_PATH`.** Update paths are formatted into a 128-byte buffer (`upath` in `print.c`), and a long path crashed dbgen.

**What it produces:**
- **The update sets.** Sets 1 to 3 are the measured pairs and set 4 is the optional warm-up pair. Each set has 15,000 Orders and 15,000 deletes at SF 10, with about 60,000 Lines.
- **The base tables:** 1,500,000 Customers, 15,000,000 Orders and 59,986,052 Lines, about 9.7 GB.
- **Time:** about ten minutes (verified, tpch-kit `852ad0a`).

## Prepare the workloads

`prepare_inputs.py` writes one directory per transaction size, and each holds both workloads. Its module docstring states the selection rules; the decided rules are in [MEASUREMENT.md](MEASUREMENT.md#decided).

```bash
for rows in 10 100 1000; do
  python3 prepare_inputs.py --base "$DBGEN_OUT" --updates "$DBGEN_OUT" --out "$R5_INPUTS/k$rows" \
    --seed 1 --pairs 3 --warm-up --transaction-rows "$rows"
done
```

- **`--transaction-rows K`** cuts every RF1 and RF2 set into transactions of about K rows of whole Orders, where an Order counts 1 + its Lineitems. The window applies all of RF1 1, then all of RF2 1, and so on.
- **`--order-keys dbgen|sequence|uuid`** (default `dbgen`) chooses the Order keys. `uuid` writes re-keyed base tables into `<out>/base/`, which need their own images. Its sort uses GNU `sort`, so `<out>` needs room for about twice `lineitem.tbl`. Example:

  ```bash
  python3 prepare_inputs.py --base "$DBGEN_OUT" --updates "$DBGEN_OUT" --out "$R5_INPUTS/uuid-k10" \
    --seed 1 --pairs 3 --warm-up --transaction-rows 10 --order-keys uuid
  ```

- **`--warm-up`** also writes `<workload>/warm-up/` from update set 4.
- **The output.** For each of `clustered/` and `scattered/`: the `rf1-orders-K.tbl`, `rf1-lines-K.tbl` and `rf2-delete-K.tbl` files, and a `refreshes.json` ordered RF1 1, RF2 1, RF1 2, and so on.
  - `prepare-manifest.json` records the seed, the counts, each pair's distinct Customers, and every SHA-256.
  - The script refuses an `--out` that already holds a workload.
  - At SF 10 one size takes about 100 s (verified).

## Measurement dimensions

How each active dimension is selected, and whether it needs its own images. The reasons are in [MEASUREMENT.md](MEASUREMENT.md#dimensions).

| Dimension | Selected by | Own images? |
| --- | --- | --- |
| Transaction size | the refresh list's directory, `$R5_INPUTS/k<K>` (`--transaction-rows`) | no |
| Workload | `clustered/` or `scattered/` refresh list | no |
| Order keys | `$R5_INPUTS/<keys>-k<K>` (`--order-keys`), with images built from its `base/` | yes |
| Prefix filter | `--index-prefix-filter`, on `--build-images` and on every campaign that reopens those images | yes |
| Memtable | `--memtable-mib N` on the campaign; below half of `--memory-target-bytes` | no |
| Memory budget | `--cache-bytes` and `--memory-target-bytes` (decided: one budget, 512/512) | no |
| Device | `--storage-path` and `--device-label ssd\|hdd` | no |
| Repetitions | `--runs` (1 for a pilot, 3 for the campaign) | no |

**Images**, which [Build the images](#build-the-images) describes:
- **Naming.** Name each set `$R5_OUT/images-v<version>[-pf][-<keys>]`, for example `images-v18-pf` or `images-v17-uuid`.
- **When to rebuild.** A campaign refuses an image whose sources changed (`IMAGE_SOURCE_PATHS` in `measure.py`), so a measurement version that touches them needs new sets.

**Decided settings** apply to every run and are not options here: prefix truncation and value-header elision on, the seek pass, no setup compaction, base eviction, and storage-resident filters ([MEASUREMENT.md](MEASUREMENT.md#decided)).

## Build the images

Loading is not measured. It runs once per image set, with no cgroup limit and no memory target, and every measured run starts a new process from its own copy of an image.

```bash
# in sql-to-dbsp-compiler/experiments/q3-ivm-baseline; a new directory on the SSD
python3 measure.py --build-images "$R5_OUT/images-v18-pf" \
  --customer "$DBGEN_OUT/customer.tbl" --orders "$DBGEN_OUT/orders.tbl" \
  --lineitem "$DBGEN_OUT/lineitem.tbl" \
  --elide-value-headers --index-prefix-filter --device-label ssd
```

- **What it builds.** Both release executables, then two loads at once (`IMAGE_ARMS`: arm A only), writing `native-A` and `merged-index-A`.
  - **The refresh program reopens only an image with its own storage settings** (`ImageSettings`: mode, workers, the two encodings, the prefix filter).
  - **A failed load** is reported only after every load finishes, and each completed image is kept.
- **Load settings** (`LOAD_MEMORY_SHARE`, `LOAD_BATCH`, `LOAD_COMPACTION_TIMEOUT_SECS`, `LOAD_MODE_WEIGHTS`; decisions of 2026-11-17 and 2026-11-18).
  - **Cache.** Each load's cache is 75% of `MemAvailable`, split by mode weight: merged-index twice native.
  - **Transactions.** 1,000,000 rows, with a 3600 s wait for merges.
  - **Overrides.** Don't pass the campaign's `--cache-bytes` or `--batch` here: no full compaction follows loading, so the batch shapes the runs, and every image of a set must use the same settings.
- **Why the cache is sized from the host.** A merged-index load reads customer groups scattered over the index while it loads in `orderkey` order. With a small cache it read the SSD (17 to 29 GB at 16 GiB per load) and slowed to under 1% of `lineitem.tbl` per minute (verified).
- **Time and size.** About 1.5 h for a set of two (2026-11-20). A set takes about 35 GB: `native-A` about 19.5 GB, `merged-index-A` about 17.4 GB.
- **`--complete-images DIR`** loads, at once, the images that `images.json` lacks, with the executables recorded there. An empty image directory keeps it from starting that load.
- **`images.json`** records the settings, revision, input hashes, executables' SHA-256, `image_sources_sha256`, and each image's size and load time.
  - **The image.** Each image is `storage/` (one DBSP checkpoint of both circuits, plus the merged index and its orphan lines) and `image.json`. The checkpoint writes every in-memory run to a file.
  - **Images built before the digest** take it with `--adopt-image-sources DIR`, after `git diff` shows no image source or generator changed.

## cgroup v2 root

`measure.py` creates one child cgroup per run under `--cgroup-root` and moves the run into it (`cgroup_available`, `configure_cgroup`, `enter_cgroup`, `verify_membership`). It requires:

- the unified hierarchy, with `memory` in the root's `cgroup.controllers` and `cgroup.subtree_control`;
- a root writable by the user, with no ancestor's `memory.max` below `--total-memory-bytes`;
- each child's `memory.max` and `memory.swap.max` (0) written and read back;
- the child moves itself in from a `preexec_fn`. The kernel requires write access to the common ancestor's `cgroup.procs`, so `measure.py` must sit in a leaf next to the run cgroups.

The recipe (verified, Ubuntu 24.04, kernel 6.8):

```bash
sudo mkdir -p /sys/fs/cgroup/r5/driver
echo +memory | sudo tee /sys/fs/cgroup/cgroup.subtree_control   # harmless if already enabled
echo +memory | sudo tee /sys/fs/cgroup/r5/cgroup.subtree_control
sudo chown -R "$USER" /sys/fs/cgroup/r5
```

- **The tree does not survive a reboot.**
- **The runner never removes the child cgroups.** After the runs:

  ```bash
  find /sys/fs/cgroup/r5 -mindepth 1 -maxdepth 1 -name 'r5-*' -type d -exec rmdir {} +
  ```

**Move the shell and start `measure.py` in one command.** Only the moved shell's descendants inherit the cgroup, and an agent's tool starts a fresh shell for each command:

```bash
bash -c 'echo $$ | sudo tee /sys/fs/cgroup/r5/driver/cgroup.procs >/dev/null &&
  grep -qx "0::/r5/driver" /proc/self/cgroup && exec python3 measure.py ...'
```

The first move needs `sudo`, since the source and destination share only the real root. A `measure.py` left in `0::/user.slice/...` fails in `enter_cgroup`.

## Storage path and disk budget

- **`--storage-path`** goes on local ext4 or xfs, not tmpfs or an overlay.
- **The inputs are shared.** Every run reads `$DBGEN_OUT` and `$R5_INPUTS` in place. Don't edit, touch or regenerate them: each run checks their sizes and modification times against the campaign's record.
- **A run's copy.** Each run copies its image into `<run dir>/image` (`cp -a`, outside its cgroup) and deletes the copy once its report is recorded. `--keep-run-storage` keeps it. A failed run keeps its copy, and the campaign stops.
- **The SSD therefore holds:**
  - the inputs, about 10 GB, plus about twice `lineitem.tbl` for a uuid preparation;
  - each image set, about 35 GB;
  - one run's copy at a time, about 20 GB;
  - the loads' peak while building, about 31 to 38 GB per load, two at once;
  - the build, about 18 GB.
- **Run counts.** One campaign of `--runs N` makes `2N` runs, native and merged-index. A pilot is `--runs 1`.

## Pilot command

Run from `sql-to-dbsp-compiler/experiments/q3-ivm-baseline`, inside the cgroup tree (see above), with fresh `--report` and `--storage-path` values each time. The current baseline pilot:

```bash
python3 measure.py \
  --images "$R5_OUT/images-v18-pf" \
  --refresh-list "$R5_INPUTS/k10/clustered/refreshes.json" \
  --report "$R5_OUT/pilot-A-seek-clustered-k10-<tag>/manifest.json" \
  --storage-path "$R5_OUT/pilot-A-seek-clustered-k10-<tag>/runs" \
  --cgroup-root /sys/fs/cgroup/r5 \
  --total-memory-bytes 1073741824 \
  --cache-bytes 536870912 \
  --memory-target-bytes 536870912 \
  --runs 1 \
  --no-verify \
  --front-code-byte-keys \
  --elide-value-headers \
  --index-prefix-filter \
  --session-pass seek \
  --device-label ssd
```

- **`<tag>`** names the arm, for example `v18-pf-t512-c512` for version, filter, target and cache. Add `-m64` for `--memtable-mib 64`, or `-diag` for a `--diagnostics` rerun.
- **`--images`:** the image set of the arm. With `--index-prefix-filter` it must be a `-pf` set; the runner rejects the other arm.
- **`--refresh-list`:** the dimension's inputs, `k<K>/<workload>` or `<keys>-k<K>/<workload>`.
- **`--total-memory-bytes`:** a 1 GiB `memory.max`, with no swap. Use 2 GiB with `--diagnostics`, whose logs are held in memory (a 1 GiB diagnostics run was OOM-killed).
- **`--cache-bytes`, `--memory-target-bytes`:** the one budget, 512/512 ([decision](DECISIONS.md#2026-11-18-one-memory-budget-the-cache-about-equals-the-memory-target)). The cache is a whole number of MiB. In the memtable arms the cache is 448 MiB, with `--memtable-mib 64` or without it.
- **`--no-verify`:** required to qualify.
- **`--front-code-byte-keys`, `--elide-value-headers`, `--session-pass seek`:** decided settings.
- **`--warm-up LIST`:** runs one pair untimed before the window. Leave it out unless the user asks for the warm arm.

**Pass criterion, per pilot:**
1. **The manifest.** Its `status` is `complete` and `errors` is empty.
2. **Both runs qualify.** Each run's `qualification.status` is `qualifying`. That requires:
   - Linux with the cgroup verified (`cgroup.enforced`);
   - the inputs and the image copy evicted (`cgroup.proof.inputs_evicted`);
   - verification off;
   - the image's loaded size above the limit (`image.load.observed_storage_bytes`).

   A qualifying run shows scale, not pressure.

**Then read the evidence** in each run's `driver.json`, as evidence, not proof of pressure:
- **Window RSS.** `sequence.window_peak_rss_bytes` and `window_end_rss_bytes` are numbers, and `window_peak_rss_error` is `null`.
- **`sequence.cgroup`.** It is not `null`. If `peak.window_peak_supported` is false, which it always is on 6.8, use `max_sampled_memory_current_bytes`, a lower bound.
- **Input eviction.** `inputs_evicted.evicted` is true.
- **Pressure, two ways.** `refreshes[].memory_pressure` is DBSP's level, and `cgroup.delta` (`pgscan`, `pgsteal`, `workingset_refault_*`, `memory.events max`) is the kernel's. Report them apart.
- **The version and fallbacks.** `measurement_version` is 18, and the `layout_fallbacks` of `front_coding` and `value_header_elision` are 0. The runner gates on both.
- **The arm.** `configuration.index_prefix_filter` and `configuration.memtable_mib` are as asked. The runner checks this too.
- **The phases.** `delta_ms`, `q3_ms`, `ivm_ms` and `base_ms`, with their `transaction_latency` percentiles.
- **Reads and spines.**
  - the run reads per owner;
  - `filtered_probes` and `filtered_prefix_probes`;
  - `block_expansions`;
  - `run_counts`, with each spine's runs, merges, memtable fields and `device_reads` by thread.
- **The writes.** With `--diagnostics`, `python3 spill_kinds.py <manifest>` classes the window's files by write kind.

## Hard-disk probe

The hard disk gets only pilots, as a probe ([decision](DECISIONS.md#2026-11-16-the-ssd-runs-the-campaign-the-hard-disk-only-a-pilot-probe)). It has not run yet. Run it after the SSD pilots of the arms the user chooses.

**What changes.** Repeat each pilot with its run directories on the hard disk; every other flag stays:

```bash
--report "$R5_OUT/pilot-hdd-<arm>-<workload>-<tag>/manifest.json" \
--storage-path "$HDD_MOUNT/r5/pilot-hdd-<arm>-<workload>-<tag>/runs" \
--device-label hdd
```

- **What stays on the SSD.** The inputs, the images and the build. Each run copies its image to the hard disk, so the probe changes only where the maintained state lives.
- **How to report it.** The pass criterion applies, and `storage_device.rotational` is `true`. Report the probe as evidence only, never summarized with the SSD manifests, until the user decides a comparison.

## Campaign

Run a campaign per arm the user chooses, after its pilot passes: the pilot command with `--runs 3` and fresh destinations, `campaign-<arm>-<workload>-<tag>`. The remaining arms are listed in [MEASUREMENT.md](MEASUREMENT.md#pending-linux-campaign).

**Then check:**
- each manifest's `configuration`, which must show the arm;
- that the input hashes match across the manifests compared.

**Summarize** the manifests together:

```bash
python3 summarize.py "$R5_OUT"/campaign-*/manifest.json --out "$R5_OUT/summary"
```

`summarize.py` labels each manifest by its parent directory, so each needs its own.

## Report back

**Report, view circuit first:**
- **Setup.** The pre-flight results, with `uname -r`, and the storage path's filesystem and sector size.
- **Outcome.** Each manifest's `status` and `errors`, each run's `qualification`, and `cgroup.proof.inputs_evicted`.
- **Window evidence.** `window_peak_rss_error` and `peak.window_peak_supported`.
- **Provenance.**
  - `manifest.revision`;
  - `git_status_short`, which must be empty: don't edit the checkout during a campaign, and use a separate worktree for edits;
  - `measurement_version`, `storage_device`, and the arm's `configuration`.
- **Timing.** Per refresh: `delta_ms`, `q3_ms`, `ivm_ms` and their percentiles first, then `base_ms`, amortized.
- **Reads, writes and spines.** The run reads as `summarize.py` prints them, `block_expansions`, `run_counts`, and for diagnostics runs the `spill_kinds.py` table.
- **Pressure** per refresh, in both modes.
- **Paths** of the manifests and of `summary.md`. The raw artifacts stay on the machine.

Then record the results in a dated section of [MEASUREMENT.md](MEASUREMENT.md) and update its Synopsis.

## Known limits

- **The `step` pass cannot finish at small transactions.** `ScanSession::next_group` under `Pass::Step` steps one entry at a time from the first key of `Old`, so a transaction of about 10 rows walks about half the index (about 76 million entries at SF 10). A-step clustered's merged-index run read 15 GB in 49 minutes and was stopped. If the pass returns, run it only at the TPC-H batch size, or after it seeks to each transaction's first group.
- **Without `--cgroup-root`, a Linux run is `nonqualifying`,** yet the campaign reports `status: complete`. Read each run's `qualification`.
- **`memory.peak`, `memory.events` and `io.stat` are read once, after the run exits.** They cover the whole refresh process: eviction, restore, republish, warm-up and the refreshes. Loading ran elsewhere. `/proc/self/io` is per interval but process-wide.
- **DBSP's pressure follows process RSS only**, while `memory.max` also charges page cache ([two pressure signals](MEASUREMENT.md#two-pressure-signals)). In every pilot so far DBSP's target bound (`critical`) and the kernel's limit did not (`memory.events max` 0).
- **The jemalloc allocator** and its `malloc_conf` (`prof:true,prof_active:true,lg_prof_sample:19`) are defined once, in `src/main.rs`. Whether the symbol takes effect from the driver on Linux is unverified.
- **The campaign runs with Q3's routing hook** (`Routing::hook` in `merged-index-col/src/q3.rs`). Generic-reader work ([deferred.md](../../../docs/design/merged-index/deferred.md#generic-readers-before-and-after-state-and-declared-join-readers)) must not land on a campaign's revision while it runs.
- **A seek expands at most one encoded block, in full, on a cache miss**, and the cache holds the expanded bytes ([storage-layout.md](../../../docs/design/merged-index/storage-layout.md#prefix-truncation-on-the-read-path)).
- **`FelderaCache` needs whole 512-byte sectors.** Under `O_DIRECT` every read and write must be whole sectors. The fix (verified, 2026-11-17): whole-file metadata (the checkpoint catalog, operator state) goes through the page cache (`PosixBackend::read` and `write`), and batch files keep `O_DIRECT`.
- **Claims.** Make none without raw artifacts: of runtime pressure, of reclaim, of beyond-memory behavior, of equal-memory performance, of a disk saving, or of a maintenance-only I/O figure.
  - **The pilots are one repetition each.** No three-repetition campaign has run.
  - **The window fields are evidence, not proof.**
