#!/usr/bin/env bash
set -euo pipefail

experiment_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
manifest="${experiment_dir}/Cargo.toml"
col_manifest="${experiment_dir}/../merged-index-col/Cargo.toml"
col_dir="${experiment_dir}/../merged-index-col"
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

report_artifacts() {
  local status="$?"
  trap - EXIT
  echo "Verification artifacts: ${artifacts}"
  exit "${status}"
}
trap report_artifacts EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

run_check native-generation "${experiment_dir}/generate.sh"
run_check native-format cargo +1.96.1 fmt --manifest-path "${manifest}" --all -- --check
run_check native-tests env Q3_VERIFICATION_TRANSCRIPT="${artifacts}/native.json" \
  Q3_GENERAL_TRANSCRIPT="${artifacts}/native-general.json" \
  cargo +1.96.1 test --manifest-path "${manifest}" --offline --locked -j2
run_check native-clippy cargo +1.96.1 clippy --manifest-path "${manifest}" \
  --offline --locked --all-targets --workspace -j2 -- -D warnings

# The reconstructed build needs no generation: its view circuit is written by hand.
run_check reconstructed-format cargo +1.96.1 fmt --manifest-path "${manifest}" --all -- --check
run_check reconstructed-tests env Q3_VERIFICATION_TRANSCRIPT="${artifacts}/reconstructed.json" \
  Q3_GENERAL_TRANSCRIPT="${artifacts}/reconstructed-general.json" \
  cargo +1.96.1 test --manifest-path "${manifest}" --offline --locked -j2 --features merged-index
run_check reconstructed-clippy cargo +1.96.1 clippy --manifest-path "${manifest}" \
  --offline --locked --all-targets --workspace -j2 --features merged-index -- -D warnings

run_check trie-circuit-tests cargo +1.96.1 test --manifest-path "${manifest}" --offline --locked -j2 \
  -p q3-trie-view-circuit

test -s "${artifacts}/native.json"
test -s "${artifacts}/reconstructed.json"
test -s "${artifacts}/native-general.json"
test -s "${artifacts}/reconstructed-general.json"
run_check transcript-comparison cmp "${artifacts}/native.json" "${artifacts}/reconstructed.json"
run_check general-transcript-comparison cmp "${artifacts}/native-general.json" \
  "${artifacts}/reconstructed-general.json"

# The committed generated module must match what col.mi and the compiler produce now.
run_check col-generation "${col_dir}/generate.sh" "${artifacts}/generated.rs"
run_check col-generated-fresh cmp "${artifacts}/generated.rs" "${col_dir}/src/generated.rs"
run_check col-format cargo +1.96.1 fmt --manifest-path "${col_manifest}" --all -- --check
run_check col-tests cargo +1.96.1 test --manifest-path "${col_manifest}" --offline --locked -j2
run_check col-clippy cargo +1.96.1 clippy --manifest-path "${col_manifest}" \
  --offline --locked --all-targets -j2 -- -D warnings
run_check runner-tests python3 -m unittest discover -s "${experiment_dir}" -p test_measure.py -v
run_check summarize-tests python3 -m unittest discover -s "${experiment_dir}" -p test_summarize.py -v
run_check prepare-inputs-tests python3 -m unittest discover -s "${experiment_dir}" -p test_prepare_inputs.py -v
# bash -n checks only its first file, so check each script separately.
for script in "${experiment_dir}/generate.sh" "${experiment_dir}/verify.sh" "${col_dir}/generate.sh"; do
  run_check "shell-syntax-$(basename "$(dirname "${script}")")-$(basename "${script}" .sh)" bash -n "${script}"
done
run_check whitespace git -C "${experiment_dir}" diff --check -- . ../merged-index-col
echo "RF1/RF2 and general suites, reader projections, and exact transaction transcripts agree."
