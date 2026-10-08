# R5 memory-pressure evidence and Linux campaign

## Status

The historical R5 v1 macOS runs used forced final compaction. Their highest
reported RSS was 50,135,040 bytes (about 47.8 MiB), against a 512 MiB runtime
target whose pressure threshold is 85%, or 435.2 MiB. The configured cache was
32 MiB, occupancy stayed below 2 MiB, and backend reads were zero. Those runs
did not demonstrate runtime memory pressure or beyond-memory behavior.

The revised R5 completion boundary has no forced full compaction: loading waits
only for naturally eligible background merges to become idle and publishes
once; refreshes may leave eligible work running; there is no final wait or extra
transaction. The revised macOS smoke comparisons completed 18 fresh processes.
Their maximum RSS was 45,285,376 bytes (43.2 MiB), below the 435.2 MiB threshold;
the tiny initial base remained entirely in RAM. Linux qualification is pending.
No input-preparation helper for the campaign below is implemented yet.

## Linux pilot and campaign

Use SF 10 dbgen input as a file-backed workload large enough to test under a
bounded cgroup. With local `tpch-kit/dbgen`, generate the base relations and
three refresh batches using `dbgen -s 10 -T c`, `dbgen -s 10 -T o`, and
`dbgen -s 10 -U 3`. This yields approximately 1.5 million Customers, 15 million
Orders, and 60 million Lines. Each RF1 batch adds about 15,000 Orders and 60,000
Lines.

Prepare three sets of 15,000 distinct deletion IDs from cold, existing base
Orders offline. For the clustered workload, select from the range belonging to
the first 7,500 Customers. For the scattered workload, use fixed-seed uniform sampling of existing base
Order rows across the full relation, rather than sampling integers from the
sparse Order-key range. Record the seed. Do not reuse deletion IDs across pairs. Pair both cases with
RF1 inputs containing the same complete Order/Lineitem groups: map clustered
Orders to a narrow Customer-key range and scattered Orders across a broad range.
Use identical input files and verify their hashes match between native and
reconstructed modes. Keep RF1 Orders ascending and Lineitems grouped by Order.

Run each mode in a fresh process, storage directory, and cgroup v2 child. Copy
all private input files into that cgroup before loading so input pages and every
process/thread count against the same total-memory limit. Start with a 1 GiB
`memory.max`, `memory.swap.max=0`, a 128 MiB configured cache, 512 MiB runtime
RSS target, one worker, 10,000-row loading target, and three repetitions. Keep
all other run settings identical across modes. On Linux the Feldera storage
cache uses `O_DIRECT` for state data; ordinary input reads still use the OS page
cache and count against the cgroup. macOS uses ordinary caching and its results
remain smoke evidence only. Consult the kernel's [cgroup v2 memory
controller](https://docs.kernel.org/admin-guide/cgroup-v2.html) documentation
when preparing the delegated cgroup.

First run a one-repetition pilot. Require
`load.observed_storage_bytes > memory.max` in both modes and target roughly four
times the limit before starting the campaign. This checks stored-state scale;
do not substitute transient peak disk size. Stored bytes exceeding the memory
limit alone do not prove that the runtime reached memory pressure or that
maintenance caused reclaim.

From the repository root, after preparing the clustered inputs and a writable
cgroup parent with the memory controller enabled for its children, run the pilot:

```sh
python3 sql-to-dbsp-compiler/experiments/q3-ivm-baseline/measure.py \
  --customer "$R5_INPUTS/customer.tbl" \
  --orders "$R5_INPUTS/orders.tbl" \
  --lineitem "$R5_INPUTS/lineitem.tbl" \
  --refresh-list "$R5_INPUTS/refreshes.json" \
  --report target/r5-linux-clustered-pilot.json \
  --storage-path target/r5-linux-clustered-pilot-runs \
  --cgroup-root "$R5_CGROUP_ROOT" \
  --total-memory-bytes 1073741824 \
  --cache-bytes 134217728 --memory-target-bytes 536870912 \
  --batch 10000 --runs 1 --no-verify
```

`R5_INPUTS` is the prepared workload directory; `R5_CGROUP_ROOT` is the delegated
cgroup parent. Java 21 and Rust 1.96.1 must be available as described in the
experiment README. Repeat with scattered inputs and fresh report/storage paths.
After both pilots pass and pressure instrumentation is added, use `--runs 3`
with fresh destinations for each campaign. Reserve space for the private input
copies and maintained storage in every fresh run.

## Pressure evidence still needed

The current runner records cgroup limits, memory peak/events, and I/O, but does
not sample runtime memory-pressure levels or maintenance-only reclaim/refault
counters. Add instrumentation before making a pressure or reclaim claim:

- Sample `Runtime::current_memory_pressure()` through the refresh sequence and
  preserve each observed level with its interval.
- Snapshot cgroup `memory.current`, `memory.events`, `memory.stat` reclaim and
  refault counters, and `memory.pressure` immediately before and after measured
  maintenance. Report the deltas separately from loading and input preparation.
- Retain cgroup memory peak and `io.stat` as run-level evidence. Explain that
  driver, runtime, auxiliary threads, configured cache, and cgroup-charged input
  page cache share the enforced total-memory limit.

As a separate pressure probe, repeat a pilot with a 192 MiB runtime target and
check that the new samples show the intended pressure levels. An optional delta
stress case can concatenate eight complete RF1 sets (120,000 Orders and about
480,000 Lines) in RAM. A run that exceeds its budget must fail as one complete
refresh; the harness must not split it to make it fit.

The revised R5 smoke rerun is complete. Keep the Linux pilot, instrumentation,
and qualifying three-repetition campaign as separate pending milestones. The campaign is not
ready until its input-preparation helper, workload hashes, storage pilot, and
pressure measurements are recorded in [experiment results](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/RESULTS.md).
