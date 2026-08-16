#!/bin/busybox sh
set -eu
i=0
while [ ! -d /sys/class/net/eth0 ] && [ "$i" -lt 50 ]; do
    $BB sleep 0.1
    i=$((i + 1))
done
[ -d /sys/class/net/eth0 ]
$BB ip link set eth0 up
[ "$($BB cat /sys/class/net/eth0/operstate)" != down ]
