#!/usr/bin/env bash
# Run one R5 pilot (measure.py --images ... --runs 1) inside /sys/fs/cgroup/r5/driver.
# Arm A-seek only: prefix truncation on and the seek pass (both decided 2026-11-18).
# Usage: linux/run-pilot.sh <workload: clustered|scattered> <rows: 10|100|1000> [ssd|hdd]
# Environment (after ~/r5-env.sh):
#   R5_IMAGES       required: the image set, e.g. $R5_OUT/images-v18-pf
#   R5_TAG          suffix of the output name, e.g. v18-pf-t512-c448-m64
#   R5_EXTRA        extra measure.py flags, e.g. "--index-prefix-filter --memtable-mib 64";
#                   --index-prefix-filter must match the image set (-pf)
#   R5_INPUTS_ARM   an --order-keys arm's inputs ($R5_INPUTS/<arm>-k<rows>), e.g. uuid
#   R5_CACHE_BYTES  default 536870912 (one budget: cache = target, decided 2026-11-18)
#   R5_TARGET_BYTES default 536870912 (DBSP memory-pressure target)
#   R5_TOTAL_BYTES  default 1073741824 (cgroup memory.max); 2 GiB for --diagnostics reruns
#   R5_RUNS         default 1 (a pilot); 3 for a campaign, whose outputs are named campaign-*
# Output: $R5_OUT/<pilot|campaign>-A-seek-<workload>-k<rows>[-<tag>]/manifest.json. About 40 to 50 min
# per timed pilot at K = 10 on the first host, about 70 to 80 min with --diagnostics.
set -euo pipefail
workload="$1"; rows="$2"; device="${3:-ssd}"
. "$HOME/r5-env.sh"
: "${R5_IMAGES:?set R5_IMAGES to an image set directory}"

cache_bytes="${R5_CACHE_BYTES:-536870912}"
target_bytes="${R5_TARGET_BYTES:-536870912}"
total_bytes="${R5_TOTAL_BYTES:-1073741824}"
tag="${R5_TAG:+-$R5_TAG}"
runs="${R5_RUNS:-1}"
kind=pilot; (( runs == 1 )) || kind=campaign
if [[ "$device" == ssd ]]; then
  name="${kind}-A-seek-${workload}-k${rows}${tag}"
  storage="$R5_OUT/$name/runs"
else
  name="${kind}-hdd-A-seek-${workload}-k${rows}${tag}"
  storage="$HDD_MOUNT/r5/$name/runs"
fi
report="$R5_OUT/$name/manifest.json"
if [[ -e "$report" || -e "$storage" ]]; then
  echo "refusing: $report or $storage already exists" >&2; exit 2
fi
cd "$REPO/sql-to-dbsp-compiler/experiments/q3-ivm-baseline"
# The manifest records git_status_short, which must be empty: edit docs in another worktree.
[[ -z "$(git status --short)" ]] || { echo "refusing: the checkout is dirty" >&2; exit 2; }
mkdir -p "$(dirname "$report")"

# Move this shell into the driver leaf; measure.py and its runs inherit it.
echo $$ | sudo tee /sys/fs/cgroup/r5/driver/cgroup.procs >/dev/null
grep -qx "0::/r5/driver" /proc/self/cgroup

# shellcheck disable=SC2086  # R5_EXTRA is a list of flags
exec python3 measure.py \
  --images "$R5_IMAGES" \
  --refresh-list "$R5_INPUTS/${R5_INPUTS_ARM:+${R5_INPUTS_ARM}-}k${rows}/$workload/refreshes.json" \
  --report "$report" \
  --storage-path "$storage" \
  --cgroup-root /sys/fs/cgroup/r5 \
  --total-memory-bytes "$total_bytes" \
  --cache-bytes "$cache_bytes" \
  --memory-target-bytes "$target_bytes" \
  --runs "$runs" \
  --no-verify \
  --front-code-byte-keys \
  --elide-value-headers \
  --session-pass seek \
  --device-label "$device" \
  ${R5_EXTRA:-}
