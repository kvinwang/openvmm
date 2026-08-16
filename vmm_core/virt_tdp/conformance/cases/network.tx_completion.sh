#!/bin/busybox sh
set -eu
before=$($BB cat /sys/class/net/eth0/statistics/tx_packets)
"$PROBE" nettx eth0 256
i=0
while [ "$i" -lt 50 ]; do
    after=$($BB cat /sys/class/net/eth0/statistics/tx_packets)
    [ $((after - before)) -ge 256 ] && exit 0
    $BB sleep 0.1
    i=$((i + 1))
done
echo "tx_packets advanced from $before to $after"
exit 1
