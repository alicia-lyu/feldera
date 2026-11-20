# Building a second merged index

This guide is the starting point for a second merged index, MI_P (Part > Partsupp > Lineitem), and
the IVM programs it serves. It says what exists, what Q3 built by hand, and what is still open. It
decides nothing new; the open questions at the end belong to the user and the collaborator.

The served programs come from Chapter 4's appendix of the thesis
(`key-trie-merged-index-order.tex` in the sibling repository `merged_index_vldb2026_slides`, input
file `docs/key-trie-tpch-ivm.tex`, section "the served programs, one by one"). On MI_P they are Q2,
Q9, Q14, Q16, Q17, Q19, and Q20.

## Read first

| Tier | File | Why |
| --- | --- | --- |
| Research-wide | [root `CLAUDE.md`](../../../CLAUDE.md) | One worker, fair mechanism, report only what was measured, terms, the two decision logs |
| Any index and harness | [`experiments/CLAUDE.md`](../../../sql-to-dbsp-compiler/experiments/CLAUDE.md) | One runtime, transaction order, net delta, general deltas, accounting, placement, images, regeneration |
| Design | [general-definition.md](general-definition.md), [shared-traversal.md](shared-traversal.md), [reconstruction.md](reconstruction.md) | The spec language, the scan and the readers, how partner reads replace traces |
| Decisions | [decisions.md](decisions.md) | Generic decisions that bind MI_P too |

The Q3 directories (`merged-index-col/`, `q3-ivm-baseline/`) are worked examples, not rules.

## What exists and is generic

Reuse these unchanged.

| Piece | Where | Notes |
| --- | --- | --- |
| `.mi` spec parser, `MergedIndexValidator`, Rust emitter | [`SQL-compiler/CLAUDE.md`](../../../sql-to-dbsp-compiler/SQL-compiler/CLAUDE.md) | A trie has exactly one root domain; MI_P's is `partkey` |
| Key trie, encoders, folded keys, `prefix_successor`, batches | [`merged_index/CLAUDE.md`](../../../crates/dbsp/src/trace/merged_index/CLAUDE.md) | Schema-independent; layout in [storage-layout.md](storage-layout.md) |
| `RunMerge`, `ScanSession`, `Pass` | same | One scan of the touched groups of `Old` per transaction |
| Generic readers: `ReaderDefinition`, `capture`, `route`, `RowBuilder`, sealing | same | One reader per partner read |
| Supplied-snapshot join, several circuits in one runtime, `RunReads`, read owners, `BlockExpansions` | [`crates/dbsp/CLAUDE.md`](../../../crates/dbsp/CLAUDE.md) | The join reads a reader's batch instead of its own trace |

## What Q3 built by hand

MI_P needs its own versions of these. Use Q3's as models.

| Q3 piece | Where | What it does |
| --- | --- | --- |
| `col.mi`, `generate.sh`, `src/generated.rs` | [`merged-index-col/CLAUDE.md`](../../../sql-to-dbsp-compiler/experiments/merged-index-col/CLAUDE.md) | The index definition, generated and committed |
| Q3 readers, `Q3State`, the routing hook | same | The four partner reads `C⁺`, `O⁻`, `J⁺`, `L⁻`. The hook computes `J` and `ΔJ`, since generic `route` copies entries and has no notion of a join. [deferred.md](deferred.md) ("Generic readers") plans to replace the hook with declared join readers |
| `q3-trie-view-circuit` | [`q3-ivm-baseline/CLAUDE.md`](../../../sql-to-dbsp-compiler/experiments/q3-ivm-baseline/CLAUDE.md) | The hand-written view circuit, with `TrieInputs` input handles |
| `ManualIndex`, `reconstruction::prepare`, `OrphanLines` | same | Index upkeep on an auxiliary thread; completing `Dlt` with the before and after owners of each change; the side store for rows that have no owner |
| `base.sql`, `q3.sql`, `verify.sh`, `measure.py` | same | The base and native view circuits, the transcript comparison, and R5 measurement |

## Reading the appendix's reads

Each join of a served program has two delta terms, `ΔR ⋈ S⁺` and `R⁻ ⋈ ΔS`. The appendix gives
each partner read a kind:

- **Path:** a read at the entity's own node; one entry per requested key. Q3's `C⁺` and `J⁺` are
  path reads.
- **Range:** a read of every entry below a node of the trie. Q3's `O⁻` and `L⁻` are range reads.
- **Trace:** DBSP's own integrated copy of the rows. The rows are outside the index, or the
  operator's key is not a node of the trie.

Path and range reads are what the generic readers serve: a reader's requests are folded-key
prefixes of its driving delta at its request depth (`ReaderDefinition::REQUEST_DEPTH`). A trace
read is a native DBSP trace.

The MI_P rows of the appendix, transcribed with no new claims:

| Query | View's tables | Index reads (key, kind) | Traces |
| --- | --- | --- | --- |
| Q2 | P, PS, S, N, R | `PS ⋈ P` at `partkey`: `P⁺` path, `PS⁻` range | S, N, R (the three later joins) |
| Q9 | P, PS, L, O, S, N | `L ⋈ PS` at (`partkey`, `suppkey`): `PS⁺` path, `L⁻` range; `⋈ P` at `partkey`: `P⁺` path, `J₁⁻` range | O, S, N |
| Q14 | P, L | `L ⋈ P` at `partkey`: `P⁺` path, `L⁻` range | none |
| Q16 | P, PS, S | `PS ⋈ P` at `partkey`: `P⁺` path, `PS⁻` range | the antijoin with S; the grouping on `brand`, `type`, `size` |
| Q17 | P, L | `L ⋈ P` at `partkey`: `P⁺` path, `L⁻` range; the average `A = γ(L)` per `partkey` reads `L⁻` as a range, and so does the join with `A` | none |
| Q19 | P, L | `L ⋈ P` at `partkey`: `P⁺` path, `L⁻` range | none |
| Q20 | P, PS | `PS ⋈ P` at `partkey`: `P⁺` path, `PS⁻` range | none |

Q14, Q19, and Q20 have one join each, so they are the smallest. Q11 has no view; the appendix
serves it with a separate index on Partsupp.

## Steps, as COL was built

1. Write the spec (for example `mi_p.mi`). Validate it with the compiler, and generate and commit
   its module ("Regenerate an index module" in [`experiments/CLAUDE.md`](../../../sql-to-dbsp-compiler/experiments/CLAUDE.md)).
   `testing::check_against_trie` ties the generated fold constants to the trie.
2. Add storage tests like COL's, building file runs on purpose.
3. Define one `ReaderDefinition` per path or range read, plus a hook wherever rows are combined
   rather than copied (as Q3's `J`). Wrap them in a per-program state like `Q3State`
   (`collect`, `finish`).
4. Write the view circuit, keeping DBSP's operators. Only the partner reads come from the index
   (decision of 2026-11-11).
5. Write the harness: the base circuit, the native view circuit generated from SQL, the index
   upkeep, completing `Dlt` (owners and a side store), and the transcript comparison of the two
   modes. Then do measurement, under the rules of the experiments tier.

## Open questions

These are not decided. Record each answer in [decisions.md](decisions.md), or in the harness's
own decision log if it is harness-specific.

- How one view circuit mixes index reads with native trace reads. Q3's program has no trace reads.
- How an aggregation on a key of the trie (Q17's `A`) or an antijoin (Q16) reads the index. The
  generic readers serve join partners only. The appendix's last section also proposes storing such
  aggregates in the index, which turns their range reads into path reads.
- Whether the generic join readers of [deferred.md](deferred.md) come before MI_P, or MI_P writes
  its own hook first.
- Which side store MI_P needs: which rows can arrive before their owner (a line item before its
  supply or part).
