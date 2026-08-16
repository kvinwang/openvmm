#!/bin/busybox sh
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
set -eu
id=${1:?case ID is required}
case "$id" in
    smp.atomic.add.1k) exec "$PROBE" atomic "$EXPECTED_CPUS" 1000 ;;
    smp.atomic.add.100k) exec "$PROBE" atomic "$EXPECTED_CPUS" 100000 ;;
    *) exec "$PROBE" semantic "$id" ;;
esac
