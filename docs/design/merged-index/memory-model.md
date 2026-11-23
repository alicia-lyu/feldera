# The memory model: RSS pressure against budgets

**Status:** designed 2026-11-23; built 2026-11-23 (`--memory-model rss|budgets`, measurement version 20; the charge sub-arm, version 21, with its cap and estimate decided 2026-11-23 as good enough for now; the compaction pool, `--compaction-memory pool|charge` and `--compaction-jobs`, version 22, the default sub-arm from 2026-11-23). Decision: [2026-11-23](decisions.md#2026-11-23-the-memtable-dimension-becomes-the-memory-model-rss-pressure-against-budgets). The reading that motivates it is in [MEASUREMENT.md](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/MEASUREMENT.md#memtable-at-512-mib-2026-11-20). The memtable itself is in [memtable.md](memtable.md).

**Goal.** One knob, `StorageOptions::memory_model`, chooses how DBSP decides where a new run goes:

- **`rss`** (today, the default): Feldera's four pressure levels, computed on process RSS, place every run.
- **`budgets`**: two budgets, the buffer cache for reads and the memtable for writes, and a fixed rule for every other run. A sub-arm, `compaction_memory`, bounds compaction's working memory: `pool` (the default) limits the merges open at once to `compaction_jobs`, as RocksDB's `max_background_jobs` does, and `charge` charges compaction's buffers to the cache's budget.

Both modes of the Q3 experiment run under the same model, so the comparison stays fair.

**Terms.**

- *Pressure*: the level (`low`, `moderate`, `high`, `critical`) that the `rss-monitor` thread computes once a second from RSS against `max_rss_bytes`, at 85, 90 and 95% (`raw_memory_pressure`, `runtime.rs`).
- *Placement*: whether a new run is built in memory or as a file.
- *Step run*: a run passed between operators within one step. It is dropped when the step ends.
- *Accumulator spine*: a spine with `TraceRole::Accumulator`. It holds one transaction's delta and never spans transactions.

## Why: two controls relieve the same memory

With the memtable on, an integral insert never reaches pressure: `Spine::insert` puts it into the memtable before `spill_if_due`. But the memtable's bytes and the cache count in RSS. A cache sized near the target therefore reaches `critical` once it is full. Pressure then writes runs that the memtable never held, each as a file of about 2 KiB:

| Run | What pressure does today (`runtime.rs`, `fallback/utils.rs`) |
| --- | --- |
| step run | `min_step_storage_bytes` = 0 at `critical` |
| accumulator insert | `min_insert_storage_bytes` = 0 from `high` |
| compaction output | `min_merge_storage_bytes` = 0 from `moderate`; at `low`, a file only from 10 MiB |
| memtable seal | the merge rule (`pick_merge_destination`) |
| merge scheduling | from `high`, `must_relieve_memory_pressure` forces merges, and the merger loop wakes on `memory_pressure_notify` (`spine_async.rs`) |

**Compaction fills the cache.** The writer caches every block it writes (`from_raw_with_cache`, `writer.rs`), and a merge's reads insert on a miss (`DataBlock::new`, `reader.rs`). With the defaults, S3-FIFO and `SharedPerWorkerPair`, the merger threads (`ThreadType::Background`) and the worker share one cache. In the v18 c448 arm, compaction wrote about 377 KiB per transaction through that cache.

## What RocksDB does

Sources checked on 2026-11-23:

- [`table.h`](https://raw.githubusercontent.com/facebook/rocksdb/main/include/rocksdb/table.h): `prepopulate_block_cache`, `cache_usage_options`;
- [`write_buffer_manager.h`](https://raw.githubusercontent.com/facebook/rocksdb/main/include/rocksdb/write_buffer_manager.h);
- [`cache.h`](https://raw.githubusercontent.com/facebook/rocksdb/main/include/rocksdb/cache.h): `CacheEntryRole`;
- [`options.h`](https://raw.githubusercontent.com/facebook/rocksdb/main/include/rocksdb/options.h);
- [`compaction_job.cc`](https://raw.githubusercontent.com/facebook/rocksdb/main/db/compaction/compaction_job.cc): `InitializeReadOptionsAndBoundaries`;
- the wiki pages [Memory usage](https://github.com/facebook/rocksdb/wiki/Memory-usage-in-RocksDB), [Write Buffer Manager](https://github.com/facebook/rocksdb/wiki/Write-Buffer-Manager) and [Remote Compaction](https://github.com/facebook/rocksdb/wiki/Remote-Compaction).

| Concern | RocksDB |
| --- | --- |
| Compaction reads | `read_options.fill_cache = false` for compaction input. `fill_cache` decides whether a block read is "placed in block cache". Whether a compaction still uses a block that is already cached was not confirmed from source. |
| Compaction and flush outputs | "On a compaction, output SST files are written to disk but not placed in the block cache by default." `prepopulate_block_cache` is `kDisable` by default; `kFlushOnly` warms flush outputs, and `kFlushAndCompaction` also warms compaction outputs, at the lowest priority. |
| Memtables | `WriteBufferManager(buffer_size, cache, allow_stall)` bounds every memtable together. Given a cache, it puts "dummy entries in the cache and cost[s] the memory allocated to the cache", so the two share one number. |
| Other memory charged to the cache | `cache_usage_options` by `CacheEntryRole`. Charged by default: `kCompressionDictionaryBuildingBuffer`. Opt-in: `kFilterConstruction`, `kBlockBasedTableReader`, `kFileMetadata`, and `kReadaheadBuffer`, the prefetch buffers of iterators and compaction. A compaction's iterator blocks and output table builder have no role. |
| Who does the work | Flushes and compactions are background jobs (`max_background_jobs`). Writes slow down or stop when level 0 or pending compaction bytes pass their triggers. |
| Compaction elsewhere | `CompactionService` (experimental) runs a job "in a different process or even on a remote host"; the primary installs its SST files. |
| A global switch on RSS | none |

## What is reused

| Piece | Where | Role under `budgets` |
| --- | --- | --- |
| The memtable and `MemtableBudget` | `trace/ord/memtable/`, `circuit/memtable_budget.rs` | The write budget, unchanged. `budgets` requires `memtable_mib`. |
| The buffer cache and its occupancy | `storage/buffer_cache/cache.rs`, `crates/buffer-cache` | The read budget, unchanged. |
| The four thresholds | `Runtime::min_merge_storage_bytes`, `min_insert_storage_bytes`, `min_step_storage_bytes` | They stay the only placement hooks; under `budgets` they return fixed values. |
| `pick_merge_destination`'s hint | `fallback/utils.rs` | A seal passes `Some(BatchLocation::Storage)` (`MergeWorkers::seal_memtable`). |
| The thread-local owner pattern | `with_read_owner`, `with_write_owner` (`storage/attribution.rs`) | A thread-local cache-fill policy follows the same pattern. |
| The `FilterResidency` option | `feldera-types` `config.rs`, `Runtime::filter_residency` | The model is an enum option read the same way. |
| The `rss-monitor` | `runtime.rs` | It still samples RSS and reports the level; only its consumers change. |

## The bridging pieces

1. **The option.** `StorageOptions::memory_model: MemoryModel { Rss, Budgets }`, default `Rss`, `compaction_memory: CompactionMemory { Pool, Charge }`, default `Pool`, and `compaction_jobs: Option<usize>`, the pool's limit, default 2. `Runtime::memory_model()` reads the first, as `filter_residency` is read. Startup refuses `Budgets` without `memtable_mib`, `Charge` without `Budgets`, `compaction_jobs` without `Budgets` or under `Charge`, and `compaction_jobs = 0`. Under `rss` neither sub-arm applies: compaction is unbounded, as it always was. `openapi.json` is edited by hand, as it was for `memtable_mib`.
2. **Placement under `budgets`.**
   - `min_merge_storage_bytes` returns `Some(0)`: compaction outputs and seals are always files.
   - `min_step_storage_bytes` and the accumulator's insert threshold return `usize::MAX`: these runs stay in memory, bounded by the transaction.
   - An integral batch that bypasses the memtable (above a quarter of the budget) is written as a file. This needs the spine's role in `spill_if_due`, which the spine already has.
3. **Scheduling under `budgets`.** Merge eligibility and the merger's wake-ups read an *effective* level that is always `low`. The raw level is still reported (`current_memory_pressure`), so a run can show what `rss` would have done.

## What is genuinely new

**(a) A cache-fill policy for compaction.** A thread-local `CacheFill { Fill, NoFill }`, set by `with_cache_fill(NoFill, f)` around a merge or a seal on the merger thread.

- The reader still looks up the cache, so an already cached block is used.
- On a miss, it builds the block without inserting it (`DataBlock::from_raw` in place of `from_raw_with_cache`, likewise for index blocks).
- The writer skips its inserts (`writer.rs`, data and index blocks).
- Sealed memtables follow RocksDB's default, `kDisable`, so seals do not warm the cache either; warming would be a later arm, as `kFlushOnly`.
- Trailers and filter partitions keep today's path; they are small, and the trailer is read back at close.

**(b) The pool (default sub-arm, decided 2026-11-23).** Compaction has its own pool with a primitive limit, as RocksDB's `max_background_jobs` (default 2) bounds its compactions. The user proposed it as the simpler way to bound compaction's memory: about the limit times one job's buffers, with no charge to the cache and no wait ordering to get right.

- **Why it is needed.** Nothing else bounds the open merges. With one worker the merger runtime has one thread, but each spine level runs its own merger task (`AsyncMerger::run`), and a merge holds its input cursors and output builder across many steps. Many merges can therefore be open at once.
- **The pool** is one `tokio::sync::Semaphore` per runtime of `compaction_jobs` permits (`CompactionJobs`, `circuit/compaction_jobs.rs`), made beside the memtable budget. Tokio's semaphore serves waiters first-come, first-served, so no merge starves.
- **Where a merge waits** follows the charge's pattern. `merge_step` takes the merge's inputs, then returns `MergeStep::NeedsPermit`; the merger loop awaits the permit, so the wait holds no thread, and hands it back before the merge opens its cursors and builder. The `Merge` keeps the permit until its output is closed, and releases it on drop, so it returns however the merge ends.
- **Seals are outside the limit** (decided 2026-11-23), as RocksDB's flushes have their own high-priority pool. A spine seals at most one memtable at a time.
- **Counters** (`Runtime::compaction_job_stats`; the Q3 refresh report's `compaction_jobs`): merges that waited, their wait time, and `peak_open_merges`, the most merges open at once since the refresh began. The peak is counted under every model, `rss` included, so the unbounded concurrency of a run without the pool is measured, not inferred.

**(c) The charge (alternative sub-arm).** A merge or seal reserves its buffer bytes in the cache's budget for the length of the job:

- one block per input cursor;
- the output builder's open data and index blocks and its filter;
- the writer's buffer.

**A reservation is a pinned dummy entry, as in RocksDB.**

- **RocksDB.** `CacheReservationManagerImpl` keeps the handles of its dummy entries (`dummy_handles_`) until it releases the reservation (`cache/cache_reservation_manager.h`). A referenced entry is not evicted: per `cache.h`, an insert must fit "along with all the existing referenced (pinned) cache entries".
- **Why pinning is required.** A dummy entry the cache could evict would end the charge while the job's real buffers stay allocated. The cache would then count less than the process holds, which is the very gap the charge exists to close.
- **In DBSP.**
  - A dummy is a `CacheEntry` with the reserved cost and no data, under a key space no file uses.
  - S3-FIFO pins it through `quick_cache`'s `Lifecycle::is_pinned` (0.6.19: "Items that are pinned can't be evicted"), which `s3_fifo.rs` does not use today.
  - The LRU backend's `evict_to` (`lru.rs`) skips pinned entries.
  - The reservation is released when the job ends, including on error, through a guard's `Drop`.
- **Reserve, then allocate.** The job reserves its estimated buffer bytes before it opens its cursors and builder. The cache evicts unpinned blocks to make room, so compaction takes its memory from the cache's budget at the cache manager's discretion: it does not add to RSS beside it.

**When the reservation does not fit: the job waits** (decided 2026-11-23, against RocksDB's default overshoot, where "Insert() never fails").

- The cache refuses a reservation that would push its pinned bytes past a cap. The merge or seal then waits until another reservation is released.
- A job larger than the cap alone is admitted once no other reservation is held, so it cannot wait forever.
- A merge is background work, so waiting is cheap. A seal that waits lets the memtable overshoot its soft cap, as it may today, since inserts never stall.
- So the cache manager is the admission control for compaction.
- Reported: the reservations' wait time and the peak of pinned bytes.
- The cap is a quarter of the cache's capacity (decided 2026-11-23, good enough for now).

**Built 2026-11-23 (measurement version 21; now `compaction_memory: charge`, version 22).** Refused without `Budgets`: `Error::CompactionChargeNeedsBudgets`.

- **The pin.** `CacheEntry::is_pinned` (default false). S3-FIFO passes it to `quick_cache` through a `Lifecycle` (`PinnedEntryLifecycle`); the LRU backend's `evict_to` skips pinned entries. Pinned bytes count in the cache's charge, so the cache evicts blocks around them. A pinned entry heavier than the rest of its shard makes the shard exceed its capacity, since `quick_cache` then has nothing left to evict.
- **The reservation.** `BufferCache::try_reserve` and `reserve` (`storage/buffer_cache/cache/reservation.rs`). A reservation is split into pinned entries of at most a sixteenth of a shard (confirmed 2026-11-23), so the hash spreads it over the shards; its keys use a fresh `FileId`, which no file has, so `evict_files` and per-file eviction never touch it. The ledger is per cache backend (handles that share a backend share it). Dropping the `Reservation` removes its entries and wakes the waiters.
- **The cap** is a quarter of the cache's capacity (`RESERVATION_CAP_DIVISOR`; decided 2026-11-23, good enough for now). A request that would pin more waits; a request is admitted when nothing is reserved. A large job can wait behind a stream of small ones: the wait is not first-come, first-served, so starvation is possible. Decided 2026-11-23: accepted for now, with no scheduler, since the charge is only one arm. Counters per cache backend: waits, wait time, peak pinned bytes (`ReservationStats`; the Q3 refresh report's `compaction_reservations`).
- **The wait is an `await`.** The merger runs each merge step and seal as a tokio task, so a blocking wait would hold a worker thread, and with it a runnable merge. A merge takes its inputs from the spine, then asks the loop for a reservation (`MergeStep::NeedsReservation`), which the loop awaits and hands back before the merge opens its cursors and builder. A merge holds its reservation from then until its output is closed, across all its steps, so a merge step does not re-reserve per slice of fuel. A seal reserves once, before `seal_memtable`, and releases when it ends. A job holds at most one reservation and waits only while holding none, so waiting cannot deadlock.
- **The estimate** (`compaction_buffer_bytes`, `spine_async/compaction_charge.rs`), from the writer's parameters (`Runtime::file_writer_parameters`):
  - one data block per column (2) of each input run's cursor, at `min_data_block`;
  - the output builder's open data block and 3 open index blocks per column, at `min_data_block` and `min_index_block`;
  - the writer's buffers, 2 data blocks;
  - the key filter, sized for all input keys at the configured false positive rate (`-ln(rate) / ln(2)^2` bits per key).

  The column count, the 3 index levels, and the 2 writer blocks are assumptions, not measurements. It omits the input runs' index blocks along a cursor's path, which the cache holds as ordinary blocks, and the transient allocations of one key.

**(d) Compaction on its own axis.** The Q3 report gains the merger threads' CPU and device time per refresh, beside `ivm_ms` and `base_ms`, if `run_counts` does not already give them.

## The Q3 arm

- `--memory-model rss|budgets`, `--compaction-memory pool|charge` and `--compaction-jobs N` in the driver and `measure.py`, recorded in `configuration` (`compaction_memory` and `compaction_jobs` are null under `rss`).
- The pilots: `rss` against `budgets` (pool, then charge), with the memtable on, in both modes; then scattered.
- The change touches `spine_async.rs` and `storage/file/`, both image sources, so new images and a new measurement version.

## Tests

- **Placement under `budgets`:**
  - at mock `critical` pressure, a step run and an accumulator insert stay in memory, and an integral insert goes into the memtable;
  - at `low`, a merge output and a seal are files.
- **Scheduling:** at mock `high` pressure under `budgets`, no forced merge.
- **Cache fill:** a merge under `NoFill` leaves cache occupancy unchanged, yet uses a block that was already cached (a hit counted).
- **Startup:** `Budgets` without `memtable_mib` is refused, and so are the charge without `Budgets`, `compaction_jobs` without `Budgets` or under the charge, and `compaction_jobs = 0`.
- **The pool:** with the only permit held, a merge waits and starts after the release; no permit is taken under the charge or `rss`; a seal proceeds while the pool is full; `peak_open_merges` reaches 2 without the pool.
- **Q3:** `transcripts_do_not_depend_on_the_memtable` gains a twin, `transcripts_do_not_depend_on_the_memory_model`, in both modes.

## Open questions

- Whether the fixed up-front estimate needs correcting: record each job's real peak buffer bytes beside its estimate, or charge as memory grows, as RocksDB's dynamic charge does. The fixed estimate is decided as good enough for now (2026-11-23).
- Whether step memory needs its own cap, once K = 1,000 or the TPC-H batch is measured.
- Whether `budgets` should warm the cache with seal outputs (`kFlushOnly`).
