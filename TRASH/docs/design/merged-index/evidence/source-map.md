# Source map and evidence provenance

Inspected on 2026-09-28. Line numbers in the reports refer to these revisions or the explicitly identified
working-tree content. No sibling checkout was modified.

## Revisions

| Source ID | Checkout / revision | Provenance and role |
| --- | --- | --- |
| Feldera | `f3c06614f53b1c01e0f6b8745d690ad6a2bcac7c` | Unmodified runtime/compiler baseline from `feldera/feldera`, after fetching and merging `origin/main`. Documentation commits are later and do not alter implementation. |
| DBSP note | `DBSP_w_merged_index` at `ac8380511fe4463b81651a3f6d6991b849577ff7` | Supplied lecture note, operator-state guide, and semantic checkers. `main.tex` has an unrelated local change; cited note and checker files are clean. |
| MI paper | `merged_index_interesting_orderings` at `6c4c3a5d851c9044352da2aead6b419c45c77237` | Supplied local manuscript and active `sections/experiments_revised.tex`. `main.tex` has a local comment change; its inspected content is hashed below. `poster.tex` and local agent metadata also differ but are not evidence. |
| Pipeline manuscript | `query_execution_using_MI` at `e485dd5b5775cabf9cfbf0aa44a0e5bd64a8f925` | Clean local manuscript; contextual pipeline source only. Unfinished or out-of-scope portions supply no performance evidence. |
| `mi_db` | `d50d29dfe559258351c1071e59ca5365eacf09d0` **plus working-tree changes** | Architecture and scan-session proposals. All five Markdown documents are locally modified; hashes below identify what was read. This is not an implemented runtime. |
| LeanStore | `305ad0a98b147d048a37a1eba3787b35b1181b85` | `alicia-lyu/leanstore` query/adapter implementation. Only `paper-data/diagrams.yaml` and `paper-data/scripts/plot_paper_sweep.py` are modified; cited implementation is clean. |
| Sibling Calcite | `d87f6dac0ace51c434c58092016c9e26b6b7c302` | Clean `alicia-lyu/calcite` checkout. Its custom rules are research inputs, not evidence that Feldera uses them. |

The user intends to create `alicia-lyu/feldera` and use it as upstream. At the time of investigation that fork
is not yet created. Work remains on local branch `docs/single-pipeline-merged-index`; `origin` still names
`feldera/feldera`. No push or fork creation is part of this documentation delivery. The public baseline
citations remain pinned even after the future remote changes.

## Local source navigation

The relative links below assume the supplied sibling checkout layout under `/Users/alicialyu/Local/`. They are
usable locally but are not portable GitHub links. `#L` fragments are line locators for source viewers; plain
Markdown viewers may not implement them. Feldera and clean public sibling code use pinned GitHub links in the
reports where available. Local manuscript provenance is deliberately not represented as a published paper or a
publicly retrievable revision.

| Source | Entry point and useful locations |
| --- | --- |
| DBSP note | [Feasibility source](../../../../../../DBSP_w_merged_index/dbsp-merged-index-feasibility.tex#L156): Q3 definitions 156–186; join derivation 336–399; five integrators 405–418; reconstruction 508–579; proposed experiments 581–611. |
| Operator-state guide | [operator-state.tex](../../../../../../DBSP_w_merged_index/operator-state.tex#L39): support keys and aggregate existence 39–57; other state families 68–245. |
| Q3 semantic model | [check_q3.py](../../../../../../DBSP_w_merged_index/validation/check_q3.py#L121): base/pending reconstruction 121–203; independent oracle 43–74; tests 391–648. |
| Operator semantic model | [check_operators.py](../../../../../../DBSP_w_merged_index/validation/check_operators.py): exact-arithmetic operator-family checks. |
| Prior decisions | [Note README](../../../../../../DBSP_w_merged_index/README.md), [buffer](../../../../../../DBSP_w_merged_index/BUFFER.md), [original feasibility plan](../../../../../../DBSP_w_merged_index/plans/dbsp-merged-index-feasibility.md), [Q3 coherence plan](../../../../../../DBSP_w_merged_index/plans/coherent-merged-index-q3.md), [expose accumulations](../../../../../../DBSP_w_merged_index/plans/expose-q3-accumulations.md), [clarify access](../../../../../../DBSP_w_merged_index/plans/clarify-accumulated-access.md). Historical exclusions and assumptions are reconciled with the current user plan. |
| MI paper | [main.tex](../../../../../../merged_index_interesting_orderings/main.tex#L473): hierarchy and extension 473–540; [active experiments](../../../../../../merged_index_interesting_orderings/sections/experiments_revised.tex#L61): Q3/Q5/Q10 pipeline boundaries 61–70 and Q10 partial view 148–164. |
| Pipeline manuscript | [main.tex](../../../../../../query_execution_using_MI/main.tex): pipeline context; no cross-pipeline integration is proposed here. |
| `mi_db` | [architecture](../../../../../../mi_db/docs/architecture.md#L108): backend contracts 108–165, sessions 193–271; [feasibility](../../../../../../mi_db/docs/feasibility.md); [experiments](../../../../../../mi_db/docs/experiments.md); [roadmap](../../../../../../mi_db/docs/roadmap.md). |
| LeanStore COL | [col_pipeline.tpp](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/tpch_family/col_pipeline.tpp#L49), [views_col.hpp](https://github.com/alicia-lyu/leanstore/blob/305ad0a98b147d048a37a1eba3787b35b1181b85/frontend/tpch/tpch_family/views_col.hpp#L31); detailed symbol inventory in [storage report](../reports/merged-index-storage.md#source-ledger). |
| Feldera implementation | [Index inventory](../reports/feldera-indexes.md), [compiler path](../reports/compiler-control.md), and [runtime contracts](../reports/runtime-reconstruction.md) own the symbol/line references and interpretation. |

## Working-tree fingerprints

SHA-256 identifies locally modified source content; it is not a replacement for archiving that content if a
later experiment must be reproduced elsewhere.

| File | SHA-256 |
| --- | --- |
| `merged_index_interesting_orderings/main.tex` | `44bf60963e6b94f6aaad04cdacea3ecc31e248a95cb10dd1118ff2e6fa17a9c2` |
| `mi_db/README.md` | `66af70b414b6d563a9404b5117d66567d15f5662e5de378b54a705fd1b609ac9` |
| `mi_db/docs/architecture.md` | `d043b9b28121fe044980fde032d95e4b00c331e1d7a137ce709bd70eb6914e79` |
| `mi_db/docs/feasibility.md` | `78e53002f778d222cf159f2cfe28020e085575f5d808ec6511b35c00d78ad306` |
| `mi_db/docs/experiments.md` | `6535f6e80aee7a780debe5dbbb6728b09621502f19ee61b8987e90ca53250e5b` |
| `mi_db/docs/roadmap.md` | `eddcd926e2ac95ceacaa3d1042e29309cf96e9374c1400b350af0dffa86ecfd8` |

## Executed semantic checks

From this investigation, both existing standard-library Python suites passed:

```sh
python3 /Users/alicialyu/Local/DBSP_w_merged_index/validation/check_q3.py
python3 /Users/alicialyu/Local/DBSP_w_merged_index/validation/check_operators.py
```

The first ran 12 test methods; the second ran 8. Seeded pairs and sequential batches are included inside these
methods. These checks demonstrate the supplied model's semantics, not physical storage, generated Feldera
circuit shape, recovery, or beyond-memory performance. Q5/Q10 formulas in this package are analytical
derivations; they have not been executed by those Q3/operator suites.

Use the [validation plan](validation-plan.md) for the unexecuted conformance and performance experiments. No
new diagnostics, runtime APIs, or semantic test code were introduced by this documentation task.
