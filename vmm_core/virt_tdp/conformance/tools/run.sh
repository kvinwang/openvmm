#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

usage() {
    cat <<'USAGE'
Usage: run.sh --openvmm PATH --kernel PATH --initrd PATH [options]

Runs the virt_tdp conformance workload repeatedly in detached tmux sessions,
strictly parses every case result, and verifies clean OpenVMM shutdown.

Options:
  --openvmm PATH       OpenVMM binary with virt_tdp
  --kernel PATH        Uncompressed Linux kernel
  --initrd PATH        Conformance initramfs
  --manifest PATH      Case manifest (default: suite manifest)
  --processors N       L2 processor count (default: 8)
  --memory-gib N       L2 memory GiB (default: 4)
  --repetitions N      Complete boot/stop cycles (default: 2)
  --timeout SECONDS    Per-boot and shutdown timeout (default: 240)
  --output-dir PATH    Result directory (required)
  --openvmm-arg ARG    Additional OpenVMM argument (repeatable)

Environment:
  TDP_TMUX may be "sudo tmux" inside the L1.
  VIRT_TDP_WAKE_SIGNAL is forced to 0.
USAGE
}

root=$(cd "$(dirname "$0")/.." && pwd)
manifest="$root/manifest.tsv"
openvmm= kernel= initrd= output_dir=
processors=8 memory_gib=4 repetitions=2 timeout=240
openvmm_args=()
while (($#)); do
    case "$1" in
        --openvmm) openvmm=$2; shift 2 ;;
        --kernel) kernel=$2; shift 2 ;;
        --initrd) initrd=$2; shift 2 ;;
        --manifest) manifest=$2; shift 2 ;;
        --processors) processors=$2; shift 2 ;;
        --memory-gib) memory_gib=$2; shift 2 ;;
        --repetitions) repetitions=$2; shift 2 ;;
        --timeout) timeout=$2; shift 2 ;;
        --output-dir) output_dir=$2; shift 2 ;;
        --openvmm-arg) openvmm_args+=("$2"); shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done
for value in openvmm kernel initrd output_dir; do
    [[ -n ${!value} ]] || { echo "--${value//_/-} is required" >&2; exit 2; }
done
for value in processors memory_gib repetitions timeout; do
    [[ ${!value} =~ ^[1-9][0-9]*$ ]] || { echo "$value must be positive" >&2; exit 2; }
done

openvmm=$(realpath "$openvmm")
kernel=$(realpath "$kernel")
initrd=$(realpath "$initrd")
manifest=$(realpath "$manifest")
mkdir -p "$output_dir"
output_dir=$(realpath "$output_dir")
read -r -a tmux_cmd <<<"${TDP_TMUX:-tmux}"
session_prefix="virt-tdp-cts-$$"
active=
cleanup() {
    [[ -z $active ]] || "${tmux_cmd[@]}" kill-session -t "$active" 2>/dev/null || true
}
trap cleanup EXIT

wait_for_end() {
    local serial=$1 deadline=$((SECONDS + timeout))
    while ((SECONDS < deadline)); do
        grep -q 'VIRT_TDP_CTS END ' "$serial" 2>/dev/null && return 0
        "${tmux_cmd[@]}" has-session -t "$active" 2>/dev/null || return 1
        sleep 1
    done
    return 1
}

stop_vmm() {
    local deadline=$((SECONDS + timeout))
    "${tmux_cmd[@]}" send-keys -t "$active" quit Enter
    while ((SECONDS < deadline)); do
        if ! "${tmux_cmd[@]}" has-session -t "$active" 2>/dev/null; then
            active=
            return 0
        fi
        sleep 1
    done
    return 1
}

attempts_json=()
for ((attempt=1; attempt<=repetitions; attempt++)); do
    active="$session_prefix-$attempt"
    serial="$output_dir/attempt-$attempt.serial.log"
    vmm_log="$output_dir/attempt-$attempt.vmm.log"
    result="$output_dir/attempt-$attempt.json"
    : >"$serial"
    argv=(env VIRT_TDP_WAKE_SIGNAL=0
        "$openvmm" --hypervisor "tdp:memory=$memory_gib"
        -m "${memory_gib}G" -p "$processors" -k "$kernel" -r "$initrd"
        -c 'console=ttyS0 earlyprintk=serial,ttyS0,115200 nokaslr rdinit=/init'
        --com1 "file=$serial" "${openvmm_args[@]}")
    printf -v quoted '%q ' "${argv[@]}"
    "${tmux_cmd[@]}" new-session -d -s "$active" "exec $quoted>$vmm_log 2>&1"
    wait_for_end "$serial" || {
        echo "attempt $attempt did not produce an END record" >&2
        tail -120 "$serial" >&2 || true
        exit 1
    }
    "$root/tools/parse-results.py" "$serial" "$manifest" --output "$result"
    stop_vmm || { echo "attempt $attempt did not stop cleanly" >&2; exit 1; }
    attempts_json+=("$result")
    echo "conformance attempt $attempt passed and stopped cleanly"
done

python3 - "$output_dir/summary.json" "$processors" "$memory_gib" "${attempts_json[@]}" <<'PY'
import json, sys
output, processors, memory, *paths = sys.argv[1:]
attempts = [json.load(open(path)) for path in paths]
summary = {
    "schema_version": 1,
    "processors": int(processors),
    "memory_gib": int(memory),
    "repetitions": len(attempts),
    "clean_shutdowns": len(attempts),
    "passed": all(attempt["passed"] for attempt in attempts),
    "attempts": attempts,
}
with open(output, "w") as stream:
    json.dump(summary, stream, indent=2, sort_keys=True)
    stream.write("\n")
PY
echo "all $repetitions conformance attempts passed; summary: $output_dir/summary.json"
