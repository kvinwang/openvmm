#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail
usage() {
    cat <<'USAGE'
Usage: run-reboot.sh --openvmm PATH --kernel PATH --initrd PATH --output-dir PATH [options]

Verifies that a guest-initiated reboot starts the same initramfs again in the
same OpenVMM process. The result is written to reboot.json.

Options:
  --processors N      L2 processor count (default: 4)
  --memory-gib N      L2 memory GiB (default: 4)
  --timeout SECONDS   Reboot observation timeout (default: 60)
  --openvmm-arg ARG   Additional OpenVMM argument (repeatable)

Environment:
  TDP_TMUX may be "sudo tmux" inside the L1.
  VIRT_TDP_WAKE_SIGNAL is forced to 0.
USAGE
}
openvmm= kernel= initrd= output_dir=
processors=4 memory_gib=4 timeout=60
openvmm_args=()
while (($#)); do
    case "$1" in
        --openvmm) openvmm=$2; shift 2 ;;
        --kernel) kernel=$2; shift 2 ;;
        --initrd) initrd=$2; shift 2 ;;
        --output-dir) output_dir=$2; shift 2 ;;
        --processors) processors=$2; shift 2 ;;
        --memory-gib) memory_gib=$2; shift 2 ;;
        --timeout) timeout=$2; shift 2 ;;
        --openvmm-arg) openvmm_args+=("$2"); shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done
for value in openvmm kernel initrd output_dir; do
    [[ -n ${!value} ]] || { echo "--${value//_/-} is required" >&2; exit 2; }
done
for value in processors memory_gib timeout; do
    [[ ${!value} =~ ^[1-9][0-9]*$ ]] || { echo "$value must be positive" >&2; exit 2; }
done
openvmm=$(realpath "$openvmm"); kernel=$(realpath "$kernel"); initrd=$(realpath "$initrd")
mkdir -p "$output_dir"; output_dir=$(realpath "$output_dir")
serial="$output_dir/reboot.serial.log"; vmm_log="$output_dir/reboot.vmm.log"
: >"$serial"; : >"$vmm_log"
read -r -a tmux_cmd <<<"${TDP_TMUX:-tmux}"
session="virt-tdp-reboot-$$"
active=1
cleanup() {
    ((active == 0)) || "${tmux_cmd[@]}" kill-session -t "$session" 2>/dev/null || true
}
trap cleanup EXIT
argv=(env VIRT_TDP_WAKE_SIGNAL=0 "$openvmm" --hypervisor "tdp:memory=$memory_gib"
    -m "${memory_gib}G" -p "$processors" -k "$kernel" -r "$initrd"
    -c 'console=ttyS0 earlyprintk=serial,ttyS0,115200 nokaslr rdinit=/init'
    --com1 "file=$serial" "${openvmm_args[@]}")
printf -v quoted '%q ' "${argv[@]}"
"${tmux_cmd[@]}" new-session -d -s "$session" "exec $quoted>$vmm_log 2>&1"
deadline=$((SECONDS + timeout)); outcome=timeout
while ((SECONDS < deadline)); do
    begins=$(grep -c 'VIRT_TDP_LIFECYCLE BEGIN action=reboot' "$serial" 2>/dev/null || true)
    if ((begins >= 2)); then outcome=rebooted; break; fi
    if grep -q 'VIRT_TDP_LIFECYCLE RETURNED action=reboot' "$serial" 2>/dev/null; then
        outcome=reboot_returned; break
    fi
    if ! "${tmux_cmd[@]}" has-session -t "$session" 2>/dev/null; then
        outcome=vmm_stopped; active=0; break
    fi
    if grep -q 'fatal error\|reset failed' "$vmm_log" 2>/dev/null; then
        outcome=vmm_error; break
    fi
    sleep 1
done
begins=$(grep -c 'VIRT_TDP_LIFECYCLE BEGIN action=reboot' "$serial" 2>/dev/null || true)
if ((active)); then
    for _ in {1..20}; do
        "${tmux_cmd[@]}" send-keys -t "$session" quit Enter 2>/dev/null || true
        "${tmux_cmd[@]}" has-session -t "$session" 2>/dev/null || { active=0; break; }
        sleep 0.25
    done
fi
python3 - "$output_dir/reboot.json" "$outcome" "$begins" "$((active == 0))" <<'PY'
import json, sys
path, outcome, begins, clean_stop = sys.argv[1:]
result = {
    "schema_version": 1,
    "case": "lifecycle.guest_reboot",
    "outcome": outcome,
    "boot_markers": int(begins),
    "clean_stop": bool(int(clean_stop)),
    "passed": outcome == "rebooted" and bool(int(clean_stop)),
}
with open(path, "w") as stream:
    json.dump(result, stream, indent=2, sort_keys=True)
    stream.write("\n")
print(json.dumps(result, sort_keys=True))
raise SystemExit(not result["passed"])
PY
