// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Partition and processor types for the TDX L2 backend.

use crate::TdcallDevice;
use crate::hugemem::HugeRegion;
use crate::tdx::L2Vm;
use inspect::Inspect;
use parking_lot::Mutex;
use std::sync::Arc;
use thiserror::Error;
use virt::PartitionCapabilities;

#[derive(Error, Debug)]
pub enum TdpError {
    #[error("this TD was created without L2 slots, so it is not an L1 VMM")]
    NotAnL1Vmm,
    #[error("the TDCALL device is unavailable; is the dstack_tdcall module loaded?")]
    NoDevice(#[source] anyhow::Error),
    #[error("the layout puts guest memory at {:#x}..{:#x}, but the L2's physical extents are {available:?}, and aliasing cannot move a page", requested.start, requested.end)]
    MemoryLayout {
        requested: std::ops::Range<u64>,
        available: Vec<std::ops::Range<u64>>,
    },
    #[error("could not determine partition capabilities: {0}")]
    Capabilities(String),
    #[error("{0} is not supported for an L2")]
    Unsupported(&'static str),
    #[error(transparent)]
    Tdx(#[from] anyhow::Error),
    #[error(transparent)]
    State(#[from] Box<virt::state::StateError<TdpError>>),
}

/// The backend itself.
///
/// One of these per process; it owns the handle to the TDCALL device and
/// answers what the platform can do before a partition exists.
pub struct Tdp {
    device: Arc<TdcallDevice>,
    /// The memory a partition will be built around.
    ///
    /// It is assigned before the partition exists because its guest physical
    /// address decides the memory layout, not the other way round.
    memory: Option<Arc<TdpMemory>>,
    /// How many L2 VMs the TD was created with. Zero means the TD is not an L1
    /// VMM and nothing here will work.
    l2_slots: u64,
}

impl Tdp {
    pub fn new() -> Result<Self, TdpError> {
        let device = Arc::new(TdcallDevice::open().map_err(TdpError::NoDevice)?);
        Self::from_device(device)
    }

    pub fn from_device(device: Arc<TdcallDevice>) -> Result<Self, TdpError> {
        let l2_slots = L2Vm::num_l2_vms(&device)?;
        if l2_slots == 0 {
            return Err(TdpError::NotAnL1Vmm);
        }
        device.claim_vm(crate::hypervisor::FIRST_L2_SLOT)?;
        Ok(Self {
            device,
            memory: None,
            l2_slots,
        })
    }

    /// Whether this process can run L2 VMs at all.
    pub fn is_available() -> bool {
        Self::new().is_ok()
    }

    pub fn l2_slots(&self) -> u64 {
        self.l2_slots
    }

    pub fn device(&self) -> &Arc<TdcallDevice> {
        &self.device
    }

    /// Assign the memory a partition will use. Its address is what the memory
    /// layout has to be built from.
    pub fn set_memory(&mut self, memory: Arc<TdpMemory>) {
        self.memory = Some(memory);
    }

    pub(crate) fn take_memory(&mut self) -> Option<Arc<TdpMemory>> {
        self.memory.take()
    }
}

/// The memory an L2 will use, reserved before any VM exists.
///
/// It has to come first, and it has to be process-wide. The guest's physical
/// addresses are the pages' own, so the memory layout is built from this
/// allocation's address — which means the address has to be known before the
/// VM is configured, not after.
static RESERVED: std::sync::OnceLock<Arc<TdpMemory>> = std::sync::OnceLock::new();

/// Reserve the memory an L2 will run in, and report where it landed.
///
/// Returns the guest physical address the memory layout must be built around,
/// and the file backing it, which a VMM that maps guest memory itself needs.
pub fn reserve_guest_memory(
    size: usize,
) -> anyhow::Result<(Vec<std::ops::Range<u64>>, std::fs::File)> {
    let memory = match RESERVED.get() {
        Some(memory) => {
            anyhow::ensure!(
                memory.region().len() == size,
                "L2 memory was already reserved as {:#x} bytes, not the requested {size:#x}",
                memory.region().len()
            );
            memory.clone()
        }
        None => {
            let memory = Arc::new(TdpMemory::new(HugeRegion::alloc(size)?));
            let _ = RESERVED.set(memory.clone());
            RESERVED.get().expect("just set").clone()
        }
    };
    let file = memory.region().device().try_clone_file()?;
    Ok((memory.gpa_ranges(), file))
}

/// The memory reserved by [`reserve_guest_memory`], if any.
pub fn reserved_guest_memory() -> Option<Arc<TdpMemory>> {
    RESERVED.get().cloned()
}

/// Guest memory, which under TD partitioning the L1 does not get to place.
///
/// Aliasing publishes the L1's own guest physical address into the L2's Secure
/// EPT, so the guest's memory map is dictated by where the L1's pages happen
/// to be. The layout has to be built around this rather than the other way
/// round.
pub struct TdpMemory {
    region: HugeRegion,
    /// Pages already aliased, so re-mapping the same range is cheap.
    aliased: Mutex<Vec<bool>>,
}

struct LowMemoryState {
    memory: crate::driver::TdMemory,
    published_pages: usize,
}

/// Executable conventional memory reserved from the L1 at boot.
///
/// Unlike the main hugetlb region, this has a fixed GPA: Linux's AP reset
/// trampoline has to execute below 1 MiB and TD Partitioning aliases only at
/// the L1 page's original GPA. The restricted driver owns the reservation;
/// this type copies in the loader's initial contents once and publishes every
/// page before any VP can enter.
pub(crate) struct TdpLowMemory {
    state: Mutex<LowMemoryState>,
}

impl TdpLowMemory {
    pub(crate) fn new(device: &TdcallDevice) -> anyhow::Result<Self> {
        let memory = device.alloc_reserved()?;
        anyhow::ensure!(
            memory.gpa() == crate::lowmem::LOW_MEMORY_BASE
                && memory.len() == crate::lowmem::LOW_MEMORY_SIZE,
            "the driver reserved low memory at {:#x}..{:#x}, but OpenVMM requires {:#x}..{:#x}",
            memory.gpa(),
            memory.gpa() + memory.len() as u64,
            crate::lowmem::LOW_MEMORY_BASE,
            crate::lowmem::LOW_MEMORY_RESERVED_END,
        );
        Ok(Self {
            state: Mutex::new(LowMemoryState {
                memory,
                published_pages: 0,
            }),
        })
    }

    pub(crate) fn prepare(
        &self,
        vm: &L2Vm<'_>,
        source: &guestmem::GuestMemory,
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        let total_pages = state.memory.len() / 4096;
        if state.published_pages == total_pages {
            return Ok(());
        }
        if state.published_pages == 0 {
            source
                .read_at(crate::lowmem::LOW_MEMORY_BASE, state.memory.as_mut_slice())
                .map_err(|err| anyhow::anyhow!("reading the L2 low-memory image: {err}"))?;
        }
        while state.published_pages < total_pages {
            let gpa = crate::lowmem::LOW_MEMORY_BASE + (state.published_pages * 4096) as u64;
            vm.add_page_alias(gpa, crate::tdx::gpa_attr::RWX)?;
            state.published_pages += 1;
        }
        Ok(())
    }

    fn unmap_all(&self, vm: &L2Vm<'_>) -> anyhow::Result<usize> {
        let mut state = self.state.lock();
        if state.published_pages == 0 {
            return Ok(0);
        }
        let mut unmapped = 0;
        while state.published_pages != 0 {
            let page = state.published_pages - 1;
            let gpa = crate::lowmem::LOW_MEMORY_BASE + (page * 4096) as u64;
            vm.drop_page_alias(gpa)?;
            state.published_pages = page;
            unmapped += 1;
        }
        Ok(unmapped)
    }
}

impl TdpMemory {
    pub fn new(region: HugeRegion) -> Self {
        let pages = region.len() / 4096;
        Self {
            region,
            aliased: Mutex::new(vec![false; pages]),
        }
    }

    /// The guest physical range this memory will appear at, which is not a
    /// choice.
    pub fn gpa_ranges(&self) -> Vec<std::ops::Range<u64>> {
        self.region
            .segments()
            .iter()
            .map(|segment| segment.gpa..segment.gpa + segment.len as u64)
            .collect()
    }

    pub fn region(&self) -> &HugeRegion {
        &self.region
    }

    pub fn region_mut(&mut self) -> &mut HugeRegion {
        &mut self.region
    }

    fn page_index(&self, gpa: u64) -> Option<usize> {
        let mut page_base = 0;
        for segment in self.region.segments() {
            let end = segment.gpa + segment.len as u64;
            if gpa >= segment.gpa && gpa < end {
                return Some(page_base + ((gpa - segment.gpa) / 4096) as usize);
            }
            page_base += segment.len / 4096;
        }
        None
    }

    pub fn backing_offset(&self, gpa: u64, len: usize) -> Option<usize> {
        let end = gpa.checked_add(len as u64)?;
        let mut offset = 0;
        for segment in self.region.segments() {
            let segment_end = segment.gpa + segment.len as u64;
            if gpa >= segment.gpa && end <= segment_end {
                return Some(offset + (gpa - segment.gpa) as usize);
            }
            offset += segment.len;
        }
        None
    }

    pub fn contains_range(&self, range: std::ops::Range<u64>) -> bool {
        range
            .end
            .checked_sub(range.start)
            .and_then(|len| self.backing_offset(range.start, len as usize))
            .is_some()
    }

    /// Whether any page in the range has been aliased into the L2.
    pub fn any_aliased(&self, range: std::ops::Range<u64>) -> bool {
        let aliased = self.aliased.lock();
        let mut page_base = 0;
        for segment in self.region.segments() {
            let segment_end = segment.gpa + segment.len as u64;
            let start = range.start.max(segment.gpa);
            let end = range.end.min(segment_end);
            if start < end {
                let first = page_base + ((start - segment.gpa) / 4096) as usize;
                let last = page_base + (end - segment.gpa).div_ceil(4096) as usize;
                if aliased[first..last].iter().any(|&value| value) {
                    return true;
                }
            }
            page_base += segment.len / 4096;
        }
        false
    }

    /// Alias a range into the L2, skipping pages already done.
    pub fn map(&self, vm: &L2Vm<'_>, range: std::ops::Range<u64>) -> anyhow::Result<usize> {
        anyhow::ensure!(
            range.start % 4096 == 0 && range.end % 4096 == 0,
            "L2 alias creation must be page aligned"
        );
        anyhow::ensure!(
            self.contains_range(range.clone()),
            "L2 alias range is outside a physical extent"
        );
        let mut aliased = self.aliased.lock();
        let mut mapped = 0;
        for gpa in range.step_by(4096) {
            let Some(index) = self.page_index(gpa) else {
                anyhow::bail!("gpa {gpa:#x} is outside this partition's memory");
            };
            let Some(done) = aliased.get_mut(index) else {
                anyhow::bail!("gpa {gpa:#x} is outside this partition's memory");
            };
            if *done {
                continue;
            }
            vm.add_page_alias(gpa, crate::tdx::gpa_attr::RWX)?;
            *done = true;
            mapped += 1;
        }
        Ok(mapped)
    }

    /// Revoke every alias in a range, skipping pages that were never granted.
    pub fn unmap(&self, vm: &L2Vm<'_>, range: std::ops::Range<u64>) -> anyhow::Result<usize> {
        anyhow::ensure!(
            range.start % 4096 == 0 && range.end % 4096 == 0,
            "L2 alias revocation must be page aligned: {:#x}..{:#x}",
            range.start,
            range.end
        );
        let mut aliased = self.aliased.lock();
        let mut unmapped = 0;
        let mut page_base = 0;
        for segment in self.region.segments() {
            let segment_end = segment.gpa + segment.len as u64;
            let start = range.start.max(segment.gpa);
            let end = range.end.min(segment_end);
            for gpa in (start..end).step_by(4096) {
                let index = page_base + ((gpa - segment.gpa) / 4096) as usize;
                if !aliased[index] {
                    continue;
                }
                vm.drop_page_alias(gpa)?;
                aliased[index] = false;
                unmapped += 1;
            }
            page_base += segment.len / 4096;
        }
        Ok(unmapped)
    }

    pub fn unmap_all(&self, vm: &L2Vm<'_>) -> anyhow::Result<usize> {
        self.gpa_ranges()
            .into_iter()
            .try_fold(0, |total, range| Ok(total + self.unmap(vm, range)?))
    }
}

/// What a partition owns, shared with its processors.
///
/// Separate from [`TdpPartition`] because the processors outlive the value the
/// hypervisor hands back, which is the shape every backend here uses.
#[derive(Inspect)]
pub struct TdpPartitionInner {
    #[inspect(skip)]
    pub(crate) device: Arc<TdcallDevice>,
    #[inspect(skip)]
    pub(crate) memory: Arc<TdpMemory>,
    #[inspect(skip)]
    pub(crate) low_memory: Arc<TdpLowMemory>,
    #[inspect(skip)]
    pub(crate) caps: PartitionCapabilities,
    /// The L2 slot this partition occupies. Slot 0 is the L1 itself.
    pub(crate) vm_id: u8,
    /// Number of processors in the topology exposed to the L2.
    pub(crate) vp_count: u32,
    /// Interrupts raised by devices, drained by the run loop.
    #[inspect(skip)]
    pub(crate) interrupts: Arc<crate::traits::PendingInterrupts>,
    /// The local APICs, from OpenVMM's model rather than another one written
    /// here. An L2's APIC accesses reach the VMM — the TDX module does not
    /// virtualize them — so the VMM needs a real APIC to hand them to, and
    /// the one thing that stays specific to an L2 is how the result is
    /// published: into the virtual-APIC page, not into an interrupt
    /// controller of the VMM's own.
    #[inspect(skip)]
    pub(crate) apics: Arc<virt_support_apic::LocalApicSet>,
    #[inspect(skip)]
    pub(crate) vmtime: vmcore::vmtime::VmTimeSource,
}

/// A partition: one L2 VM.
#[derive(Inspect)]
#[inspect(transparent)]
pub struct TdpPartition {
    pub(crate) inner: Arc<TdpPartitionInner>,
}

impl TdpPartitionInner {
    /// A time accessor, for a processor being bound.
    pub(crate) fn vmtime_access(&self) -> vmcore::vmtime::VmTimeAccess {
        self.vmtime.access("l2-apic")
    }
}

impl Drop for TdpPartitionInner {
    fn drop(&mut self) {
        let vm = L2Vm::new(&self.device, self.vm_id);
        match self.memory.unmap_all(&vm) {
            Ok(0) => {}
            Ok(pages) => tracing::info!(pages, vm_id = self.vm_id, "revoked L2 memory aliases"),
            Err(error) => tracing::error!(
                error = %error,
                vm_id = self.vm_id,
                "failed to revoke every L2 memory alias"
            ),
        }
        match self.low_memory.unmap_all(&vm) {
            Ok(0) => {}
            Ok(pages) => tracing::info!(pages, vm_id = self.vm_id, "revoked L2 low-memory aliases"),
            Err(error) => tracing::error!(
                error = %error,
                vm_id = self.vm_id,
                "failed to revoke every L2 low-memory alias"
            ),
        }
    }
}
