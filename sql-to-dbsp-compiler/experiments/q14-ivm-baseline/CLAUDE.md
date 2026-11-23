# q14-ivm-baseline: TPC-H Q14 over MI_P

This harness maintains `V = Part ⋈ Lineitem` on `partkey` (Q14 without its predicates) in two modes, native and reconstructed from the MI_P merged index, and checks that they agree. It is step 5 of [new-merged-index.md](../../../docs/design/merged-index/new-merged-index.md), reduced to correctness (user decision of 2026-11-20), and its measurement, which uses Q3's image and cgroup method ("Measuring from images"). The index, its readers, and `Q14State` are in [merged-index-p](../merged-index-p/CLAUDE.md); the rules for any index and harness are in [experiments/CLAUDE.md](../CLAUDE.md). The Q3 counterpart, whose structure this follows, is [q3-ivm-baseline](../q3-ivm-baseline/CLAUDE.md).

## Files

| Piece | Location |
| --- | --- |
| Base circuit: the three MI_P tables with primary keys, no view (generated, git-ignored) | `base.sql`, crate `q14-base-circuit` |
| Native view circuit: keyless Part and Lineitem plus `V`, as in `../merged-index-p/schema.sql` (generated, git-ignored). `V` reads no Partsupp, so this circuit has none | `q14.sql`, crate `q14-view-circuit` |
| Reconstructed view circuit, hand-written: one `join_lineitems_parts`; two `add_input_stream` handles (`TrieInputs`) take `ΔP` and `ΔL` from `Q14State::collect`; `V`'s seven columns, so a line's `l_suppkey` is dropped; `build` registers `V` in a catalog as the generated circuit does; no `ERROR_VIEW` | `q14-trie-view-circuit/` |
| `Pipeline`: both circuits in one runtime, base first; `transact` runs a change and returns the net delta; `view_output` reads the change of `V` and `V` | `src/pipeline.rs` |
| `ManualIndex` (the index's owner thread on the runtime's buffer cache: snapshot, append, background-merge state, and save and restore into a checkpoint) and `index_rows` (the net delta folded row by row) | `src/index.rs` |
| Rows, `Change`, `ViewRow`, and `Reference` (the tests' oracle by primary key) | `src/data.rs` |
| Tests and the transcript | `src/verification.rs` |
| Driver: the command line (`Config`), loading, and the refreshes, each timed and checked against `Reference` unless `--no-verify`; the load program (`--write-image`) and the refresh program (`--from-image`); the JSON report (`--report`); `main.rs` only adds the allocator | `src/driver.rs`, `src/main.rs` |
| Runner: builds both drivers and their images, then runs the refresh programs under a cgroup limit | `measure.py` |
| dbgen's base files loaded in transactions of at most `--batch` rows; refresh lists in the Q3 format (`rf1_lines` or `rf2`; `rf1_orders` ignored; `transaction_orders` refused), read before the first refresh | `src/workload.rs` |
| Generates the native pair and `plan.json`, as Q3's does | `generate.sh` |

The Cargo feature `merged-index` builds the reconstructed mode (`q14-trie-view-circuit` in place of `q14-view-circuit`).

Q14's tables are Part, Partsupp, and Lineitem, so TPC-H's refreshes reach it only through Lineitem: RF1 inserts `lineitem.tbl.uN`, and RF2 deletes every line item of the orders in `delete.N`. Both modes find RF2's line items in the base circuit's Lineitem (`Pipeline::order_lines`), since MI_P is keyed by part. Measurement follows Q3's accounting (user decision of 2026-11-20), and since 2026-11-22 Q3's method too: images, a 1 GiB cgroup, and base-block eviction (see "Measuring from images"). `Pipeline::transact` returns each transaction's `Timings`: `preparation` (the change into the base circuit's input rows), `base_circuit`, `index` (`index_rows` and `append`), `forward` (the net-delta read), `delta` (native: the net delta into the view's input rows; reconstructed: `Collection::delta_preparation` and the hand-off to the input handles), `view` (the view transaction, and in reconstructed mode the collection's scan), `publication` (`publish` and `finish`), and `base_eviction`. The report gives per refresh `base_ms = base_circuit + index` beside `ivm_ms = delta_ms + q14_ms`, and `discovery_ms` (RF2's lookup) and `total_ms`. The reference's upkeep is outside every timer.

## Q14 instances of the shared rules

- **One runtime, one cache.** `Pipeline::new` builds both circuits with `Runtime::init_circuits`. `ManualIndex::new` errors if its thread is not on `Runtime::foreground_buffer_cache(0)`; the session collects with that cache bound.
- **Transaction order.** The base circuit commits first; its net delta (`Pipeline::net_delta`, the base tables' delta streams, enabled in `new`) goes natively to the view circuit's keyless tables, whole rows for deletions, and in reconstructed mode to `index_rows`, `ManualIndex::append`, `Q14State::publish`, `collect`, the input handles, the view transaction, and `finish`. A stepped transaction has three steps in each circuit (`Change::steps`: parts, Partsupp, lines); the reconstructed view's steps share one collection.
- **General deltas need no owners.** Every MI_P path is made of its own row's columns (a line item's part and supplier are `l_partkey` and `l_suppkey`), so the net delta folds row by row: no `prepare`, no side store. A line item of an absent part folds under its partkey and joins nothing until the part arrives.

## Measuring from images

As in Q3 ([LINUX-HANDOFF.md](../q3-ivm-baseline/LINUX-HANDOFF.md)), loading is not measured; the user asked on 2026-11-22 for Q3's method:

- **Images.** The load program (`--write-image DIR`) loads, waits for the background merges, and checkpoints both circuits and the index (`Pipeline::write_checkpoint`) into `DIR/storage`, with `DIR/image.json` (`ImageManifest`: format, storage settings, checkpoint, inputs, `LoadRecord`). The refresh program (`--from-image DIR --refresh-list F --no-verify`) checks the settings, reopens the image in place (`Pipeline::reopen`), runs one empty transaction so the outputs are published, checks `V`'s rows against the load's, and runs the refreshes. `DIR` must be the run's own copy.
- **Base-block eviction.** The refreshes of every program evict the base tables' blocks from the buffer cache after each base transaction (`Pipeline::set_evict_base`; `base_eviction_ms` and `base_evicted_bytes`, in neither phase), as Q3 measurement version 16 does. Loading does not evict.
- **Storage format.** `--front-code-byte-keys` and `--elide-value-headers`, Q3's arm A, for both modes' files.
- **Window evidence.** Per refresh, DBSP's memory pressure before and after and the storage reads by thread and owner; per run, `window_peak_rss_bytes` (`VmHWM` reset by `clear_refs` at window start).
- **Runner.** `measure.py build --storage DIR --inputs SF_DIR` builds both release drivers into `DIR/images/bin` and loads both images at once (1,000,000-row batches, 256 MiB cache). `measure.py run --storage DIR --refresh-list F --out REPORTS [--runs 3]` runs each mode `--runs` times, alternating which goes first. Each run copies its image with `cp -a`, drops the inputs and the copy from the page cache (`posix_fadvise`), and starts the refresh program in a new cgroup child with `memory.max` 1 GiB and `memory.swap.max` 0, a 256 MiB cache, a 512 MiB memory target, and the `seek` pass. It samples `memory.current` (kernel 5.15 has no `memory.peak`) and records `memory.events` and `memory.stat`. A run qualifies when it succeeded, its inputs were evicted, and its image exceeds the limit. `measure.py summarize REPORTS/<ts>` prints the medians.
- **Cgroup setup.** Once per boot: `sudo mkdir -p /sys/fs/cgroup/q14/driver; echo +memory | sudo tee /sys/fs/cgroup/q14/cgroup.subtree_control; sudo chown -R $USER /sys/fs/cgroup/q14`. Start the runner from `bash -c 'echo $$ | sudo tee /sys/fs/cgroup/q14/driver/cgroup.procs >/dev/null && exec python3 measure.py run ...'`. Remove leftover run cgroups with `rmdir`.

## Tests and gate

```bash
./generate.sh                                                   # native pair; needs JDK 21 and the compiler JAR
cargo +1.96.1 test --offline --locked                           # native
cargo +1.96.1 test --offline --locked --features merged-index   # reconstructed
cargo +1.96.1 test --offline --locked -p q14-trie-view-circuit  # the hand-written circuit alone
./verify.sh                                                     # the gate
cargo +1.96.1 run --release --offline --locked [--features merged-index] -- \
  --part part.tbl --partsupp partsupp.tbl --lineitem lineitem.tbl --refresh-list list.json \
  [--batch 10000] [--storage-path NEW_DIR --cache-bytes B] [--memory-target-bytes B] [--session-pass seek|step] \
  [--front-code-byte-keys] [--elide-value-headers] [--report FILE] [--no-verify]
# Load program: base files and --write-image NEW_DIR; refresh program: --from-image DIR --refresh-list F --no-verify
```

- `general_transcript`: ten transactions over every kind of general change (load with a line of an absent part, empty, part retyped, lines moved between parts and suppliers in three steps, part deleted with its lines kept, part inserted for waiting lines, Partsupp-only, part returned with a new type and a line in three steps, lines and a part deleted, empty in three steps). After each, the committed change of `V` and the materialized `V` must equal `Reference`'s; the contract check is on. With `Q14_GENERAL_TRANSCRIPT` set, it writes the transcript.
- `the_net_delta_has_whole_rows_for_upserts_and_deletions`; `random_general_changes_match_the_reference` (proptest, 16 sequences of random table states, one step or three); `the_transcript_does_not_depend_on_the_pass` (reconstructed only).
- Driver: `a_run_loads_and_refreshes_in_agreement_with_the_reference` (file-backed, both modes), `an_image_reopens_where_its_loader_stopped` (the image programs give the refreshes what a single verified run gives them, and refuse other storage settings), the command line's checks, the dbgen parsers, loading's batches, and the refresh lists.
- `q14-trie-view-circuit`: `general_deltas_match_the_brute_force_join` and `random_transactions_match_the_brute_force_join`, against a brute-force join with no base circuit.
- `verify.sh` generates the native pair, runs fmt, both builds' tests and Clippy, the hand-written circuit's tests, compares the two transcripts with `cmp`, checks that MI_P's `generated.rs` is fresh, runs MI_P's fmt, tests, and Clippy, and checks shell syntax and whitespace. Logs go to `target/q14-verification/<run>/`.
