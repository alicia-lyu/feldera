#!/usr/bin/env bash
set -euo pipefail

experiment_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
compiler="${experiment_dir}/../../SQL-compiler/sql-to-dbsp"
if [[ -n "${JAVA_HOME:-}" ]]; then
  export PATH="${JAVA_HOME}/bin:${PATH}"
fi

"${compiler}" "${experiment_dir}/query.sql" -i \
  --plan "${experiment_dir}/plan.json" \
  -o "${experiment_dir}/src/lib.rs"

perl -pi -e 's/[ \t]+$//' "${experiment_dir}/src/lib.rs"
# The compiler emits redundant conversions and dereferences in generated Rust.
# Keep their Clippy allowances scoped to this file so handwritten code remains strict.
perl -0pi -e 's{\A// Automatically-generated file\n}{// Automatically-generated file\n#![allow(clippy::useless_conversion, clippy::explicit_auto_deref, clippy::clone_on_copy, clippy::type_complexity)]\n}' "${experiment_dir}/src/lib.rs"
rustfmt --edition 2021 "${experiment_dir}/src/lib.rs"
