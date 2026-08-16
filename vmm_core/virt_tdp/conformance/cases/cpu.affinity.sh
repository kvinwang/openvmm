#!/bin/busybox sh
set -eu
cpu=0
while [ "$cpu" -lt "$EXPECTED_CPUS" ]; do
    actual=$($BB taskset -c "$cpu" $BB sh -c '$BB awk "{ print \$39 }" /proc/self/stat' 2>/dev/null) || {
        echo "CPU$cpu rejected an affinity-bound task"
        exit 1
    }
    [ "$actual" = "$cpu" ] || {
        echo "task requested CPU$cpu but ran on CPU$actual"
        exit 1
    }
    cpu=$((cpu + 1))
done
