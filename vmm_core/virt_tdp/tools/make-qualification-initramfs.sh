#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

if (($# < 2 || $# > 3)); then
    echo "usage: $0 BASE_INITRAMFS OUTPUT_INITRAMFS [EXPECTED_CPUS]" >&2
    exit 2
fi

base=$(realpath "$1")
output=$2
expected_cpus=${3:-1}
if [[ ! $expected_cpus =~ ^[1-9][0-9]*$ ]]; then
    echo "EXPECTED_CPUS must be a positive integer" >&2
    exit 2
fi
work=$(mktemp -d -t virt-tdp-qualification.XXXXXX)
trap 'rm -rf "$work"' EXIT

cat >"$work/init" <<'INIT'
#!/bin/busybox sh
set -eu

bb=/bin/busybox
$bb mount -t proc proc /proc
$bb mount -t sysfs sysfs /sys
$bb mount -t devtmpfs devtmpfs /dev

fail() {
    echo "<3>##### L2 QUALIFICATION FAILED: $* #####" >/dev/kmsg
    $bb poweroff -f
}

cpu_count=$($bb grep -c '^processor' /proc/cpuinfo) || true
[ "$cpu_count" = @EXPECTED_CPUS@ ] || fail "cpu count is $cpu_count, expected @EXPECTED_CPUS@"
echo "<3>##### L2 CPU PASS #####" >/dev/kmsg

if [ @EXPECTED_CPUS@ -gt 1 ]; then
    $bb mkdir -p /tmp
    # Keep the coordinator on the BSP so each AP is exercised by the pinned
    # peer rather than by incidental migration of PID 1 between rounds.
    $bb taskset -pc 0 $$ >/dev/null || fail "could not pin the coordinator to CPU0"
    cpu=1
    while [ "$cpu" -lt @EXPECTED_CPUS@ ]; do
        echo "<3>##### L2 TESTING CPU$cpu #####" >/dev/kmsg
        resched_before=$($bb awk -v column=$((cpu + 2)) '/^RES:/ { print $column }' /proc/interrupts)
        [ -n "$resched_before" ] || fail "CPU$cpu reschedule IPI counter is unavailable"
        ping=/tmp/cpu-ping-$cpu
        pong=/tmp/cpu-pong-$cpu
        $bb mkfifo "$ping" "$pong"
        $bb taskset -c "$cpu" $bb sh -c '
        exec 3<>"$1"
        exec 4<>"$2"
        i=0
        while [ "$i" -lt 256 ]; do
            read -r token <&3
            [ "$token" = "$i" ] || exit 1
            echo "$i" >&4
            i=$((i + 1))
        done
        ' sh "$ping" "$pong" &
        peer=$!
        exec 3<>"$ping"
        exec 4<>"$pong"
        i=0
        while [ "$i" -lt 256 ]; do
            echo "$i" >&3
            read -r token <&4
            [ "$token" = "$i" ] || fail "CPU$cpu wake returned $token, expected $i"
            i=$((i + 1))
        done
        wait "$peer" || fail "CPU$cpu cross-CPU wake peer failed"
        exec 3>&- 4>&-
        $bb rm -f "$ping" "$pong"
        resched_after=$($bb awk -v column=$((cpu + 2)) '/^RES:/ { print $column }' /proc/interrupts)
        [ "$resched_after" -gt "$resched_before" ] || \
            fail "CPU$cpu reschedule IPI count did not increase"
        cpu=$((cpu + 1))
    done
    echo "<3>##### L2 CROSS-CPU WAKE PASS #####" >/dev/kmsg
    echo "<3>##### L2 RESCHEDULE IPI PASS #####" >/dev/kmsg

    nmi_before=$($bb awk '/^NMI:/ { for (i = 2; i <= NF; i++) total += $i; print total + 0 }' /proc/interrupts)
    [ -n "$nmi_before" ] || fail "NMI counters are unavailable"
    [ -w /proc/sysrq-trigger ] || fail "SysRq NMI broadcast trigger is unavailable"
    echo l >/proc/sysrq-trigger || fail "NMI broadcast request failed"
    $bb sleep 1
    nmi_after=$($bb awk '/^NMI:/ { for (i = 2; i <= NF; i++) total += $i; print total + 0 }' /proc/interrupts)
    [ $((nmi_after - nmi_before)) -ge $((@EXPECTED_CPUS@ - 1)) ] || \
        fail "NMI broadcast reached only $((nmi_after - nmi_before)) CPUs"
    echo "<3>##### L2 NMI BROADCAST PASS #####" >/dev/kmsg

    cpu1=/sys/devices/system/cpu/cpu1/online
    [ -w "$cpu1" ] || fail "CPU1 does not support hotplug"
    echo 0 >"$cpu1" || fail "CPU1 offline request failed"
    [ "$($bb cat "$cpu1")" = 0 ] || fail "CPU1 stayed online"
    echo 1 >"$cpu1" || fail "CPU1 online request failed"
    [ "$($bb cat "$cpu1")" = 1 ] || fail "CPU1 stayed offline"
    echo "<3>##### L2 CPU HOTPLUG PASS #####" >/dev/kmsg
fi

before=$($bb cut -d. -f1 /proc/uptime)
$bb sleep 3
after=$($bb cut -d. -f1 /proc/uptime)
[ $((after - before)) -ge 2 ] || fail "monotonic time did not advance"
echo "<3>##### L2 TIME PASS #####" >/dev/kmsg

i=0
while [ ! -b /dev/vda ] && [ "$i" -lt 50 ]; do
    $bb sleep 0.1
    i=$((i + 1))
done
[ -b /dev/vda ] || fail "virtio block device did not appear"
echo virt-tdp-block-check | $bb dd of=/dev/vda bs=512 seek=8 conv=sync 2>/dev/null
$bb dd if=/dev/vda of=/tmp.block bs=512 skip=8 count=1 2>/dev/null
$bb grep -q '^virt-tdp-block-check$' /tmp.block || fail "virtio block readback differs"
echo "<3>##### L2 BLOCK PASS #####" >/dev/kmsg

i=0
while [ ! -d /sys/class/net/eth0 ] && [ "$i" -lt 50 ]; do
    $bb sleep 0.1
    i=$((i + 1))
done
[ -d /sys/class/net/eth0 ] || fail "virtio network device did not appear"
$bb ip link set eth0 up || fail "virtio network link could not be enabled"
[ "$($bb cat /sys/class/net/eth0/operstate)" != down ] || fail "virtio network link stayed down"
echo "<3>##### L2 NETWORK PASS #####" >/dev/kmsg

echo "<3>##### L2 QUALIFICATION PASSED #####" >/dev/kmsg
# Leave shutdown to the harness. It polls the serial log once per second and
# then asks OpenVMM to quit, so powering off immediately can win that race and
# turn a successful qualification into a test of an unrelated ACPI S5 path.
# The fallback keeps a standalone qualification guest from running forever.
$bb sleep 30
$bb poweroff -f
INIT
sed -i "s/@EXPECTED_CPUS@/$expected_cpus/g" "$work/init"
chmod +x "$work/init"

overlay="$work/overlay.cpio"
(cd "$work" && printf 'init\n' | cpio -o -H newc --quiet >"$overlay")
{
    gzip -dc "$base"
    cat "$overlay"
} | gzip -n -9 >"$output"
