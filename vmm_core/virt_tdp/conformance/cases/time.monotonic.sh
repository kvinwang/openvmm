#!/bin/busybox sh
set -eu
before=$($BB cut -d. -f1 /proc/uptime)
$BB sleep 3
after=$($BB cut -d. -f1 /proc/uptime)
[ $((after - before)) -ge 2 ] || {
    echo "uptime advanced from $before to $after"
    exit 1
}
