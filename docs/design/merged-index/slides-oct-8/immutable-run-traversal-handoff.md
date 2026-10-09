# Immutable-run construction and row traversal

Implemented 2026-10-09 from the supplied plan. The deck now has **15 main
topics, five appendix topics, and 25 rendered pages per PDF**.

## Changes

- Preserved the starting user edits in `figures/base-storage.tex`: removed
  trace heading, merge edges/labels, and shared seek branches stay removed.
  Added one independent rightward cursor inside every run on both advances;
  retained the dashed enlargement connector. The key caption is
  `K → [start,end)`, and lookup notes locate the first value row and iterate
  the group without a target `V`.
- Added slide 3, “Creating an immutable run,” immediately after base storage.
  Moved the storage-insufficiency callout from accumulated state, corrected
  “become” to “becomes,” and retained the trace-sharing statement. The two
  pseudocode columns expose writer state, writes, calling order, and `close`.
  Key 43's pending rows advance from `[2,2)` to `[2,4)`, then reset to `[4,4)`.
- Added four cumulative advances in Appendix E. Three fixed nodes show
  separate key items/boundaries, cumulative row totals and child descriptors,
  and the selected leaf's header, map, and two tuples. Traversal selects
  `[2,4)`, child index 1, offset 16/size 1 in 512-byte units, and local index
  `2−2=0`; it iterates both tuples and stops before row 4.
- Updated counts, page maps, explanatory-program references, terminology,
  and source evidence. Historical recovery and original planning records
  remain unchanged. No runtime or experiment code changed.

Native “columns” describe nested file trees rather than SQL attribute
columns; explanatory pseudocode uses `key_tree` and `value_tree`. Notes
explain file-wide counters, leaf flushes, `write1`'s lack of a position
argument, completion at `close`, and the illustrative one-run case versus
transactions that produce several runs. Appendix notes distinguish omitted
metadata and symbolic block offsets from derived traversal context.

## Verification

- `sh build.sh` passed: exact join-example checks, 729 nonnegative and 729
  signed cases, both PDFs at 25 pages, and no TeX overflow.
- Visually inspected both storage advances, construction, accumulated state,
  all four Appendix E advances, and their corresponding notes. Checked
  indentation, counters, array indices, descriptor units, arrow routing,
  fixed geometry, and the exclusive stop boundary.
- Text-bound checks found all text inside both PDFs' page boundaries.
  Appendix E's block titles and header metadata retain identical positions
  over all four advances.
- Markdown lint passed for the updated documentation, and `git diff --check`
  found no whitespace errors.
- Compared every unaffected presentation page at 144 dpi with a fresh
  baseline built from the initial working tree, including the user edits.
  Fifteen pages retain identical text and horizontal positions. Excluding
  renumbered footer digits, nine match pixel for pixel; six have vertical
  layout differences of at most **0.031 pt**. The comparison records those
  subpixel differences rather than claiming all pages match exactly.
- Separately inspected the intentionally removed callout (baseline page 7
  to current page 8) and corrected program references (baseline pages 16/20
  to current pages 17/21). Baseline pages 2–3 are intentionally edited;
  current page 4 and pages 22–25 are new.
- Fetched `origin` and merged the tracking branch, `origin/main`, before
  committing; the merge reported already up to date.

Baseline PDFs, review images, temporary comparison sources/scripts, and logs
stay under ignored `build/`. Beamer's generated source-directory `*.vrb`
files are also ignored. Editable content remains in `main.tex` and
`figures/*.tex`; `notes.tex` uses the same source.
