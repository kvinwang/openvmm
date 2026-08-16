# Hardware results, 2026-08-16

Both sustained runs used a real Ubuntu 24.04 Server L2, Docker 28.2.2, a
dynamically allocated 12 GiB VHDX, 4 GiB RAM, and
`VIRT_TDP_WAKE_SIGNAL=0`. Every case passed and OpenVMM stopped cleanly.
The generated, unedited gates are `hardware-4vcpu-60s.json` and
`hardware-8vcpu-60s.json`; `hardware-observations.json` records warnings and
host limitations that are not represented by the serial result protocol.

## Selected measurements

| Measurement | 4 vCPUs | 8 vCPUs | Observation |
|---|---:|---:|---|
| CPU probe | 928.4 M iterations/s | 1,559.6 M iterations/s | 1.68x scaling |
| Memory probe | 21,275 MiB/s | 20,345 MiB/s | Bandwidth saturated at 4 vCPUs |
| Process churn | 2,796 processes/s | 623 processes/s | Severe negative SMP scaling |
| Sequential write | 172.3 MB/s | 151.7 MB/s | VHDX + virtio-blk + Docker `vfs` path |
| PostgreSQL pgbench | 331 TPS | 422 TPS | 0 failed transactions in both runs |
| PostgreSQL average latency | 24.1 ms | 37.7 ms | Client count scales as 2x vCPU count |
| Raw virtio-net transmit | 6,066 packets/s | 6,506 packets/s | 8.7–9.3 MiB/s (about 73–78 Mbit/s) |
| Mixed phase elapsed | 60.9 s | 104.1 s | 8-vCPU concurrency regressed despite more CPU |

PostgreSQL survived a container stop/remove/recreate cycle with one million
accounts intact. Redis survived the same lifecycle using AOF. The filesystem
case wrote, synced, reread, and SHA-256-verified 512 MiB. The mixed case ran
pgbench, Redis concurrency, CPU load, and a 1 GiB memory workload together.

## Bottlenecks and warnings

The CPU-only probe scales, but process churn, Redis, and the mixed phase become
substantially slower at 8 vCPUs. This points to a contention or wakeup problem
in the current SMP/APIC/TD-entry path rather than a lack of raw execution
capacity. The 8-vCPU guest also logged one `NMI received for unknown reason 21`
on CPU 6 during the mixed phase. Linux continued, persistence checks passed,
and the VM stopped cleanly, but the NMI source requires follow-up before this
path can be considered production-stable.

The host had only four 1 GiB hugepages; a 6 GiB L2 allocation therefore failed
and all evidence uses 4 GiB. OpenVMM also does not currently replay the VHDX
journal. Even after a clean OpenVMM stop, this image needed
`qemu-img check -r all` before its next launch.
