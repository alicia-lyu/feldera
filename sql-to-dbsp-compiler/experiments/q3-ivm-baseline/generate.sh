#!/usr/bin/env bash
# Generates the two chained circuits of the Q3 baseline:
#   q3-base-circuit/src/lib.rs  from base.sql  (primary-key tables, no view)
#   q3-view-circuit/src/lib.rs  from q3.sql    (keyless tables and V), plus plan.json
# `--q3-merged-index` makes the view circuit read the merged index instead of its join traces.
set -euo pipefail

experiment_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
compiler="${experiment_dir}/../../SQL-compiler/sql-to-dbsp"
# Before the base/view split, the generator wrote src/lib.rs and src/stubs.rs here. They are
# untracked and no longer ignored. In an old checkout Cargo takes src/lib.rs for a library
# target, and the build fails with "failed to resolve mod `udf`". Stop early instead.
for stale in "${experiment_dir}/src/lib.rs" "${experiment_dir}/src/stubs.rs"; do
  if [[ -e "${stale}" ]]; then
    echo "$0: stale pre-split generated file ${stale}" >&2
    echo "Move it to TRASH/ (it is not used any more; the circuits are generated into q3-base-circuit/ and q3-view-circuit/), then rerun." >&2
    exit 1
  fi
done

if [[ -n "${JAVA_HOME:-}" ]]; then
  export PATH="${JAVA_HOME}/bin:${PATH}"
fi

view_args=()
if [[ "${1:-}" == "--q3-merged-index" && "$#" -eq 1 ]]; then
  view_args+=("--q3-merged-index")
elif [[ "$#" -ne 0 ]]; then
  echo "usage: $0 [--q3-merged-index]" >&2
  exit 2
fi

# generate_circuit SQL CRATE_DIR PLAN_FILE [COMPILER_ARG...]
# An empty PLAN_FILE skips the relational plan. The compiler wrapper always exits 0, so each
# run goes to a scratch directory and the crate is replaced only when the compiler wrote a
# library.
generate_circuit() {
  local sql="$1" crate="$2" plan="$3"
  shift 3
  local scratch
  scratch="$(mktemp -d "${TMPDIR:-/tmp}/q3-generate.XXXXXX")"
  local plan_args=()
  if [[ -n "${plan}" ]]; then
    plan_args+=(--plan "${scratch}/plan.json")
  fi
  "${compiler}" "${sql}" -i -o "${scratch}/lib.rs" "${plan_args[@]+"${plan_args[@]}"}" "$@"
  if [[ ! -s "${scratch}/lib.rs" ]]; then
    echo "$0: the compiler did not write a circuit for ${sql}" >&2
    exit 1
  fi
  perl -pi -e 's/[ \t]+$//' "${scratch}/lib.rs"
  # Each generated library carries a global allocator and a malloc_conf symbol, and a binary
  # can link only one of them. The driver (src/main.rs) defines them for both circuits.
  perl -0pi -e 's{#\[cfg\(not\(target_env = "msvc"\)\)\]\n#\[global_allocator\]\nstatic ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;\n#\[allow\(non_upper_case_globals\)\]\n#\[export_name = "malloc_conf"\]\npub static malloc_conf: &\[u8\] = [^\n]*\n}{}' "${scratch}/lib.rs"
  if grep -q 'global_allocator' "${scratch}/lib.rs"; then
    echo "$0: could not remove the allocator preamble from ${sql}'s circuit" >&2
    exit 1
  fi
  # The compiler emits redundant conversions and dereferences in generated Rust.
  # Keep their Clippy allowances scoped to this file so handwritten code remains strict.
  perl -0pi -e 's{\A// Automatically-generated file\n}{// Automatically-generated file\n#![allow(clippy::useless_conversion, clippy::explicit_auto_deref, clippy::clone_on_copy, clippy::type_complexity)]\n}' "${scratch}/lib.rs"
  mv -- "${scratch}/lib.rs" "${crate}/src/lib.rs"
  if [[ -f "${scratch}/stubs.rs" ]]; then
    mv -- "${scratch}/stubs.rs" "${crate}/src/stubs.rs"
  fi
  # rustfmt resolves the `stubs` and `udf` modules, so it runs where they live.
  rustfmt --edition 2021 "${crate}/src/lib.rs"
  if [[ -n "${plan}" ]]; then
    mv -- "${scratch}/plan.json" "${plan}"
  fi
  rmdir -- "${scratch}"
}

generate_circuit "${experiment_dir}/base.sql" "${experiment_dir}/q3-base-circuit" "" --alltables
# plan.json is the relational plan of the view circuit.
generate_circuit "${experiment_dir}/q3.sql" "${experiment_dir}/q3-view-circuit" \
  "${experiment_dir}/plan.json" "${view_args[@]+"${view_args[@]}"}"
