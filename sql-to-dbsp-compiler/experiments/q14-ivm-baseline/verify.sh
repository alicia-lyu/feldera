#!/usr/bin/env bash
# The Q14 harness's gate: both builds' tests and lints, the byte-for-byte comparison of their
# transcripts, the hand-written circuit's tests, and MI_P's freshness, tests, and lints. Each
# stage logs to a fresh directory under target/q14-verification/; the script stops at the
# first failing stage and prints the end of its log.
set -euo pipefail

experiment_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
manifest="${experiment_dir}/Cargo.toml"
index_dir="${experiment_dir}/../merged-index-p"
index_manifest="${index_dir}/Cargo.toml"
artifacts="${experiment_dir}/target/q14-verification/$(date -u +%Y%m%dT%H%M%SZ)-$$"
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
run_check format cargo +1.96.1 fmt --manifest-path "${manifest}" --all -- --check
run_check native-tests env Q14_GENERAL_TRANSCRIPT="${artifacts}/native-general.json" \
  cargo +1.96.1 test --manifest-path "${manifest}" --offline --locked -j2
run_check native-clippy cargo +1.96.1 clippy --manifest-path "${manifest}" \
  --offline --locked --all-targets --workspace -j2 -- -D warnings

# The reconstructed build needs no generation: its view circuit is written by hand.
run_check reconstructed-tests env Q14_GENERAL_TRANSCRIPT="${artifacts}/reconstructed-general.json" \
  cargo +1.96.1 test --manifest-path "${manifest}" --offline --locked -j2 --features merged-index
run_check reconstructed-clippy cargo +1.96.1 clippy --manifest-path "${manifest}" \
  --offline --locked --all-targets --workspace -j2 --features merged-index -- -D warnings
run_check trie-circuit-tests cargo +1.96.1 test --manifest-path "${manifest}" --offline --locked -j2 \
  -p q14-trie-view-circuit

test -s "${artifacts}/native-general.json"
test -s "${artifacts}/reconstructed-general.json"
run_check general-transcript-comparison cmp "${artifacts}/native-general.json" \
  "${artifacts}/reconstructed-general.json"

# The committed generated module must match what mi_p.mi and the compiler produce now.
run_check index-generation "${index_dir}/generate.sh" "${artifacts}/generated.rs"
run_check index-generated-fresh cmp "${artifacts}/generated.rs" "${index_dir}/src/generated.rs"
run_check index-format cargo +1.96.1 fmt --manifest-path "${index_manifest}" --all -- --check
run_check index-tests cargo +1.96.1 test --manifest-path "${index_manifest}" --offline --locked -j2
run_check index-clippy cargo +1.96.1 clippy --manifest-path "${index_manifest}" \
  --offline --locked --all-targets -j2 -- -D warnings
# bash -n checks only its first file, so check each script separately.
for script in "${experiment_dir}/generate.sh" "${experiment_dir}/verify.sh" "${index_dir}/generate.sh"; do
  run_check "shell-syntax-$(basename "$(dirname "${script}")")-$(basename "${script}" .sh)" bash -n "${script}"
done
run_check whitespace git -C "${experiment_dir}" diff --check -- . ../merged-index-p
echo "Both builds agree with the reference, and their transcripts are byte-identical."
