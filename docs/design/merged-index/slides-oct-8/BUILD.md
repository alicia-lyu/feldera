# Build and inspect the physical-design deck

## Requirements and build

Use Python 3.10 or newer and a TeX distribution with Beamer, TikZ, Libertine,
NewTX, and `latexmk`. The verified environment is TeX Live 2024 / latexmk 4.83.
Poppler supplies `pdfinfo`, `pdftotext`, and `pdftoppm` for review.

From this directory:

```sh
sh build.sh
```

The build first runs `verify_examples.py`, then compiles `main.tex` and
`notes.tex`. It checks the final TeX logs for overflow and the expected page
count. The presentation keeps PDF links active. The notes build disables
hyperlinks through `hypersetup{draft}` to avoid duplicate destinations in
Beamer's two-screen layout; its draft-mode notice is expected.

The two outputs are `build/dbsp-physical-design.pdf` and
`build/dbsp-physical-design-notes.pdf`. Keep generated artifacts inside the
ignored `build/` directory. Source-directory editor artifacts and Python
caches are also ignored by the local `.gitignore`. Beamer's generated
source-directory `*.vrb` files are ignored as well.

## Render and inspect

```sh
mkdir -p build/rendered build/rendered-notes
pdftoppm -scale-to 1600 -png build/dbsp-physical-design.pdf build/rendered/slide
pdftoppm -scale-to 2200 -png build/dbsp-physical-design-notes.pdf build/rendered-notes/slide
pdfinfo build/dbsp-physical-design.pdf
pdfinfo build/dbsp-physical-design-notes.pdf
```

Inspect all presentation and notes pages at readable size, including both slide-2
advances. Check clipping, overlap, stable positions, readable
tree/search connectors, fields/weights, grammar, and source labels.
The deck currently has no speaker notes, so the notes pages are blank.
On slide 2, check one independent downward cursor above each of the six
unconnected runs, the dashed enlargement connector, the `K → [start,end)`
caption, and identical geometry on both advances. The lookup locates the
first value row and iterates the group without a target value.
On slide 3, inspect all three advances: the memtable panel alone, then the
DBSP L0 levels of shrinking runs with merge-sort labels inside the box, then
the teal callout from the smallest run with the key 43 → `[2,4)` and sorted
value arrays. Panels stay fixed across advances and no L1 or L2 appears.
On Appendix E, inspect all four cumulative advances: three fixed block
outlines, separate key items/boundaries, zero-based array indices, 512-byte
descriptor units, symbolic leaf offsets, and connectors clear of labels.
The leaf's first row is traversal context, not a header field.

For this revision, use the freshly built 20-page baseline captured before edits:
old page 1 stays page 1; old pages 4–20 become pages 5–21. Exclude only the
renumbered main-slide footer in pixel comparisons. Old page 7 becomes page 8
with the callout moved to the new page 4. Old pages 16 and 20 become pages 17
and 21 with corrected explanatory-program references. Compare all other
content and positions, reporting any subpixel TeX layout differences separately.
Old pages 2–3 are the intentionally edited storage
topic; new pages 22–25 are Appendix E.
A successful TeX build alone does not establish visual correctness.

| PDF pages | Topic / advance |
| --- | --- |
| 1 | Main slide 1 |
| 2–3 | Slide 2, base storage, structure then lookup |
| 4–6 | Slide 3, memtable vs. in-memory runs, advances 1–3 |
| 7 | Main slide 4, example and notation |
| 8–9 | Slide 5, complete program, advances 1–2 |
| 10–19 | Main slides 6–15 |
| 20–23 | Appendices A–D |
| 24–27 | Appendix E, traversal, advances 1–4 |

Both PDFs have 27 pages. The main talk has 15 topics: advancing an
overlay does not advance its slide number. The notes PDF uses one wide page
per rendered overlay, with slide and notes side by side.

## Example checks

```sh
python3 verify_examples.py
```

Exact assertions cover both join orders and each branch for simultaneous
insertion/deletion, individual payload overwrites, the Customer-overwrite
then deletion sequence, Customer deletion, multiplicities, and cancellation.
The exhaustive cases compare each intermediate delta, integrated output, and
both join orders against full before/after evaluation. This does not run DBSP,
measure native memory, or extend prototype workload support.
