# Linux handoff

This file is for a fresh agent on a fresh Linux machine that will run the R5 pilot. It is the
only place for these commands; [MEASUREMENT.md](MEASUREMENT.md) defines what is measured and why.
Anything marked "unverified" could not be checked from the repository.

## Machine and OS

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
  the build, and the campaign's storage. The hard disk holds only the probe's 8 runs
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

## Pre-flight checks

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

## State a clone needs

- Branch `main`, or any commit that contains this section. Since 2026-11-17 the fork works on
  `main` (`docs/merged-index-tidy-up` is retired), so a plain `git clone` checks out the right
  branch. Before the Linux clone, check that `origin` has the commit: `git rev-list --count
  origin/main..HEAD` must print 0. Otherwise the user must push the verified commit, or supply it
  (for example as a `git bundle`).
- `q3-ivm-baseline/q3-base-circuit/src/{lib,stubs}.rs`, `q3-view-circuit/src/{lib,stubs}.rs`, and
  `plan.json` are generated and git-ignored. `measure.py` runs `generate.sh` itself, which
  generates only the native pair. `q3-trie-view-circuit/` is hand-written and tracked.
- Raw artifacts (reports, run directories) are git-ignored and local to the machine.

## Toolchain

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

## Generate TPC-H inputs

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

## Prepare the workloads

The campaign needs one `refreshes.json` per workload (clustered and scattered; format in "Measure R5"),
next to the refresh files it names. Both workloads share the base files in `$DBGEN_OUT`. The helper is `prepare_inputs.py` in
`q3-ivm-baseline`:

```bash
for rows in 10 100 1000; do
  python3 prepare_inputs.py --base "$DBGEN_OUT" --updates "$DBGEN_OUT" --out "$R5_INPUTS/k$rows" \
    --seed 1 --pairs 3 --warm-up --transaction-rows "$rows"
done
```

`--transaction-rows K` cuts every RF1 and RF2 set into consecutive transactions of about K rows
(whole Orders; an Order counts 1 + its Lineitems), and each refresh of `refreshes.json` then carries
`transaction_orders`, the Order count of each transaction. The window applies all of RF1 1 as small
transactions, then all of RF2 1, and so on (decision of 2026-11-17). Without the option each set is
one transaction, the TPC-H batch. Each size gets its own `--out`; the images serve every size.

`--warm-up` also writes `<workload>/warm-up/` (pair 4, from update set 4, with its own
`refreshes.json`) for the refresh program's optional warm-up. The measured pairs are the same with
or without it.

It writes, for each of `clustered/` and `scattered/`, the files `rf1-orders-K.tbl`,
`rf1-lines-K.tbl`, and `rf2-delete-K.tbl` for each pair K, and a `refreshes.json` ordered RF1 1,
RF2 1, RF1 2, and so on. `$R5_INPUTS/prepare-manifest.json` records the seed, the base and per-pair
counts, each pair's distinct Customers and the Order counts of its top 1, 10, and 100 Customers, and the SHA-256 of every input and output. The script refuses
an `--out` that already holds either workload. Only Customers whose key is not a multiple of 3
receive RF1 Orders, as TPC-H specifies. Scattered draws them uniformly; clustered draws TPC-C's
NURand(1023, 1, 3000) over blocks of 3,000 eligible Customers, in sessions of
`--orders-per-session` (default 4) Orders per Customer per transaction, independent of the RF2 sets. The selection rules are in its module docstring and
summarized in [MEASUREMENT.md](MEASUREMENT.md#pending-linux-campaign). The whole path (dbgen,
`prepare_inputs.py`, `measure.py`, `summarize.py`) ran end to end on macOS at SF 0.01. At SF 10 the
command above took about 100 seconds and wrote 15,000 RF1 Orders and 15,000 RF2 deletes per pair in
both workloads (verified on the first Linux host).

## Build the images

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

**Completing an image directory.** `measure.py --complete-images "$R5_OUT/images" --customer ...
--orders ... --lineitem ... --elide-value-headers` loads, all at once, the images that
`images.json` lacks, with the executables recorded there (so every image comes from the same
build), and marks the manifest complete. Each load's cache is 75% of `MemAvailable` split by mode,
a merged-index load getting twice a native one (`LOAD_MODE_WEIGHTS`), unless `--cache-bytes` is
given. A missing image's directory may exist only empty: an empty directory is how a running
sequential build is kept from starting a load (the load program refuses an existing directory),
since stopping `measure.py` would stop its running load too.

**Load settings.** Loading has its own defaults in `measure.py` (`LOAD_MEMORY_SHARE`, `LOAD_BATCH`,
`LOAD_COMPACTION_TIMEOUT_SECS`; decisions of 2026-11-17): each load's buffer cache is 75% of the
host's `MemAvailable`, split evenly among the loads that run at once (all four by default, about
35 GiB each on a 187 GiB host; one with `--sequential-loads`, which gives each load the whole
budget), with 1,000,000-row load transactions and a 3600 s wait for the background merges.
`images.json` records the cache used (`configuration.cache_bytes`, `concurrent_loads`,
`mem_available_bytes`). Flags override them, but do not pass the campaign's
`--cache-bytes 268435456` or `--batch 10000` here. None of
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
`Q3State::collect`, and its time went to block reads, checksums, and block expansion. With a
16 GiB cache per load (four at once) and 1,000,000-row transactions it read about 2% per minute
and nothing from the device until the caches filled, at about 30% of `lineitem.tbl`; then it read
17 to 29 GB back from the SSD (`read_bytes` in `/proc/<pid>/io`) and slowed to about 0.9% per
minute, and native from about 1.8% to 1.4%. Hence the cache sized from the host. Each load holds up
to its cache plus a few GiB of working memory; if `read_bytes` still grows, the working set does
not fit, and `--sequential-loads` gives each load the whole budget.

Measured on the first Linux host (SF 10, revision `da074ff7e`, four loads at once with a 16 GiB
cache each and 1,000,000-row transactions, before the cache was sized from the host):

| Image | Load (min) | Of which merge wait (min) | Stored (GB) | Peak while loading (GB) |
| --- | ---: | ---: | ---: | ---: |
| `native-A` | 78.9 | 18.9 | 19.51 | 34.9 |
| `native-B` | 82.3 | 22.4 | 19.51 | 35.8 |
| `merged-index-A` | 135.5 | 8.1 | 17.41 | 25.4 |
| `merged-index-B` | 142.1 | 6.5 | 20.40 | 29.2 |

The four images take 77 GB, and the build peaked near 125 GB on the SSD.

The measurement version 15 images (revision `c274d00d7`, 2026-11-18): `native-A` loaded alone with
`--sequential-loads`; the other three then loaded at once with `--complete-images`, each
merged-index load getting twice native's cache (decision of 2026-11-18):

| Image | Cache (GiB) | Load (min) | Of which merge wait (min) | Stored (GB) | Peak while loading (GB) | Read back from the SSD (GB) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `native-A` | 138 | 60.2 | 6.3 | 19.55 | 37.7 | about 0 |
| `native-B` | 27.7 | 80.7 | 21.7 | 19.51 | 35.3 | over 31.8 |
| `merged-index-A` | 55.3 | 88.5 | 18.5 | 17.41 | 30.9 | over 16.9 |
| `merged-index-B` | 55.3 | 87.0 | 16.2 | 20.40 | 34.1 | over 22.4 |

The read-back figures are `/proc/<pid>/io` `read_bytes` at the last sample before each load
ended (80 minutes in). All three parallel loads began to read back once their caches filled (at
about 30 minutes for `native-B`, 50 for the merged-index loads). So a native load gains from a
large cache too, not only a merged-index one: `native-B` with 27.7 GiB took 20 minutes longer than
`native-A` with 138 GiB, most of it in the merge wait.

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

## cgroup v2 root

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

## Prepare the disks

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

## Storage path and disk budget

- Put `--storage-path` on a local ext4 or xfs filesystem, not tmpfs or an overlay filesystem. Storage
  files open with `O_DIRECT`, and `FBuf::ALIGNMENT` is 512 bytes (see
  [MEASUREMENT.md](MEASUREMENT.md#pending-linux-campaign)).
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
  four pilots make 8 and the four campaigns 24. In the hard-disk probe, each run copies its image
  from the SSD's `$R5_OUT/images` to its run directory on the hard disk.
- `measure.py` also hashes the inputs once for the manifest, and builds both release executables
  under `q3-ivm-baseline/target/`; keep room for the build.

## Pilot command

Run once per workload, from `sql-to-dbsp-compiler/experiments/q3-ivm-baseline`, with the
compiler built and `JAVA_HOME` set, from a shell inside the cgroup tree above. Use fresh
`--report` and `--storage-path` values each time.

```bash
python3 measure.py \
  --images "$R5_OUT/images" \
  --refresh-list "$R5_INPUTS/k10/clustered/refreshes.json" \
  --report "$R5_OUT/pilot-A-seek-clustered/manifest.json" \
  --storage-path "$R5_OUT/pilot-A-seek-clustered/runs" \
  --cgroup-root /sys/fs/cgroup/r5 \
  --total-memory-bytes 1073741824 \
  --cache-bytes 268435456 \
  --memory-target-bytes 536870912 \
  --runs 1 \
  --no-verify \
  --front-code-byte-keys \
  --elide-value-headers \
  --session-pass seek \
  --device-label ssd
```

This is arm `A-seek` for clustered. The two arms differ in one flag ("Campaign and two arms"
below): A keeps `--front-code-byte-keys` and B drops it. Both use `--session-pass seek`; the step
arms were dropped on 2026-11-18 ("Known limits").
Give each pilot its own `--report` and `--storage-path`, named by arm and workload:
`--report "$R5_OUT/pilot-<arm>-<workload>/manifest.json"` and
`--storage-path "$R5_OUT/pilot-<arm>-<workload>/runs"`. Run the four pilots in the order `A-seek`,
`B-seek` for clustered, then the same for scattered. Why each flag:

- `--images`: start each run from a copy of its mode's and arm's image ("Build the images"),
  in a new process that runs only the refreshes (the driver's `--from-image`). The window starts
  with the buffer cache empty; add `--warm-up "$R5_INPUTS/k10/clustered/warm-up/refreshes.json"` (the
  workload's own warm-up pair) to run one unmeasured RF1/RF2 pair before it instead. Run the
  pilots without `--warm-up` unless the user asks for the warm arm.
- `--cgroup-root`, `--total-memory-bytes`: enforce a 1 GiB `memory.max` with no swap, per run.
- `--cache-bytes 268435456`: the 256 MiB worker cache (a whole number of MiB), which since measurement version 15 also holds Bloom filter partitions (decision of 2026-11-18).
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

**Pilot pass criterion.** For each of the four pilots, and for both runs in its report (`runs[].qualification`), `status` must be
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

## Hard-disk probe

The user decided on 2026-11-16 that the hard disk gets only the pilots, as a probe (8 runs since the step arms were dropped)
([DECISIONS.md](DECISIONS.md#2026-11-16-the-ssd-runs-the-campaign-the-hard-disk-only-a-pilot-probe)).
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

## Campaign and two arms

Run the campaign on the SSD, only after all four SSD pilots pass and the pressure instrumentation
exists ([MEASUREMENT.md](MEASUREMENT.md#pending-linux-campaign)); the hard-disk probe does not
gate it. It has two arms, prefix truncation on or off with the `seek` pass (decisions of
2026-11-11 and 2026-11-18), each the pilot command with `--runs 3` and
fresh `--report` and `--storage-path` values:

| Arm | `--front-code-byte-keys` | `--session-pass` |
| --- | --- | --- |
| `A-seek` | yes | `seek` |
| `B-seek` | no | `seek` |

Keep every other flag. Run them in the order `A-seek`, `B-seek` for clustered,
then the same for scattered. Give each run its own directory named by arm and workload, with
`--report "$R5_OUT/campaign-A-seek-clustered/manifest.json"` and
`--storage-path "$R5_OUT/campaign-A-seek-clustered/runs"`, and so on. Then check:

- each manifest's `configuration.front_code_byte_keys` and `configuration.session_pass` against its
  arm (`measure.py` already rejects a driver report whose `session_pass` is not the one it asked
  for);
- that the input hashes match across all four manifests;
- that native's storage and run reads are equal across the two arms of each workload.

Then summarize the four manifests together:

```bash
python3 summarize.py "$R5_OUT"/campaign-*/manifest.json --out "$R5_OUT/summary"
```

`summarize.py` labels each manifest by its parent directory, which is why each needs its own, and
its Markdown names each workload's session pass. The two arms double both stages: the pilot is
2 arms × 2 workloads × 1 repetition × 2 modes = 8 runs per device (the SSD, then the hard-disk
probe), and the campaign 2 × 2 × 3 × 2 = 24 on the SSD. The inputs are shared, so the disk holds
them once, plus each run's storage (see "Storage path and disk budget").

## Report back

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

## Known limits

- The `step` session pass cannot finish at small transaction sizes. `ScanSession::next_group`
  under `Pass::Step` steps one entry at a time from the session's position, which starts at the
  first key of `Old`, to the next touched group. A TPC-H batch transaction touches thousands of
  customers spread over the index, so one pass over the index serves them all; a transaction of
  about 10 rows touches 1 to 3 customers, so each transaction walks about half the index (about
  76 million entries at SF 10). On the first Linux host, the reconstructed run of pilot
  `A-step` clustered at K = 10 read 15 GB from the SSD in 49 minutes, against about 7 minutes for
  the whole `A-seek` run, and was stopped (2026-11-18). The step arms were dropped on 2026-11-18
  ([DECISIONS.md](DECISIONS.md#2026-11-18-the-step-arms-are-dropped)). If they return, run them only at the TPC-H batch
  size, or after the pass seeks to the first touched group of each transaction.
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
  ([MEASUREMENT.md](MEASUREMENT.md#pending-pressure-evidence)). The Linux paths have never run.
  The resets they depend on (`clear_refs`, `memory.peak`) are unverified; see the pre-flight rows.
- DBSP's pressure level follows process RSS only, while `memory.max` also charges page cache, so
  the cgroup can near its limit while DBSP reports Low pressure
  ([MEASUREMENT.md](MEASUREMENT.md#two-pressure-signals)). Input-cache eviction and refresh-file
  pre-reading narrow the gap and do not close it.
- The jemalloc global allocator and its `malloc_conf` (`prof:true,prof_active:true,lg_prof_sample:19`)
  are defined once, in the driver's `src/main.rs`, because a binary can link one allocator;
  `generate.sh` strips them from the generated circuits. Whether the symbol takes effect from the
  driver on Linux (it changes allocator behavior and RSS) is unverified.
- The campaign runs with Q3's routing hook (`Routing::hook` in `merged-index-col/src/q3.rs`). The
  generic-reader work on the Mac ([deferred.md](../../../docs/design/merged-index/deferred.md#generic-readers-before-and-after-state-and-declared-join-readers))
  must not land on the campaign's revision while the campaign runs.
- A seek expands at most one prefix-truncated or elided block, in full, on a buffer-cache miss, and
  the cache holds the expanded bytes. The campaign pays this in every arm that encodes; it is
  documented as suboptimal ([storage-layout.md](../../../docs/design/merged-index/storage-layout.md#prefix-truncation-on-the-read-path)).
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
