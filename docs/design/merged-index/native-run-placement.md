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

A file builder (`FileIndexedWSetBuilder` over `Writer2` in
`storage/file/writer.rs`) is a streaming, bottom-up B-tree writer:

- It keeps a key tree and a value tree. `push_val` appends a (value, weight)
  row to the value tree (`write1`). The key's `push_key` follows its values
  and appends (key, ()) with the range of value rows just written (`write0`).
- Each tree keeps one open data block (`DataBlockBuilder`; `min_data_block`
  defaults to 8 KiB before compression and must be at least 4 KiB). A full
  block is compressed (Snappy by default), padded to a multiple of 512 bytes,
  and written, and its
  summary goes into the open index block one level up (`index_blocks`). Index
  blocks fill and propagate upward the same way. Nothing is revisited, so
  memory stays at about one block per tree level, whatever the run size.
- Index blocks record each child's file offset and on-disk (compressed)
  size in 512-byte units (`get_child_location` in `storage/file/reader.rs`).
- `close` flushes partial blocks, writes the index roots, the key filter
  (Bloom or roaring bitmap, the one structure that grows with the run), and
  the trailer. Each data block also enters the buffer cache as it is written
  (`from_raw_with_cache`), so a new file run starts warm.

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

## Merging

Each level has its own background merger task, started on demand
(`MergeWorkers` in `trace/spine_async.rs`). A merger wakes when a run is added
to its level or when memory pressure changes (`memory_pressure_notify`).

- **Regular merge.** A level starts a merge when it has no merge in flight
  and holds at least its minimum number of loose runs (`Slot::try_start_merge`,
  `MERGE_COUNTS`): 8 at L0 and L1, 3 at L2–L5, 2 at L6–L8. A merge takes the
  oldest loose runs, at most 128 at L0 and 64 elsewhere.
- **Early merge.** From High pressure up, `must_relieve_memory_pressure` lets
  a level start as soon as it holds any memory run, even one. Merge outputs
  go to files at that pressure, so an early merge mainly moves memory runs to
  disk.
- **Requested compaction.** A level asked to compact
  (`CompactionStatus::Requested`) merges all its runs regardless of count. The
  request then passes to the next level down. This file does not trace who
  issues these requests.

An L0 merge runs to completion at once. Deeper merges run in slices sized
like an average L0 merge and yield between slices. Inputs stay readable until
the merge completes. `merge_complete` derives the output's level again and
adds it as a loose run there, which can start the next merge.

The 128-loose-run backpressure does not start merges; it pauses foreground
insertion until the mergers catch up.

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

Recording location per run is deliberate. A run's location is its
representation: memory arrays or a file, the two variants of one batch type
(`FallbackIndexedWSet` in `trace/ord/fallback/indexed_wset.rs`). Runs are
immutable, so a new location means a rewrite. Merges rewrite runs constantly
as they move down the levels, and each rewrite applies the current policy.
Early merges actively rewrite memory runs under High pressure. Placement is
therefore lazy but converges.

At a fixed moment and pressure, location does split the levels, but by bytes
rather than by level. With records of about 100 bytes, 10 MiB is about 100k
records, near the L1/L2 boundary, so L0–L1 runs sit in memory and deeper runs
in files. Two kinds of run cross the line: a large inserted run stays in
memory at Low pressure until a merge rewrites it, and snapshots can pin
obsolete memory runs.

### The harness's override

The R5 harness's `runtime_config` (`sql-to-dbsp-compiler/experiments/q3-ivm-baseline/src/main.rs`) sets `min_storage_bytes: Some(0)`. It dates from commit `5ac3a8769` (2026-10-01) and was not recorded as a decision until 2026-11-09. It sets the merge threshold to 0, so every merge output goes to a file, however small, while inserted runs follow `min_insert_storage_bytes` and stay in memory at low pressure. Measurements through version 6 therefore ran with a placement that default DBSP never produces at low pressure.

The user decided on 2026-11-09 to use the default placement in both modes; see [decisions.md](decisions.md). The harness will drop the override (planned, with measurement version 7), and memory pressure will decide what reaches disk. `merged-index-col/src/storage_tests.rs` keeps its own setting because it builds file runs on purpose.

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
