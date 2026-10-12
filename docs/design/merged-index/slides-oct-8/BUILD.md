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

Inspect all presentation and notes pages at readable size, including both slide-3
advances. Check clipping, overlap, stable positions, readable
tree/search connectors, fields/weights, grammar, and source labels.
Only slide 4 has a speaker note; the other notes pages are blank.
On slide 3, check one independent downward cursor above each of the six
unconnected runs, the dashed enlargement connector, the `K → [start,end)`
caption, and identical geometry on both advances. The lookup locates the
first value row and iterates the group without a target value.
On slide 4, inspect all three advances: the memtable panel alone, then the
DBSP L0 levels of shrinking runs with merge-sort labels inside the box, then
the teal callout from the smallest run with the key 43 → `[2,4)` and sorted
value arrays. Panels stay fixed across advances and no L1 or L2 appears.
On slide 5, check that the inserted run's arrow skips L0–L1 and that the
merge output stays in L2 on advance 2. On advance 3, check the dashed 10 MiB
line between L1 and L2, stepping down around the small L2 run that stays
under 10 MiB, hatched file runs below it, and the teal inserted run left in
memory. On slide 9, check two equal-size L0 runs and matching reference dots: ink for
C⁻, teal for C⁺. On slide 10, check that only the head's plum dots move on
advance 2. The faded L0 runs keep their snapshot dots, and *m* has only a
head dot.
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
| 2 | Main slide 2, record structure |
| 3–4 | Slide 3, base storage, structure then lookup |
| 5–7 | Slide 4, memtable vs. in-memory runs, advances 1–3 |
| 8–10 | Slide 5, run levels, advances 1–3 |
| 11 | Main slide 6, example and notation |
| 12–13 | Slide 7, complete program, advances 1–2 |
| 14–15 | Main slides 8–9 |
| 16–17 | Slide 10, merging the first level, advances 1–2 |
| 18 | Main slide 11, memory pressure table |
| 19 | Main slide 12, native mechanisms and prototype |
| 20–23 | Appendices A–D |
| 24–27 | Appendix E, traversal, advances 1–4 |

Both PDFs have 27 pages. The main talk has 12 topics: advancing an
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
