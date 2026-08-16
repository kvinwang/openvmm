#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail
usage() {
    cat <<'USAGE'
Usage: run.sh --openvmm PATH --kernel PATH --disk PATH --output-dir PATH [options]

Boots a persistent Ubuntu Server disk, runs Docker database and stress
workloads, parses their serial protocol, and verifies a clean OpenVMM stop.

Options:
  --processors N       L2 processor count (default: 8)
  --memory-gib N       L2 memory GiB (default: 6)
  --duration SECONDS   Per-duration workload interval (default: 60)
  --timeout SECONDS    Whole-suite and shutdown timeout (default: 1800)
  --openvmm-arg ARG    Additional OpenVMM argument (repeatable)

Environment:
  TDP_TMUX may be "sudo tmux" inside the L1.
  VIRT_TDP_WAKE_SIGNAL is forced to 0.
USAGE
}
openvmm= kernel= disk= output_dir=
processors=8 memory_gib=6 duration=60 timeout=1800
openvmm_args=()
while (($#)); do
    case "$1" in
        --openvmm) openvmm=$2; shift 2;; --kernel) kernel=$2; shift 2;;
        --disk) disk=$2; shift 2;; --output-dir) output_dir=$2; shift 2;;
        --processors) processors=$2; shift 2;; --memory-gib) memory_gib=$2; shift 2;;
        --duration) duration=$2; shift 2;; --timeout) timeout=$2; shift 2;;
        --openvmm-arg) openvmm_args+=("$2"); shift 2;;
        -h|--help) usage; exit 0;; *) echo "unknown argument: $1" >&2; usage >&2; exit 2;;
    esac
done
for value in openvmm kernel disk output_dir; do [[ -n ${!value} ]] || { echo "--${value//_/-} is required" >&2; exit 2; }; done
for value in processors memory_gib duration timeout; do [[ ${!value} =~ ^[1-9][0-9]*$ ]] || { echo "$value must be positive" >&2; exit 2; }; done
openvmm=$(realpath "$openvmm"); kernel=$(realpath "$kernel"); disk=$(realpath "$disk")
mkdir -p "$output_dir"; output_dir=$(realpath "$output_dir")
serial="$output_dir/serial.log"; vmm_log="$output_dir/openvmm.log"; result="$output_dir/result.json"
: >"$serial"; : >"$vmm_log"
read -r -a tmux_cmd <<<"${TDP_TMUX:-tmux}"
session="virt-tdp-stress-$$"; active=1
cleanup() { ((active == 0)) || "${tmux_cmd[@]}" kill-session -t "$session" 2>/dev/null || true; }
trap cleanup EXIT
argv=("$openvmm" --hypervisor "tdp:memory=$memory_gib"
    -m "${memory_gib}G" -p "$processors" -k "$kernel"
    -c "root=/dev/vda1 rootwait rw console=ttyS0 earlyprintk=serial,ttyS0,115200 nokaslr fstab=no systemd.unified_cgroup_hierarchy=1 virt_tdp_stress_seconds=$duration"
    --com1 "file=$serial" --virtio-blk-mmio "file:$disk"
    --virtio-net none --virtio-net-bus mmio "${openvmm_args[@]}")
printf -v quoted '%q ' "${argv[@]}"
"${tmux_cmd[@]}" new-session -d -s "$session" "exec $quoted>$vmm_log 2>&1"
deadline=$((SECONDS + timeout)); complete=0
while ((SECONDS < deadline)); do
    if grep -q 'VIRT_TDP_STRESS END ' "$serial" 2>/dev/null; then complete=1; break; fi
    "${tmux_cmd[@]}" has-session -t "$session" 2>/dev/null || break
    sleep 2
done
((complete)) || { echo "Ubuntu Docker stress suite did not complete" >&2; tail -120 "$serial" >&2 || true; exit 1; }
parser=$(cd "$(dirname "$0")" && pwd)/parse-results.py
parse_rc=0; "$parser" "$serial" --output "$result" || parse_rc=$?
"${tmux_cmd[@]}" send-keys -t "$session" quit Enter
for _ in {1..120}; do
    if ! "${tmux_cmd[@]}" has-session -t "$session" 2>/dev/null; then active=0; break; fi
    sleep 0.25
done
((active == 0)) || { echo "OpenVMM did not stop cleanly" >&2; exit 1; }
python3 - "$result" "$output_dir/summary.json" "$processors" "$memory_gib" "$duration" <<'PY'
import json, sys
source, output, processors, memory, duration = sys.argv[1:]
result = json.load(open(source))
summary = {"schema_version": 1, "os": "Ubuntu Server", "container_runtime": "Docker",
           "processors": int(processors), "memory_gib": int(memory), "duration_seconds": int(duration),
           "clean_shutdown": True, "passed": result["passed"], "result": result}
with open(output, "w") as stream: json.dump(summary, stream, indent=2, sort_keys=True); stream.write("\n")
PY
exit "$parse_rc"
