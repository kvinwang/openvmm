#!/bin/busybox sh
set -eu
$BB taskset -pc 0 $$ >/dev/null
cpu=1
while [ "$cpu" -lt "$EXPECTED_CPUS" ]; do
    column=$((cpu + 2))
    before=$($BB awk -v column="$column" '/^RES:/ { print $column }' /proc/interrupts)
    [ -n "$before" ]
    i=0
    while [ "$i" -lt 64 ]; do
        $BB taskset -c "$cpu" $BB true
        i=$((i + 1))
    done
    after=$($BB awk -v column="$column" '/^RES:/ { print $column }' /proc/interrupts)
    [ "$after" -gt "$before" ] || {
        echo "CPU$cpu RES counter stayed at $before"
        exit 1
    }
    cpu=$((cpu + 1))
done
