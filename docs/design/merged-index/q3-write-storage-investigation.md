# Q3 write and storage investigation

Reconstructed mode should keep less state than native mode. It drops the four join traces
(Customer, Orders, Lineitem, and Orders ⋈ Customer) and reads join partners from one merged
index of the three source tables. Yet in the R5 smoke runs it writes more and keeps more on disk.
This file records why, using direct evidence only.

This file is a staging area. Concurrent agents are reorganizing the merged-index docs, so the
findings live here until the workspace is clean. They will then move into one section of
`measurement.md`.

## Terms

- **Batch:** the delta of one transaction.
- **Run:** an immutable sorted collection in a Spine. "In-memory run" and "on-disk run" name the
  location when it matters. Code fields such as `memory_batch_count` count runs; they are quoted
  as code.
- **Compaction:** LSM compaction, never "merge." Background compaction is distinct from a
  requested full compaction (`DBSPHandle::start_compaction`).
- **Merged index:** the COL index that `ManualIndex` maintains.
- **Base-relation state:** the base tables stored by primary key. Feldera calls this the "input
  map"; the name appears here only to locate code.
- **Operations**, named separately:
  - finish a transaction
  - request full compaction
  - wait for compaction to finish
  - publish snapshots
  - release snapshots

## Starting evidence

Checked against `ce1188031` and `target/r5-smoke-natural/` (local, git-ignored). Each item is
labelled an observation, a deduction, or a hypothesis.

### How the numbers are measured (observations)

- **Write counters.** They are process-wide. "Operations" counts backend flushes; "bytes" counts
  bytes written (`posixio_impl.rs`, read in `measurement.rs`).
- **Measurement window.** It starts just before the first refresh and ends after the last
  refresh releases its snapshots. It covers every writer: base-relation state, V, the join traces
  or the merged index, and any background compaction that falls inside it.
- **Disk usage.** It is read just after the window closes, without waiting for compaction.

### Large-group results (observations)

All three repetitions of each mode agree.

| | Native | Reconstructed |
| --- | --- | --- |
| Whole sequence | 6 ops, 765,952 B | 72 ops, 2,171,392 B |
| First RF1 (10,000 lines) | 0 ops | 69 ops, 1,590,272 B |
| Disk usage at end | 765,952 B | 2,171,392 B |

### Other observations

- **Variation.** Clustered and scattered results vary between repetitions in both modes, so every
  repetition is reported.
- **Disk usage vs. bytes written.** Disk usage equals bytes written in 14 of 18 runs and exceeds
  it in 4. It is never lower. Nothing is on disk when loading ends.
- **Loading.** One Customer transaction is followed by six Order and Lineitem transactions.
  - The merged index gains 7 in-memory runs.
  - Native Orders, Lineitem, and V gain 6 each.
  - Background compaction starts at 8 runs (`MIN_LEVEL0_MERGE_BATCHES`).
- **Final merged index.** It ends with 1 on-disk run and 3 in-memory runs.

### Deductions

- **No deleted files.** No file was deleted during any sequence, so here bytes written equals disk
  used. In general the two differ.
- **Late writes.** In 4 runs, some files were written after the window closed.

### Hypotheses

- **H1: index compaction at the first RF1.** The merged index compacts during the first RF1,
  because 7 loaded runs plus 1 reach the threshold of 8. Native traces reach only 7.
- **H2: a POSIX writer bug inflates the 69 ops.** After a flush, the writer may clear its buffer
  but not its length counter, so the 1 MiB flush threshold would stay tripped and each later write
  would flush on its own.

## Step 1: state kept by each mode

Checked in the code at `c124e6158`. Only the join implementation differs between the modes.

| State | Native | Reconstructed | Created by |
| --- | --- | --- | --- |
| Base-relation state (Customer, Orders, Lineitem) | yes | yes | `add_input_map_persistent` in the generated circuit |
| Delta accumulators (join inputs) | yes | yes | `dyn_shard_accumulate`, `Accumulator::new` (`TraceRole::Accumulator`) |
| Four join traces | yes | no | `dyn_shard_accumulate_trace` in `dyn_join_generic` |
| V | yes | yes | `register_materialized_output_zset_persistent` (`catalog.rs`) |
| Merged index (one Spine) | no | yes | `ManualIndex::new` (`reconstruction.rs`, `TraceRole::Integral`) |

- **Native joins.** `dyn_join_generic` builds a trace for each input with
  `dyn_shard_accumulate_trace`. The two joins (Orders ⋈ Customer, then Lineitem ⋈ that result)
  therefore keep four join traces.
- **Reconstructed joins.** `dyn_join_generic_with_snapshot_readers` calls only
  `dyn_shard_accumulate` on each delta. It builds no trace and no delay; `JoinTrace` reads join
  partners through the supplied snapshot readers at the commit flush.
- **Merged index.** `ManualIndex` owns one Spine, created on its auxiliary thread. `Q3State` holds
  its before and after snapshots (`SpineSnapshot`) for each transaction.

So reconstructed mode replaces four join traces with one merged-index Spine and keeps everything
else. The extra writes must come from the merged index, from a difference in when compaction runs,
or from the storage layer.

### The POSIX length counter (code reading)

`VectoredWrite::write` in `posixio_impl.rs` flushes when `self.len >= 1024 * 1024`, then adds the
buffer's length to `self.len`. `VectoredWrite::flush` clears `self.buffers` but never resets
`self.len`. Once a file's writer has buffered 1 MiB, every later `write` flushes the buffers queued
since the previous call. This confirms the mechanism behind H2. Whether it explains the 69
operations still needs the flush log of Step 2.

## Steps 2 and 3: diagnostics and writes by owner

Commits `4fe5cc655` (DBSP: owners, flush and compaction logs) and `ed974b7e9` (harness:
`--diagnostics`, per-owner report fields). Raw artifacts are in `target/r5-diagnostics/` (local,
git-ignored): three workloads, three repetitions per mode, measurement version 2.

### Checks (observations)

- **Diagnostics do not perturb the totals.** Large-group with and without `--diagnostics` gives
  identical write operations, bytes, and disk usage in every repetition. They also equal the
  earlier smoke results (native 6 ops and 765,952 B; reconstructed 72 ops and 2,171,392 B).
- **The flush log sums to the counters** in all 18 diagnostic runs (`matches_counters`), and the
  three owners sum to the totals.

### Naming the traces

The logs name Spines by circuit node. Owners come from the code; the table names are
deductions from record counts (64 Customers, 512 Orders, 2,048 Lines) and from block bytes. The
native trace whose blocks match the reconstructed V exactly is taken to be V.

| Spine | Owner | Native | Reconstructed |
| --- | --- | --- | --- |
| `Z1Trace nn13`, `nn24` | base relation | Orders, Lineitem state | same |
| `AccumulateZ1Trace nn42`, `nn37`, `nn56`, `nn51` | query | join traces: Customer, Orders, Orders ⋈ Customer, Lineitem | absent |
| `AccumulateZ1Trace nn65` (native), `nn51` (reconstructed) | query | V | V |
| `q3_manual_merged_index` | merged index | absent | merged index |

### Large-group (observations; all repetitions identical)

| | Native | Reconstructed |
| --- | --- | --- |
| Writes, base relation | 2 ops, 250,368 B | 2 ops, 250,368 B |
| Writes, query | 4 ops, 515,584 B | 1 op, 330,752 B (V) |
| Writes, merged index | 0 | 69 ops, 1,590,272 B |
| Join state on disk at window end | 184,832 B (Customer trace in memory) | 1,590,272 B |
| V on disk | 330,752 B | 330,752 B |

- **H2 confirmed.** The merged-index compaction output is written as one 1,048,576-byte flush
  followed by 68 small ones (60 of 8,192 B, 4 of 512 B, 1 of 4,608 B). Every other file in every
  run is written in a single flush. The bug inflates operations, not bytes.
- **H1 confirmed.** The merged index compacts its 8 runs during the first RF1: 12,625 records in,
  12,625 out, written to disk. Every native trace compacts at 8 runs too, but one refresh later,
  so its compaction sees RF1 and RF2 together: 22,048 records in, 2,048 out.
- **Window-end contents.** The merged index holds 42,628 records in 4 runs, 20,002 of them with
  negative weight, against 2,624 consolidated rows. Its one on-disk run is the 12,625 records
  written during the first RF1, including the 10,000 RF1 Lines that RF2 later deleted.

### Clustered and scattered (observations)

Results vary between repetitions, as before, because native compactions sometimes finish after
the window closes ("late"). The merged index writes the same 327,168 B in all six reconstructed
runs.

| | Native (3 reps each) | Reconstructed (3 reps each) |
| --- | --- | --- |
| Merged index on disk | — | 327,168 B: 2,704 records, 2,624 consolidated rows |
| Join traces on disk | 184,832 or 188,928 B | — |
| V on disk | 283,136 B | 291,840 B |

### Record size (observation, then deduction)

In clustered, where the on-disk merged-index run is nearly consolidated, data blocks take
314,880 B for 2,704 records, about 116 B per record. The native join traces' data blocks take
178,688 B for 3,072 rows: Lineitem 67.5 B, Orders 27 B, and Orders ⋈ Customer 52 B per row. So
the merged index is about 1.75 times the size of the native join state on disk here, although it
holds fewer rows. This contradicts the expected saving.

Deduction: once H1 and H2 are fixed, record size is the remaining gap. Its cause is under
investigation.
