#!/bin/busybox sh
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
set -eu
id=${1:?case ID is required}
block_io() {
    size=$1 seek=$2
    source=/tmp/cts-block-source; result=/tmp/cts-block-result
    $BB dd if=/dev/zero of="$source" bs="$size" count=1 2>/dev/null
    printf '%s' "$id" | $BB dd of="$source" conv=notrunc 2>/dev/null
    $BB dd if="$source" of=/dev/vda bs="$size" seek="$seek" count=1 conv=fsync 2>/dev/null
    $BB dd if=/dev/vda of="$result" bs="$size" skip="$seek" count=1 2>/dev/null
    $BB cmp "$source" "$result"
}
net_tx() {
    count=$1 size=$2
    $BB ip link set eth0 up
    before=$($BB cat /sys/class/net/eth0/statistics/tx_packets)
    "$PROBE" nettx eth0 "$count" "$size"
    i=0
    while [ "$i" -lt 50 ]; do
        after=$($BB cat /sys/class/net/eth0/statistics/tx_packets)
        [ $((after-before)) -ge "$count" ] && return 0
        $BB sleep 0.1; i=$((i+1))
    done
    echo "tx_packets advanced by $((after-before)), expected $count"; return 1
}
case "$id" in
    block.io.size512) block_io 512 4096 ;;
    block.io.size1k) block_io 1024 2049 ;;
    block.io.size4k) block_io 4096 513 ;;
    block.io.size64k) block_io 65536 33 ;;
    block.io.offset.low) block_io 512 128 ;;
    block.io.offset.middle) block_io 512 8192 ;;
    block.io.offset.high) block_io 512 65536 ;;
    block.io.repeated32)
        i=0; while [ "$i" -lt 32 ]; do block_io 512 $((16384+i)); i=$((i+1)); done ;;
    block.capacity.nonzero) [ "$($BB cat /sys/class/block/vda/size)" -gt 0 ] ;;
    block.logical_block_size) [ "$($BB cat /sys/class/block/vda/queue/logical_block_size)" = 512 ] ;;
    network.tx.size64) net_tx 32 64 ;;
    network.tx.size128) net_tx 32 128 ;;
    network.tx.size512) net_tx 32 512 ;;
    network.tx.size1024) net_tx 32 1024 ;;
    network.tx.size1500) net_tx 32 1500 ;;
    network.tx.count1) net_tx 1 128 ;;
    network.tx.count256) net_tx 256 128 ;;
    network.tx.count1024) net_tx 1024 128 ;;
    network.mtu) [ "$($BB cat /sys/class/net/eth0/mtu)" -ge 1500 ] ;;
    network.mac) $BB grep -Eq '^([0-9a-f]{2}:){5}[0-9a-f]{2}$' /sys/class/net/eth0/address ;;
    *) echo "unknown device case: $id"; exit 2 ;;
esac
