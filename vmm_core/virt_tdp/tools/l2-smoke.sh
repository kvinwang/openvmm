#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

usage() {
    cat <<'USAGE'
Usage: l2-smoke.sh --openvmm PATH --kernel PATH --initrd PATH [options]

Boots an L2 twice on the same L1, verifies that userspace is reached, and
requests a clean OpenVMM shutdown after each boot. Run this inside the L1 after
the TD Partitioning driver has been loaded.

Options:
  --openvmm PATH       OpenVMM binary built with the virt_tdp feature
  --kernel PATH        Uncompressed Linux kernel image
  --initrd PATH        Initramfs containing the success marker
  --marker TEXT        Serial success marker (default: L2 USERSPACE REACHED)
  --timeout SECONDS    Per-operation timeout (default: 120)
  --memory-gib N       L2 memory in GiB (default: 4)
  --processors N       Number of L2 virtual processors (default: 1)
  --output-dir PATH    Keep serial and VMM logs here (default: temporary dir)
  --openvmm-arg ARG    Append one OpenVMM argument (repeatable)
  --keep-running       Do not stop the final successful L2
  -h, --help           Show this help

Environment:
  VIRT_TDP_WAKE_SIGNAL defaults to 0. Enabling device kicks is diagnostic and
  unsafe unless the L1 kernel explicitly supports interrupting TDG.VP.ENTER.
  TDP_TMUX may name a tmux command wrapper, for example "sudo tmux".
USAGE
}

openvmm=
kernel=
initrd=
marker='L2 USERSPACE REACHED'
timeout=120
memory_gib=4
processors=1
output_dir=
keep_running=0
openvmm_args=()

while (($#)); do
    case "$1" in
        --openvmm) openvmm=$2; shift 2 ;;
        --kernel) kernel=$2; shift 2 ;;
        --initrd) initrd=$2; shift 2 ;;
        --marker) marker=$2; shift 2 ;;
        --timeout) timeout=$2; shift 2 ;;
        --memory-gib) memory_gib=$2; shift 2 ;;
        --processors) processors=$2; shift 2 ;;
        --output-dir) output_dir=$2; shift 2 ;;
        --openvmm-arg) openvmm_args+=("$2"); shift 2 ;;
        --keep-running) keep_running=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

for value in openvmm kernel initrd; do
    if [[ -z ${!value} ]]; then
        echo "--${value} is required" >&2
        exit 2
    fi
    if [[ ! -r ${!value} ]]; then
        echo "${value} is not readable: ${!value}" >&2
        exit 2
    fi
done

if [[ ! $timeout =~ ^[1-9][0-9]*$ ]] || [[ ! $memory_gib =~ ^[1-9][0-9]*$ ]] ||
   [[ ! $processors =~ ^[1-9][0-9]*$ ]]; then
    echo "timeout, memory-gib, and processors must be positive integers" >&2
    exit 2
fi

if [[ -z $output_dir ]]; then
    output_dir=$(mktemp -d -t virt-tdp-smoke.XXXXXX)
else
    mkdir -p "$output_dir"
fi
output_dir=$(realpath "$output_dir")
openvmm=$(realpath "$openvmm")
kernel=$(realpath "$kernel")
initrd=$(realpath "$initrd")

read -r -a tmux_cmd <<<"${TDP_TMUX:-tmux}"
session="virt-tdp-smoke-$$"
active_session=

capture_failure() {
    local serial=$1
    echo "serial log: $serial" >&2
    tail -n 120 "$serial" >&2 2>/dev/null || true
    if [[ -n $active_session ]]; then
        echo "OpenVMM log:" >&2
        "${tmux_cmd[@]}" capture-pane -pt "$active_session" -S -160 >&2 2>/dev/null || true
    fi
}

cleanup() {
    if [[ -n $active_session ]]; then
        "${tmux_cmd[@]}" kill-session -t "$active_session" 2>/dev/null || true
    fi
}
trap cleanup EXIT

wait_for_marker() {
    local serial=$1
    local deadline=$((SECONDS + timeout))
    while ((SECONDS < deadline)); do
        if grep -Fq -- "$marker" "$serial" 2>/dev/null; then
            return 0
        fi
        if ! "${tmux_cmd[@]}" has-session -t "$active_session" 2>/dev/null; then
            return 1
        fi
        sleep 1
    done
    return 1
}

stop_openvmm() {
    local deadline=$((SECONDS + timeout))
    "${tmux_cmd[@]}" send-keys -t "$active_session" quit Enter
    while ((SECONDS < deadline)); do
        if ! "${tmux_cmd[@]}" has-session -t "$active_session" 2>/dev/null; then
            active_session=
            return 0
        fi
        sleep 1
    done
    return 1
}

for attempt in 1 2; do
    active_session="${session}-${attempt}"
    serial="$output_dir/l2-${attempt}.serial.log"
    vmm_log="$output_dir/l2-${attempt}.vmm.log"
    : >"$serial"

    argv=(
        env "VIRT_TDP_WAKE_SIGNAL=${VIRT_TDP_WAKE_SIGNAL:-0}"
        "$openvmm" --hypervisor "tdp:memory=${memory_gib}"
        -m "${memory_gib}G" -p "$processors" -k "$kernel" -r "$initrd"
        -c 'console=ttyS0 earlyprintk=serial,ttyS0,115200 nokaslr rdinit=/init'
        --com1 "file=$serial"
        "${openvmm_args[@]}"
    )
    printf -v command 'exec '
    printf -v quoted '%q ' "${argv[@]}"
    command+="$quoted>$vmm_log 2>&1"

    "${tmux_cmd[@]}" new-session -d -s "$active_session" "$command"
    if ! wait_for_marker "$serial"; then
        echo "L2 boot attempt $attempt failed" >&2
        capture_failure "$serial"
        exit 1
    fi
    echo "L2 boot attempt $attempt reached userspace"

    if ((attempt == 2 && keep_running)); then
        echo "final L2 remains in tmux session $active_session"
        trap - EXIT
        exit 0
    fi
    if ! stop_openvmm; then
        echo "OpenVMM did not stop cleanly after attempt $attempt" >&2
        capture_failure "$serial"
        exit 1
    fi
    echo "L2 boot attempt $attempt stopped cleanly"
done

echo "two consecutive L2 boots completed; logs: $output_dir"
