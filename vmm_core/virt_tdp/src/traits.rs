// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The `virt` trait implementations that make this a backend OpenVMM can use.
//!
//! Most of it is the shape every backend has. Two places are specific to
//! running an L2 and worth reading:
//!
//! [`PartitionMemoryMap::map_range`] does not map anything. Under TD
//! partitioning a guest's physical address *is* the L1's, so a mapping is not
//! a placement — it is an assertion that the pages behind this virtual address
//! already live at this guest physical address, followed by aliasing them into
//! the L2's Secure EPT. Anything else is rejected, loudly, because a silently
//! misplaced range would surface as the guest reading someone else's memory.
//!
//! [`X86Partition::ioapic_routing`] is where OpenVMM's IOAPIC hands over. It
//! produces an MSI, and delivering it means writing the virtual-APIC page and
//! raising RVI rather than using the VM-entry interruption field, because the
//! TDX module forces APIC virtualization on for an L2.

use crate::partition::TdpError;
use crate::partition::TdpMemory;
use crate::partition::TdpPartition;
use crate::partition::TdpPartitionInner;
use crate::vp::TdpProcessor;
use anyhow::Context as _;
use hvdef::Vtl;
use std::sync::Arc;
use virt::PartitionCapabilities;
use virt::PartitionMemoryMap;
use virt::VpIndex;
use virt::irqcon::IoApicRouting;
use virt::irqcon::MsiRequest;
use virt::state::StateError;

/// Interrupts raised by devices, waiting for the run loop to deliver them.
///
/// The IOAPIC signals on a shared reference and delivery needs the L2's VMCS,
/// so requests queue here and the run loop drains them before its next entry.
pub struct PendingInterrupts {
    routes: parking_lot::Mutex<[Option<MsiRequest>; 24]>,
    /// Targeted vectors retained for the diagnostic hand-written APIC path.
    vectors: parking_lot::Mutex<Vec<(VpIndex, u8)>>,
    vp_wakes: Vec<VpWake>,
    apics: Arc<virt_support_apic::LocalApicSet>,
    device: Arc<crate::TdcallDevice>,
}

#[derive(Default)]
struct VpWake {
    pending: std::sync::atomic::AtomicBool,
    /// Whether that thread is inside an entry right now. The signal is only
    /// worth sending then: outside the entry the processor is about to drain
    /// the queue anyway, and a signal per device interrupt starves the entry
    /// into never running at all.
    in_entry: std::sync::atomic::AtomicBool,
    /// Whether this entry has already been signalled. One is enough — the
    /// processor drains every queued vector before it enters again — and
    /// sending more is actively harmful: under a storm each signal ends the
    /// entry a little sooner, until the guest executes nothing per entry and
    /// so never consumes the interrupts that keep the storm going.
    kicked: std::sync::atomic::AtomicBool,
    /// Woken when a vector is queued, so a processor waiting out a halt does
    /// not have to poll to notice.
    halt_waker: parking_lot::Mutex<Option<std::task::Waker>>,
}

impl PendingInterrupts {
    pub fn new(
        vp_count: u32,
        apics: Arc<virt_support_apic::LocalApicSet>,
        device: Arc<crate::TdcallDevice>,
    ) -> Self {
        Self {
            routes: parking_lot::Mutex::new([None; 24]),
            vectors: parking_lot::Mutex::new(Vec::new()),
            vp_wakes: (0..vp_count).map(|_| VpWake::default()).collect(),
            apics,
            device,
        }
    }

    fn vp_wake(&self, vp_index: VpIndex) -> &VpWake {
        &self.vp_wakes[vp_index.index() as usize]
    }

    /// Wake a VP for APIC work such as INIT, SIPI, or an IPI.
    pub fn wake_vp(&self, vp_index: VpIndex) {
        let vp_wake = self.vp_wake(vp_index);
        vp_wake
            .pending
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(waker) = vp_wake.halt_waker.lock().take() {
            waker.wake();
        }
        self.kick(vp_index);
    }

    /// Arrange for `waker` to be woken by the next interrupt.
    pub fn wake_on_interrupt(&self, vp_index: VpIndex, waker: &std::task::Waker) {
        *self.vp_wake(vp_index).halt_waker.lock() = Some(waker.clone());
    }

    /// Whether anything is waiting to be delivered.
    pub fn is_empty(&self, vp_index: VpIndex) -> bool {
        !self
            .vp_wake(vp_index)
            .pending
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Take the vectors raised since the last call.
    pub fn drain(&self, vp_index: VpIndex) -> Vec<u8> {
        self.vp_wake(vp_index)
            .pending
            .store(false, std::sync::atomic::Ordering::Release);
        let mut vectors = self.vectors.lock();
        let mut mine = Vec::new();
        vectors.retain(|&(target, vector)| {
            if target == vp_index {
                mine.push(vector);
                false
            } else {
                true
            }
        });
        mine
    }

    pub fn request(&self, request: MsiRequest) {
        // The vector comes out of the data word through its own accessor, not
        // by taking the low byte: the layout is a bitfield and reading it by
        // hand yields zero, which is a vector the guest has no handler for and
        // never notices.
        let vector = x86defs::msi::MsiData::from(request.data).vector();
        tracing::debug!(
            vector,
            data = request.data,
            address = request.address,
            "an interrupt was raised"
        );
        self.apics
            .request_interrupt(request.address, request.data, |vp_index| {
                self.vectors.lock().push((vp_index, vector));
                self.wake_vp(vp_index);
            });
    }

    /// Tell the processor its next entry — or the one it is blocked in right
    /// now — has an interrupt waiting. Without this, an entry that put the
    /// guest to sleep stays asleep until the host happens to interrupt it for
    /// its own reasons, which on an idle host is never.
    fn kick(&self, vp_index: VpIndex) {
        use std::sync::atomic::Ordering;
        let vp_wake = self.vp_wake(vp_index);
        if !vp_wake.in_entry.load(Ordering::SeqCst) {
            // The processor is between entries; it drains the queue before
            // the next one.
            return;
        }
        if vp_wake.kicked.swap(true, Ordering::SeqCst) {
            // Already signalled out of this entry.
            return;
        }
        if let Err(err) = self.device.kick_vp(vp_index.index()) {
            tracing::error!(vp = vp_index.index(), %err, "failed to kick an L2 VP");
        }
    }

    /// Interrupt a running entry so OpenVMM can stop or inspect the VP.
    ///
    /// Unlike the device-interrupt path, this is not optional. The generic
    /// VP controller waits for `run_vp` to observe its stop flag, and a halted
    /// L2 can otherwise remain inside `TDG.VP.ENTER` forever, preventing both
    /// process shutdown and the alias-revocation teardown from running.
    pub fn request_yield(&self, vp_index: VpIndex) {
        use std::sync::atomic::Ordering;

        let vp_wake = self.vp_wake(vp_index);
        if !vp_wake.in_entry.load(Ordering::SeqCst) {
            return;
        }
        if let Err(err) = self.device.kick_vp(vp_index.index()) {
            tracing::error!(vp = vp_index.index(), %err, "failed to stop an L2 VP");
        }
    }

    /// Register the calling thread as the one to signal out of an entry.
    pub fn register_waker_thread(&self, vp_index: VpIndex) {
        let _ = vp_index;
    }

    /// Mark the start of an entry, so an interrupt raised during it signals
    /// the thread out rather than being slept through.
    ///
    /// Deliberately does not refuse the entry when vectors are already
    /// pending. They were drained and published into the virtual-APIC page
    /// before this, and entering is what *delivers* them — refusing instead
    /// livelocks the moment interrupts arrive faster than one loop takes,
    /// which a timer alone manages: the guest never runs, so it never
    /// consumes them, so they never stop arriving.
    pub fn entry_begin(&self, vp_index: VpIndex) {
        use std::sync::atomic::Ordering;
        let vp_wake = self.vp_wake(vp_index);
        vp_wake.kicked.store(false, Ordering::SeqCst);
        vp_wake.in_entry.store(true, Ordering::SeqCst);
    }

    /// Mark the end of an entry.
    pub fn entry_end(&self, vp_index: VpIndex) {
        self.vp_wake(vp_index)
            .in_entry
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl IoApicRouting for PendingInterrupts {
    fn set_irq_route(&self, irq: u8, request: Option<MsiRequest>) {
        tracing::debug!(irq, ?request, "the guest routed an interrupt line");
        if let Some(slot) = self.routes.lock().get_mut(usize::from(irq)) {
            *slot = request;
        }
    }

    fn assert_irq(&self, irq: u8) {
        let route = self.routes.lock().get(usize::from(irq)).copied().flatten();
        tracing::debug!(irq, routed = route.is_some(), "a device asserted its line");
        let Some(request) = route else {
            // The guest has not programmed this entry. That is the guest's
            // business, not an error.
            return;
        };
        self.request(request);
    }
}

/// Memory, which the L1 does not get to place.
pub struct TdpMemoryMap {
    pub(crate) memory: Arc<TdpMemory>,
    pub(crate) partition: Arc<TdpPartitionInner>,
}

impl PartitionMemoryMap for TdpMemoryMap {
    unsafe fn map_range(
        &self,
        _data: *mut u8,
        size: usize,
        addr: u64,
        _writable: bool,
        _exec: bool,
    ) -> anyhow::Result<()> {
        // The separately boot-reserved low window is published just before
        // first entry, after the loader has populated this backing.
        if addr + size as u64 <= crate::lowmem::LOW_MEMORY_END {
            tracing::debug!(addr, size, "deferred low-memory handling");
            return Ok(());
        }
        anyhow::ensure!(
            self.memory.contains_range(addr..addr + size as u64),
            "an L2's guest physical address is the L1's, so memory cannot be placed at \
             {addr:#x}..{:#x}; this partition has no matching physical extent",
            addr + size as u64
        );
        // The driver pinned and validated the complete file-backed mapping
        // before reporting this GPA, so no userspace PFN lookup is needed.

        let vm = crate::tdx::L2Vm::new(&self.partition.device, self.partition.vm_id);
        let mapped = self.memory.map(&vm, addr..addr + size as u64)?;
        tracing::debug!(addr, size, mapped, "aliased guest memory into the L2");
        Ok(())
    }

    fn unmap_range(&self, addr: u64, size: u64) -> anyhow::Result<()> {
        // The VMM clears the whole address space before it maps anything, and
        // most of that space is not this L2's memory, so there is nothing to
        // remove. Only a range that really was aliased would need the page
        // attributes cleared, and getting that wrong leaves the guest reading
        // a page it should no longer have — so it fails rather than pretends.
        let end = addr.checked_add(size).context("L2 unmap range overflow")?;
        if !self.memory.any_aliased(addr..end) {
            return Ok(());
        }
        let vm = crate::tdx::L2Vm::new(&self.partition.device, self.partition.vm_id);
        let unmapped = self.memory.unmap(&vm, addr..end)?;
        tracing::debug!(addr, size, unmapped, "revoked guest memory from the L2");
        Ok(())
    }
}

impl virt::PartitionMemoryMapper for TdpPartition {
    fn memory_mapper(&self, vtl: Vtl) -> Arc<dyn PartitionMemoryMap> {
        assert_eq!(vtl, Vtl::Vtl0, "an L2 has no VTLs");
        Arc::new(TdpMemoryMap {
            memory: self.inner.memory.clone(),
            partition: self.inner.clone(),
        })
    }
}

impl virt::Partition for TdpPartition {
    fn supports_reset(&self) -> Option<&dyn virt::ResetPartition<Error = TdpError>> {
        // Resetting an L2 means rebuilding its VMCS from scratch, which is
        // straightforward and untested, so it is not offered.
        None
    }

    fn caps(&self) -> &PartitionCapabilities {
        &self.inner.caps
    }

    fn request_yield(&self, vp_index: VpIndex) {
        self.inner.interrupts.request_yield(vp_index);
    }

    fn request_msi(&self, vtl: Vtl, request: MsiRequest) {
        assert_eq!(vtl, Vtl::Vtl0, "an L2 has no VTLs");
        self.inner.interrupts.request(request);
    }
}

impl virt::X86Partition for TdpPartition {
    fn ioapic_routing(&self) -> Arc<dyn IoApicRouting> {
        self.inner.interrupts.clone()
    }

    fn pulse_lint(&self, _vp_index: VpIndex, _vtl: Vtl, lint: u8) {
        // LINT0 as ExtINT is how a guest without a MADT expects its interrupts,
        // and forced APIC virtualization cannot express that. A guest here is
        // given a MADT precisely so it never asks.
        tracing::warn!(lint, "an L2 cannot take a local interrupt pin");
    }
}

impl virt::PartitionAccessState for TdpPartition {
    type StateAccess<'a> = &'a TdpPartition;

    fn access_state(&self, vtl: Vtl) -> Self::StateAccess<'_> {
        assert_eq!(vtl, Vtl::Vtl0, "an L2 has no VTLs");
        self
    }
}

impl virt::Hv1 for TdpPartition {
    type Error = TdpError;
    type Device = virt::x86::apic_software_device::ApicSoftwareDevice;

    fn reference_time_source(&self) -> Option<vmcore::reference_time::ReferenceTimeSource> {
        // The Hyper-V reference time is an enlightenment for guests that know
        // they are virtualized. An L2 here is told it is bare metal.
        None
    }

    fn new_virtual_device(
        &self,
    ) -> Option<&dyn virt::DeviceBuilder<Device = Self::Device, Error = Self::Error>> {
        None
    }

    fn synic(&self) -> anyhow::Result<Arc<dyn vmcore::synic::SynicPortAccess>> {
        anyhow::bail!("an L2 has no synthetic interrupt controller")
    }
}

/// Errors that can only come from state access.
impl From<StateError<TdpError>> for TdpError {
    fn from(err: StateError<TdpError>) -> Self {
        TdpError::State(Box::new(err))
    }
}

/// Binds a processor to the thread that will run it.
///
/// An L2 vCPU lives in the TDVPS of the L1 vCPU that enters it, so binding is
/// not a formality: the thread that programs the VMCS has to be the thread
/// that enters, and it must not migrate afterwards.
pub struct TdpProcessorBinder {
    pub(crate) partition: Arc<TdpPartitionInner>,
    pub(crate) vp_index: VpIndex,
    pub(crate) vp_info: vm_topology::processor::x86::X86VpInfo,
    pub(crate) memory: Arc<TdpMemory>,
    pub(crate) guest_memory: guestmem::GuestMemory,
    pub(crate) apic: Option<virt_support_apic::LocalApic>,
    pub(crate) vmtime: vmcore::vmtime::VmTimeAccess,
    pub(crate) processor: Option<TdpProcessor>,
}

impl virt::BindProcessor for TdpProcessorBinder {
    type Processor<'a> = &'a mut TdpProcessor;
    type Error = TdpError;

    fn bind(&mut self) -> Result<Self::Processor<'_>, Self::Error> {
        crate::tdx::pin_to_cpu(self.vp_index.index() as usize)?;
        let processor = match &mut self.processor {
            Some(processor) => processor,
            slot => slot.insert(TdpProcessor::new(
                self.partition.clone(),
                self.vp_index,
                self.vp_info,
                self.memory.clone(),
                self.guest_memory.clone(),
                self.apic.take(),
                std::mem::replace(&mut self.vmtime, self.partition.vmtime_access()),
            )?),
        };
        Ok(processor)
    }
}
