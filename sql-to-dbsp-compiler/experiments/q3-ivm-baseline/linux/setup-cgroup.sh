#!/usr/bin/env bash
# Create the cgroup v2 tree that measure.py --cgroup-root /sys/fs/cgroup/r5 needs, then run the
# pre-flight checks that need it (LINUX-HANDOFF.md, "cgroup v2 root" and "Pre-flight checks").
# Needs sudo. Idempotent. The tree does not survive a reboot: rerun this after every boot.
# Usage: linux/setup-cgroup.sh [--clean]   (--clean also removes finished runs' r5-* children)
set -euo pipefail
root=/sys/fs/cgroup/r5

[[ "$(stat -fc %T /sys/fs/cgroup)" == cgroup2fs ]] || { echo "not unified cgroup v2" >&2; exit 1; }
grep -qw memory /sys/fs/cgroup/cgroup.controllers || { echo "no memory controller" >&2; exit 1; }

sudo mkdir -p "$root/driver"
echo +memory | sudo tee /sys/fs/cgroup/cgroup.subtree_control >/dev/null
echo +memory | sudo tee "$root/cgroup.subtree_control" >/dev/null
sudo chown -R "$USER" "$root"

if [[ "${1:-}" == --clean ]]; then
  find "$root" -mindepth 1 -maxdepth 1 -name 'r5-*' -type d -exec rmdir {} +
fi

[[ -e "$root/driver/memory.swap.max" ]] || { echo "swap accounting is off" >&2; exit 1; }
# Delegation: a moved shell must land in the driver leaf.
bash -c "echo \$\$ | sudo tee $root/driver/cgroup.procs >/dev/null && grep -qx '0::/r5/driver' /proc/self/cgroup" \
  || { echo "cannot move a shell into $root/driver" >&2; exit 1; }
echo "cgroup tree ready at $root (kernel $(uname -r))"
