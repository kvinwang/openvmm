#!/bin/busybox sh
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
set -eu
id=${1:?case ID is required}
case "$id" in
    lifecycle.init.pid1) [ "$($BB readlink /proc/1/exe)" = /bin/busybox ] ;;
    lifecycle.mount.procfs) $BB grep -q '^proc /proc proc ' /proc/mounts ;;
    lifecycle.mount.sysfs) $BB grep -q '^sysfs /sys sysfs ' /proc/mounts ;;
    lifecycle.mount.devtmpfs) $BB grep -q '^devtmpfs /dev devtmpfs ' /proc/mounts ;;
    *) echo "unknown lifecycle case: $id"; exit 2 ;;
esac
