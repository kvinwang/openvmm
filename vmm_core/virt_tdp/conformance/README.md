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
VIRT_TDP_CTS END total=127 pass=127 fail=0 skip=0
```

`tools/parse-results.py` rejects missing, duplicate, unknown, malformed, or
summary-inconsistent records. `tools/run.sh` also exercises optional in-place
partition resets, requires every OpenVMM instance to stop cleanly, and writes
a JSON result for each boot plus an aggregate lifecycle summary.

Run the parser's positive and negative self-test with:

```bash
conformance/tools/test-parser.sh
```

## Coverage

The current guest corpus contains 127 independently reported cases, plus the
disruptive `lifecycle.guest_reboot` host-orchestrated case. Parameterized
implementations share runner code, but every manifest ID receives its own
process, timeout, prerequisites, and result record.

- CPUID policy, architectural instructions, and independently reported
  invalid-opcode, breakpoint, divide-error, page-fault, and NX exceptions;
- CPU enumeration, online state, affinity, and SIMD state on every vCPU;
- ordered CPU0-to-AP exchanges and atomic coherency under short and sustained
  contention;
- per-AP reschedule IPIs, NMI all-excluding-self delivery, and INIT/SIPI
  hotplug;
- monotonic, raw, boottime, and realtime clocks, multiple timer intervals,
  clock resolution, and TSC progress;
- multiple anonymous-page sizes, zeroing, copy-on-write, protection faults,
  NX, unaligned data, and repeated map/TLB teardown;
- virtio block capacity, geometry, request sizes, offsets, repetition, and
  exact readback; and
- virtio network enumeration, MTU/MAC properties, frame sizes, batch sizes,
  and TX completion.

Lifecycle coverage comes from repeated fresh OpenVMM starts, optional in-place
`reset` commands followed by another complete corpus execution, and clean
OpenVMM stops on the same L1.

`virt_tdp` does not currently implement partition reset, so `--resets 1` is
also a deliberate capability test: it fails with the backend's explicit
`reset not supported` error rather than silently substituting a fresh process.
Use `--resets 0` to qualify the currently supported fresh-start/clean-stop
lifecycle independently.

Guest-initiated reboot is a distinct path. Build its purpose-specific
initramfs and require a second boot marker in the same OpenVMM process with:

```bash
conformance/tools/make-reboot-initramfs.sh \
  base.cpio.gz virt-tdp-reboot.cpio.gz
VIRT_TDP_WAKE_SIGNAL=0 TDP_TMUX="sudo tmux" \
  conformance/tools/run-reboot.sh \
  --openvmm ./openvmm --kernel ./vmlinux \
  --initrd ./virt-tdp-reboot.cpio.gz --output-dir ./reboot-results
```

The runner writes `reboot.json` and fails unless the guest reaches init twice
and OpenVMM then stops cleanly. This catches reset requests that return, stop
the VM, or become stuck in legacy reset-port emulation.

## Adding a case

1. Add one tab-separated record to `manifest.tsv`.
2. Select a runner script. A runner may serve one case or dispatch multiple related
   IDs; every manifest entry still executes in a separate process. Exit 0 for pass,
   77 for skip, or another value for failure.
3. Use only BusyBox applets or `/opt/virt-tdp-cts/cts-probe` unless the
   generator is updated to package another static helper.
4. Keep the case independent and restore any global state with a trap.

The manifest columns are ID, timeout seconds, minimum CPU count, required
device (`none`, `block`, or `network`), runner name, and description. Run
`tools/validate-manifest.py` after changing the corpus.

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
  --repetitions 2 --resets 1 --output-dir ./cts-results \
  --openvmm-arg --virtio-blk-mmio --openvmm-arg mem:64M \
  --openvmm-arg --virtio-net --openvmm-arg none \
  --openvmm-arg --virtio-net-bus --openvmm-arg mmio
```
