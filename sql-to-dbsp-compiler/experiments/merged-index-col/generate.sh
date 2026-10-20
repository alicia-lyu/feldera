#!/usr/bin/env bash
# Generates the COL merged-index module from col.mi and the Q3 schema.
# usage: generate.sh [output.rs]   (default: src/generated.rs)
set -euo pipefail

col_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
compiler="${col_dir}/../../SQL-compiler/sql-to-dbsp"
if [[ -n "${JAVA_HOME:-}" ]]; then
  export PATH="${JAVA_HOME}/bin:${PATH}"
fi

if [[ "$#" -gt 1 ]]; then
  echo "usage: $0 [output.rs]" >&2
  exit 2
fi
output="${1:-${col_dir}/src/generated.rs}"

# The compiler wrapper always exits 0, and the compiler writes the module only
# when the spec is valid, so an empty scratch file is the failure signal. The
# output is replaced only after a successful run.
scratch="$(mktemp "${TMPDIR:-/tmp}/col-generated.XXXXXX")"
"${compiler}" "${col_dir}/../q3-ivm-baseline/query.sql" --noRust \
  --merged-index "${col_dir}/col.mi" --merged-index-output "${scratch}"
if [[ ! -s "${scratch}" ]]; then
  echo "$0: the compiler did not write the merged-index module" >&2
  exit 1
fi
# Pin the formatter so that the committed file has one exact form.
rustfmt +1.96.1 --edition 2021 "${scratch}"
mv -- "${scratch}" "${output}"
