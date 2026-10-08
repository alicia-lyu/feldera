# DBSP physical design: advisor presentation

This separate 16:9 Beamer deck contains 16 main slides for a 20–25 minute talk
and three appendix slides. `main.tex` and `figures/*.tex` are editable LaTeX/TikZ.
`notes.tex` places each slide beside its speaker notes. No sibling checkout is
needed to build the deck.

## Build

Install a TeX distribution with Beamer, TikZ, Libertine, NewTX, and `latexmk`,
plus Python 3.10 or newer. Run from this directory:

```sh
sh build.sh
```

The deliverables are:

- [Presentation PDF](build/dbsp-physical-design.pdf).
- [Slides with speaker notes PDF](build/dbsp-physical-design-notes.pdf).

Generated PDFs, logs, and rendered images stay in ignored `build/`.
TeX output from editor builds in the source directory is also ignored.
For visual review, with Poppler installed:

```sh
mkdir -p build/rendered build/rendered-notes
pdftoppm -scale-to 1600 -png build/dbsp-physical-design.pdf build/rendered/slide
pdftoppm -scale-to 2200 -png build/dbsp-physical-design-notes.pdf build/rendered-notes/slide
```

## Content and evidence

The running example maintains the implemented predicate-free three-way join.
It uses the full projected payloads, with aliases on slides to keep the worked
joins readable. It excludes Q3 filters, aggregation, ranking, and limit.
All figures are schematic and convey no measured sizes or savings.

[SOURCE-EVIDENCE.md](SOURCE-EVIDENCE.md) records code references, configuration,
revision, working-tree qualifications, and visual provenance.
[advisor-presentation-implementation-plan.md](advisor-presentation-implementation-plan.md)
preserves the planning handoff and execution decisions.
`verify_examples.py` checks each branch against complete before/after evaluation,
including concurrent inserts, deletion, overwrites, multiplicities, and cancellation.

The style and opening backdrop derive from the prior advisor deck at
`/Users/alicialyu/Local/query_execution_using_MI/advisor-deck/main.tex`.
The backdrop macros are copied locally into `figures/query-update-backdrop.tex`.
Ink titles, teal rules, Libertine/NewTX fonts, and native TikZ diagrams preserve
the reference style. Teal and amber retain the opening figure's query-time and
proposed merged-index maintenance meanings. Later native storage diagrams use
ink, gray, and teal for navigation. Amber on slide 16 identifies deferred work.

## Talk timing

| Slides | Subject | Minutes |
| --- | --- | ---: |
| 1–3 | Question, base storage, query | 4 |
| 4–7 | Complete program and both worked joins | 7 |
| 8–10 | Records, representation, matching reads | 4 |
| 11–13 | Growing trace, snapshots, merge ownership | 4 |
| 14–16 | Memory controls and merged-index connection | 5 |
| Total | Main talk | 24 |

Appendix A carries payload definitions. Appendix B carries customer overwrite
and deletion. Appendix C collects references. Speaker notes include the less
visible qualifications and code pointers.
