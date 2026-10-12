#!/bin/sh
set -eu
cd "$(dirname "$0")"
mkdir -p build
python3 verify_examples.py
latexmk -pdf -outdir=build -jobname=dbsp-physical-design \
    -interaction=nonstopmode -halt-on-error main.tex
latexmk -pdf -outdir=build -jobname=dbsp-physical-design-notes \
    -interaction=nonstopmode -halt-on-error notes.tex

# Fail on clipped TeX content, including notes, and on missing overlays.
python3 - <<'PYTHON'
from pathlib import Path
import re

for name in ("dbsp-physical-design", "dbsp-physical-design-notes"):
    log = Path(f"build/{name}.log").read_text()
    if "Overfull" in log:
        raise SystemExit(f"Overflow in build/{name}.log")
    pages = re.search(r"Output written on .*?\((\d+) pages", log, re.DOTALL)
    if pages is None or int(pages.group(1)) != 27:
        raise SystemExit(f"Expected 27 rendered pages in {name}")
print("Both PDFs: 27 rendered pages, no TeX overflow")
PYTHON
