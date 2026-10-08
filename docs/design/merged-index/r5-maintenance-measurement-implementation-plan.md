# R5 maintenance measurement harness implementation plan

## Intent and qualification

Compare native and reconstructed predicate-free Q3 RF1/RF2 maintenance with
identical logical refreshes and file-backed maintained state. Initial loading
is setup. Refresh deltas remain complete in RAM; oversized legitimate refreshes
must fail rather than be split to fit a budget. macOS evidence is smoke testing.
Equal enforced total-memory, beyond-memory Linux measurements remain pending.
Refresh completion does not imply durability; checkpoint costs are deferred.

## Inspected implementation

- `q3-ivm-baseline/src/main.rs` initially buffered base files, retained `Reference`
  even without verification, and discovered RF2 rows from those maps.
- Generated Catalog base maps expose indexed materialized snapshots keyed by
  `Tup1<i64>` (Customer/Orders) and `Tup2<i64, i32>` (Lineitem). Indexed serialized
  cursors support seeks and complete logical payload conversion.
- `src/reconstruction.rs` builds physical index rows, owns an auxiliary-thread
  `Spine`, and publishes before/after snapshots through the Q3 transaction.
  COL codecs and prefix cursors support complete Order line-range discovery.
- `CircuitConfig.max_rss_bytes`, `StorageOptions.cache_mib`,
  `min_storage_bytes`, compression and `dev_tweaks.fetch_join` are existing
  settings. Circuit and Spine compaction expose start/status methods.
- Storage block-size histograms provide cumulative backend operation counts
  and byte sums. Runtime cache occupancy is available. OS I/O must be recorded
  separately, with unsupported counters explicitly unavailable.

## Implementation steps

1. Stream Customers in bounded batches, then ascending Orders and grouped
   Lineitems in complete groups using the same transactions in both modes.
   Retain only the current loading batch plus lookahead; a group may exceed
   the loading target. Reject unsupported file ordering explicitly.
2. Separate logical input submission from optional independent `Reference`
   validation. Discover RF2 from current Catalog Orders and native Lineitem
   ranges or merged-index ranges. Deduplicate IDs and report absent Orders.
   Validate identities, references, payloads and complete change directions
   against current maintained storage before mutation in both modes.
3. Add ordered refresh-list parsing, explicit R5 storage configuration, and
   JSON measurement reports. Attribute discovery, checks/preparation, index
   insertion, Q3 and publication/cleanup; exclude correctness validation. During
   loading, wait for naturally eligible background merges to become idle, then
   publish with one empty transaction. Do not force full compaction during load
   or refresh, and do not wait or run an extra transaction after the final
   refresh. End sequence timing at its publication/cleanup boundary.
4. Build a runner preserving both release executables and restoring native
   generation on exit. Use fresh processes/directories, three repetitions,
   hashes and revision/configuration provenance. Linux qualification requires
   private input copies prepared inside cgroup v2 with equal `memory.max` and
   disabled swap; macOS always remains a smoke test.
5. Add regression tests, run R4 verification, format/strict Clippy, runner
   unit tests, shell syntax and whitespace checks. Run clustered, scattered
   and large-group macOS comparisons. Update experiment instructions, workflow,
   results and roadmap with actual evidence and pending qualification.

## Boundaries

No second persistent storage system, full base copy, refresh replay, or normal
RF2 base-file scans. Independent full-join validation is enabled only when
requested and excluded from maintenance measurements. Reconstruction decoding,
copying, sorting, spill and output cleanup stay in their actual intervals.
Generated artifacts and logs remain under ignored `target/` directories.
Broader updates, residual Q3, periodic checkpoint measurements and Q5/Q10 remain
deferred. Fetch and merge the tracking branch before each commit; stage explicit
files only.

## Verified integration findings

Catalog `concat()` consumes its output mailbox. The driver therefore retains
one `BaseState` of snapshot references and replaces it after each transaction,
reusing the same snapshot for RF2 discovery and operational checks. Snapshots
share maintained batches rather than copying rows.

At low memory pressure, insertion may leave batches in memory even when
`min_storage_bytes=0`. `SerBatch::location()` reports the actual location;
storage summaries include snapshot, memory, and storage batch counts for each
Catalog relation. `manual_index_storage` describes the separately maintained
index. The existing full-compaction API can reduce eligible state to at most one
batch per trace, but R5 does not invoke it. Loading waits only for natural
background merges to become idle and uses one empty publication transaction.
There is no final wait or extra transaction; sequence completion follows the
last refresh's publication and cleanup.

Driver snapshot reads and the manual-index auxiliary owner otherwise select the
runtime's fixed 256 MiB fallback cache, which is outside `StorageOptions.cache_mib`.
A scoped `Runtime::with_buffer_cache` binding makes both reuse the configured
worker cache. It restores prior thread-local cache state, including on unwind;
no new cache or storage system is introduced. The reported cache capacity then
covers driver reads, Q3 workers, manual-index ownership, and merger tasks.

## Completion evidence

The original R5 v1 implementation and macOS smoke work completed. Its results
include forced final compaction and are retained as historical evidence; the
revised completion boundary and smoke rerun remain pending. `verify.sh` passed native
(18), reconstructed (15), COL (21), and runner (10) tests, exact R4 transcript
comparison, formatting, strict experiment/COL Clippy, shell syntax, and whitespace
checks. Five focused DBSP cache/compaction tests and strict production-library
Clippy passed. Broader DBSP test-code Clippy reported 56 existing lints outside
the modified files; its log is retained under ignored experiment `target/`.

Clustered, scattered, and complete 10,000-line-group campaigns each completed
three fresh runs per mode under the original v1 completion boundary. They do not
establish runtime memory pressure. The current v2 smoke rerun and Linux
equal-total-memory, beyond-memory qualification remain pending. See the
[experiment results](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/RESULTS.md)
and [Linux pressure campaign plan](r5-pressure-and-linux-campaign-documentation-plan.md).
Deferred items remain as listed above.
