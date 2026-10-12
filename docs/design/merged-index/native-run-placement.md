# Native run building, levels, placement, and memory pressure

This file records how native DBSP creates runs, assigns them to levels, and
decides whether each run lives in memory or in a file. It also records what
memory pressure changes. The claims were checked against the code on
2026-10-12 and cite symbols, not line numbers. Paths are relative to
`crates/dbsp/src/` unless stated otherwise.

The slide deck in [slides-oct-8/](slides-oct-8/README.md) depends on these
facts.

## Building a run

An operator builds each output run by appending already sorted
(key, value, weight) records to a builder. Sorting happens earlier, in
memory, on bounded chunks:

- Joins, aggregates, `distinct`, and similar operators collect output in a
  batcher. They seal a run every `splitter_output_chunk_size()` records
  (`circuit/dbsp_handle.rs`). The size comes from `splitter_chunk_size_records`
  and defaults to 10,000 (`feldera-types/src/config/dev_tweaks.rs`).
- One transaction can therefore produce one or more runs. A run never
  contains more than one transaction's output until a merge combines runs.
- Operators do not aim at any particular run size; merges build larger runs.

A builder writes to memory, to a file, or to memory up to a byte threshold
(`BuildTo` and `BuilderInner` in `trace/ord/fallback/indexed_wset.rs`). In the
threshold mode, the builder appends to memory arrays. Once the bytes reach the
threshold, `spill` copies the records so far into a file builder and keeps
appending there. This works without sorting again because records arrive in
order.

The threshold is `min_step_storage_bytes`, read through
`Runtime::min_step_storage_bytes` (`circuit/runtime.rs`). It defaults to
unlimited, so operators build runs in memory. Only at Critical memory pressure
does it drop to zero; nonempty runs are then written to a file from their
first record.

## Levels

A run's level depends only on its record count (`Spine::size_to_level` in
`trace/spine_async.rs`):

| Level | Records |
| --- | --- |
| L0 | up to 14,999 (`MAX_LEVEL0_BATCH_SIZE_RECORDS`, configurable) |
| L1 | up to 99,999 |
| L2–L8 | each level holds runs ten times larger than the one before |

- An inserted run goes straight to its level (`AsyncMerger::add_batch` with
  `merge=false`). It does not climb through L0.
- A level merges once it holds enough loose runs (`MERGE_COUNTS`,
  `min_batches_to_merge`): at least 8 runs at L0 and L1, and 3 (or 2 at
  L6–L8) deeper down.
- A merge output's level is derived again from its size. It can land back in
  the level it came from and be merged again. The code comment on
  `min_batches_to_merge` states the usual case: merging three runs stays in
  the level, and merging ten or more moves the result down one level.
- For merge outputs only, each negative-weight record counts as several
  records. Runs with many retractions therefore move down sooner and meet the
  insertions they cancel.

## Location: memory or file

Location is a separate, per-run decision made when a run is created. Level
does not determine it, so no level boundary divides memory from disk. One
level can hold memory and file runs together. The thresholds below assume
storage is configured; without storage, every run stays in memory.

- **Inserted runs.** `Spine::insert` calls `Spine::maybe_flush_batch`, which
  asks `pick_insert_destination` (`trace/ord/fallback/utils.rs`). Below
  Moderate pressure, a run stays in memory at any size. The merger decides
  later.
- **Merge outputs.** `pick_merge_destination` writes a file once the inputs
  total `min_storage_bytes` or more, defaulting to 10 MiB
  (`Runtime::min_merge_storage_bytes`). Smaller outputs stay in memory.

A memory run moves to disk only by being rewritten, through a merge or an
eager spill on insertion. Spilling writes a copy, so readers can keep the
memory original alive.

## Memory pressure

A runtime thread samples the process resident set size (RSS) about once a
second and compares it with `max_rss_bytes` (`Runtime::raw_memory_pressure`
and the `rss-monitor` thread in `circuit/runtime.rs`). Hysteresis is applied
to the result before use.

| RSS / `max_rss_bytes` | Pressure | Inserted run | Merge output | Early merge | New runs built |
| --- | --- | --- | --- | --- | --- |
| below 85% | Low | memory | inputs below 10 MiB: memory; 10 MiB or more: file | no | in memory |
| 85% or more | Moderate | below 10 MiB: memory; 10 MiB or more: file | file | no | in memory |
| 90% or more | High | file | file | yes | in memory |
| 95% or more | Critical | file | file | yes | straight to file |

**Early merge.** From High up, `Slot::must_relieve_memory_pressure` lets any
level holding a memory-resident loose run start a merge at once, even with one
run. The merge output goes to a file, so this moves memory runs to disk.

Memory pressure does not:

- Resize the block cache.
- Pause connectors. The adapter controller only reports it in statistics.
- Control the 128-loose-run backpressure (`LooseBatchCount`), which applies
  independently of pressure.

## Memory components

The memory that runs use is the same kind any LSM tree uses:

- **Memory runs**, held whole. This includes obsolete runs that snapshots
  still reference after a merge.
- **Merge output under construction.**
- **Block cache** for file runs (`storage/buffer_cache`). Readers fetch file
  blocks through it, so a snapshot that pins a file run pins the file, not its
  bytes in memory.
- **Key filters and index metadata** for file runs (`storage/file/reader.rs`).
  Modular Bloom filters can be partly resident.

The difference from a standard LSM is the absence of a fixed memtable budget,
not a different set of components. RocksDB, for example, bounds its memory
part with `write_buffer_size`. DBSP instead lets sealed runs stay in memory
under the thresholds above and uses process RSS to tighten placement.

## Answers from the 2026-10-12 slide review

These answer points raised while revising the deck. Each answer follows from
the sections above.

- **An inserted run does not have to fit in memory as one sorted unit.**
  Operators sort in memory in chunks of about 10,000 records and emit one run
  per chunk; merges do the rest of the sorting. The builder's spill to a file
  is an emergency valve, off by default, not a sizing rule.
- **The builder's spill is for output that never passes through a sort.**
  Many operators stream already sorted inputs, possibly file runs, into a
  builder. `filter_map` walks its input with a cursor
  (`operator/dynamic/filter_map.rs`). `distinct` walks the delta and the
  integral trace together (`operator/dynamic/distinct.rs`). Such output can
  exceed memory, and the builder holds its only in-memory copy. Operators that
  do sort, such as joins through a batcher (`trace/ord/merge_batcher.rs`),
  sort in memory with no explicit budget. Only chunking bounds the sort; the
  RSS controller reacts afterwards. Merges choose memory or file before they
  start (`pick_merge_destination`) and do not use the threshold builder.
- **"Building" and "inserting" are separate steps.** Building means appending
  sorted records to a builder. Inserting means handing the finished run to a
  trace, which may copy a memory run to a file.
- **An inserted run can start below L0.** It enters the level its record count
  dictates; a run with five million records starts in L3.
- **A merge output can stay in its input level.** It rejoins that level as a
  loose run and is merged again later.
- **A merge output can stay in memory at any level,** provided its inputs
  total under 10 MiB and pressure is Low. In practice deep levels are files:
  L4 runs hold at least ten million records, far above 10 MiB.
- **No boundary separates memory levels from disk levels.** Record count sets
  the level (the spine's per-level slots); byte size and pressure at creation
  set the location. A diagram should not tie location to level.
- **DBSP's "L0" does not mean "in memory".** In DBSP, L0 means up to 14,999
  records, and any level can hold memory runs. In the standard LSM numbering
  the deck uses, L0 is the in-memory memtables and L1 is the first on-disk
  level. RocksDB and LevelDB instead number their first on-disk level L0.
- **Memory pressure affects four things:** inserted-run placement, merge-output
  placement, early merges, and building directly to files at Critical.
- **An early merge** starts a merge in a level holding a memory run without
  waiting for the level's usual minimum run count.
- **"Memory if inputs < 10 MiB" and "file if ≥ 10 MiB"** are byte thresholds on
  a run's size. They decide whether a merge output or an inserted run is
  written to a file.
- **A run referenced by a snapshot is held whole only if it is a memory run.**
  A file run costs only the blocks read through the block cache, plus filters
  and metadata.
- **RSS** is the physical memory the operating system currently maps to the
  process. The metric is generic; DBSP's peculiarity is using it to choose
  where runs go.
- **Base tables, traces, and views are not memory categories.** Each is a trace
  with the same components listed above. Connector buffers, thread stacks, and
  allocator overhead are ordinary process memory, not LSM memory.
