# Ubuntu Docker stress workload

This workload boots a persistent Ubuntu Server L2 on the `virt_tdp` backend,
runs real Docker containers, and emits a serial protocol that the host converts
to JSON. It covers CPU/SMP, memory bandwidth, process churn, persistent and
sequential filesystem I/O, PostgreSQL transactions and restart persistence,
Redis concurrency and AOF persistence, raw virtio-net transmit, and a concurrent
database/CPU/memory phase.

## L1 requirements

In addition to the main 1 GiB hugepage allocation, reserve 512 KiB through
1 MiB from the L1 at boot and give that exact range to `dstack_tdcall`:

```text
memmap=512K$512K
reserved_gpa_base=0x80000 reserved_gpa_size=0x80000
```

The complete upper legacy window must be backed by same-GPA aliases. Ubuntu
and glibc use vector loads while scanning DMI/ROM addresses; XMM state is not
available in the TD Partitioning L2-exit ABI and therefore those reads cannot
be correctly replayed by an instruction emulator. The driver must accept the
range through `0x100000` (older revisions capped it at `0xa0000`).

L2 RAM is allocated at 4 KiB granularity. The allocator uses 1 GiB hugetlb
pages for the main body, 2 MiB hugetlb pages for the remainder, and
driver-owned base pages only for a final remainder smaller than 2 MiB. Reserve
both hugepage sizes on the L1; boot-time reservation keeps 2 MiB pages clustered
and reduces the number of sparse RAM extents advertised to the guest. The x86
zero-page e820 table has 128 entries, so OpenVMM rejects more than 96 L2 RAM
extents rather than silently hiding memory from Linux.

The hardware host used for validation has four 1 GiB hugepages. Its 2 MiB pool
was configured with 512 pages for mixed-size validation. VP wakeup uses the
driver capability-negotiated kernel-IPI path; POSIX signals are not delivered
across `TDG.VP.ENTER`.

The backend does not expose architectural PMU capabilities because PMU state
is not virtualized across L2 entry. It also traps external NMIs by default:
NMIs explicitly requested through the APIC model receive a one-entry bypass,
while an NMI that escapes TD Partitioning's NMI-exiting control is contained in
L1 and logged instead of appearing in Linux as an unexplained tenant NMI.

## Build the dynamic disk

The image builder needs `qemu-img`, `growpart`, `losetup`, `e2fsck`, a static C
compiler, Docker, and root access for the loop mount. Docker invocations use the
required `sudo su kvin -c "docker ..."` form.

```bash
vmm_core/virt_tdp/stress/tools/build-image.sh \
  noble-server-cloudimg-amd64.img ubuntu-docker.vhdx
```

The result is a 12 GiB dynamically allocated VHDX (about 2.1 GiB initially).
OpenVMM does not natively consume QCOW2. The guest uses Docker 28.2.2 with the
`vfs` storage driver and preloads `postgres:15-alpine`, `redis:7-alpine`, and
the statically linked workload probe.

Direct kernel boot intentionally passes `fstab=no`: the cloud image's EFI
partition needs an NLS module that is absent from the standalone test kernel,
and neither `/boot` nor `/boot/efi` is required by the workload.

## Run

Hardware runs must be launched in a detached tmux session. On the L1:

```bash
tmux new-session -d -s virt-tdp-ubuntu-stress \
  'TDP_TMUX="sudo tmux" vmm_core/virt_tdp/stress/tools/run.sh \
    --openvmm /path/to/openvmm --kernel /path/to/vmlinux \
    --disk /path/to/ubuntu-docker.vhdx \
    --output-dir /path/to/results --processors 8 --memory-gib 4 \
    --duration 60 --timeout 1800 > /path/to/runner.log 2>&1'
```

Use `--memory-mib` for an exact MiB size or `--memory-kib` for a 4 KiB-aligned
size. For example, the following requests exactly 3 GiB + 2 MiB + 12 KiB and
exercises every backing tier:

```bash
--memory-kib 3147788
```

`summary.json` is the machine-readable gate. A pass requires every case marker
and a clean OpenVMM process stop. `serial.log` and `openvmm.log` retain primary
evidence. Writable VHDX opens replay a validated pending journal automatically.
A clean OpenVMM stop drains disk I/O, closes the VHDX journal, and clears its
log GUID; an abrupt process or host failure deliberately leaves the journal for
replay.
