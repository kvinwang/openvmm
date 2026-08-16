#!/bin/busybox sh
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -u

bb=/bin/busybox
root=/opt/virt-tdp-cts
manifest=$root/manifest.tsv
expected_cpus=${EXPECTED_CPUS:?EXPECTED_CPUS is required}
export BB=$bb EXPECTED_CPUS=$expected_cpus PROBE=$root/cts-probe

pass=0
fail=0
skip=0
total=0

emit() {
    echo "<3>VIRT_TDP_CTS $*" >/dev/kmsg
}

available() {
    case "$1" in
        none) return 0 ;;
        block) [ -b /dev/vda ] ;;
        network) [ -d /sys/class/net/eth0 ] ;;
        *) return 1 ;;
    esac
}

emit "BEGIN version=1 expected_cpus=$expected_cpus"
while IFS="$(printf '\t')" read -r id timeout min_cpus requires description; do
    case "$id" in ''|'#'*) continue ;; esac
    total=$((total + 1))
    if [ "$expected_cpus" -lt "$min_cpus" ]; then
        emit "RESULT id=$id status=SKIP duration_ms=0 reason=min_cpus_$min_cpus"
        skip=$((skip + 1))
        continue
    fi
    if ! available "$requires"; then
        emit "RESULT id=$id status=SKIP duration_ms=0 reason=requires_$requires"
        skip=$((skip + 1))
        continue
    fi

    output=/tmp/cts-output
    start=$($bb date +%s)
    $bb timeout -s KILL "$timeout" "$bb" sh "$root/cases/$id.sh" >"$output" 2>&1
    rc=$?
    end=$($bb date +%s)
    duration=$(((end - start) * 1000))
    if [ "$rc" -eq 0 ]; then
        emit "RESULT id=$id status=PASS duration_ms=$duration reason=none"
        pass=$((pass + 1))
    elif [ "$rc" -eq 77 ]; then
        emit "RESULT id=$id status=SKIP duration_ms=$duration reason=case_skip"
        skip=$((skip + 1))
    else
        emit "RESULT id=$id status=FAIL duration_ms=$duration reason=exit_$rc"
        $bb sed -n '1,20{s/[^A-Za-z0-9_.:,+\/-]/_/g;s/^/<3>VIRT_TDP_CTS DETAIL /;w /dev/kmsg
}' "$output"
        fail=$((fail + 1))
    fi
done <"$manifest"

emit "END total=$total pass=$pass fail=$fail skip=$skip"
[ "$fail" -eq 0 ]
