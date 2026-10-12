# Source and visual evidence

> **Historical: pinned to `415bd4940a91e09eef4c2502c5195db00fd5806d`** (2026-10-08), with a
> construction addendum pinned to `9ec749f84665eb8ba48f4036152020afd9bbf45d`. A diff against
> the current branch shows `crates/dbsp/src/trace/spine_async.rs` and `crates/dbsp/src/trace.rs`
> changed; the other cited DBSP sources are byte-identical, but their line anchors and hashes
> were not re-derived. The hashes and anchors for the documentation files in the source map
> (state-read contracts, `CIRCUIT.md`, the experiment README, `WORKFLOW.md`) describe
> files that have since been rewritten. Use the pinned links, not the local ones, to check a
> claim. Confirmed project decisions are in [decisions.md](../decisions.md).

## Inspection identity and configuration

Final revision inspected on 2026-10-08 at checkout
[`415bd4940a91e09eef4c2502c5195db00fd5806d`](https://github.com/feldera/feldera/tree/415bd4940a91e09eef4c2502c5195db00fd5806d),
branch `feat/merged-index-storage`. The working tree was clean at the start.
An earlier inspection covered revision `7d3628513c390ba8053adf54f6c12e06fc901e37`
with concurrent native/experiment edits; its working-tree qualifications are
historical and do not identify this revision's bytes. The refreshed hashes
below identify the exact source used for the current deck.

This revision edits presentation sources, the example checker, and supporting
documentation only. It does not alter or regenerate runtime, compiler, API,
SQL, or experiment code. Generated `q3-ivm-baseline/src/lib.rs` was read only
as corroboration; the tracked `CIRCUIT.md` and state-read contracts establish
its actual program. No generated screenshot is evidence of native execution.

Physical claims assume ordinary non-adaptive joins, unit/root-circuit relation
runs, and storage configuration wherever indexed files or RSS-driven placement
are discussed. Optional `fetch_join` is configuration-gated. The deck does not
generalize untimed runs' lack of per-record transaction epochs to timed batches.
Placement does not establish checkpoint durability or optimal update I/O.

## Current source map

All paths below are relative to the repository root. The pinned links identify
the inspected committed source. Hashes are SHA-256 of current file bytes.

| Claim | Source | SHA-256 |
| --- | --- | --- |
| Input-map API and typed trace | [local](../../../../crates/dbsp/src/operator/input.rs#L465), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/dbsp/src/operator/input.rs#L465) | `2c8f2bb847180541e9a8020dd3e6f01f7a5ca5ab23c633ca35a0bf63a5b7f379` |
| Input upsert consolidation, immediate sealing and separate append | [local](../../../../crates/dbsp/src/operator/dynamic/input_upsert.rs#L266), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/dbsp/src/operator/dynamic/input_upsert.rs#L266) | `179a44d3650775b7b02b29d6614bd113b190d5b829b04a858a5a636f6b21ab9e` |
| Generic tuple buffering and sealing | [local](../../../../crates/dbsp/src/trace.rs#L865), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/dbsp/src/trace.rs#L865) | `aec031dba25f1a9bc90d48e6923bd2612c6dd0dedc845b719e42de065b2ac338` |
| Unordered buffer sort/consolidation | [local](../../../../crates/dbsp/src/trace/ord/merge_batcher/merge_sorter.rs#L101), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/dbsp/src/trace/ord/merge_batcher/merge_sorter.rs#L101) | `20cd67a98d10fc4bfcde0a8c983adf3b05a9410d9f95f42f2fdee0de24c5182c` |
| Batcher sealing through ordered builder | [local](../../../../crates/dbsp/src/trace/ord/merge_batcher.rs#L41), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/dbsp/src/trace/ord/merge_batcher.rs#L41) | `2662ea14403674eec311bb001acc20bfe7e5040bb29a6a12501eb2db52571352` |
| Memory arrays, ordered push and done | [local](../../../../crates/dbsp/src/trace/ord/vec/indexed_wset_batch.rs#L158), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/dbsp/src/trace/ord/vec/indexed_wset_batch.rs#L158) | `16c7247296ce5c77ef101d0bb5aaebcbbe4c8c8609cebb2bd79dbd9ea9fc60ca` |
| Native join identity, driving direction and output chunks | [local](../../../../crates/dbsp/src/operator/dynamic/join.rs#L698), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/dbsp/src/operator/dynamic/join.rs#L698) | `16261118da53cbfabd880a1a531f274689ba39ddcd00f2264ef87d84880936cc` |
| Default 10,000-record join output target | [local](../../../../crates/feldera-types/src/config/dev_tweaks.rs#L332), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/feldera-types/src/config/dev_tweaks.rs#L332) | `15ba5b5120f501d71b129c9026d84da3df1df6990edab3f9b9e326fd44a4c682` |
| Same-stream trace reuse and delayed visibility | [local](../../../../crates/dbsp/src/operator/dynamic/accumulate_trace.rs#L590), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/dbsp/src/operator/dynamic/accumulate_trace.rs#L590) | `adfd5d76bf4ce4c1913c46901a285dddec48307defd2b1f208e315f646f1d946` |
| Spine insertion, merging, completion, backpressure and spill | [local](../../../../crates/dbsp/src/trace/spine_async.rs#L310), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/dbsp/src/trace/spine_async.rs#L310) | `10ae2112c18230c38d084e27f14c56ded0f474a7487ea9058738477467d9ad01` |
| Memory/file merge destination | [local](../../../../crates/dbsp/src/trace/ord/fallback/indexed_wset.rs#L546), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/dbsp/src/trace/ord/fallback/indexed_wset.rs#L546) | `83aebc36d5139c9fe7f2ccbef7d24213a91bf44fbea260a9927a9c35bda91299` |
| Memory/file indexed type alias | [local](../../../../crates/dbsp/src/trace/ord.rs#L5), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/dbsp/src/trace/ord.rs#L5) | `a929686686eda1db1427423f8eab936f44465c4238d187a65974acf55e5639b7` |
| Strong snapshot references and membership | [local](../../../../crates/dbsp/src/trace/spine_async/snapshot.rs#L55), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/dbsp/src/trace/spine_async/snapshot.rs#L55) | `484feb64e672a3b9577a2c55838c16a9ca0ba5c9b28425d98208e9f34ebd9d73` |
| Nested immutable indexed layer files | [local](../../../../crates/dbsp/src/storage/file.rs#L3), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/dbsp/src/storage/file.rs#L3) | `3b8785119f567c44d73c2abc8ef9d09a4bd017b2d71619c77261b836956a3a2b` |
| Cursor navigation | [local](../../../../crates/dbsp/src/trace/cursor.rs#L1), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/dbsp/src/trace/cursor.rs#L1) | `b7e57451f437aef2a2fca362bea64a4ea535f0648d85e6e7e56d765fcf26d81e` |
| Consolidation across retained runs | [local](../../../../crates/dbsp/src/trace/cursor/cursor_list.rs#L1), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/dbsp/src/trace/cursor/cursor_list.rs#L1) | `d248aeff29636d1b3b341ab04f6042de2677fc9264119575380d214282683113` |
| Final-owner temporary-file deletion | [local](../../../../crates/dbsp/src/storage/backend/posixio_impl.rs#L615), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/dbsp/src/storage/backend/posixio_impl.rs#L615) | `29a11790672dd3f8e510a653c715e4dc853a0e475dc2afe06dc6d7f19574fa33` |
| RSS sampling, hysteresis and placement | [local](../../../../crates/dbsp/src/circuit/runtime.rs#L848), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/dbsp/src/circuit/runtime.rs#L848) | `fa336bffb66b8447b59f7450aa7eb3b345bcd9067dea1e45e47fbc2e8982c5c3` |
| Raw 85/90/95 percent thresholds | [local](../../../../crates/feldera-types/src/memory_pressure.rs#L6), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/feldera-types/src/memory_pressure.rs#L6) | `4192301884a40296c439e361036d1461e18387d91642dfec3d959a2627485c35` |
| Cache allocation context | [local](../../../../crates/dbsp/src/storage/buffer_cache/cache.rs#L1), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/crates/dbsp/src/storage/buffer_cache/cache.rs#L1) | `5daf84ed1c6a3f8a47e35a6bd9655816e9c6d55bff9070520e12b9ad6e281bed` |
| Native memory accounting documentation | [local](../../../../docs.feldera.com/docs/operations/memory.md#L1), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/docs.feldera.com/docs/operations/memory.md#L1) | `549fe82b9949565fa5f40e3f5c379fa79bd4ec913442fbe5090c3dd60c84543c` |
| Actual experiment state-read contracts and B payload | [local](../../../../docs/design/merged-index/native-state-read-contracts.md#L47), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/docs/design/merged-index/native-state-read-contracts.md#L47) | `159c60eaae538a91ce29bd768566d4eccb9146287d272df1a18de5f22a7601da` |
| Actual optimized experiment order | [local](../../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md#L140), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md#L140) | `18e4881d7a269e94da142418d9c34ed73a3f0f6ddb0045bc99b0ac161900db73` |
| Restricted RF1/RF2 workload and qualification | [local](../../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/README.md#L50), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/sql-to-dbsp-compiler/experiments/q3-ivm-baseline/README.md#L50) | `47fc8a47a741aba3916e5d873d212fbc1ed574c3254410e884c044a262ad9a05` |
| Actual SQL projection | [local](../../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/query.sql#L1), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/sql-to-dbsp-compiler/experiments/q3-ivm-baseline/query.sql#L1) | `b803f5261ecddbdc361bb3935ae609a790a37eb6b23c25439aad1e5fb24cc435` |
| Experiment workflow | [local](../../../../docs/design/merged-index/WORKFLOW.md#L20), [pinned](https://github.com/feldera/feldera/blob/415bd4940a91e09eef4c2502c5195db00fd5806d/docs/design/merged-index/WORKFLOW.md#L20) | `11d52bb48a55bda52f2169e6138cdab4fe45317d32cea709ed10c31dc0d076a4` |

## Construction, retention, and size evidence

- **Immediate input sealing and separate retention:**
  `dynamic/input_upsert.rs:601–622,680–691` consolidates per-key updates,
  pushes ordered records, and returns `builder.done()`. Its circuit construction
  at `303–333` feeds the resulting delta to a separate `UntimedTraceAppend`.
  The source is **dynamic/input_upsert.rs**, not `operator/input.rs:321`.
- **Join buffering and consolidation:** `join.rs:1921–1957` appends tuples to
  a buffer, submits it to the batcher, yields sealed chunks, and seals remaining
  output. `trace.rs:865–875` exposes tuple-buffer push followed by seal.
  `merge_batcher/merge_sorter.rs:101–130,293–309` sorts/consolidates complete
  key/value pairs and removes zero totals. `merge_batcher.rs:65–85` finishes
  sorting/merging and constructs the ordered builder.
- **Individual memory-builder pushes do not consolidate:**
  `indexed_wset_batch.rs:734–773,906–930` requires ordered keys/values,
  appends values and weights, and constructs arrays in `done()`. Input upsert
  performs its own consolidation before these pushes.
- **Separate runs and merging in memory:** `spine_async.rs:310–365,446–463`
  schedules merges and retains incoming runs as loose runs.
  `fallback/indexed_wset.rs:546–560` explicitly selects memory or storage for
  merge output. These paths seal outputs before retention and lack a mutable
  memtable that absorbs updates across transactions. This is an inspected-path
  structural limitation, not a claim about every DBSP operator or measured cost.
- **Run-size and transaction questions:** one weighted record can be sealed.
  A transaction can produce several per-step/chunk runs. Merges can combine
  contributions from several transactions and eliminate canceling pairs.
  No fixed transactions-per-run or general run-size cap follows from these paths.
  `dev_tweaks.rs:332` defaults the join-output target to 10,000 records.
  `join.rs:1926–1957` checks buffered output at roughly one-third-target intervals, so the
  target can be overshot, and seals final partial output. Its comment describes
  33% overshoot for that chunking strategy; the deck treats the size as a target.
  This target does not cap input-map or merged runs. Diagram sizes and merge
  timing are illustrative, not the native scheduling rule.
- **Trace sharing:** `accumulate_trace.rs:590–640` caches trace construction
  by indexed stream identity, and `773–789,1416–1429` obtains delayed visibility
  and snapshots from that same trace. Before/after consumers share state, and
  consumers of the same indexed stream can also reuse trace construction.
  These logical roles imply neither exclusive sharing nor a fixed allocation count.
- **Snapshots and reclamation:** `snapshot.rs:55–63,170–179` retains strong
  references to immutable runs. Merge machinery retains sources while building
  output; `spine_async.rs:527–575` installs replacement output and releases spine
  source references. Other owners, including old snapshots, may retain sources.
  Reclamation follows final strong-owner release. Spill can copy shared memory
  (`2457–2504`). Temporary file reader ownership and deletion are in
  `posixio_impl.rs:615–635`; checkpoint-marked/reopened persistent files differ.
- **Memory controls:** `runtime.rs:848–904` samples RSS roughly once per second
  and applies hysteresis. Effective High persists until raw Low; Critical until
  raw Moderate or Low. `1269–1356` selects inserted, merged, and transient run
  placement. Raw thresholds are 85/90/95% of configured `max_rss_bytes`.
  Moderate inserted-run placement uses configured `min_storage_bytes` or
  default 10 MiB. `spine_async.rs:680–691` separately pauses insertion at 128
  loose runs; `455–463` excludes in-progress merge sources from that count.
  This is backlog control, not a byte quota. The pressure path does not resize
  storage cache, and pinned shared allocations need not be reclaimed immediately.

## Explanatory and actual programs

Slides 2 and 5–7 use the equivalent explanatory Orders–Lineitem-first program:

```text
ΔOL_c,o = ΔL_o ⋈ O_o+ + L_o− ⋈ ΔO_o
ΔV = ΔOL_c,o ⋈ C_c+ + OL_c,o− ⋈ ΔC_c
V+ = V− + ΔV
```

`OL_c,o` has native Customer ID key and values led by Order ID, then line
number. Its value retains Order date, priority, price, discount, and ship date.
Customer changes leave this intermediate unchanged and read its retained rows
directly. Rows are identified by fields, such as `(7;43,1,…)`, not relation
subscripts. Visibility superscripts are distinct from signed weights.

The existing experiment and RF1/RF2 prototype use Orders–Customer first:

```text
ΔB_o = ΔO_c ⋈ C_c+ + O_c− ⋈ ΔC_c
ΔV = ΔL_o ⋈ B_o+ + L_o− ⋈ ΔB_o
V+ = V− + ΔV
```

Its actual accesses remain `C_c+`, `O_c−`, `B_o+`, and `L_o−`.
Slide 12 and Appendix D explicitly identify that different order. Prototype
support remains subsequent RF1 complete Order/Lineitem insertion and RF2
deletion under referential integrity after setup. Customer updates, replacements,
key moves, orphans, and independent Lineitem updates remain excluded.
General native signed examples do not extend prototype scope.

## Theory and visual provenance

[Budiu et al., DBSP, VLDB 2023](https://docs.feldera.com/vldb23.pdf) supports
integration (§2.3), bilinear incremental joins (Theorem 3.4), weighted relations
(§4), and indexed/nested groups (§7.3). These sections were checked again on
2026-10-08. Physical claims come from the implementation above. The §7.3
citation concerns indexed groups; §7.4 concerns aggregation.

Reference source: `/Users/alicialyu/Local/query_execution_using_MI/advisor-deck/main.tex`,
SHA-256 `61e71ddcbe1218cafe03f314d947cc00c0e4efcc8a9832a3edec49ebc9e367a4`.
Libertine/NewTX typography, ink titles, teal rules, and editable TikZ diagrams
preserve that style. The copied opening backdrop remains in
`figures/query-update-backdrop.tex` (SHA-256
`c29f087dbd7231793559c5388218bbe8a27e02b0d487920596be78e3f77e3f7d`)
for provenance; the final revision does not display it. Worked joins use teal
for active information and readable quiet ink for completed steps. Amber marks
deferred work. All diagrams are editable and illustrative.

## Verification and delivery

The standalone checker asserts exact branches for both orders, simultaneous
insertion/deletion, Customer/Order/Lineitem overwrites, Customer deletion,
the overwrite-then-deletion sequence, multiplicities, and cancellation.
It compares intermediate changes and integrated output with full evaluation
for 729 nonnegative and 729 signed cases. It does not simulate storage, retest
runtime execution, or measure memory/performance.

The current deck preserves the user's removal of the two worked-join topics.
It has 12 main topics, two advances each on slides 3, 6, and 9, three
on slide 4, and five appendices, with four advances in Appendix E: 25 rendered
pages in each PDF.
Slide 3 shows six unconnected runs with independent downward cursors and
one enlarged immutable file run. Its key and value trees
follow `storage/file.rs:3–65`; `(K, ())` and `(V, weight)` follow
`trace/ord/file/indexed_wset_batch.rs:41–83`. Row-group metadata and index
block offsets follow `storage/file/format.rs:15–62`. The notes distinguish
per-key value ordering and overlapping runs from implementation scheduling.
Construction and traversal sources and qualifications are detailed below.
The build and visual-review procedure is in [BUILD.md](BUILD.md). The deck claims no measured savings, update costs, or
comparative performance results.

## Immutable-run construction and row traversal, 2026-10-09

This addition inspected the files below at checkout
`9ec749f84665eb8ba48f4036152020afd9bbf45d`. Their hashes identify the source
bytes used; the earlier source-map identity remains a historical record.

| Source (repository-relative) | SHA-256 |
| --- | --- |
| `crates/dbsp/src/storage/file/writer.rs` | `d63377d0e771477423bfb5f3bb7943d86cd5dfb280e6843f4d207747b02c533f` |
| `crates/dbsp/src/storage/file/reader.rs` | `780f4a969197d880d34af5e6a689d0203bf3b1472b69c7e13e26a95cf531502f` |
| `crates/dbsp/src/storage/file/format.rs` | `9e97e5d3c7cc7130ed54f9ef13e0c1e3ad8f4d893367ec2797513af9ad09feb3` |

Native “columns” describe nested file trees, not SQL attribute columns.
For an untimed indexed weighted run, tree 0 stores `(K, ())` with separate
row-group metadata selecting tree 1's `(V, weight)` items. Slide 4's callout
draws these as a key array and a value array.
This terminology prevents the file layout from being mistaken for a SQL
column store.

- `writer.rs:224–246` initializes the tree's row range and takes pending rows
  by replacing the range with `[end,end)`. `writer.rs:1234–1255` takes a
  nonempty child group before incrementing the parent counter and appending.
  `writer.rs:1660–1737` exposes `write0`, `write1`, and `close`. `write1`
  takes an item, with no position argument. Construction spans many calls
  and completes at `close`; the callout's one-run case is illustrative.
- The tree's counters survive data-leaf flushes: `ColumnWriter::add_item`
  (`writer.rs:411–427`) writes a completed leaf after `DataBlockBuilder::add_item`
  (`writer.rs:588–605`) builds the full leaf and retries the item. Neither
  resets `ColumnWriter.rows`. Each tree's row ordinals are file-wide.
- `reader.rs:648–670` reads adjacent row-boundary entries. Key 43's item
  index 1 selects entries 1 and 2 of `[0,2,4,6]`, hence `[2,4)`.
- `format.rs:50–62` and `reader.rs:1124–1186` establish cumulative row
  totals and descriptor units. For the illustrated branch starting at row 0,
  row 2 selects child 1 from `[2,4,6]`, with offset 16 and size 1: byte offset
  8192 and size 512. Other children and unused value bounds are omitted.
- `writer.rs:662–680` writes the leaf header/map;
  `reader.rs:560–570,673–706` locates an item via the map and derives local
  index from `row - first_row`. The leaf's first row 2 is traversal context
  derived from the preceding cumulative total, not an on-disk header field.
  Symbolic block offsets `m`, `a`, and `b` avoid invented serialization sizes.
  The map illustration uses the supported explicit-array case; the native
  format also supports fixed-stride encoding.

Appendix E draws exactly the key leaf, value branch, and selected value leaf.
It iterates rows 2 and 3 through the two map entries to `(8,+1)` and `(15,+1)`,
then stops before row 4. It omits key-item offsets and unused branch value
bounds. These numeric values are a compact layout example, not complete
Orders payloads or a measurement of block packing.
