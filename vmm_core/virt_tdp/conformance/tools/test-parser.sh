#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d -t virt-tdp-cts-parser.XXXXXX)
trap 'rm -rf "$work"' EXIT
serial="$work/serial.log"
"$root/tools/validate-manifest.py" "$root/manifest.tsv" "$root/cases"

echo 'VIRT_TDP_CTS BEGIN version=1 expected_cpus=8' >"$serial"
count=0
while IFS=$'\t' read -r id _; do
    case "$id" in ''|'#'*) continue ;; esac
    echo "VIRT_TDP_CTS RESULT id=$id status=PASS duration_ms=1 reason=none" >>"$serial"
    count=$((count + 1))
done <"$root/manifest.tsv"
echo "VIRT_TDP_CTS END total=$count pass=$count fail=0 skip=0" >>"$serial"
"$root/tools/parse-results.py" "$serial" "$root/manifest.tsv" \
    --output "$work/result.json"
python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["passed"]' \
    "$work/result.json"

grep -v 'id=cpu.affinity ' "$serial" >"$work/missing.log"
if "$root/tools/parse-results.py" "$work/missing.log" "$root/manifest.tsv" \
        >/dev/null 2>&1; then
    echo "parser accepted a result with a missing case" >&2
    exit 1
fi

cp "$serial" "$work/duplicate.log"
grep 'id=cpu.affinity ' "$serial" >>"$work/duplicate.log"
if "$root/tools/parse-results.py" "$work/duplicate.log" "$root/manifest.tsv" \
        >/dev/null 2>&1; then
    echo "parser accepted a duplicate case" >&2
    exit 1
fi

sed '0,/status=PASS/s//status=UNKNOWN/' "$serial" >"$work/status.log"
if "$root/tools/parse-results.py" "$work/status.log" "$root/manifest.tsv" \
        >/dev/null 2>&1; then
    echo "parser accepted an unknown status" >&2
    exit 1
fi

cat "$serial" "$serial" >"$work/two-epochs.log"
"$root/tools/extract-epoch.py" "$work/two-epochs.log" 2 "$work/epoch-2.log"
cmp "$serial" "$work/epoch-2.log"
if "$root/tools/extract-epoch.py" "$work/two-epochs.log" 3 "$work/missing-epoch.log" \
        >/dev/null 2>&1; then
    echo "extractor accepted an absent epoch" >&2
    exit 1
fi

echo "parser self-test passed"
