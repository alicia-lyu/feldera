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
- Each generated crate exports `<circuit>::SqlCircuit::build` (see the root `CLAUDE.md`).

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

## Test filters

From the repository root, `cargo +1.96.1 test --offline -p dbsp --lib <filter>`:

- `merged_index` and its sub-filters (see the merged-index CLAUDE.md);
- `supplied_snapshot_join`, `run_reads`, `read_owner_cursor`;
- `init_circuits`, `separate_runtimes_miss_each_others_blocks`.
