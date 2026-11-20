# Q3 harness decisions

This file records the user's confirmed decisions for the Q3 harness (`q3-ivm-baseline/`) and its measurement: the Linux arms, the SSD and hard disk, images, inputs, the counters as measured, the smoke defaults, the `measure.py` gates, the hand-written Q3 view circuit, `delta_ms`, and the COL crate. Each entry is dated and sorted ascending.

Split rule (user decision of 2026-11-17): decisions are split by owner. Rules that apply to any merged index or IVM harness (the trie and spec syntax, encodings, the session, the readers, the one runtime, placement, and the process and terminology) are in the generic log, [decisions.md](../../../docs/design/merged-index/decisions.md). When in doubt, an entry stays there. A decision elsewhere without an entry in either file is unconfirmed.

Terminology: entries before 2026-11-11 say "front coding" for the key encoding of `--front-code-byte-keys`; the user renamed it "prefix truncation" on 2026-11-11. The code's identifiers keep the old name.

## Current decisions

### 2026-10-12: The compiler hook is the current approach

The `--q3-merged-index` compiler option is endorsed for now. It pattern-matches the exact Q3 circuit and emits the reconstructed joins. This supersedes the 2026-09-28 preference for hand-composed circuits (see [Superseded](#superseded-and-withdrawn)). *The hand-written circuit replaced the hook on 2026-11-10, and the hook was removed on 2026-11-13.*

An open question remains; see [Open questions](#open-questions).

### 2026-10-12: Drop the R1 measurement table

The R1 table in `q3-ivm-baseline/RESULTS.md` has no raw artifact behind it, and its results are not actionable. It is removed.

### 2026-10-21: Pin and retire the typed and rkyv value measurements

At roadmap Step 5, record the final typed (`SourcePayload`) and rkyv value-level bytes in [storage-layout.md](../../../docs/design/merged-index/storage-layout.md), then retire those parts of `layout_tests.rs`. Only the packed measurement remains. Generated types keep plain derives. This replaces the last sentence of the 2026-10-20 plain-derives entry.

Summing spine run bytes instead (`layer_file_block_bytes` per spine file, as `q3-ivm-baseline/src/diagnostics.rs` does) was considered and does not help. It changes how bytes are counted, not what is stored, so the typed and rkyv encodings would still need their rkyv and DBData derives.

### 2026-10-21: COL re-exports its generated module and keeps only `build_batch`

At roadmap Step 5, `merged-index-col/src/lib.rs` re-exports the generated module at the crate root, so callers write `merged_index_col::Key`. The stateless wrapper `CustomerOrdersLineitemIndex` is dropped, because its methods only delegated. `build_batch` is a free function and the only hand-written storage code.

### 2026-10-21: Retire the generated-versus-hand-written tests

`generated_tests.rs` is retired with the hand-written definition it compared against. The golden-byte tests in `tests.rs`, whose expected bytes the switch left unchanged, are the regression guard for the byte format.

### 2026-10-22: The Linux measurement A/B tests key front coding

The Linux R5 measurement includes an A/B test of `--front-code-byte-keys`: runs with key front coding and runs without it, all else equal. Both the pilot and the campaign run both arms. The arms alternate per workload (clustered A, clustered B, scattered A, scattered B). Arm B keeps `--elide-value-headers`, so the arms differ in the key encoding alone. (Setup compaction was also kept in both arms until 2026-11-09; see [The Linux campaign runs without setup compaction](#2026-11-09-the-linux-campaign-runs-without-setup-compaction).) The arms and their cost are in [MEASUREMENT.md](MEASUREMENT.md#pending-linux-campaign).

### 2026-10-23: Linux campaign refresh selection

The user decided how `prepare_inputs.py` chooses refreshes:

- RF1 inserts and RF2 deletes are chosen independently and never deliberately correlated.
- Clustered RF1 draws each new Order's Customer from a Zipf distribution over a seeded random permutation of the Customer keys. The skew is in popularity, with no custkey locality. The exponent is `--zipf-exponent`, default 1.0, which the user confirmed on 2026-10-26. *Superseded on 2026-11-17: clustered draws TPC-C's NURand in sessions ([the entry](#2026-11-17-clustered-rf1-draws-tpc-cs-nurand-in-sessions-only-eligible-customers-order)).*
- Clustered RF2 takes the Orders of the lowest-keyed Customers, as many as needed, with each pair taking the next block. *Superseded on 2026-11-04: both workloads delete the lowest Order keys.*
- Scattered RF1 is uniform over all Customer keys, including keys with no Orders. Scattered RF2 is a seeded uniform sample of existing Orders. *Superseded on 2026-11-04, as above. Superseded on 2026-11-17 for RF1: only Customers whose key is not a multiple of 3, as TPC-H specifies.*

`prepare_inputs.py` implements this.

### 2026-10-26: Feldera's buffer cache stays warm at window start

*Superseded on 2026-11-17 for runs started from an image: their window starts with the cache empty, or warmed by `--warm-up` ([the entry](#2026-11-17-load-once-into-an-image-each-refresh-run-is-a-new-process-from-a-copy-of-it)). It still holds for the single-process driver.*

The harness does not clear Feldera's buffer cache after loading and setup compaction. The occupancy at window start is recorded (`cache_occupancy_at_window_start_bytes` in the `main.rs` report). Before 2026-11-05 it was recorded per runtime, under both the base and the view runtime; there is now one runtime and one cache. Clearing the cache would need a new DBSP API: `BufferCache` in `crates/dbsp/src/storage/buffer_cache` offers only a per-file `evict`, and that acts only under the `eager_evict` developer tweak.

### 2026-11-04: RF2 deletes the oldest Orders

In both workloads, RF2 deletes the lowest base Order keys, as an analytical database ages out its oldest history. Pair K takes the next block, as large as its RF1. The two workloads then share their RF2 sets and differ only in RF1 (Zipf against uniform Customers). This replaces the RF2 rules of 2026-10-23; the RF1 rules stand.

### 2026-11-09: No setup compaction is the default workload

No compaction before the refreshes is the more realistic workload, so it is the default. `--setup-full-compaction` stays as a flag, to isolate the fragmentation effect: it starts both modes from one run per Spine, which removes where each Spine stands against the compaction threshold from the comparison. The flag was added by an agent on 2026-10-14 without a decision entry; this entry confirms its role.

Results so far (versions 4 to 6) ran with the flag, and each recorded result keeps its settings as run. `main.rs` and `measure.py` already take the flag as opt-in. The standard R5 commands in [CLAUDE.md](CLAUDE.md) drop it.

### 2026-11-09: The Linux campaign runs without setup compaction

The Linux pilot and campaign drop `--setup-full-compaction` in both front-coding arms, as the default workload of 2026-11-09 does. The arms still differ in the key encoding alone: both keep `--elide-value-headers`, and arm B drops only `--front-code-byte-keys`. This amends 2026-10-22, which kept the flag in both arms.

### 2026-11-10: The step-3 publication of `(Old, Dlt)`

The user confirmed on 2026-11-10:

- The publication change shares measurement version 7 with the default run placement, since no run at version 7 had been measured.
- `index_layout` keeps its `before` and `after` keys; `after` is the layout of `Old`'s runs plus `Dlt`, the view that the after reads take.
- An empty `Dlt` adds no run to the after view.
- The after reads take `Dlt` from memory, never from a file the spine spilled, so their counts can move from file decodes to `memory_*` reads. *Amended on 2026-11-10 (see the next entry):* `Dlt` follows step placement, so at Critical memory pressure it is a file.

### 2026-11-10: The hand-written view circuit replaces the generated one

The user decided on 2026-11-10 (roadmap step 4). The hook that this entry keeps was removed on 2026-11-13 (user decision; see that day's entry):

- The hand-written circuit (`q3-ivm-baseline/q3-trie-view-circuit`) replaces the generated reconstructed circuit in the `merged-index` build. This amends 2026-10-29's "the compiler hook stays": the hook (`--q3-merged-index`, `Q3MergedIndexBinding.java`) stayed in Java, unused, until its removal on 2026-11-13.
- Both modes now build the view circuit from different sources: native from `q3.sql`, reconstructed from the committed crate. `generate.sh` takes no option, and `verify.sh` no longer regenerates for the reconstructed run.
- The circuit meets (A1) and (A2): `J` joins on `(custkey)` and keeps `(custkey, orderkey)`, `V` joins on `(custkey, orderkey)`, and the inputs are keyed by their full trie paths.

### 2026-11-10: Inputs through typed handles fed from `Dlt`

The hand-written circuit's inputs arrive through typed input handles (`ZSetHandle`). The harness collects the touched groups with `Q3State::collect`, then pushes the driving deltas (the buffers' `Dlt` halves) with `Q3State::push_driving_deltas`. The view side no longer reads the SQL net delta.

### 2026-11-10: Pipelining types take the suffix `Vector`

The reader's type aliases are renamed `*Batch` to `*Vector` (`CustomerByCustkeyVector`, `OrdersByCustkeyVector`, `JoinedByCustkeyOrderkeyVector`, `LineitemByCustkeyOrderkeyVector`). Rationale: "batch" suggests a run of an LSM spine, which stores records, but these types hold no index state. They are the unit DBSP hands from one operator to the next: a weighted, key-sorted set of rows per step, or per output chunk at the flush. A Volcano iterator hands over one row; DBSP hands over a set of rows, as MonetDB/X100 hands over a vector. The same types also carry a reader's partner rows into the snapshot-reader join; the records themselves live in the scan session's buffers. (Since 2026-11-12 there are no buffers: the records live in the readers' requested slices.)

### 2026-11-11: The session-pass knob shares measurement version 10

The driver's `--session-pass seek|step` (default `seek`) selects the scan session's `Pass` in reconstructed mode, and the report records it as `configuration.session_pass` (`null` in native mode). It shares measurement version 10, since no version 10 run had been recorded, as the publication change shared version 7 on 2026-11-10.

### 2026-11-11: The Linux campaign crosses prefix truncation with the session pass

The Linux pilot and campaign run four arms, a full 2×2: prefix truncation on (A) or off (B), crossed with the scan session's pass, `seek` or `step`. The arms are `A-seek`, `A-step`, `B-seek`, and `B-step`. All keep `--elide-value-headers` and drop `--setup-full-compaction`. Native runs in every arm, though neither factor changes it, and must match across the four. This amends the two-arm design of 2026-10-22 and 2026-11-09. The user calls the key encoding of `--front-code-byte-keys` prefix truncation from 2026-11-11. *Amended on 2026-11-16: the full campaign runs on the SSD, and the hard disk gets only the eight pilots as a probe (see [the entry](#2026-11-16-the-ssd-runs-the-campaign-the-hard-disk-only-a-pilot-probe)). The four arms stand.* *Amended on 2026-11-18: the step arms are dropped, leaving `A-seek` and `B-seek` (see [the entry](#2026-11-18-the-step-arms-are-dropped)).*

### 2026-11-11: One set of inputs, read in place; every run starts cold

Every run of every campaign reads one set of TPC-H input files in place; no run copies them. Each run starts in a new process, with the inputs evicted from the page cache, or after a fixed warm-up routine; it never starts warm from an earlier run. `measure.py` checks each input's size and modification time against the campaign's hash record and evicts the inputs inside the run's cgroup before the driver starts. A run whose inputs were not evicted does not qualify. The runner has no warm-up routine; none is used.

### 2026-11-11: The macOS smoke runs at the current defaults

The version 10 macOS smoke run uses the current default workload, with no setup compaction, and otherwise the settings of the version 6 smoke run.

### 2026-11-12: Preparing the view's transaction delta is view work, in `delta_ms`

The user decided on 2026-11-12 that preparing the view circuit's transaction delta is view maintenance in both modes, timed inside the IVM window as a new sub-phase, `delta_ms` (measurement version 11, shared with the reader redesign).

- **Native:** the conversion of the net delta into the view circuit's input rows (`net_delta.apply_optional` into the input handles).
- **Reconstructed:** building the driving deltas from `Dlt` and handing them to the input handles (phase A, timed inside `Q3State::collect`, which returns it, then `Pipeline::build_view_delta`). The scan of `Old` (phase B) stays in `q3_ms`.
- **`forward_ms`** keeps only the net-delta read (`workload::net_delta`), the same operation in both modes.
- **`ivm_ms = delta_ms + q3_ms`.** `q3_ms` stays the view circuit's transaction, plus the scan session's collection in reconstructed mode. The report shows `delta_ms` beside `ivm_ms`, so a reader can compare the modes with and without it: the two operations are not identical.

This revises the 2026-11-11 rule that the boundary copy lies outside the IVM window (measurement version 9).

### 2026-11-16: The SSD runs the campaign; the hard disk only a pilot probe

The user decided on 2026-11-16:

- **The full four-arm campaign runs on the SSD.** The eight pilots run there first (16 runs), then the campaign (48 runs).
- **The hard disk gets only the eight pilots, as a probe (16 runs),** with `--storage-path` on it. An HDD campaign is decided after the probe, so the probe is evidence only. The TPC-H inputs and the build stay on the SSD in both cases.
- **`measure.py` stores the storage device in the manifest** (`storage_device`) and takes an optional `--device-label ssd|hdd`, which must not contradict the device's `rotational` flag.

The probe comes first so that an HDD campaign is decided on evidence. This adds a device to the pilot and campaign of the 2026-11-11 four-arm entry and leaves its arms unchanged.

### 2026-11-16: `measure.py` gates on layout fallbacks

A run is rejected when `front_coding.layout_fallbacks` is not 0 with prefix truncation on, or `value_header_elision.layout_fallbacks` is not 0 with elision on. A layout fallback is an eligible block that the encoder did not understand, so it was written unencoded and the arm would not measure the encoding it names. `size_fallbacks` (blocks that the encoding would not have shrunk by a sector) stay unchecked, since they are legitimate.

### 2026-11-17: Load once into an image; each refresh run is a new process from a copy of it

Loading is not measured, so it runs once, with no cgroup limit and no DBSP memory target, and writes a reusable image: one DBSP checkpoint of the base and the view circuit (and, in reconstructed mode, the merged index and its orphan lines), with `image.json` beside it (`measure.py --build-images`, the driver's `--write-image`). Every measured run starts a new process from its own full copy of the matching image (`measure.py --images`, the driver's `--from-image`), under the cgroup limit from the start and with the DBSP memory target, and runs only the refreshes. Whatever the refreshes need besides the loaded state is the refresh program's own work (the republish transaction, the catalogs, `Q3State`). The image is per mode and per prefix-truncation arm (`native-A`, `native-B`, `merged-index-A`, `merged-index-B`), because the refresh program reopens only an image written with its own storage settings. The first Linux pilot prompted this: it loaded SF 10 under the 1 GiB limit for 96 minutes and timed out in the background merges before any refresh ran. Measurement version 13.

- **Cache at the window's start:** cold by default, since every run starts from the same image in a new process. Optionally (`--warm-up`, user answer "both"), one RF1/RF2 pair from an extra dbgen update set, untimed and unreported, runs before the window in both modes. This supersedes the 2026-10-26 rule that the window starts with the cache warm from loading.
- **Every restored run is a file:** the restore reopens each batch from its file, so the window starts with no in-memory run.
- **One runtime still hosts both circuits** (2026-10-23, 2026-11-05). DBSP now checkpoints a runtime with several circuits as one checkpoint: each circuit's persistent ids carry its index (`-c0-`, `-c1-`), the handles share one checkpointer, and the checkpoint's fingerprint combines the circuits' in order (`DBSPHandle::checkpoint_circuits`).

### 2026-11-17: The load program uses its own cache and 1,000,000-row load transactions

Loading is unmeasured, so its settings serve speed, not the campaign. `measure.py --build-images` defaulted to a 16 GiB buffer cache per load (four loads at once; amended below), 1,000,000-row load transactions, and a 3600 s merge wait (`LOAD_CACHE_BYTES`, `LOAD_BATCH`, `LOAD_COMPACTION_TIMEOUT_SECS`). None reaches a refresh run, which takes the campaign's `--cache-bytes`; `ImageSettings` holds only the mode, the workers, and the two storage encodings. No full compaction runs after loading (user answer of 2026-11-17, keeping 2026-11-09), so the batch size shapes each image's runs, and all four images are loaded with the same settings.

The first SF 10 build, with the campaign's 128 MiB cache and 10,000-row transactions, loaded native in 84 to 95 minutes and reconstructed at 0.35% of `lineitem.tbl` per minute (about 5 hours). A profile put about 68% of the reconstructed load's main thread in the view phase's scan session (`Q3State::collect`): the loader reads orders in `orderkey` order, the index clusters by customer, and storage files bypass the page cache (`O_DIRECT`), so the scattered group reads missed a small buffer cache and read the SSD. With the new settings, both modes read about 2% per minute and nothing from the device in the first 15 minutes. *Amended on 2026-11-17 (user direction): the fixed 16 GiB was too small once the caches filled at about 30% of `lineitem.tbl`, when the reconstructed loads read 17 to 29 GB back from the SSD and slowed from about 2% to 0.9% per minute. Each load's cache is now 75% of `MemAvailable` split among the loads that run at once (`LOAD_MEMORY_SHARE`), and `--sequential-loads` runs them one at a time with the whole budget.* Rejected for now: sorting the load input by customer, which needs a loader change and costs native's `orderkey`-keyed join its locality.

### 2026-11-17: Transaction size is a dimension, small transactions first

The TPC-H refresh functions are batch loads: at SF 10 one RF1 is a single transaction of 15,000 Orders and about 60,000 Lineitems, the ETL pattern that incremental view maintenance exists to avoid. The campaign measures small transactions first, closer to a hybrid transactional and analytical database: about 10, 100, and 1,000 rows per transaction (`prepare_inputs.py --transaction-rows`), with the TPC-H batch kept as a later ETL reference point. A transaction holds whole Orders, an Order counting 1 + its Lineitems. Inserts and deletes stay in groups: pair by pair, all of RF1 K as consecutive small transactions, then all of RF2 K, since interleaving them would mix the two operations within the size being measured. Each transaction is timed on its own (`transaction_latency`, measurement version 14), and percentiles, not only sums, are reported per phase.

### 2026-11-17: Clustered RF1 draws TPC-C's NURand in sessions; only eligible Customers order

- **Eligible Customers.** As TPC-H specifies, RF1 Orders go only to Customers whose key is not a multiple of 3, in both workloads (1,000,000 of 1,500,000 at SF 10). Scattered draws them uniformly.
- **Popularity.** Clustered replaces the Zipf draw of 2026-10-23 with TPC-C's customer NURand(1023, 1, 3000) (clause 2.1.6), with one seeded constant C. TPC-H has no districts, so consecutive blocks of 3,000 eligible Customers stand in for them (user choice among three synthetic scalings: these blocks, one NURand over all eligible Customers, or nations); a block is chosen in proportion to its size. Real districts arrive with CH-benCHmark.
- **Locality within a transaction.** Independent draws make the Customers a transaction spans a function of popularity and size, and a Zipf steep enough to put 10 Orders on 2 or 3 Customers would hand most Orders to one Customer. So locality is its own parameter: each transaction of n Orders draws ceil(n / m) Customers and gives them its Orders m at a time (`--orders-per-session`, m = 4), which keeps each Customer's expected Order count while bunching it. At about 10 rows a transaction is one Customer's session, as a TPC-C New-Order is.
- **Unchanged.** Deletes stay oldest-first in both workloads; dbgen's Order keys stay (the order-key dimension was deferred, [deferred.md](deferred.md#order-key-assignment-as-a-dimension); it is an arm since 2026-11-19).

### 2026-11-18: The step arms are dropped

The pilot and the campaign run two arms, `A-seek` and `B-seek`: prefix truncation on or off, with the scan session's `seek` pass. Under the `step` pass a small transaction walks the index from the session's start to its touched groups, about half the index per transaction ([LINUX-HANDOFF.md](LINUX-HANDOFF.md#known-limits)); the reconstructed run of `A-step` clustered at K = 10 was stopped after 49 minutes. `Pass::Step` and `--session-pass` stay in the code. This amends the four arms of 2026-11-11.

### 2026-11-18: Bloom filters are storage-resident, read through the buffer cache

The K = 10 pilots showed every file run's filter held in memory for the run's life: 305 MiB native and 461 MiB reconstructed after restore. Two of them, the merged index's (175 MiB) and `V`'s materialized trace's (137.5 MiB), were never probed. The user chose a generic budget over a per-spine opt-out, after RocksDB's filter blocks in the block cache with partitioned filters. DBSP now writes each new run's Bloom filter as partitions that stay on storage, and a probe reads one partition through the buffer cache, where it is evicted like a data block (`StorageOptions::filter_residency`, default `storage`, in DBSP and in the experiment). An unprobed filter costs no memory. A filter small enough for one partition stays in memory, because the per-transaction runs of small transactions would otherwise each write a 4 KiB partition to save a few dozen bytes. Measurement version 15.

### 2026-11-18: The refresh runs' buffer cache is 256 MiB

Filter partitions now share the buffer cache with data blocks, so the cache grows from 128 MiB to 256 MiB in both modes (user answer). The 512 MiB memory target and the 1 GiB cgroup limit are unchanged.

### 2026-11-18: Compaction is deferred; its options are documented with measurements

Small transactions leave many small runs (14 to 75 in the merged index at the ends of the K = 10 refreshes), and each group read seeks every run. The user deferred any compaction change and asked for a document that sets the options against measurements, respecting the current spine design and aiming at a good layout for every spine, not only the merged index: [compaction.md](../../../docs/design/merged-index/compaction.md). Measurement version 15 adds the counters it needs (`run_counts`).

### 2026-11-18: The version 15 images load native-A alone, then the other three at once

The rebuild started sequentially (user direction: as much memory as possible). During native-A the user judged parallel loads with a larger budget for merged-index better, at least for the remaining three. Native-A finished alone with the whole budget; native-B, merged-index-A, and merged-index-B load at once with `measure.py --complete-images`, which splits 75% of `MemAvailable` by mode weight, merged-index twice native (`LOAD_MODE_WEIGHTS`). `images.json` records each image's `cache_bytes`.

### 2026-11-18: Arm B and the step pass are retired

Only arm `A-seek` runs: prefix truncation and value-header elision on, with the `seek` pass. Arm B (no prefix truncation) and every step setting are retired (user decisions). `measure.py` builds only the A images (`IMAGE_ARMS`) and offers only `--session-pass seek` (`SESSION_PASSES`). The driver keeps `--session-pass step` and DBSP keeps `Pass::Step`, so that the version 15 executables, and with them the images, stay valid; nothing in the campaign selects them. This amends the two arms of the morning's entry and the arms of 2026-11-11.

### 2026-11-18: Images are reused while their sources are unchanged

A campaign used to reopen an image only with the byte-identical executable that wrote it (2026-11-17), so any driver change, even a measurement timer, cost a rebuild of 1.5 to 2.5 hours. Now `images.json` records `image_sources_sha256`, a digest of the files that define an image's content (`IMAGE_SOURCE_PATHS` in `measure.py`), and a campaign refuses an image only if the checkout's digest differs. The driver's own load and save path is versioned by hand (`ImageManifest::FORMAT`), and DBSP's circuit fingerprint and `ImageSettings` checks stay. The executables' SHA-256 is still recorded, for provenance. `--adopt-image-sources` gives the digest to images built before it, after `git diff` shows that no image source or circuit generator changed since their revision. This supersedes the executable rule of 2026-11-17.

### 2026-11-18: Base-table blocks leave the cache after each base transaction

Report the view circuit (user direction): the base circuit's cost, merged-index upkeep included, is assumed amortized over the views an index serves (two indexes cover 18 of the 22 TPC-H queries), and its side effects on the view are mitigated ad hoc in both modes. Neither view reads the base tables, but in the version 15 pilot the base tables and the merged index evicted each other's blocks from the one cache. Keeping base blocks out of the cache altogether was rejected, since it would also starve their compaction. So the base circuit uses the cache normally, and when its transaction commits the driver evicts its tables' blocks before the view phase (measurement version 16). Later, one base circuit will feed several chained view circuits on the same index.

### 2026-11-20: The memtable arm splits one 512 MiB budget: 448 MiB cache, 64 MiB memtable

- **Arms.** All three run at measurement version 18 from `images-v18-pf`: A-seek clustered, K = 10, dbgen keys, prefix filters on (`--index-prefix-filter`), base eviction on, memory target 512 MiB, cgroup 1 GiB. The split is the one [memtable.md](../../../docs/design/merged-index/memtable.md#the-arm-and-its-limits) specifies.
  - **512/512, memtable off:** the baseline, as in the prefix-filter A/B.
  - **448 cache, memtable off:** the control, which separates the smaller cache's effect from the memtable's.
  - **448 cache + `--memtable-mib 64`:** the arm.
- **Why the cache gives way.** The memtable counts in RSS. A cache equal to the target keeps the run at critical pressure by construction: RSS was about 720 MiB in the 512/512 pilot. So the memtable's 64 MiB comes out of the cache, and the target is unchanged.
- **Writes by kind.** These come from separate `--diagnostics` reruns in a 2 GiB cgroup, since the diagnostics logs are held in memory. Their latencies are not reported.

### 2026-11-19: The sequence arm is stopped; cancelled rows are a watch item

- **Stopped.** The `sequence` order-key arm's 512/512 pilot was stopped during its native run, after 3 h 37 min (the dbgen pilot ran in 23 min), and the arm is not measured further. Its key space is close enough to dbgen's for this work (user, 2026-11-19).
- **Why it was slow.** RF2 deletes the lowest keys, and with sequence keys no RF1 Order lands among them, so the cancelled rows form one stretch at the front of base Orders' and Lineitem's key range. They wait for a merge into each spine's largest run, and every new cursor walks them (`CursorList::new`). The driver's per-lookup cursors in `BaseState` read that stretch again in every transaction: 85 of the run's 86 GB of device reads. Evidence and the existing solutions: [compaction.md](../../../docs/design/merged-index/compaction.md#watch-for-cancelled-rows-at-the-front-of-the-key-range).
- **No fix yet.** A driver-side change to `BaseState` would hide the effect rather than remove it; the root cause is that size-tiered merging keeps a deletion apart from the row it cancels. The choice among deletion-triggered compaction, key-partitioned levels, and read-side bounds waits until the `uuid` arm shows whether it meets the same growth (user, 2026-11-19).
- **Order of runs.** The prefix-filter A/B (dbgen keys, version 17) runs first, then the `uuid` pilot at version 17 with the filter off, from new `images-v17-uuid`; the version 16 `uuid` images no longer match the image sources.

### 2026-11-19: Order keys are a dimension: dbgen, sequence, uuid

- **Arms** (`prepare_inputs.py --order-keys`). The selection is the same in every arm: RF2 deletes the oldest Orders and RF1 Orders arrive in dbgen's file order; only the keys the files carry differ.
  - `dbgen` (the default, as before): RF1 keys fill the gaps of the low range RF2 deletes from.
  - `sequence`: a database sequence, so dense keys. Base Orders get 1..N, which keeps their order, and RF1 Orders get N+1, N+2, … in arrival order. The base keys are dense too (user, 2026-11-19), not dbgen's sparse ones: roaring filters are on by default and `FilterPlan::decide_filter` chooses them by key density, so sparse base keys would get another filter than a real sequence.
  - `uuid`: random keys, as with UUIDv4 primary keys (CockroachDB and Spanner recommend them to spread inserts; PostgreSQL's `gen_random_uuid()`). Every key, base and RF alike, is a random positive BIGINT, so both modes keep 8-byte keys; a 16-byte key is a separate dimension. A key drawn twice is redrawn, as a unique index rejects a clash and the client retries; the manifest counts redraws. RF2 still deletes the oldest Orders, now scattered over the key space, and the RF2 files list them oldest first.
- **Dropped:** timestamp IDs (Snowflake-style: time, node, counter; MySQL's `UUID()`, MongoDB's ObjectId). With one writer they order keys as a sequence does, with gaps that DBSP does not see (user, 2026-11-19).
- **Base tables and images.** `sequence` and `uuid` re-key the base tables into `<out>/base/`, so each needs its own images. `measure.py --images` refuses refreshes prepared for other base files (`refresh_base_problems`, against `refresh_base` in `prepare-manifest.json`).

### 2026-11-18: One memory budget: the cache about equals the memory target

With Bloom filters storage-resident there is no point in two layers of memory management (user direction). The buffer cache is set equal to the memory target, or nearly, in both modes: the merged index never calls `seek_key_exact`, so its share of memory can all go to cache. First run: target 512 MiB, cache 512 MiB. If that leaves pressure low in both modes, both are halved, and halved again, to gauge performance under memory pressure, the target workload of this work.

## Superseded and withdrawn

Rows that apply to any index or harness are in the [generic log](../../../docs/design/merged-index/decisions.md#superseded-and-withdrawn).

| Date | Earlier position | Replaced by |
| --- | --- | --- |
| 2026-11-11 | The boundary copy of the driving deltas into the view circuit's input handles is outside the IVM window in both modes, in `forward_ms` (measurement version 9, kept in version 10). | 2026-11-12: preparing the view's transaction delta is view work, timed in `delta_ms` inside `ivm_ms` (measurement version 11). `forward_ms` keeps only the net-delta read. |
| 2026-11-11 | The driving deltas are pushed row by row into the view circuit's `ZSetHandle`s (measurement version 9). | 2026-11-11: the readers' `Dlt` batches are handed whole to `add_input_stream` handles (measurement version 10). |
| 2026-11-11 | Three `TransactionGenerator` sources emit the readers' `Dlt` batches at the commit flush, cloning all three. | 2026-11-11: DBSP's `Input` operator, as native mode uses; `ΔO` and `ΔL` moved, `ΔC` cloned. |
| 2026-10-29 | The compiler hook generates the reconstructed circuit. | 2026-11-10: the hand-written `q3-trie-view-circuit` replaces it. |
| 2026-10-01 | `min_storage_bytes: Some(0)` in the harness's `runtime_config` (never recorded as a decision). | 2026-11-09: DBSP's default placement in both modes. |
| 2026-10-22 and 2026-11-09 | The Linux campaign has two arms, with and without prefix truncation (then called front coding). | 2026-11-11: four arms, prefix truncation crossed with the session pass. |
| 2026-11-11 | The pilot (16 runs) and the campaign (48 runs) of the four arms run on one machine's storage with no device choice. | 2026-11-16: the campaign runs on the SSD; the hard disk gets the eight pilots (16 runs) as a probe. |
| Before 2026-11-11 | Each run copies every input into its own directory inside its cgroup (`private_inputs`). | 2026-11-11: one set of inputs, read in place; each run evicts it from the page cache and starts cold. |
| 2026-10-22 | Keep `--setup-full-compaction` in both arms of the Linux front-coding A/B. | 2026-11-09: the campaign runs without setup compaction. |

## Open questions

None specific to the Q3 harness. The open questions of the design are in the [generic log](../../../docs/design/merged-index/decisions.md#open-questions).
