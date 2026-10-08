# R5 maintenance measurement harness implementation plan

## Intent and qualification

Compare native and reconstructed predicate-free Q3 RF1/RF2 maintenance with
identical logical refreshes and file-backed maintained state. Initial loading
is setup. Refresh deltas remain complete in RAM; oversized legitimate refreshes
must fail rather than be split to fit a budget. macOS evidence is smoke testing.
Equal enforced total-memory, beyond-memory Linux measurements remain pending.
Refresh completion does not imply durability; checkpoint costs are deferred.

## Inspected implementation

- `q3-ivm-baseline/src/main.rs` currently buffers base files, retains `Reference`
  even without verification, and discovers RF2 rows from those maps.
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
3. Add ordered refresh-list parsing, explicit R5 storage configuration,
   index/circuit drainage with timeout, and JSON measurement reports. Attribute
   discovery, checks/preparation, index insertion, Q3 and publication/cleanup;
   exclude correctness validation. Count drainage in sequence totals.
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
