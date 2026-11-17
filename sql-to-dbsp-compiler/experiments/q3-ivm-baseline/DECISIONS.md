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

At roadmap Step 5, record the final typed (`SourcePayload`) and rkyv value-level bytes in [col-storage.md](../../../docs/design/merged-index/storage-layout.md), then retire those parts of `layout_tests.rs`. Only the packed measurement remains. Generated types keep plain derives. This replaces the last sentence of the 2026-10-20 plain-derives entry.

Summing spine run bytes instead (`layer_file_block_bytes` per spine file, as `q3-ivm-baseline/src/diagnostics.rs` does) was considered and does not help. It changes how bytes are counted, not what is stored, so the typed and rkyv encodings would still need their rkyv and DBData derives.

### 2026-10-21: COL re-exports its generated module and keeps only `build_batch`

At roadmap Step 5, `merged-index-col/src/lib.rs` re-exports the generated module at the crate root, so callers write `merged_index_col::Key`. The stateless wrapper `CustomerOrdersLineitemIndex` is dropped, because its methods only delegated. `build_batch` is a free function and the only hand-written storage code.

### 2026-10-21: Retire the generated-versus-hand-written tests

`generated_tests.rs` is retired with the hand-written definition it compared against. The golden-byte tests in `tests.rs`, whose expected bytes the switch left unchanged, are the regression guard for the byte format.

### 2026-10-22: The Linux measurement A/B tests key front coding

The Linux R5 measurement includes an A/B test of `--front-code-byte-keys`: runs with key front coding and runs without it, all else equal. Both the pilot and the campaign run both arms. The arms alternate per workload (clustered A, clustered B, scattered A, scattered B). Arm B keeps `--elide-value-headers`, so the arms differ in the key encoding alone. (Setup compaction was also kept in both arms until 2026-11-09; see [The Linux campaign runs without setup compaction](#2026-11-09-the-linux-campaign-runs-without-setup-compaction).) The arms and their cost are in [measurement.md](MEASUREMENT.md#pending-linux-campaign).

### 2026-10-23: Linux campaign refresh selection

The user decided how `prepare_inputs.py` chooses refreshes:

- RF1 inserts and RF2 deletes are chosen independently and never deliberately correlated.
- Clustered RF1 draws each new Order's Customer from a Zipf distribution over a seeded random permutation of the Customer keys. The skew is in popularity, with no custkey locality. The exponent is `--zipf-exponent`, default 1.0, which the user confirmed on 2026-10-26.
- Clustered RF2 takes the Orders of the lowest-keyed Customers, as many as needed, with each pair taking the next block. *Superseded on 2026-11-04: both workloads delete the lowest Order keys.*
- Scattered RF1 is uniform over all Customer keys, including keys with no Orders. Scattered RF2 is a seeded uniform sample of existing Orders. *Superseded on 2026-11-04, as above.*

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

The Linux pilot and campaign run four arms, a full 2×2: prefix truncation on (A) or off (B), crossed with the scan session's pass, `seek` or `step`. The arms are `A-seek`, `A-step`, `B-seek`, and `B-step`. All keep `--elide-value-headers` and drop `--setup-full-compaction`. Native runs in every arm, though neither factor changes it, and must match across the four. This amends the two-arm design of 2026-10-22 and 2026-11-09. The user calls the key encoding of `--front-code-byte-keys` prefix truncation from 2026-11-11. *Amended on 2026-11-16: the full campaign runs on the SSD, and the hard disk gets only the eight pilots as a probe (see [the entry](#2026-11-16-the-ssd-runs-the-campaign-the-hard-disk-only-a-pilot-probe)). The four arms stand.*

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
