#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail
if (($# != 2)); then
    echo "usage: $0 BASE_INITRAMFS OUTPUT_INITRAMFS" >&2
    exit 2
fi
base=$(realpath "$1")
output=$2
work=$(mktemp -d -t virt-tdp-reboot.XXXXXX)
cleanup() { python3 - "$work" <<'PY'
import shutil, sys
shutil.rmtree(sys.argv[1])
PY
}
trap cleanup EXIT
cat >"$work/init" <<'INIT'
#!/bin/busybox sh
set -u
bb=/bin/busybox
$bb mkdir -p /proc /sys /dev
$bb mount -t proc proc /proc || $bb poweroff -f
$bb mount -t sysfs sysfs /sys || $bb poweroff -f
$bb mount -t devtmpfs devtmpfs /dev || $bb poweroff -f
echo '<3>VIRT_TDP_LIFECYCLE BEGIN action=reboot' >/dev/kmsg
$bb sync
$bb reboot -f
echo '<3>VIRT_TDP_LIFECYCLE RETURNED action=reboot' >/dev/kmsg
$bb sleep 30
$bb poweroff -f
INIT
chmod +x "$work/init"
(cd "$work" && printf 'init\0' | cpio --null -o -H newc --quiet >overlay.cpio)
{
    gzip -dc "$base"
    cat "$work/overlay.cpio"
} | gzip -n -9 >"$output"
