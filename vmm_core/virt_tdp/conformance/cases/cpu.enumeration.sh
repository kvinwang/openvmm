#!/bin/busybox sh
set -eu
count=$($BB grep -c '^processor' /proc/cpuinfo) || true
[ "$count" = "$EXPECTED_CPUS" ] || {
    echo "enumerated $count CPUs, expected $EXPECTED_CPUS"
    exit 1
}
