#!/usr/bin/env bash
# Build one image set (native-A and merged-index-A) with measure.py --build-images.
# About 1.5 h on the first host (two cold release builds of about 13 and 9 min, then the two
# loads at once, 66 and 74 min) and about 35 GB. Run it as a background job.
# Usage: linux/build-images.sh <name> [measure.py flags...]
#   name:  the set's directory under $R5_OUT, by the convention images-v<version>[-pf][-<keys>]
#   flags: --index-prefix-filter for a -pf set; for another --order-keys arm, pass
#          R5_BASE=$R5_INPUTS/<arm>-k10/base to load its re-keyed base tables.
# Never pass the campaign's --cache-bytes or --batch: loading has its own defaults.
set -euo pipefail
. "$HOME/r5-env.sh"
name="$1"; shift
out="$R5_OUT/$name"
[[ ! -e "$out" ]] || { echo "refusing: $out exists" >&2; exit 2; }
base="${R5_BASE:-$DBGEN_OUT}"
cd "$REPO/sql-to-dbsp-compiler/experiments/q3-ivm-baseline"
[[ -z "$(git status --short)" ]] || { echo "refusing: the checkout is dirty" >&2; exit 2; }
echo "$(date -u +%FT%TZ) images $name at $(git rev-parse --short HEAD)"
python3 measure.py --build-images "$out" \
  --customer "$base/customer.tbl" --orders "$base/orders.tbl" --lineitem "$base/lineitem.tbl" \
  --elide-value-headers --device-label ssd "$@"
echo "$(date -u +%FT%TZ) images $name done"
