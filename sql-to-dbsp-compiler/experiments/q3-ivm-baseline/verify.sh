#!/usr/bin/env bash
set -euo pipefail

experiment_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
manifest="${experiment_dir}/Cargo.toml"
col_manifest="${experiment_dir}/../merged-index-col/Cargo.toml"
artifacts="${experiment_dir}/target/r4-verification/$(date -u +%Y%m%dT%H%M%SZ)-$$"
mkdir -p "${artifacts}"

export RUSTUP_TOOLCHAIN=1.96.1
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${experiment_dir}/target}"

run_check() {
  local name="$1"
  shift
  echo "Running ${name}"
  if "$@" > "${artifacts}/${name}.log" 2>&1; then
    return 0
  else
    local status="$?"
    tail -n 80 "${artifacts}/${name}.log" >&2
    return "${status}"
  fi
}

restore_native() {
  local status="$?"
  trap - EXIT
  if ! run_check restore-native "${experiment_dir}/generate.sh"; then
    echo "Default native generation could not be restored; see ${artifacts}/restore-native.log" >&2
    status=1
  fi
  echo "Verification artifacts: ${artifacts}"
  exit "${status}"
}
trap restore_native EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

run_check native-generation "${experiment_dir}/generate.sh"
run_check native-format cargo +1.96.1 fmt --manifest-path "${manifest}" --all -- --check
run_check native-tests env Q3_VERIFICATION_TRANSCRIPT="${artifacts}/native.json" \
  cargo +1.96.1 test --manifest-path "${manifest}" --offline --locked -j2
run_check native-clippy cargo +1.96.1 clippy --manifest-path "${manifest}" \
  --offline --locked --all-targets -j2 -- -D warnings

run_check reconstructed-generation "${experiment_dir}/generate.sh" --q3-merged-index
run_check reconstructed-format cargo +1.96.1 fmt --manifest-path "${manifest}" --all -- --check
run_check reconstructed-tests env Q3_VERIFICATION_TRANSCRIPT="${artifacts}/reconstructed.json" \
  cargo +1.96.1 test --manifest-path "${manifest}" --offline --locked -j2 --features merged-index
run_check reconstructed-clippy cargo +1.96.1 clippy --manifest-path "${manifest}" \
  --offline --locked --all-targets -j2 --features merged-index -- -D warnings

test -s "${artifacts}/native.json"
test -s "${artifacts}/reconstructed.json"
run_check transcript-comparison cmp "${artifacts}/native.json" "${artifacts}/reconstructed.json"

run_check col-format cargo +1.96.1 fmt --manifest-path "${col_manifest}" --all -- --check
run_check col-tests cargo +1.96.1 test --manifest-path "${col_manifest}" --offline --locked -j2
run_check col-clippy cargo +1.96.1 clippy --manifest-path "${col_manifest}" \
  --offline --locked --all-targets -j2 -- -D warnings
run_check runner-tests python3 -m unittest discover -s "${experiment_dir}" -p test_measure.py -v
run_check shell-syntax bash -n "${experiment_dir}/generate.sh" "${experiment_dir}/verify.sh"
run_check whitespace git -C "${experiment_dir}" diff --check -- . ../merged-index-col
echo "RF1/RF2 suites, reader projections, and exact transaction transcripts agree."
