#!/usr/bin/env bash
# Generates the MI_P merged-index module from mi_p.mi and schema.sql.
# usage: generate.sh [output.rs]   (default: src/generated.rs)
set -euo pipefail

index_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
compiler="${index_dir}/../../SQL-compiler/sql-to-dbsp"
if [[ -n "${JAVA_HOME:-}" ]]; then
  export PATH="${JAVA_HOME}/bin:${PATH}"
fi

if [[ "$#" -gt 1 ]]; then
  echo "usage: $0 [output.rs]" >&2
  exit 2
fi
output="${1:-${index_dir}/src/generated.rs}"

# The compiler wrapper always exits 0, and the compiler writes the module only
# when the spec is valid, so an empty scratch file is the failure signal. The
# output is replaced only after a successful run.
scratch="$(mktemp "${TMPDIR:-/tmp}/mi-p-generated.XXXXXX")"
"${compiler}" "${index_dir}/schema.sql" --noRust \
  --merged-index "${index_dir}/mi_p.mi" --merged-index-output "${scratch}"
if [[ ! -s "${scratch}" ]]; then
  echo "$0: the compiler did not write the merged-index module" >&2
  exit 1
fi
# Pin the formatter so that the committed file has one exact form.
rustfmt +1.96.1 --edition 2021 "${scratch}"
mv -- "${scratch}" "${output}"
