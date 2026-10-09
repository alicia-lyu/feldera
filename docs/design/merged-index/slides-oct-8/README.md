# DBSP physical design: advisor presentation

This editable 16:9 Beamer/TikZ deck has **14 main slide topics** and four
appendix topics. Slide 2 first shows the LSM-like trace and the key/value trees
inside one immutable file run, then reveals the teal search for Order ID 43.
Slide 4 reveals the downstream Customer circuit and its delta equation on
the second overlay. Both topics retain their slide number across advances.
Each PDF has **20 rendered pages**: 16 main-talk pages and four appendix pages.
`notes.tex` places each overlay beside its corresponding speaker notes.

The user's edits were recovered from `/Users/alicialyu/Desktop/main.pdf` on
2026-10-08. At recovery, all 29 rebuilt presentation pages matched that
reference pixel for pixel at 144 dpi. Subsequent user edits removed the two worked-join
topics; the current slide-2 illustration adds one rendered advance. All
other current presentation pages retain their pre-illustration rendering.
The recovered opening slides simplify the introduction,
base storage, and example notation; the circuit uses unsubscripted relations
and delta-labeled integrators; the state table lists four indexed states.
Existing speaker notes were retained because the reference has no notes.
See [the recovery record](restore-user-pdf-plan.md) for scope and verification.

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
**Orders–Customer first**, then Lineitem. Slide 14 and Appendix D retain their
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

- [SOURCE-EVIDENCE.md](SOURCE-EVIDENCE.md) records the inspected revision,
  source hashes, configuration, construction/retention evidence, and provenance.
- [Final revision handoff](final-revision-handoff.md) records implementation,
  verification, and remaining scope boundaries.
- [Original handoff](advisor-presentation-implementation-plan.md) remains intact
  as historical context. Its original join-order instructions are superseded
  by the final revision handoff.
- [Dated advisor guidance](advisor-working-style-2026-10-08.md) records the latest
  task-specific guidance.

`verify_examples.py` asserts exact branches for both programs, simultaneous
insertion/deletion, Customer/Order/Lineitem overwrites, multiplicities, and
cancellation. It compares both orders with full evaluation for the original
729 nonnegative weighted cases and an additional 729 signed cases. It checks
algebra and examples, not storage behavior or runtime performance.

## Style and talk structure

Libertine/NewTX typography, ink titles, teal rules, and editable TikZ diagrams
follow the prior advisor deck at
`/Users/alicialyu/Local/query_execution_using_MI/advisor-deck/main.tex`.
The former opening backdrop remains in `figures/query-update-backdrop.tex`
for provenance, but this revision does not display it. Teal highlights the
key-search path on slide 2; gray arrows show schematic merging.
Amber identifies deferred work on slide 14.

| Topics | Subject |
| --- | --- |
| 1–3 | DBSP context, base storage, example and notation |
| 4–5 | Complete program and accumulated state/runs |
| 6–8 | Records, representations, changed-Customer reads |
| 9–11 | Separate small runs, snapshots, merge ownership |
| 12–14 | Memory accounting/controls and prototype scope |

Appendix A gives full payloads. Appendix B shows Customer overwrite and
simultaneous Order/Lineitem deletion. Appendix C lists sources. Appendix D
shows the actual experiment program.
