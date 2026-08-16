#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

if (($# != 3)); then
    echo "usage: $0 BASE_INITRAMFS OUTPUT_INITRAMFS EXPECTED_CPUS" >&2
    exit 2
fi

base=$(realpath "$1")
output=$2
expected_cpus=$3
[[ $expected_cpus =~ ^[1-9][0-9]*$ ]] || {
    echo "EXPECTED_CPUS must be a positive integer" >&2
    exit 2
}

root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d -t virt-tdp-cts.XXXXXX)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/opt/virt-tdp-cts/cases"
cp "$root/manifest.tsv" "$work/opt/virt-tdp-cts/"
cp "$root/guest/run-suite.sh" "$work/opt/virt-tdp-cts/"
cp "$root/cases/"*.sh "$work/opt/virt-tdp-cts/cases/"

cc=${CC:-cc}
"$cc" -O2 -static -pthread -msse2 -Wall -Wextra -Werror \
    "$root/guest/cts-probe.c" -o "$work/opt/virt-tdp-cts/cts-probe"

cat >"$work/init" <<INIT
#!/bin/busybox sh
set -u
bb=/bin/busybox
\$bb mkdir -p /proc /sys /dev /tmp
\$bb mount -t proc proc /proc || \$bb poweroff -f
\$bb mount -t sysfs sysfs /sys || \$bb poweroff -f
\$bb mount -t devtmpfs devtmpfs /dev || \$bb poweroff -f
EXPECTED_CPUS=$expected_cpus /bin/busybox sh /opt/virt-tdp-cts/run-suite.sh
rc=\$?
# The host harness owns normal shutdown. This fallback makes a standalone run
# terminate, while leaving enough time for its one-second serial poll.
\$bb sleep 30
\$bb poweroff -f
exit \$rc
INIT
chmod +x "$work/init" "$work/opt/virt-tdp-cts/run-suite.sh" \
    "$work/opt/virt-tdp-cts/cases/"*.sh

overlay="$work/overlay.cpio"
(cd "$work" && find init opt -print0 | cpio --null -o -H newc --quiet >"$overlay")
{
    gzip -dc "$base"
    cat "$overlay"
} | gzip -n -9 >"$output"
