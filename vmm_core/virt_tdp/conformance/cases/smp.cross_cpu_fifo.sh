#!/bin/busybox sh
set -eu
$BB taskset -pc 0 $$ >/dev/null
cpu=1
while [ "$cpu" -lt "$EXPECTED_CPUS" ]; do
    ping=/tmp/cts-ping-$cpu
    pong=/tmp/cts-pong-$cpu
    $BB mkfifo "$ping" "$pong"
    $BB taskset -c "$cpu" $BB sh -c '
        exec 3<>"$1" 4<>"$2"
        i=0
        while [ "$i" -lt 256 ]; do
            read -r token <&3
            [ "$token" = "$i" ] || exit 1
            echo "$i" >&4
            i=$((i + 1))
        done
    ' sh "$ping" "$pong" &
    peer=$!
    exec 3<>"$ping" 4<>"$pong"
    i=0
    while [ "$i" -lt 256 ]; do
        echo "$i" >&3
        read -r token <&4
        [ "$token" = "$i" ] || exit 1
        i=$((i + 1))
    done
    wait "$peer"
    exec 3>&- 4>&-
    $BB rm -f "$ping" "$pong"
    cpu=$((cpu + 1))
done
