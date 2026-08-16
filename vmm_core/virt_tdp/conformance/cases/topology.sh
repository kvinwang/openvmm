#!/bin/busybox sh
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
set -eu
id=${1:?case ID is required}
leaf=${id##*.}
cpu=${leaf#cpu}
case "$cpu" in ''|*[!0-9]*) echo "invalid CPU suffix: $leaf"; exit 2;; esac
[ "$cpu" -lt "$EXPECTED_CPUS" ] || exit 77
case "$id" in
    cpu.affinity.cpu*)
        actual=$($BB taskset -c "$cpu" $BB sh -c '$BB awk "{ print \$39 }" /proc/self/stat')
        [ "$actual" = "$cpu" ]
        ;;
    cpu.online.cpu*)
        directory=/sys/devices/system/cpu/cpu$cpu
        [ -d "$directory" ]
        [ ! -f "$directory/online" ] || [ "$($BB cat "$directory/online")" = 1 ]
        ;;
    cpu.fpu.cpu*) exec "$PROBE" fpu-one "$cpu" ;;
    smp.fifo.cpu*)
        [ "$cpu" -gt 0 ] || exit 2
        ping=/tmp/cts-ping-$cpu; pong=/tmp/cts-pong-$cpu
        cleanup() { $BB rm -f "$ping" "$pong"; }
        trap cleanup EXIT
        $BB mkfifo "$ping" "$pong"
        $BB taskset -pc 0 $$ >/dev/null
        $BB taskset -c "$cpu" $BB sh -c '
            exec 3<>"$1" 4<>"$2"; i=0
            while [ "$i" -lt 256 ]; do read -r token <&3; [ "$token" = "$i" ]; echo "$i" >&4; i=$((i+1)); done
        ' sh "$ping" "$pong" & peer=$!
        exec 3<>"$ping" 4<>"$pong"; i=0
        while [ "$i" -lt 256 ]; do echo "$i" >&3; read -r token <&4; [ "$token" = "$i" ]; i=$((i+1)); done
        wait "$peer"
        ;;
    apic.reschedule.cpu*)
        [ "$cpu" -gt 0 ] || exit 2
        column=$((cpu + 2))
        before=$($BB awk -v c="$column" '/^RES:/ {print $c}' /proc/interrupts)
        i=0; while [ "$i" -lt 128 ]; do $BB taskset -c "$cpu" $BB true; i=$((i+1)); done
        after=$($BB awk -v c="$column" '/^RES:/ {print $c}' /proc/interrupts)
        [ "$after" -gt "$before" ]
        ;;
    apic.nmi.cpu*)
        [ "$cpu" -gt 0 ] || exit 2
        $BB taskset -pc 0 $$ >/dev/null
        column=$((cpu + 2))
        before=$($BB awk -v c="$column" '/^NMI:/ {print $c}' /proc/interrupts)
        attempt=0
        while [ "$attempt" -lt 3 ]; do
            echo l >/proc/sysrq-trigger; $BB sleep 1
            after=$($BB awk -v c="$column" '/^NMI:/ {print $c}' /proc/interrupts)
            [ "$after" -gt "$before" ] && exit 0
            attempt=$((attempt + 1))
        done
        echo "CPU$cpu NMI counter stayed at $before after three shorthand deliveries"
        exit 1
        ;;
    apic.hotplug.cpu*)
        [ "$cpu" -gt 0 ] || exit 2
        online=/sys/devices/system/cpu/cpu$cpu/online
        [ -w "$online" ]
        restore() { echo 1 >"$online" 2>/dev/null || true; }
        trap restore EXIT
        echo 0 >"$online"; [ "$($BB cat "$online")" = 0 ]
        echo 1 >"$online"; [ "$($BB cat "$online")" = 1 ]
        trap - EXIT
        ;;
    *) echo "unknown topology case: $id"; exit 2 ;;
esac
