#!/bin/busybox sh
set -eu
online=/sys/devices/system/cpu/cpu1/online
[ -w "$online" ]
restore() { echo 1 >"$online" 2>/dev/null || true; }
trap restore EXIT
echo 0 >"$online"
[ "$($BB cat "$online")" = 0 ]
echo 1 >"$online"
[ "$($BB cat "$online")" = 1 ]
trap - EXIT
