#!/bin/busybox sh
set -eu
i=0
while [ ! -b /dev/vda ] && [ "$i" -lt 50 ]; do
    $BB sleep 0.1
    i=$((i + 1))
done
[ -b /dev/vda ]
