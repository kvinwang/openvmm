#!/bin/busybox sh
set -eu
write=/tmp/cts-block-write
read=/tmp/cts-block-read
i=0
while [ "$i" -lt 256 ]; do
    printf 'virt-tdp-cts-%08d\n' "$i" >"$write"
    $BB dd if="$write" of=/dev/vda bs=512 seek=$((2048 + i)) count=1 conv=sync 2>/dev/null
    $BB dd if=/dev/vda of="$read" bs=512 skip=$((2048 + i)) count=1 2>/dev/null
    expected=$(printf 'virt-tdp-cts-%08d' "$i")
    actual=$($BB head -n 1 "$read")
    [ "$actual" = "$expected" ] || {
        echo "block iteration $i returned '$actual', expected '$expected'"
        exit 1
    }
    i=$((i + 1))
done
