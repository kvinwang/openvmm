#!/bin/busybox sh
set -eu
before=$($BB awk '/^NMI:/ { for (i=2; i<=NF; i++) total += $i; print total + 0 }' /proc/interrupts)
[ -w /proc/sysrq-trigger ]
echo l >/proc/sysrq-trigger
$BB sleep 1
after=$($BB awk '/^NMI:/ { for (i=2; i<=NF; i++) total += $i; print total + 0 }' /proc/interrupts)
delta=$((after - before))
[ "$delta" -ge $((EXPECTED_CPUS - 1)) ] || {
    echo "NMI shorthand reached $delta of $((EXPECTED_CPUS - 1)) targets"
    exit 1
}
