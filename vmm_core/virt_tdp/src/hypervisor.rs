// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Creating a partition.
//!
//! The unusual part is that the memory comes first. Every other backend is
//! told where to put a guest's RAM; this one is told where the RAM already is,
//! because aliasing publishes the L1's own guest physical address and there is
//! no second translation. So the caller allocates the region, reads its
//! address, and builds the memory layout around it — and this checks that the
//! layout it is then handed agrees.

use crate::partition::Tdp;
use crate::partition::TdpError;
use crate::partition::TdpMemory;
use crate::partition::TdpPartition;
use crate::partition::TdpPartitionInner;
use crate::traits::PendingInterrupts;
use crate::traits::TdpProcessorBinder;
use std::sync::Arc;
use virt::PartitionConfig;
use virt::ProtoPartition;
use virt::ProtoPartitionConfig;
use virt::VpIndex;

/// The L2 slot to use. Slot 0 is the L1 itself, so an L2 starts at 1.
pub(crate) const FIRST_L2_SLOT: u8 = 1;

pub struct TdpProtoPartition<'a> {
    tdp: &'a Tdp,
    config: ProtoPartitionConfig<'a>,
    memory: Arc<TdpMemory>,
}

impl virt::Hypervisor for Tdp {
    type ProtoPartition<'a> = TdpProtoPartition<'a>;
    type Partition = TdpPartition;
    type Error = TdpError;

    fn platform_info(&self) -> virt::PlatformInfo {
        virt::PlatformInfo {}
    }

    fn new_partition<'a>(
        &'a mut self,
        config: ProtoPartitionConfig<'a>,
    ) -> Result<Self::ProtoPartition<'a>, Self::Error> {
        // The memory was reserved before the VM was configured, because its
        // address is what the layout was built from.
        let memory = self
            .take_memory()
            .or_else(crate::partition::reserved_guest_memory)
            .ok_or(TdpError::Unsupported("a partition without reserved memory"))?;

        Ok(TdpProtoPartition {
            tdp: self,
            config,
            memory,
        })
    }
}

impl ProtoPartition for TdpProtoPartition<'_> {
    type Partition = TdpPartition;
    type ProcessorBinder = TdpProcessorBinder;
    type Error = TdpError;

    fn max_physical_address_size(&self) -> u8 {
        // The L2 inherits the TD's guest physical address width; anything
        // wider than the L1 can address is unreachable by construction.
        let r = std::arch::x86_64::__cpuid(0x8000_0008);
        (r.eax & 0xff) as u8
    }

    fn build(
        self,
        config: PartitionConfig<'_>,
    ) -> Result<(Self::Partition, Vec<Self::ProcessorBinder>), Self::Error> {
        // The layout is not this backend's to choose, but it is its business
        // to refuse one that does not match where the memory actually is.
        let ranges = self.memory.gpa_ranges();
        for ram in config.mem_layout.ram() {
            let start = ram.range.start();
            let end = ram.range.end();
            // Below 1 MiB is the hole the guest cannot own and the VMM fills
            // in by emulation; it is deliberately not this partition's memory.
            if end <= crate::lowmem::LOW_MEMORY_END {
                continue;
            }
            if !ranges
                .iter()
                .any(|range| start >= range.start && end <= range.end)
            {
                return Err(TdpError::MemoryLayout {
                    requested: start..end,
                    available: ranges,
                });
            }
        }

        let caps = virt::PartitionCapabilities::from_cpuid(
            self.config.processor_topology,
            &mut |eax, ecx| {
                // SAFETY: CPUID has no side effects.
                crate::cpuid::for_l2(eax, ecx, 0, self.config.processor_topology.vp_count())
            },
        )
        .map_err(|err| TdpError::Capabilities(format!("{err:?}")))?;

        let apics = Arc::new(
            virt_support_apic::LocalApicSet::builder()
                // TD Partitioning exposes the L2 local APIC through x2APIC MSRs;
                // the model handles the registers that hardware does not offload.
                .x2apic_capable(true)
                .hyperv_enlightenments(false)
                .build(),
        );

        let interrupts = Arc::new(PendingInterrupts::new(
            self.config.processor_topology.vp_count(),
            apics.clone(),
            self.tdp.device().clone(),
        ));

        let low_memory = Arc::new(crate::partition::TdpLowMemory::new(self.tdp.device())?);
        let inner = Arc::new(TdpPartitionInner {
            device: self.tdp.device().clone(),
            memory: self.memory.clone(),
            low_memory,
            caps,
            vm_id: FIRST_L2_SLOT,
            vp_count: self.config.processor_topology.vp_count(),
            interrupts,
            apics,
            vmtime: self.config.vmtime.clone(),
        });

        let binders = self
            .config
            .processor_topology
            .vps_arch()
            .map(|vp_info| TdpProcessorBinder {
                apic: Some({
                    let mut apic = inner.apics.add_apic(&vp_info, false);
                    // Hardware only ever looks at the virtual-APIC page, so
                    // the model runs offloaded from the start: interrupts it
                    // accepts are pushed into the page on scan, and accesses
                    // that need its state pull the page's contents back first.
                    // Without this the model keeps everything to itself and
                    // nothing it decides ever reaches the guest.
                    apic.enable_offload();
                    apic
                }),
                vmtime: self.config.vmtime.access("l2-apic"),
                partition: inner.clone(),
                vp_index: VpIndex::new(vp_info.base.vp_index.index()),
                vp_info,
                memory: self.memory.clone(),
                guest_memory: config.guest_memory.clone(),
                processor: None,
            })
            .collect();

        Ok((TdpPartition { inner }, binders))
    }
}
