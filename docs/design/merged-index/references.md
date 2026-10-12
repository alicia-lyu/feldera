# References

Sources outside this repository's code, grouped by role. Local links assume the sibling-checkout
layout (`../../../../` from this directory reaches the parent of the `feldera` checkout).
Public links pin the inspected revisions. Treat each source as a claim about its own subject;
this repository's code and tests decide what the prototype does.

## Design manuscripts and notes

| Source | Role |
| --- | --- |
| [Interesting-orderings manuscript](../../../../merged_index_interesting_orderings/main.tex#L515) | Primary design reference: order-sharing pipelines, heterogeneous folded keys, backend reuse, and the split between pipeline output and remaining query work. |
| [Same manuscript, experiments section](../../../../merged_index_interesting_orderings/sections/experiments_revised.tex#L173) | Defines the refresh workload and reports backend-dependent results, including the LSM comparison. |
| [Multi-pipeline query execution manuscript](../../../../query_execution_using_MI/main.tex#L88) | Future direction: composing pipelines through intermediate views. Outside this project's scope. |
| [Maintaining a Query, One Change at a Time](../../../../DBSP_w_merged_index/dbsp-merged-index-feasibility.tex#L276) | Lecture note with the historical filtered, aggregate-first Q3 circuit and signed-delta algebra. Its five-state circuit is not the view `V` maintained here. |
| [Q3 circuit figure source](../../../../DBSP_w_merged_index/figures/q3-dbsp-circuit.tex#L59) | Historical grouping, delay, and join wiring, kept for comparison. |
| [Operator-state guide](../../../../DBSP_w_merged_index/operator-state.tex#L39) | Historical aggregate-first example of group existence. |
| [`mi_db` architecture, scan-session section](../../../../mi_db/docs/architecture.md#merged-index-scan-sessions) | Basis of the deferred shared-scan-session design in [deferred.md](deferred.md). Only this section was validated; the buffer guard and the typed-buffer sections were not. The inspected copy was modified working-tree content from 2026-09-28, based on `mi_db` revision `d50d29dfe559258351c1071e59ca5365eacf09d0` (SHA-256 `d043b9b28121fe044980fde032d95e4b00c331e1d7a137ce709bd70eb6914e79`). |

## LeanStore implementation evidence

All links pin `alicia-lyu/leanstore` at `305ad0a98b147d048a37a1eba3787b35b1181b85`.

| Source | Role |
| --- | --- |
| [B-tree merged-index adapter](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/shared/adapter-scanner/LeanStoreMergedAdapter.hpp#L25) | Working B-tree storage with record-specific folded byte keys and payloads. |
| [RocksDB merged-index adapter](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/shared/adapter-scanner/RocksDBMergedAdapter.hpp#L22) | The same design on LSM storage. |
| [Q3 execution](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/q3/query.tpp#L446) | Customer–Orders–Lineitem scan, per-order aggregation, top-10 selection. It does not establish incremental maintenance. |
| [Q5 implementation](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/q5/query.tpp#L219-L335) | External Nation/Region check, supplier-field use, and actual date bounds a Q5 comparison must preserve. |
| [Q10 logical plan](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/q10/plans/family_logical.dot#L34-L69) | Selected join boundary and downstream aggregation. |
| [Q10 visitor](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/q10_family/visitor.hpp#L126-L200) | Customer and returned-line payloads that downstream consumers need. |
| [Q10 date bounds](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/q10/query.tpp#L100-L107) | The concrete day-offset predicate, which a comparison must match. |
| [Refresh discovery](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/tpch_family/refresh.hpp#L143-L178) | RF2 parent lookup and line-number discovery. |
| [Merged-index maintenance](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/tpch_family/col_pipeline.tpp#L150-L220) | Insertion and erasure paths whose read and write work belongs in maintenance accounting. |

## RocksDB

| Source | Role |
| --- | --- |
| [RocksDB overview](https://github.com/facebook/rocksdb/wiki/RocksDB-Overview) | Memtable, sorted-string-table, and key-value baseline for the storage comparison. |
| [RocksDB merge operator](https://github.com/facebook/rocksdb/wiki/Merge-Operator) | Application-defined update combination; weighted addition is not a capability RocksDB lacks. |

## Feldera sources

These files are in this repository. The links pin revision
`f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c`, so line numbers describe that revision.
Prefer the current files and their symbol names.

| Source (under `crates/dbsp/src/`) | Role |
| --- | --- |
| [`trace.rs`](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace.rs#L231-L308) | Weighted collections, the read interface, insertion of immutable batches. |
| [`trace/cursor.rs`](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/cursor.rs#L42-L109) | Key-then-value iteration, forward seeks, time and weight access. |
| [`trace/spine_async.rs`](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/spine_async.rs#L1-L7) | Collection of immutable batches and background merging. |
| [`trace/spine_async/snapshot.rs`](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/spine_async/snapshot.rs#L23-L43) | Snapshot acquisition and the combined cursor over a stable view. |
| [`trace/cursor/cursor_list.rs`](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/cursor/cursor_list.rs#L150-L174) | Weight addition across batches; zero totals are suppressed. |
| [`storage/file.rs`](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/storage/file.rs#L3-L65) | Immutable nested groups, per-level tree indexes, typed comparisons. |
| [`trace/ord/vec/indexed_wset_batch.rs`](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/vec/indexed_wset_batch.rs#L158-L199) | In-memory sorted keys, value offsets, values, and weights. |
| [`trace/ord/fallback/indexed_wset.rs`](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/fallback/indexed_wset.rs#L34-L58) | Retained indexed state may be memory-backed or file-backed. |
| [`trace/ord/file/indexed_wset_batch.rs`](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/trace/ord/file/indexed_wset_batch.rs#L447-L478) | Batched key fetching and file batch construction. |
| [`time.rs`](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/time.rs#L214-L219) | The root circuit uses weighted batches without per-record timestamps. |
| [`operator/dynamic/join.rs`](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/join.rs#L698-L749) | Delta against current-state and delayed-state wiring; batched-fetch path. |
| [`operator/dynamic/aggregate.rs`](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/aggregate.rs#L766-L796) | Retained-input and previous-output strategy of generic aggregation. |
| [`operator/dynamic/upsert.rs`](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/operator/dynamic/upsert.rs#L90-L109) | Retained-output feedback that turns replacements into retraction plus insertion. |
| [`circuit/schedule.rs`](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/schedule.rs#L186-L226) | Why a transaction may span several steps. |
| [`circuit/circuit_builder.rs`](https://github.com/feldera/feldera/blob/f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c/crates/dbsp/src/circuit/circuit_builder.rs#L7777-L7805) | Commit-flush behavior. |

## Feldera documentation

- [Pipeline API](https://docs.feldera.com/api/)
- [SQL view grammar](https://docs.feldera.com/sql/grammar/), including `CREATE LOCAL VIEW` and index rules
- [Materialized views](https://docs.feldera.com/sql/materialized/)
- [DBSP paper (VLDB 2023)](https://docs.feldera.com/vldb23.pdf)

## Provenance of pinned sources

Inspected on 2026-09-28 unless noted. Local manuscript links assume sibling checkouts under the same
parent as Feldera; their `#L` fragments are source-viewer line locators only.

| Source | Pin |
| --- | --- |
| Feldera links above | `f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c` |
| LeanStore | `305ad0a98b147d048a37a1eba3787b35b1181b85` |
| DBSP lecture note (`DBSP_w_merged_index`) | `ac8380511fe4463b81651a3f6d6991b849577ff7`; cited files clean |
| Interesting-orderings manuscript, experiments file | `6c4c3a5d851c9044352da2aead6b419c45c77237`; clean |
| Interesting-orderings manuscript, `main.tex` | Modified working-tree content, SHA-256 `44bf60963e6b94f6aaad04cdacea3ecc31e248a95cb10dd1118ff2e6fa17a9c2` |
| Multi-pipeline manuscript (`query_execution_using_MI`) | `e485dd5b5775cabf9cfbf0aa44a0e5bd64a8f925`; clean |
| `mi_db` architecture document | Revision and SHA-256 given in the table above; also at the top of the scan-session section of [deferred.md](deferred.md) |

The interesting-orderings manuscript is the primary architectural reference; LeanStore supplies
working storage and query evidence.
