# Source and visual evidence

## Inspection identity and configuration

Repository: `feldera/feldera`, inspected checkout revision
[`7d3628513c390ba8053adf54f6c12e06fc901e37`](https://github.com/feldera/feldera/tree/7d3628513c390ba8053adf54f6c12e06fc901e37).
Inspection began with unrelated working-tree modifications, including changes to the
native spine and the RF1/RF2 experiment. A pinned GitHub link therefore
identifies the clean revision, while SHA-256 values below identify the exact
working-tree source used for claims that depend on modified files.

The deck describes ordinary, non-adaptive native joins. Storage is configured
for claims about file-backed traces and RSS-driven placement. `fetch_join` is
optional and configuration-gated. The batches discussed for the maintained
relations are unit/root-circuit batches; the deck does not generalize the
no-per-record-epoch statement to DBSP timed batches. No generated circuit
snapshot or screenshot is used as evidence of native execution.

Concurrent workspace work later committed native changes as
`4001ac3a0f7e29d36bb3134a6a26300513956d35`. Documentation remained in progress.
All listed hashes identify the exact source inspected, including that concurrent
work. Status labels below describe differences from the initial revision,
rather than final `git status`.

## Source map

| Claim | Local source and location | Pinned source |
| --- | --- | --- |
| Input-map entry point and typed trace wrapper | [`input.rs`](../../../../crates/dbsp/src/operator/input.rs#L465), [`typed_batch.rs`](../../../../crates/dbsp/src/typed_batch.rs#L577) | [input map API](https://github.com/feldera/feldera/blob/7d3628513c390ba8053adf54f6c12e06fc901e37/crates/dbsp/src/operator/input.rs#L465), [typed Spine](https://github.com/feldera/feldera/blob/7d3628513c390ba8053adf54f6c12e06fc901e37/crates/dbsp/src/typed_batch.rs#L577) |
| Persistent input construction and overwrite semantics | [`input_upsert.rs`](../../../../crates/dbsp/src/operator/dynamic/input_upsert.rs#L266) | [input_upsert.rs at inspected revision](https://github.com/feldera/feldera/blob/7d3628513c390ba8053adf54f6c12e06fc901e37/crates/dbsp/src/operator/dynamic/input_upsert.rs#L266) |
| Memory/file indexed trace selection | [`ord.rs`](../../../../crates/dbsp/src/trace/ord.rs#L5), [`indexed_wset.rs`](../../../../crates/dbsp/src/trace/ord/fallback/indexed_wset.rs#L545) | [ord.rs](https://github.com/feldera/feldera/blob/7d3628513c390ba8053adf54f6c12e06fc901e37/crates/dbsp/src/trace/ord.rs#L5), [fallback indexed batches](https://github.com/feldera/feldera/blob/7d3628513c390ba8053adf54f6c12e06fc901e37/crates/dbsp/src/trace/ord/fallback/indexed_wset.rs#L545) |
| Immutable indexed layer-file structure | [`file.rs`](../../../../crates/dbsp/src/storage/file.rs#L3) | [file.rs](https://github.com/feldera/feldera/blob/7d3628513c390ba8053adf54f6c12e06fc901e37/crates/dbsp/src/storage/file.rs#L3) |
| In-memory key/value groups and offsets | [`indexed_wset_batch.rs`](../../../../crates/dbsp/src/trace/ord/vec/indexed_wset_batch.rs#L158) | [indexed_wset_batch.rs](https://github.com/feldera/feldera/blob/7d3628513c390ba8053adf54f6c12e06fc901e37/crates/dbsp/src/trace/ord/vec/indexed_wset_batch.rs#L158) |
| Join branches and optional fetch path | [`join.rs`](../../../../crates/dbsp/src/operator/dynamic/join.rs#L698) | [join.rs](https://github.com/feldera/feldera/blob/7d3628513c390ba8053adf54f6c12e06fc901e37/crates/dbsp/src/operator/dynamic/join.rs#L698) |
| Logical before/current reads and state inventory | [`native-state-read-contracts.md`](../native-state-read-contracts.md#L47) | [native-state-read-contracts.md](https://github.com/feldera/feldera/blob/7d3628513c390ba8053adf54f6c12e06fc901e37/docs/design/merged-index/native-state-read-contracts.md#L47) |
| Snapshot strong references | [`snapshot.rs`](../../../../crates/dbsp/src/trace/spine_async/snapshot.rs#L55) | [snapshot.rs](https://github.com/feldera/feldera/blob/7d3628513c390ba8053adf54f6c12e06fc901e37/crates/dbsp/src/trace/spine_async/snapshot.rs#L55) |
| Merge insertion, completion, and pressure policy | [`spine_async.rs`](../../../../crates/dbsp/src/trace/spine_async.rs#L455) | [spine_async.rs at inspected revision](https://github.com/feldera/feldera/blob/7d3628513c390ba8053adf54f6c12e06fc901e37/crates/dbsp/src/trace/spine_async.rs#L455) |
| Temporary-file reader ownership and deletion | [`posixio_impl.rs`](../../../../crates/dbsp/src/storage/backend/posixio_impl.rs#L615) | [posixio_impl.rs](https://github.com/feldera/feldera/blob/7d3628513c390ba8053adf54f6c12e06fc901e37/crates/dbsp/src/storage/backend/posixio_impl.rs#L615) |
| RSS sampling and spill thresholds | [`runtime.rs`](../../../../crates/dbsp/src/circuit/runtime.rs#L675) | [runtime.rs at inspected revision](https://github.com/feldera/feldera/blob/7d3628513c390ba8053adf54f6c12e06fc901e37/crates/dbsp/src/circuit/runtime.rs#L675) |
| Exact 85/90/95% constants | [`memory_pressure.rs`](../../../../crates/feldera-types/src/memory_pressure.rs#L6) | [pressure thresholds](https://github.com/feldera/feldera/blob/7d3628513c390ba8053adf54f6c12e06fc901e37/crates/feldera-types/src/memory_pressure.rs#L6) |
| Cache accounting context | [`cache.rs`](../../../../crates/dbsp/src/storage/buffer_cache/cache.rs) | [cache.rs](https://github.com/feldera/feldera/blob/7d3628513c390ba8053adf54f6c12e06fc901e37/crates/dbsp/src/storage/buffer_cache/cache.rs) |
| Memory terminology and process allocations | [Feldera memory operations guide](https://docs.feldera.com/operations/memory/) | Accessed 2026-10-08; also inspected local `docs.feldera.com/docs/operations/memory.md`. |
| Restricted reconstruction prototype | [`q3-ivm-baseline/README.md`](../../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/README.md#L50), [`WORKFLOW.md`](../WORKFLOW.md#L20) | [experiment README at inspected revision](https://github.com/feldera/feldera/blob/7d3628513c390ba8053adf54f6c12e06fc901e37/sql-to-dbsp-compiler/experiments/q3-ivm-baseline/README.md#L50), [workflow at inspected revision](https://github.com/feldera/feldera/blob/7d3628513c390ba8053adf54f6c12e06fc901e37/docs/design/merged-index/WORKFLOW.md#L20) |
| Implemented schema, projection, and documented circuit | [`query.sql`](../../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/query.sql), [`CIRCUIT.md`](../../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md) | [query.sql](https://github.com/feldera/feldera/blob/7d3628513c390ba8053adf54f6c12e06fc901e37/sql-to-dbsp-compiler/experiments/q3-ivm-baseline/query.sql), [circuit explanation](https://github.com/feldera/feldera/blob/7d3628513c390ba8053adf54f6c12e06fc901e37/sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CIRCUIT.md) |
| IVM theory and indexed nested groups | [DBSP, VLDB 2023](https://docs.feldera.com/vldb23.pdf): integration §2.3, incremental joins Theorem 3.4, weighted relations §4, nested indexed groups §7.3 | The paper supports logical foundations; current physical claims come from implementation sources above. |

The §7.3 citation is intentional: it defines indexed/nested groups. §7.4
concerns aggregation and is not the source for the grouping representation in
this join-only deck.

## Working-tree source hashes

SHA-256 was computed over the inspected file bytes. Files marked **modified**
were dirty relative to the stated revision; their pinned links show the base
revision for navigation, not the exact modified content. The deck's detailed
claims about those files were checked against this local working tree.

| Status | File | SHA-256 |
| --- | --- | --- |
| modified | `crates/dbsp/src/circuit/runtime.rs` | `fa336bffb66b8447b59f7450aa7eb3b345bcd9067dea1e45e47fbc2e8982c5c3` |
| modified | `crates/dbsp/src/trace/spine_async.rs` | `10ae2112c18230c38d084e27f14c56ded0f474a7487ea9058738477467d9ad01` |
| modified | `sql-to-dbsp-compiler/experiments/q3-ivm-baseline/README.md` | `47fc8a47a741aba3916e5d873d212fbc1ed574c3254410e884c044a262ad9a05` |
| modified | `docs/design/merged-index/WORKFLOW.md` | `11d52bb48a55bda52f2169e6138cdab4fe45317d32cea709ed10c31dc0d076a4` |
| clean at inspected revision | `crates/dbsp/src/operator/dynamic/input_upsert.rs` | `179a44d3650775b7b02b29d6614bd113b190d5b829b04a858a5a636f6b21ab9e` |
| clean at inspected revision | `crates/dbsp/src/trace/ord.rs` | `a929686686eda1db1427423f8eab936f44465c4238d187a65974acf55e5639b7` |
| clean at inspected revision | `crates/dbsp/src/storage/file.rs` | `3b8785119f567c44d73c2abc8ef9d09a4bd017b2d71619c77261b836956a3a2b` |
| clean at inspected revision | `crates/dbsp/src/trace/ord/vec/indexed_wset_batch.rs` | `16c7247296ce5c77ef101d0bb5aaebcbbe4c8c8609cebb2bd79dbd9ea9fc60ca` |
| clean at inspected revision | `crates/dbsp/src/trace/ord/fallback/indexed_wset.rs` | `83aebc36d5139c9fe7f2ccbef7d24213a91bf44fbea260a9927a9c35bda91299` |
| clean at inspected revision | `crates/dbsp/src/operator/dynamic/join.rs` | `16261118da53cbfabd880a1a531f274689ba39ddcd00f2264ef87d84880936cc` |
| clean at inspected revision | `crates/dbsp/src/trace/spine_async/snapshot.rs` | `484feb64e672a3b9577a2c55838c16a9ca0ba5c9b28425d98208e9f34ebd9d73` |
| clean at inspected revision | `crates/dbsp/src/storage/backend/posixio_impl.rs` | `29a11790672dd3f8e510a653c715e4dc853a0e475dc2afe06dc6d7f19574fa33` |
| clean at inspected revision | `crates/dbsp/src/storage/buffer_cache/cache.rs` | `5daf84ed1c6a3f8a47e35a6bd9655816e9c6d55bff9070520e12b9ad6e281bed` |
| clean at inspected revision | `docs/design/merged-index/native-state-read-contracts.md` | `159c60eaae538a91ce29bd768566d4eccb9146287d272df1a18de5f22a7601da` |

## Verification record

`verify_examples.py` completed successfully and reported: “729 small weighted
before/after states checked against full evaluation.” This is the exact
enumeration performed by that local algebra checker. It is not a full DBSP
runtime test or a rerun of the entire experiment harness. No generated circuit
artifact or screenshot is included or used as evidence, so no in-place circuit
regeneration is needed for this deck.

## Visual provenance

Reference source: `/Users/alicialyu/Local/query_execution_using_MI/advisor-deck/main.tex`
(SHA-256 `61e71ddcbe1218cafe03f314d947cc00c0e4efcc8a9832a3edec49ebc9e367a4`).
The reference deck's slide 3 query-time/update-time backdrop was copied into
[`figures/query-update-backdrop.tex`](figures/query-update-backdrop.tex); the
copy SHA-256 is
`c29f087dbd7231793559c5388218bbe8a27e02b0d487920596be78e3f77e3f7d`. It
retains the figure's teal and amber role meanings. The macros retain the reference content. The local figure omits the old
question-mark connector, and the frame adds the physical-design question; all later figures are
editable TikZ made for this deck. The source reference's typography and visual
language informed the Libertine/NewTX fonts, ink titles, and teal rules.

## Delivery checks

On 2026-10-08, both PDFs compiled with TeX Live 2024 / latexmk 4.83.
All 19 presentation pages and all 19 corresponding notes pages were rendered
with Poppler and inspected. Layout corrections removed overlapping labels,
ambiguous crossings, and an awkward final table row. The presentation has
16 main slides followed by three appendix slides. There are no TeX overflow
or font-substitution warnings. The notes build disables PDF hyperlinks to
avoid duplicate destinations from Beamer's two-screen layout; presentation
links remain active.

The algebra checker also asserts the complete Customer-overwrite sequence,
subsequent simultaneous deletion, and Order 42 priority replacement. Its
729-case enumeration and branch assertions pass. These validate examples,
not runtime I/O, memory use, or prototype performance.

The requested dated advisor guidance was installed at
`~/.codex/advisor-working-style.md`, with a pointer appended to
`~/.codex/AGENTS.md`. Those user-level files are outside this repository.
