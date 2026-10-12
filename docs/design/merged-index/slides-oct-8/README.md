# DBSP physical design: advisor presentation

This editable 16:9 Beamer/TikZ deck has **12 main slide topics** and five
appendix topics. Slide 2 defines a record as (key, value, weight). Slide 3
first shows the LSM-like trace and the key/value trees
inside one immutable file run, then reveals the teal search for Order ID 43.
Each run has its own downward cursor above it on both advances. Slide 4
contrasts a standard LSM's mutable L0 memtables with DBSP's L0 of in-memory
immutable runs that merge-sort into larger runs, then shows how sorted (K, V)
pairs build the smallest run. Slide 6 reveals the downstream
Customer circuit and its delta equation on the second overlay. Slide 8 draws
the before-state and after-state snapshots as reference arrows to shared runs;
only the after-state references the new transaction's run. Slide 9 merges the
first level: the head's references move to the merged run, while both
snapshots keep the faded L0 runs alive. Appendix E
reveals key-to-value row traversal over four advances. Each topic retains its
slide number across advances.
Each PDF has **25 rendered pages**: 17 main-talk pages and eight appendix pages.
`notes.tex` places each overlay beside its corresponding speaker notes.

The user's edits were recovered from `/Users/alicialyu/Desktop/main.pdf` on
2026-10-08. At recovery, all 29 rebuilt presentation pages matched that
reference pixel for pixel at 144 dpi. Subsequent user edits removed the two worked-join
topics. The subsequent slide-2 illustration added one rendered advance.
The construction/traversal revision preserves these edits, adds slide 3 and
Appendix E, and moves the storage-insufficiency callout from accumulated
state to construction. Other pages retain their baseline content and layout,
apart from updated slide numbers and explanatory-program references.
The recovered opening slides simplify the introduction,
base storage, and example notation; the circuit uses unsubscripted relations
and delta-labeled integrators; the state table lists four indexed states.
Speaker notes were removed on 2026-10-12 because agents wrote them without review.

## Build and deliverables

With Beamer, TikZ, Libertine, NewTX, `latexmk`, and Python 3.10 or newer:

```sh
sh build.sh
```

- [Presentation PDF](build/dbsp-physical-design.pdf).
- [Presentation with speaker notes](build/dbsp-physical-design-notes.pdf).
- [Build and visual-review instructions](BUILD.md).

Generated PDFs, TeX logs, Python caches, and review images stay in ignored
`build/`. The editable sources are `main.tex`, `notes.tex`, and `figures/*.tex`.
No sibling checkout is required to build.

## Program and evidence

The explanatory program joins **Orders–Lineitem first**, groups its `OL`
intermediate by Customer ID with values ordered first by Order ID then line
number, and then joins Customer. The circuit shows both branches of both
joins; the current user edits omit the two step-by-step worked-join topics. The predicate-free view and
all nine output fields remain unchanged.

The actual experiment and restricted RF1/RF2 reconstruction prototype use
**Orders–Customer first**, then Lineitem. Slide 12 and Appendix D retain their
actual accesses `C_c+`, `O_c−`, `B_o+`, and `L_o−`. The explanatory order is
logically equivalent; this revision changes no runtime, compiler, API, or
experiment code.

A **transaction delta** describes logical changes. A **run** is an immutable
sorted weighted collection retained by a trace, in memory or on disk. “Batch”
is retained only in code identifiers and source references. The inspected
input and join paths seal runs before trace insertion and lack a mutable
memtable absorbing updates across transactions. Larger runs grow through
merging, including merging in memory. Run sizes and merge timing in diagrams
are illustrative. The deck claims no measured savings, update costs, or
comparative performance.

Native file “columns” describe nested file trees, not SQL attribute columns.
The construction pseudocode therefore uses `key_tree` and `value_tree`:
the first stores `(K, ())` with separate value-row boundaries; the second
stores `(V, weight)`. `write1` advances a file-wide value-row counter, and
`write0` captures the pending nonempty group and resets it to `[end,end)`.
Leaf flushes do not reset those counters; the run completes at `close`.
The callout illustrates a one-run transaction; actual transactions can
produce several runs.

- [SOURCE-EVIDENCE.md](SOURCE-EVIDENCE.md) records the inspected revisions, source
  hashes, and construction and retention evidence. It is pinned to an earlier commit.
- [decisions.md](../decisions.md) records the user's confirmed decisions, including the
  slide-deck directions of 2026-10-08.

`verify_examples.py` asserts exact branches for both programs, simultaneous
insertion/deletion, Customer/Order/Lineitem overwrites, multiplicities, and
cancellation. It compares both orders with full evaluation for the original
729 nonnegative weighted cases and an additional 729 signed cases. It checks
algebra and examples, not storage behavior or runtime performance.

## Rules for editing the deck

- The user's own edits to the deck are authoritative. Do not reapply earlier agent guidance over
  them.
- After `sh build.sh`, inspect the rendered pages ([BUILD.md](BUILD.md)). A clean TeX build does
  not prove the slides look right. `build.sh` hard-codes the page count.
- Notation:
  - "transaction delta", not "batch", for a logical change set;
  - "run" for an immutable sorted collection;
  - subscripts for access order, superscripts for before/after state.
- Make no measured-savings, update-cost, or comparative performance claim.
- Describe the experiment's Orders–Customer-first circuit as it is, even though the explanatory
  program joins Orders–Lineitem first.

## Style and talk structure

Libertine/NewTX typography, ink titles, teal rules, and editable TikZ diagrams
follow the prior advisor deck at
`/Users/alicialyu/Local/query_execution_using_MI/advisor-deck/main.tex`.
The former opening backdrop remains in `figures/query-update-backdrop.tex`
for provenance, but this revision does not display it. Teal highlights the
independent run cursors and key-search path on slide 3, and the cumulative
traversal in Appendix E. Runs on slide 3 remain unconnected; the dashed
connector enlarges one file. Amber identifies deferred work on slide 12.
Plum marks integrators, integration text, and the Head they maintain.

| Topics | Subject |
| --- | --- |
| 1–3 | DBSP context, base storage, immutable-run construction |
| 4–6 | Example/notation, complete program, accumulated state/runs |
| 7–9 | Records, representations, changed-Customer reads |
| 10–12 | Separate small runs, snapshots, merge ownership |
| 13–15 | Memory accounting/controls and prototype scope |

Appendix A gives full payloads. Appendix B shows Customer overwrite and
simultaneous Order/Lineitem deletion. Appendix C lists sources. Appendix D
shows the actual experiment program. Appendix E follows key 43 through
boundaries `[2,4)`, branch child 1, and the selected leaf's item-offset map,
iterating `(8,+1)` and `(15,+1)` and stopping before row 4.
