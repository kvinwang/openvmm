# TD Partitioning on General-Purpose Linux

`virt_tdp` adapts OpenVMM to run an Intel TDX L2 from a normal Linux process
inside an L1 TD. It reuses OpenVMM's loader, chipset, APIC, emulator, and
device models, but does not depend on the OpenHCL host contract.

## Boundary

The intended stack is:

```text
Linux L1 TD
├── restricted TD Partitioning kernel device
└── OpenVMM
    ├── standard OpenVMM platform and devices
    └── virt_tdp
        └── TDX L2
```

The backend owns TD Partitioning state translation. The kernel device must
eventually own privileged calls, page ownership, CPU binding, and fail-safe
cleanup. Hyper-V host messages, VTL management, SynIC, and the OpenHCL image
runtime are deliberately outside this migration.

This branch contains only the OpenVMM backend. It does not contain or invoke
the earlier standalone `tdp_vmm` prototype, a QEMU L1 VMM, or a KVM TD
Partitioning backend. At runtime OpenVMM opens `/dev/dstack_tdcall` and drives
the TDX module through the restricted Linux device; it does not open
`/dev/kvm`.

## Current baseline

The exploration currently provides:

- `TDG.VP.ENTER` and return classification;
- TDVPS and L2 VMCS state access;
- CPUID, MSR, control-register, exception, PIO, and MMIO handling;
- physical-address-constrained guest memory and Linux direct boot;
- OpenVMM local APIC integration and interrupt wakeup;
- x2APIC register virtualization, with the APIC model authoritative and the
  virtual-APIC page used as the hardware offload image;
- virtio-mmio block and network plumbing;
- validated single-, two-, four-, and eight-vCPU Linux boots in the tested
  environment;
- fixed-vector, shorthand/broadcast NMI, and level-triggered EOI delivery; and
- clean VP interruption, partition teardown, and consecutive L2 boots on one
  L1 without rebooting it.

The Linux device now restricts operations to an exclusively claimed L2 slot,
pins and validates every page that may be aliased, records successful aliases,
and revokes them on descriptor teardown. Main RAM remains physically
contiguous hugetlb memory, but its GPA is learned and validated by the driver;
`/proc/self/pagemap` is not used. Per-L1-CPU state allows L2 VPs to enter
concurrently, and INIT/SIPI, fixed-vector IPI routing, NMI injection, and APIC
shorthand/broadcast target selection are implemented. The driver uses Linux's
guest-FPU API for a full XSAVE-aware fpstate switch on every L2 entry instead
of bypassing the kernel's fpstate ownership with FXSAVE/FXRSTOR.

## Migration gates

1. **Alias lifecycle:** record every successful grant, revoke it on unmap,
   partial failure, process teardown, and partition teardown, and invalidate
   translations when access is narrowed.
2. **Restricted Linux UAPI:** replace arbitrary TDCALL execution with VM,
   vCPU, memory, entry, and teardown operations checked by the kernel.
3. **Memory ownership:** allocate and pin L2 pages in the driver, return their
   actual L1 GPA ranges to OpenVMM, and remove `/proc/self/pagemap` from the
   trusted path.
4. **Multiple vCPUs:** bind each L2 VP to its issuing L1 TDVPS and implement
   INIT/SIPI, IPI, cross-VP wakeup, and teardown synchronization.
5. **Interrupt correctness:** keep OpenVMM's local APIC authoritative and use
   the virtual-APIC page only as the hardware offload image.
6. **Validation:** repeatedly boot, stop, and relaunch an L2; then qualify
   CPU, time, block, network, interrupt, and failure-recovery behavior.

Gates 1 through 6 have hardware evidence through the tested eight-vCPU Linux
direct-boot configuration. The restricted-UAPI negative test
proves that unknown leaves, invalid or duplicate VM claims, arbitrary GPAs,
and a hugetlb query against ordinary memory are rejected.

The single-vCPU portions of gates 5 and 6 also have hardware evidence. Six
consecutive clean boot/stop cycles passed CPU enumeration, monotonic time,
virtio block readback, and virtio network link-up. A device-loaded L2 was also
killed with `SIGKILL`, after which four complete qualification cycles passed
on the same L1 without reloading the driver or rebooting it. Guest RAM is
explicitly zeroed before any alias can be published, which is required for
isolation between L2 launches.

Device interrupt kicks do not signal a thread out of `TDG.VP.ENTER` by
default. Doing so without an explicit kernel/TDX contract intermittently
corrupts L2 execution; the hardware symptom was a guest TLS fault several
seconds after the signal rather than a synchronous failure at the call site.
The host's normal interrupts still return control often enough to publish
queued device interrupts. Controller-requested VP shutdown remains a separate,
mandatory kick so teardown cannot wait forever on a halted guest.

Multiple-vCPU startup now uses kernel-owned same-GPA low memory rather than a
Linux-specific instruction interpreter. The L1 reserves `0x80000..0xa0000`
with its boot `memmap` setting and loads the restricted driver with
`reserved_gpa_base=0x80000 reserved_gpa_size=0x20000`. The driver refuses a
range that overlaps System RAM, gives the complete range to only one device
descriptor, records every L2 alias, revokes them on close, and quarantines the
range with the VM slot if revocation fails. OpenVMM copies the loader's initial
low-memory image into the range before first VP entry and publishes it RWX.
The AP can consequently execute Linux's real-mode trampoline in hardware;
WBINVD exits are retired as coherent-cache no-ops.

Driver-owned mappings retain their allocations after the device descriptor is
closed and pin the module until the last VMA is gone. This prevents a stale
mapping from becoming a use-after-free view of pages returned to the kernel;
the negative test writes and reads a mapping after descriptor close, and a
separate module-lifetime test verifies that unload fails while the VMA lives.

On hardware, two consecutive clean launches each brought up two CPUs and
passed CPU topology, time, virtio block, and virtio network qualification. A
256-round ping-pong between tasks pinned to CPU 0 and CPU 1 also passed on
both launches, exercising cross-CPU scheduler wakeups under the same device
load. The CPU 1 reschedule-interrupt counter increased during each ping-pong,
validating fixed-vector IPI delivery. CPU 1 was also offlined and brought
online again on both launches, validating repeated INIT/SIPI state
transitions. A two-vCPU device-loaded L2 was then killed with `SIGKILL`; after
the hugetlb
pages had returned to the pool, two more complete qualification cycles passed
without reloading the driver or rebooting the L1. The current evidence proves
boot, fixed-vector IPI delivery, cross-CPU scheduler wake, CPU hotplug, and
ordinary device-loaded lifecycle; it does not claim every APIC delivery mode
or arbitrary topologies.

Four- and eight-vCPU device-loaded qualification now exercises every AP with
a 256-round CPU0-to-AP FIFO ping-pong and checks that AP's reschedule-IPI
counter. It then uses Linux SysRq `l`, whose x2APIC ICR request is shorthand
"all excluding self", and verifies the aggregate NMI count increased for all
other online CPUs. Two consecutive launches at each CPU count passed CPU
enumeration, all-AP scheduler wakeups, fixed-vector reschedule IPIs, NMI
shorthand/broadcast, CPU hotplug, time, virtio block readback, and virtio
network link-up.

The virtual-APIC offload publishes the APIC model's TMR as the VMX EOI-exit
bitmap. A hardware-virtualized EOI for a level-triggered vector consequently
returns to OpenVMM, which notifies the IOAPIC to clear remote IRR and permit a
still-asserted line to be delivered again. This is required for multi-vCPU
virtio-mmio block completion when Linux routes the interrupt to an AP.

Live snapshot/restore and in-place partition reset are not supported. The
backend models register, activity, and local-APIC state needed by INIT/SIPI,
but unsupported VP state elements fail snapshot reads rather than returning
plausible reset values. Initial-state writes remain accepted before first
entry so Linux direct boot can construct a fresh VP.

## Reproducible smoke test

Build OpenVMM with the TDP backend:

```bash
cargo +1.95.0 build -p openvmm --features virt_tdp
```

Reserve the low executable window in the L1 boot memory map (the evaluation
image uses a larger reservation beginning at the same address), then load the
restricted driver with the exact window OpenVMM uses:

```bash
sudo insmod dstack_tdcall.ko \
  reserved_gpa_base=0x80000 reserved_gpa_size=0x20000
```

After copying the binary, kernel, and initramfs into the L1 and loading the TD
Partitioning driver, run the lifecycle smoke test inside the L1:

```bash
TDP_TMUX="sudo tmux" vmm_core/virt_tdp/tools/l2-smoke.sh \
  --openvmm ./openvmm \
  --kernel ./vmlinux \
  --initrd ./initramfs.cpio.gz \
  --output-dir ./tdp-smoke-logs
```

The harness deliberately uses detached `tmux` sessions. Piping OpenVMM through
`script` or a logging pipeline changes its job-control state and can stop the
process, producing a false guest hang. Success requires two userspace markers
and two clean OpenVMM exits without rebooting the L1 between attempts.

Build the stronger qualification initramfs from the known-booting one:

```bash
vmm_core/virt_tdp/tools/make-qualification-initramfs.sh \
  ./initramfs.cpio.gz ./qualification-8cpu.cpio.gz 8
```

Then attach MMIO virtio devices and require the final qualification marker:

```bash
TDP_TMUX="sudo tmux" vmm_core/virt_tdp/tools/l2-smoke.sh \
  --openvmm ./openvmm --kernel ./vmlinux \
  --initrd ./qualification-8cpu.cpio.gz \
  --marker 'L2 QUALIFICATION PASSED' --processors 8 \
  --openvmm-arg --virtio-blk-mmio --openvmm-arg mem:64M \
  --openvmm-arg --virtio-net --openvmm-arg none \
  --openvmm-arg --virtio-net-bus --openvmm-arg mmio
```

With an expected CPU count greater than one, the initramfs emits separate
`CPU`, `CROSS-CPU WAKE`, `RESCHEDULE IPI`, `NMI BROADCAST`, `CPU HOTPLUG`,
`TIME`, `BLOCK`, and `NETWORK` pass markers so the first missing capability is
unambiguous.

For systematic regression rather than a single qualification path, use
`conformance/`. Its manifest gives every case a stable ID, timeout, minimum
CPU count, device requirement, and description. Cases execute in isolated
processes and emit a versioned `VIRT_TDP_CTS` serial protocol. The host runner
rejects incomplete or inconsistent result sets, requires repeated clean
OpenVMM shutdowns, and writes per-boot and aggregate JSON. See
`conformance/README.md` for the coverage matrix and reproduction commands.

## Product integration boundary

The OpenVMM feature wiring is complete: `virt_tdp` is enabled by the OpenVMM
crate's default feature set and can also be selected explicitly. The Linux
driver remains a separately built artifact because it must match the L1
kernel ABI; embedding its source in OpenVMM's Rust build would not make that
relationship reproducible.

A product image must pin compatible OpenVMM, driver, and L1-kernel revisions,
build the out-of-tree module against that kernel, reserve the low-memory
window on the kernel command line, load the module with the documented
parameters, provision hugetlb pages, and restrict device-node access to the
OpenVMM service. The driver repository's `guest/tdcall/README.md` is the
packaging contract for those Linux-side pieces.

This migration does not include live snapshot/restore, in-place partition
reset, a debugger interface, arbitrary CPU topologies, or the APIC delivery
modes beyond the fixed, INIT, SIPI, and NMI modes covered above. These are
explicit backend limitations rather than hidden dependencies on OpenHCL.
