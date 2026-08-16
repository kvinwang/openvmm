// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! What an L2 is told about the processor it is running on.
//!
//! Passing the host's answers through unchanged is wrong in a specific and
//! fatal way: the guest believes it can use features the TDX module does not
//! give an L2, acts on that belief, and dies somewhere unrelated. Five-level
//! paging is the clearest case — Linux sets `CR4.LA57` and rebuilds its page
//! tables, the L2 stays four-level, and the guest triple faults inside the
//! decompressor with nothing on the console to say why.

/// Answer CPUID for an L2.
pub fn for_l2(leaf: u32, subleaf: u32, vp_index: u32, vp_count: u32) -> [u32; 4] {
    assert!(vp_count != 0);
    assert!(vp_index < vp_count);
    let r = std::arch::x86_64::__cpuid_count(leaf, subleaf);
    let (mut eax, mut ebx, mut ecx, mut edx) = (r.eax, r.ebx, r.ecx, r.edx);

    match (leaf, subleaf) {
        (1, _) => {
            // One logical processor with APIC ID zero. Leaking the L1 thread's
            // physical APIC ID makes Linux mark a whole range of nonexistent
            // APIC IDs present and can exhaust the IOAPIC ID namespace.
            ebx = (ebx & 0x0000_ffff) | (vp_count.min(0xff) << 16) | ((vp_index & 0xff) << 24);
            ecx &= !(1 << 5); // VMX: an L2 cannot nest further
            ecx &= !(1 << 6); // SMX
            // x2APIC is left advertised, and it is not optional. The TDX
            // module virtualizes an L2's APIC through MSRs — the secondary
            // control for x2APIC virtualization is on and the one for APIC
            // access virtualization is off — so a guest in xAPIC mode sends
            // every APIC access to memory-mapped addresses nothing is
            // watching. It then configures an APIC that does not exist and
            // reports the symptom as a timer that never ticks.
            ecx &= !(1 << 24); // TSC deadline timer
            // There is no PMU state virtualization across TDG.VP.ENTER. Do
            // not let the guest program the L1's counters or arm a perf NMI
            // watchdog against state it does not own.
            ecx &= !(1 << 15); // PDCM
            // A minimal KVM-compatible hypervisor signature tells Linux that
            // x2APIC without interrupt remapping is supported. No KVM feature
            // bits are exposed; this is only the standard guest-environment
            // contract needed to keep the APIC mode TD Partitioning supports.
            ecx |= 1 << 31;
            // Machine-check. The guest would read the bank registers, and
            // there is no answer for them that is not a lie; Linux panics with
            // "MCA architectural violation" rather than tolerate an
            // inconsistent one.
            edx &= !(1 << 7);
            edx &= !(1 << 14);
        }
        (4, _) => eax = (eax & !(0x3f << 26)) | ((vp_count.min(64) - 1) << 26),
        (0xb, 0) | (0x1f, 0) => return [0, 1, 1 << 8, vp_index],
        (0xb, 1) | (0x1f, 1) => {
            let package_shift = u32::BITS - (vp_count - 1).leading_zeros();
            return [package_shift, vp_count, (2 << 8) | 1, vp_index];
        }
        (0xb, _) | (0x1f, _) => return [0; 4],
        (0x8000_0008, _) => {
            let package_shift = u32::BITS - (vp_count - 1).leading_zeros();
            ecx = (ecx & !0xffff) | (vp_count - 1).min(0xff) | (package_shift << 12);
        }
        (7, 0) => {
            ecx &= !(1 << 16); // LA57: the L2's paging level is not ours to change
        }
        // Architectural performance monitoring is not virtualized. Passing
        // the L1 leaf through makes Linux enable its hard-lockup watchdog;
        // counter-overflow NMIs then arrive without matching virtual PMU
        // state and are reported as "NMI received for unknown reason".
        (0xa, _) | (0x23, _) => return [0; 4],
        // The L1 is a TD, so this leaf reports TDX. An L2 is not a TD — it has
        // no TDCS, cannot attest, and every TDCALL it makes exits to the VMM —
        // and reporting otherwise sends Linux down its confidential-computing
        // path, which is what putting the workload in an L2 is meant to avoid.
        (0x21, _) => return [0; 4],
        (0x4000_0000, _) => {
            return [0x4000_0001, 0x4b4d_564b, 0x564b_4d56, 0x0000_004d];
        }
        (0x4000_0001, _) => return [0; 4],
        (0x4000_0002..=0x4000_ffff, _) => return [0; 4],
        _ => {}
    }
    [eax, ebx, ecx, edx]
}

#[cfg(test)]
mod tests {
    use super::for_l2;

    #[test]
    fn reports_per_vp_x2apic_topology() {
        let leaf1 = for_l2(1, 0, 3, 4);
        assert_eq!(leaf1[1] >> 16, 0x0304);

        assert_eq!(for_l2(0xb, 0, 3, 4), [0, 1, 1 << 8, 3]);
        assert_eq!(for_l2(0xb, 1, 3, 4), [2, 4, (2 << 8) | 1, 3]);
        assert_eq!(for_l2(0xb, 2, 3, 4), [0; 4]);
    }

    #[test]
    fn hides_unvirtualized_performance_monitoring() {
        assert_eq!(for_l2(0xa, 0, 0, 1), [0; 4]);
        assert_eq!(for_l2(0x23, 0, 0, 1), [0; 4]);
        assert_eq!(for_l2(1, 0, 0, 1)[2] & (1 << 15), 0);
    }
}
