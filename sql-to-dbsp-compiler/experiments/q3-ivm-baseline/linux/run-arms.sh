#!/usr/bin/env bash
# Run a list of A-seek pilot arms on one image set, then a --diagnostics rerun of each (in a
# 2 GiB cgroup, last arm first) for spill_kinds.py's writes by kind.
# Usage: linux/run-arms.sh <images dir> <arms file> [workload clustered] [rows 10]
# The arms file has one arm per line, "#" comments allowed:
#   <tag> <cache MiB> [extra measure.py flags...]
# e.g. linux/arms/memtable-2026-11-20.txt. The memory target stays 512 MiB.
# Three arms took about 6 h on the first host (timed 40-50 min each, diagnostics 65-80 min).
# An agent: start it as a background shell task (auto mode refuses nohup and setsid), log to a
# file, and keep the checkout clean until "all done".
set -euo pipefail
images="$1"; arms_file="$2"; workload="${3:-clustered}"; rows="${4:-10}"
here="$(cd "$(dirname "$0")" && pwd)"
MIB=1048576

python3 -c "import json,sys; sys.exit(json.load(open('$images/images.json'))['status'] != 'complete')" \
  || { echo "refusing: $images is not a complete image set" >&2; exit 2; }
mapfile -t arms < <(grep -v '^[[:space:]]*\(#\|$\)' "$arms_file")
(( ${#arms[@]} > 0 )) || { echo "refusing: no arms in $arms_file" >&2; exit 2; }
for arm in "${arms[@]}"; do
  read -r tag cache _ <<<"$arm"
  [[ -n "$tag" && "$cache" =~ ^[0-9]+$ ]] || { echo "bad arm line: $arm" >&2; exit 2; }
done
echo "$(date -u +%FT%TZ) ${#arms[@]} arms on $images"

pilot() {  # tag cache_mib total_bytes extra...
  local tag="$1" cache="$2" total="$3"; shift 3
  echo "$(date -u +%FT%TZ) pilot $tag"
  R5_IMAGES="$images" R5_TAG="$tag" R5_CACHE_BYTES=$((cache * MIB)) \
    R5_TARGET_BYTES=$((512 * MIB)) R5_TOTAL_BYTES="$total" R5_EXTRA="$*" \
    "$here/run-pilot.sh" "$workload" "$rows" || echo "pilot $tag exited $?"
}
for arm in "${arms[@]}"; do
  read -r tag cache extra <<<"$arm"
  # shellcheck disable=SC2086  # extra is a list of flags
  pilot "$tag" "$cache" $((1024 * MIB)) $extra
done
for (( i = ${#arms[@]} - 1; i >= 0; i-- )); do
  read -r tag cache extra <<<"${arms[$i]}"
  # shellcheck disable=SC2086
  pilot "$tag-diag" "$cache" $((2048 * MIB)) --diagnostics $extra
done
echo "$(date -u +%FT%TZ) all done"
