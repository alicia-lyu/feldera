# DBSP physical design: advisor presentation

This editable 16:9 Beamer/TikZ deck has **16 main slide topics** for a
20–25 minute talk and four appendix topics. Slides 6 and 7 each have five
fixed-layout overlays, retaining their slide number. Each PDF therefore has
**28 rendered pages**: 24 main-talk pages and four appendix pages.
`notes.tex` places each overlay beside its corresponding speaker notes.

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
number, and then joins Customer. Both branches of both joins and output
integration precede representation internals. The predicate-free SQL view and
all nine output fields remain unchanged.

The actual experiment and restricted RF1/RF2 reconstruction prototype use
**Orders–Customer first**, then Lineitem. Slide 16 and Appendix D retain their
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

## Style and talk timing

Libertine/NewTX typography, ink titles, teal rules, and editable TikZ diagrams
follow the prior advisor deck at
`/Users/alicialyu/Local/query_execution_using_MI/advisor-deck/main.tex`.
The former opening backdrop remains in `figures/query-update-backdrop.tex`
for provenance, but this revision does not display it. Teal highlights the
active worked-join step; completed information remains readable in quiet ink.
Amber identifies deferred work on slide 16.

| Topics | Subject | Minutes |
| --- | --- | ---: |
| 1–3 | DBSP context, loaded base storage, SQL and visibility | 4 |
| 4–7 | Complete program, state/runs, both worked joins | 8 |
| 8–10 | Records, representations, changed-Customer reads | 4 |
| 11–13 | Separate small runs, snapshots, merge ownership | 4 |
| 14–16 | Memory accounting/controls and prototype scope | 5 |
| Total | Main talk | 25 |

Appendix A gives full payloads. Appendix B shows Customer overwrite and
simultaneous Order/Lineitem deletion. Appendix C lists sources. Appendix D
shows the actual experiment program.
