# virt_tdp Conformance Suite

This is an extensible, test262-style workload for the OpenVMM `virt_tdp`
backend. Each case has a stable identifier and one manifest record, executes
in its own process with a case-specific timeout, and emits exactly one
machine-readable result. One failed case does not prevent later cases from
running.

## Result protocol

The guest writes records to the serial console through `/dev/kmsg`:

```text
VIRT_TDP_CTS BEGIN version=1 expected_cpus=8
VIRT_TDP_CTS RESULT id=cpu.enumeration status=PASS duration_ms=0 reason=none
VIRT_TDP_CTS END total=13 pass=13 fail=0 skip=0
```

`tools/parse-results.py` rejects missing, duplicate, unknown, malformed, or
summary-inconsistent records. `tools/run.sh` also requires every OpenVMM
instance to stop cleanly and writes a JSON result for each boot plus an
aggregate lifecycle summary.

Run the parser's positive and negative self-test with:

```bash
conformance/tools/test-parser.sh
```

## Coverage

- CPU enumeration and affinity on every vCPU;
- ordered cross-CPU scheduler wakeups against every AP;
- fixed-vector reschedule IPIs, NMI all-excluding-self shorthand, and
  INIT/SIPI hotplug;
- monotonic time and timer wakeup;
- anonymous-page zeroing, copy-on-write isolation, and SMP SIMD state;
- repeated virtio block completion and readback; and
- virtio network enumeration, link enablement, and repeated TX completion.

Lifecycle coverage comes from repeated fresh boot, complete case execution,
and clean OpenVMM stop on the same L1.

## Adding a case

1. Add one tab-separated record to `manifest.tsv`.
2. Add `cases/<id>.sh`; exit 0 for pass, 77 for skip, or another value for
   failure.
3. Use only BusyBox applets or `/opt/virt-tdp-cts/cts-probe` unless the
   generator is updated to package another static helper.
4. Keep the case independent and restore any global state with a trap.

The manifest columns are ID, timeout seconds, minimum CPU count, required
device (`none`, `block`, or `network`), and description.

## Build and run

Build from a known-booting BusyBox initramfs:

```bash
conformance/tools/make-initramfs.sh \
  base.cpio.gz virt-tdp-cts-8.cpio.gz 8
```

Inside the L1, run two complete cycles with both MMIO devices:

```bash
VIRT_TDP_WAKE_SIGNAL=0 TDP_TMUX="sudo tmux" \
  conformance/tools/run.sh \
  --openvmm ./openvmm --kernel ./vmlinux \
  --initrd ./virt-tdp-cts-8.cpio.gz --processors 8 \
  --repetitions 2 --output-dir ./cts-results \
  --openvmm-arg --virtio-blk-mmio --openvmm-arg mem:64M \
  --openvmm-arg --virtio-net --openvmm-arg none \
  --openvmm-arg --virtio-net-bus --openvmm-arg mmio
```
