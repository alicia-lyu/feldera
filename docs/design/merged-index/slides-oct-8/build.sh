#!/bin/sh
set -eu
cd "$(dirname "$0")"
mkdir -p build
python3 verify_examples.py
latexmk -pdf -outdir=build -jobname=dbsp-physical-design \
    -interaction=nonstopmode -halt-on-error main.tex
latexmk -pdf -outdir=build -jobname=dbsp-physical-design-notes \
    -interaction=nonstopmode -halt-on-error notes.tex
