#!/bin/busybox sh
set -eu
exec "$PROBE" fpu "$EXPECTED_CPUS"
