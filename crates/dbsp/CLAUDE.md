# DBSP fork changes (`crates/dbsp/`)

This crate is upstream Feldera's DBSP. The items below are fork additions outside `src/trace/merged_index/`
(see [that directory's CLAUDE.md](src/trace/merged_index/CLAUDE.md)). Native DBSP background (runs,
levels, placement, snapshots): [dbsp-background.md](../../docs/design/merged-index/dbsp-background.md).

## Several circuits in one runtime

- `Runtime::init_circuits(config, constructors)` builds one `DBSPHandle` per `CircuitConstructor` in
  one runtime: one storage directory, one worker thread set, one buffer cache. Commands that concern
  the runtime rather than a circuit report the same answer from every handle.
- `DBSPHandle::checkpoint_circuits` checkpoints all circuits as one checkpoint. Each circuit's
  persistent ids carry its index (`-c0-`, `-c1-`); the handles share one checkpointer; the
  fingerprint combines the circuits' in order. A single handle's `checkpoint()` refuses such a
  runtime. Restoring from a checkpoint of other circuits is an error, as is a deferred restore.
- Each generated crate exports `<circuit>::SqlCircuit::build` (see
  [`SQL-compiler/CLAUDE.md`](../../sql-to-dbsp-compiler/SQL-compiler/CLAUDE.md)).

## Shared buffer cache

The circuits share the runtime's buffer cache (`Runtime::foreground_buffer_cache`), tested by
behavior and not only by pointer equality, in `dbsp_handle.rs`:

- `init_circuits_read_each_others_cached_blocks`: a file one circuit's step wrote is read by the
  other's step with foreground hits and no misses.
- `separate_runtimes_miss_each_others_blocks`: its control; two runtimes miss. Test-only.
- Also `init_circuits_share_the_buffer_cache`, `init_circuits_steps_circuits_independently`, and
  `init_circuits_restore_each_circuit_from_one_checkpoint`.

## `run_reads()` is a per-thread total

`DBSPHandle::run_reads()` returns the worker thread's `RunReads` per read owner. The handles of one
runtime share the worker thread, so every handle reports the same answer
(`init_circuits_report_the_same_run_reads_on_every_handle`). A caller isolates one circuit by taking
the total around that circuit's transaction, when only it runs.

## Join with supplied snapshot readers

`dyn_join_generic_with_snapshot_readers` (`src/operator/dynamic/join.rs`, typed wrapper in `mono.rs`)
is a join that takes supplied snapshot readers instead of its own traces. Its delta drives; it does
not sample the partner. Its calls on the partner batch, and its reader callback, are counted under
the `RequestedSlice` read owner. Tests: filter `supplied_snapshot_join` (also covers a delta that a
source emits at the flush).

## Run counters

- `RunReads` (`src/trace/cursor/run_reads.rs`): per-thread seeks, filtered probes (exact probes that
  a file run's filters answer with no seek), and key, value, and weight decodes of file runs. In-memory
  runs have a separate set, `memory_*` key/value/weight reads, so a memory access is never counted as
  a deserialization. Counted in `VecIndexedWSetCursor` and `FileIndexedWSetCursor`.
- Laziness rules (both cursors): a landing reads the key only; the first read of a value or weight at
  a position reads and counts the value (the memory cursor keeps a `Cell<bool>`); a reverse seek that
  lands on a file row counts two decodes, since the reader deserializes it twice (decision of
  2026-11-16).
- Attribution: `with_read_owner(Component, f)` runs `f` with reads charged to a `Component`
  (`storage/attribution.rs`): `Query` (default), `BaseRelation`, `MergedIndex`, `RequestedSlice`,
  `PartnerTrace`. The last two are read owners only (no writes). `ReadOwnerCursor`
  (`src/trace/cursor/read_owner_cursor.rs`) wraps a join's partner cursor to charge it to an owner;
  a native join charges its partner trace cursor to `PartnerTrace`.
- `BlockExpansions` (`src/trace/cursor/block_expansions.rs`, measurement version 12): the rebuild of
  a prefix-truncated or elided data block by `decompress` on a cache miss. It is not an item decode:
  it counts blocks, encoded bytes, and expanded bytes per thread and read owner, apart from
  `RunReads`. `DBSPHandle::block_expansions` reports it.

## Storage and layer-file changes

- Write attribution by owner and opt-in write diagnostics (`storage/attribution.rs`); layer-file
  block bytes split by level; `block_stats` totals derived from the levels; the POSIX writer resets
  its buffered length after each flush.
- Prefix truncation of byte-string keys in layer-file data blocks (`front_code_byte_keys` dev
  tweak) and elision of byte-string value headers (`elide_value_headers`, a separate transform).
- `PackedField`, `PayloadEncoder`, and `PayloadDecoder` for merged-index payloads (see the
  merged-index CLAUDE.md).

## Storage-resident Bloom filters

`StorageOptions::filter_residency` (`feldera-types`), default `storage`, read through
`Runtime::filter_residency` (memory off a runtime thread):

- The writer builds a `PartitionedBloomBuilder` (`storage/file/filter/partitioned.rs`): a blocked
  Bloom filter whose block is one 4 KiB partition, chosen by the key hash's high bits. `close`
  writes the partitions back to back, then a `PartitionedBloomFilterHeader` block
  (`INCOMPATIBLE_FEATURE_PARTITIONED_FILTERS`), and hands the batch a `StoredPartitionedBloom`.
- A probe reads one partition through the thread's buffer cache (`FilterPartition` entries), so
  an unprobed filter costs no memory. An unreadable partition answers "might be present".
- A filter that fits in one partition (fewer than about 1,700 keys at the default rate) stays a
  memory-resident modular filter (`BatchKeyFilter::new_bloom`): storing it would cost a 4 KiB
  write per small run to save at most 4 KiB.
- `memory` keeps the modular filters. Roaring filters are always memory-resident. Files of either
  form load under either setting.
- `FilterStats` gained `stored_byte`, `partition_loads`, `partition_cache_hits`; the spine profile
  reports `bloom_filter_stored_bytes` and the partition counts.
- Tests: `storage::file::test::storage_resident*`, `corrupt_partition_admits_every_key`,
  `memory_resident_file_loads_under_storage_residency`, `partitioned`, and the four
  `trace::test::file_*_filter_follows_the_rate` (memory ladder plus a storage reload). A test of
  the cache must use one whose 256 shards exceed a partition: `test_buffer_cache`'s 4 KiB shards
  keep none.

## Prefix filters

A file can carry a filter over the distinct prefixes of its keys (`storage/file/filter/prefix.rs`,
decision of 2026-11-19 in `docs/design/merged-index/decisions.md`):

- **The spec.** `FilterSpec { whole_key, prefix }` lives in `FileIndexedWSetFactories`
  (`with_filter_spec`; the default is whole-key only, as before), so a spine's builds, merges,
  and `from_path` reloads all use it. `PrefixSpec::bytes::<N>()` (byte keys; `bytes_of_length`
  at run time) and `PrefixSpec::fields::<TupN, K>()` (`TuplePrefix`, Tup1 to Tup4) carry the
  prefix hash and comparison. Only indexed W-sets write prefix filters so far.
- **The file.** `Writer2::with_prefix_filter` collects one hash per distinct prefix (keys are
  sorted). `close` writes an `LFXH` header (`PrefixBloomFilterHeader`): at most
  `INLINE_PREFIXES` (120) hashes inline, kept in memory; above that a partitioned Bloom filter
  sized by the exact prefix count, read through the buffer cache. Its location is in
  `BatchMetadata::prefix_filter_offset`/`_size`, flagged by `INCOMPATIBLE_FEATURE_PREFIX_FILTERS`.
  `Reader::prefix_filter(kind)` refuses a file whose prefix differs from the spec's.
- **The probe.** `Batch::maybe_contains_prefix(probe, spec)`: key bounds by prefix, then the
  prefix filter (file runs count a rejection in `RunReads::filtered_prefix_probes`). The
  merged index's `RunMerge::seek_group` parks the runs that cannot hold the group;
  `SpineSnapshot::retain_prefix` keeps only the runs that may hold a prefix, for any spine.
- Tests: filters `prefix` (unit tests), `run_merge` (`group_seeks_with_prefix_filters_*`,
  `a_run_without_the_group_is_parked_and_counted`,
  `a_partitioned_prefix_filter_reloads_and_refuses_another_prefix`), and `prefix_tests`
  (native tuple keys with both filters).

## Spine memtable (phase 1)

A memtable on integral spines when `StorageOptions::memtable_mib` is set (design and phases:
[memtable.md](../../docs/design/merged-index/memtable.md)):

- **The tree.** `trace/ord/memtable/`: a B+-tree of general height with `Arc` nodes, copied on
  write (Rodeh 2008) only while a frozen memtable shares them. Leaves are `VecIndexedWSet`-layout
  columns (`IndexedLeaf`) or keys and weights (`WSetLeaf`), split at `TreeShape::leaf_bytes`
  (8 KiB); a split may repeat a key in the next leaf. `Vector::insert_val`, `insert_ref` and
  `remove_at` (`dynamic/vec.rs`) do the in-leaf shifts. Leaf memory is counted at allocation and
  released in `Drop`, so copies a snapshot still holds count.
- **As a batch.** `Inner::Memtable` in the fallback W-set and indexed W-set reads a
  `FrozenMemtable` in place; `Batch::new_memtable` returns a `BatchMemtable` only for those two.
  The memtable cursor counts reads as the Vec cursor of its kind does (indexed: yes; W-set: none).
- **In the spine.** `SharedState` holds the active memtable and the flushing one, under the one
  state lock. `get_batches` appends the flushing memtable and a frozen memtable of the active one
  (lazy freeze). `Trace::insert` puts a batch into the memtable unless it exceeds a quarter of
  the budget; save, `consolidate`, `complete_merges` and `initiate_compaction` first move the
  memtable in as an in-memory run (`memtable_into_run`).
- **The budget.** `circuit/memtable_budget.rs`: one `MemtableBudget` per runtime
  (`Runtime::memtable_budget`), a soft cap. Over it, the largest memtable that is not flushing is
  asked to seal; the seal runs on the merger runtime (`MergeWorkers::seal_memtable`), copies the
  memtable through `ListMerger` into a file where `pick_merge_destination` says storage, and logs
  `RunWriteKind::MemtableFlush`. Memtable bytes count in RSS; memory pressure is unchanged.
  `Runtime::memtable_live_bytes` reports the live total (test
  `memtable_live_bytes_counts_an_integral_insert`).
- **Counters.** `SpineRunCounts::memtable_*` and `flushing`; metrics `MEMTABLE_FLUSHES` and
  `MEMTABLE_STALL_NANOSECONDS` (inserts never stall, by decision of 2026-11-20, so the latter stays zero).
- Tests: filter `memtable` (tree, leaves, cursor conformance, budget, and
  `trace::spine_async::memtable_test`).

## Evicting a set of files from the buffer cache

`BufferCache::evict_files(&HashSet<FileId>)` removes every entry of those files (blocks, trailers,
filter partitions) in one pass, regardless of the `eager_evict` dev tweak, and returns the cost freed
(test `evict_files_removes_exactly_the_named_files`). The Q3 driver uses it to evict the base
tables after each base transaction; `SerBatch::file_reader` (`adapterlib`) gives a catalog batch's
file.

## Compaction counters

- `diagnostics::spine_run_counts()`: every live spine's runs per level (loose and merging), file
  runs, completed merges and merge time, read under the spine's lock without touching storage.
  Every spine registers (`register_run_counter`), whether or not diagnostics are on.
- `attribution::read_snapshot()`: device reads by reading thread (foreground, background, other),
  split once by read owner and once by write owner (`ReadCharge`, recorded in `PosixReader`).

## Test filters

From the repository root, `cargo +1.96.1 test --offline -p dbsp --lib <filter>`:

- `merged_index` and its sub-filters (see the merged-index CLAUDE.md);
- `memtable` (the spine memtable);
- `supplied_snapshot_join`, `run_reads`, `read_owner_cursor`;
- `init_circuits`, `separate_runtimes_miss_each_others_blocks`.
