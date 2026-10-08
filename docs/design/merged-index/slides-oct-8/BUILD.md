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
caches are also ignored by the local `.gitignore`.

## Render and inspect

```sh
mkdir -p build/rendered build/rendered-notes
pdftoppm -scale-to 1600 -png build/dbsp-physical-design.pdf build/rendered/slide
pdftoppm -scale-to 2200 -png build/dbsp-physical-design-notes.pdf build/rendered-notes/slide
pdfinfo build/dbsp-physical-design.pdf
pdfinfo build/dbsp-physical-design-notes.pdf
```

Inspect all presentation and notes pages at readable size, including all ten
worked-join overlays. Check clipping, overlap, stable positions, readable
completed steps, fields/weights, grammar, and source labels. Speaker notes
must include the common context and only the appropriate overlay's step.
A successful TeX build alone does not establish visual correctness.

| PDF pages | Topic / advance |
| --- | --- |
| 1–5 | Main slides 1–5 |
| 6–10 | Slide 6, Orders–Lineitem join, advances 1–5 |
| 11–15 | Slide 7, Customer join, advances 1–5 |
| 16–24 | Main slides 8–16 |
| 25–28 | Appendices A–D |

Both PDFs have 28 pages. The main talk still has 16 topics: advancing an
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
