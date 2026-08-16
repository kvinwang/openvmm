// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Running an L2 VM as a `virt::Processor`.
//!
//! The run loop is the part of this backend that is genuinely different from
//! the others, and three of its rules were expensive to learn:
//!
//! **Not every `TDG.VP.ENTER` return is an exit.** A host-routed return means
//! the host saw the exit first and resumed the L1 — which is how the patched
//! KVM hands back an L2 MMIO fault it cannot service, saying so in its own
//! source. The exit information is then in the L2 VMCS rather than in the
//! entry's return registers. A pending-interrupt return means no entry
//! happened at all. Advancing RIP on either skips a guest instruction, and the
//! guest fails much later somewhere else.
//!
//! **The guest's APIC reads never exit.** The TDX module forces APIC
//! virtualization on for an L2, so hardware serves reads from the virtual-APIC
//! page. Anything the guest should see has to be written into that page before
//! entry, and interrupts are delivered by setting IRR and raising RVI rather
//! than through the VM-entry interruption field.
//!
//! **State with no VMCS field is swapped by the kernel driver**, not here: the
//! swap has to be atomic with respect to preemption, and this is user space.

use crate::partition::TdpMemory;
use crate::partition::TdpPartitionInner;
use crate::tdx::L2GprContext;
use crate::tdx::L2Vm;
use crate::tdx::VpEnterResult;
use crate::tdx::Width;
use crate::vmcs;
use inspect::InspectMut;
use std::convert::Infallible;
use std::sync::Arc;
use virt::StopVp;
use virt::VpHaltReason;
use virt::VpIndex;
use virt::io::CpuIo;
use x86defs::SegmentRegister;
use x86emu::Gp;
use x86emu::Segment;

/// VMX basic exit reasons this backend acts on.
mod exit {
    pub const EXCEPTION_NMI: u16 = 0;
    pub const EXTERNAL_INTERRUPT: u16 = 1;
    pub const TRIPLE_FAULT: u16 = 2;
    pub const INTERRUPT_WINDOW: u16 = 7;
    pub const CPUID: u16 = 10;
    pub const HLT: u16 = 12;
    pub const CR_ACCESS: u16 = 28;
    pub const IO_INSTRUCTION: u16 = 30;
    pub const MSR_READ: u16 = 31;
    pub const MSR_WRITE: u16 = 32;
    pub const VIRTUALIZED_EOI: u16 = 45;
    pub const EPT_VIOLATION: u16 = 48;
    pub const WBINVD: u16 = 54;
    pub const XSETBV: u16 = 55;
    pub const APIC_WRITE: u16 = 56;
}

/// One L2 processor.
///
/// An L2 vCPU lives in the TDVPS of the L1 vCPU that enters it, so this is
/// pinned to a thread and must not migrate: the VMCS is programmed on one CPU
/// and entering on another would find that CPU's VMCS at its power-on
/// defaults.
pub struct TdpProcessor {
    pub(crate) partition: Arc<TdpPartitionInner>,
    pub(crate) vp_index: VpIndex,
    /// Topology information, needed to answer what this processor's state
    /// looks like at reset.
    pub(crate) vp_info: vm_topology::processor::x86::X86VpInfo,
    pub(crate) memory: Arc<TdpMemory>,
    /// The GPR image the module loads on entry and writes back on exit. It is
    /// guest-physical memory, so it is reached through its own mapping.
    pub(crate) context: crate::driver::TdMemory,
    /// Cached guest state for the instruction emulator, refreshed per exit
    /// because each field costs a TDCALL.
    pub(crate) cached: CachedState,
    pub(crate) regs: L2GprContext,
    /// Instruction bytes fetched for the emulator.
    pub(crate) instruction: Vec<u8>,
    /// An exception has been written into the entry field and not yet
    /// delivered.
    pub(crate) injection_pending: bool,
    /// An exception was delivered by the last entry, so the field it was
    /// written into has to be cleared before the next one. The field is not
    /// self-clearing: left set, it injects the same exception on every entry
    /// until the guest's entry stack overflows. Cleared too early — before the
    /// entry that carries it — and the exception is never delivered at all,
    /// which looks identical to a guest that ignores it.
    pub(crate) injection_armed: bool,
    /// An NMI accepted by the APIC model but not yet published through the
    /// VM-entry event field. It remains pending while another exception is
    /// queued or the guest is blocking NMIs.
    pub(crate) nmi_pending: bool,
    /// An NMI written into the VM-entry event field by the APIC model. While
    /// this is armed, the NMI trap is disabled for exactly one real entry.
    pub(crate) nmi_injection_pending: bool,
    /// Exception bitmap programmed for normal entries. Bit 2 is always set to
    /// contain external NMIs that TD Partitioning occasionally leaks to L2.
    pub(crate) exception_bitmap: u32,
    /// The last exit reason, and how many times it has repeated, to catch a
    /// loop the VMM handles silently and therefore never reports.
    pub(crate) last_reason: u16,
    pub(crate) same_reason: u64,
    /// Vectors the guest has finished with, waiting to be reported to the
    /// chipset. A level-triggered line stays in service in the IOAPIC until
    /// it hears this, and is never delivered again.
    pub(crate) pending_eois: Vec<u8>,
    /// The guest physical address of the access being emulated.
    pub(crate) fault_gpa: Option<u64>,
    /// The exit being serviced interrupted the delivery of an event — the
    /// IDT-vectoring information was valid — so the faulting access came from
    /// the delivery itself, not from an instruction that can be emulated.
    pub(crate) exit_during_delivery: bool,
    /// An exception the emulator asked for, to be injected on the next entry.
    pub(crate) pending_event: Option<hvdef::HvX64PendingEvent>,
    /// Guest memory, as OpenVMM sees it, for the emulator's own accesses.
    pub(crate) guest_memory: guestmem::GuestMemory,
    /// The virtual-APIC page. The TDX module forces APIC virtualization on,
    /// and VM entry then requires this to be valid even for a guest that never
    /// touches the APIC.
    pub(crate) apic_page: crate::driver::TdMemory,
    /// Whether the L2's controls have been programmed. They are per-vCPU state
    /// reached through the issuing L1 vCPU, so this cannot happen until a
    /// thread has bound the processor.
    pub(crate) initialized: bool,
    /// How many exceptions have been reported, so the first is detailed and
    /// the rest are not a million identical lines.
    pub(crate) exceptions: u64,
    /// The local APIC's timer, which the kernel switches to once it is up.
    pub(crate) apic_timer: ApicTimer,
    /// OpenVMM's local APIC model, held in an Option so it can be taken out
    /// while the rest of this is borrowed as its client.
    pub(crate) apic: Option<virt_support_apic::LocalApic>,
    /// The clock the APIC's timer runs on.
    pub(crate) vmtime: vmcore::vmtime::VmTimeAccess,
    /// Architectural run state used for INIT/SIPI startup.
    pub(crate) mp_state: virt::x86::vp::MpState,
}

/// What the APIC model asks of the backend.
///
/// Three of these are about a register an L2 does not expose separately and
/// two are about telling somebody else what happened; none of them is where
/// the interesting part lives. That is in what the run loop does with the
/// APIC's state afterwards: publish it into the virtual-APIC page, which is
/// the only thing an L2's hardware will read.
pub(crate) struct TdpApicClient<'a> {
    pub(crate) vmtime: &'a mut vmcore::vmtime::VmTimeAccess,
    /// The virtual-APIC page, which hardware has been updating while the
    /// guest ran.
    pub(crate) apic_page: &'a mut [u8],
    /// Vectors the guest has finished with, to pass on to the IOAPIC so a
    /// level-triggered line can be reasserted.
    pub(crate) eois: &'a mut Vec<u8>,
    pub(crate) interrupts: Arc<crate::traits::PendingInterrupts>,
}

impl virt_support_apic::ApicClient for TdpApicClient<'_> {
    fn cr8(&mut self) -> u32 {
        // The task-priority register is the APIC's own copy; an L2 reaches it
        // through the page rather than through CR8.
        0
    }

    fn set_cr8(&mut self, _value: u32) {}

    fn set_apic_base(&mut self, _value: u64) {
        // Reads of the APIC base MSR come through the MSR path, so there is
        // nothing to accelerate here.
    }

    fn wake(&mut self, vp_index: VpIndex) {
        self.interrupts.wake_vp(vp_index);
    }

    fn eoi(&mut self, vector: u8) {
        self.eois.push(vector);
    }

    fn now(&mut self) -> vmcore::vmtime::VmTime {
        self.vmtime.now()
    }

    fn pull_offload(&mut self) -> ([u32; 8], [u32; 8]) {
        // Takes them, rather than reads them. This is the model asking for its
        // state back, and leaving a copy behind in the page means both of them
        // believe they own the same interrupts — which the guest experiences
        // as vectors delivered twice and vectors delivered that were already
        // retired.
        const IRR: usize = 0x200;
        const ISR: usize = 0x100;
        let mut take = |base: usize| {
            std::array::from_fn(|i| {
                let o = base + i * 0x10;
                let value =
                    u32::from_le_bytes(self.apic_page[o..o + 4].try_into().expect("four bytes"));
                self.apic_page[o..o + 4].copy_from_slice(&0u32.to_le_bytes());
                value
            })
        };
        (take(IRR), take(ISR))
    }
}

/// The local APIC timer.
///
/// The guest programs it by writing the virtual-APIC page, and reads the
/// current count back from that page without exiting — so the VMM has to keep
/// the count moving itself, and deliver the interrupt when it reaches zero.
/// Nothing else provides a tick once the kernel has left the PIT behind: the
/// guest sets an initial count and waits for an interrupt that never comes,
/// and every sleep in userspace waits with it.
#[derive(Default)]
pub struct ApicTimer {
    armed: Option<std::time::Instant>,
    initial_count: u32,
    divide: u32,
    periodic: bool,
    vector: u8,
}

/// The timer counts at the bus clock divided by the divide configuration. The
/// rate does not matter to a guest that calibrates against another clock, but
/// it has to be consistent with itself.
const APIC_TIMER_HZ: u128 = 1_000_000_000;

impl ApicTimer {
    /// The guest wrote one of the timer's registers; `value` is what it wrote.
    pub fn on_write(&mut self, offset: usize, value: u32) {
        const LVT_TIMER: usize = 0x320;
        const TMICT: usize = 0x380;
        const TDCR: usize = 0x3e0;
        const MASKED: u32 = 1 << 16;
        const PERIODIC: u32 = 1 << 17;

        match offset {
            TDCR => {
                // A three-bit field with a gap at bit 2, giving divisors 1..128.
                let raw = value & 0xb;
                let index = (raw & 0x3) | ((raw & 0x8) >> 1);
                self.divide = if index == 7 { 1 } else { 2u32.pow(index + 1) };
            }
            LVT_TIMER => {
                self.vector = value as u8;
                self.periodic = value & PERIODIC != 0;
                if value & MASKED != 0 {
                    self.armed = None;
                }
            }
            TMICT => {
                self.initial_count = value;
                self.armed = (value != 0).then(std::time::Instant::now);
            }
            _ => {}
        }
    }

    pub fn is_armed(&self) -> bool {
        self.armed.is_some()
    }

    fn ticks_since(&self, started: std::time::Instant) -> u128 {
        let divide = u128::from(self.divide.max(1));
        started.elapsed().as_nanos() * APIC_TIMER_HZ / 1_000_000_000 / divide
    }

    /// Refresh the current count the guest will read, and report an expiry.
    ///
    /// The vector comes from the local vector table as it stands now, not from
    /// what was recorded when the count was set: a guest arms the count before
    /// it has finished describing where the interrupt should go, and firing
    /// vector zero at it is not a timer tick, it is a fault it has no handler
    /// for.
    fn tick(&mut self, lvt: u32) -> (u32, Option<u8>) {
        const MASKED: u32 = 1 << 16;

        let Some(started) = self.armed else {
            return (0, None);
        };
        let elapsed = self.ticks_since(started);
        let initial = u128::from(self.initial_count);
        if elapsed < initial {
            return ((initial - elapsed) as u32, None);
        }
        self.armed = self.periodic.then(std::time::Instant::now);
        let vector = lvt as u8;
        if lvt & MASKED != 0 || vector < 0x10 {
            return (0, None);
        }
        (0, Some(vector))
    }
}

pub struct CachedState {
    pub segments: [SegmentRegister; 6],
    pub efer: u64,
    pub cr0: u64,
    pub cr3: u64,
}

impl Default for CachedState {
    fn default() -> Self {
        // Zeroed rather than derived: a segment register has no meaningful
        // default, and this is only ever a placeholder before the first exit
        // refreshes it.
        Self {
            segments: [SegmentRegister {
                base: 0,
                limit: 0,
                selector: 0,
                attributes: 0.into(),
            }; 6],
            efer: 0,
            cr0: 0,
            cr3: 0,
        }
    }
}

impl TdpProcessor {
    pub(crate) fn vm(&self) -> L2Vm<'_> {
        L2Vm::new(&self.partition.device, self.partition.vm_id)
    }

    /// Create a processor bound to the calling thread.
    pub(crate) fn new(
        partition: Arc<TdpPartitionInner>,
        vp_index: VpIndex,
        vp_info: vm_topology::processor::x86::X86VpInfo,
        memory: Arc<TdpMemory>,
        vmm_memory: guestmem::GuestMemory,
        apic: Option<virt_support_apic::LocalApic>,
        vmtime: vmcore::vmtime::VmTimeAccess,
    ) -> anyhow::Result<Self> {
        let context = partition.device.alloc(4096, 0)?;
        let apic_page = partition.device.alloc(4096, 0)?;
        // Use OpenVMM's actual guest-memory view. It contains both the L2's
        // physically constrained high RAM and the separately backed low
        // range. Constructing another anonymous low range here loses the
        // boot metadata the loader wrote into OpenVMM's backing.
        let guest_memory = vmm_memory;
        Ok(Self {
            partition,
            vp_index,
            vp_info,
            memory,
            context,
            cached: CachedState::default(),
            regs: L2GprContext::default(),
            instruction: Vec::new(),
            injection_pending: false,
            injection_armed: false,
            nmi_pending: false,
            nmi_injection_pending: false,
            exception_bitmap: 1 << 2,
            last_reason: u16::MAX,
            same_reason: 0,
            pending_eois: Vec::new(),
            fault_gpa: None,
            exit_during_delivery: false,
            pending_event: None,
            guest_memory,
            apic_page,
            initialized: false,
            exceptions: 0,
            apic_timer: ApicTimer::default(),
            apic,
            vmtime,
            mp_state: if vp_info.base.is_bsp() {
                virt::x86::vp::MpState::Running
            } else {
                virt::x86::vp::MpState::WaitForSipi
            },
        })
    }

    /// Publish the register image the next entry will load.
    pub(crate) fn flush_registers(&mut self) {
        let regs = self.regs;
        self.write_context(&regs);
    }

    fn read_context(&self) -> L2GprContext {
        let ptr = self.context.as_slice().as_ptr().cast::<L2GprContext>();
        // SAFETY: the driver mapped a full page for exactly this structure and
        // nothing else aliases it.
        unsafe { std::ptr::read_volatile(ptr) }
    }

    fn write_context(&mut self, regs: &L2GprContext) {
        let ptr = self
            .context
            .as_slice()
            .as_ptr()
            .cast::<L2GprContext>()
            .cast_mut();
        // SAFETY: as above.
        unsafe { std::ptr::write_volatile(ptr, *regs) }
    }

    /// Refresh the state the emulator needs. Each of these is a TDCALL, so
    /// they are read together and only when an exit needs them.
    fn cache_state(&mut self) -> anyhow::Result<()> {
        use vmcs::field as f;
        let vm = self.vm();
        let segment = |sel, base, limit, ar| -> anyhow::Result<SegmentRegister> {
            Ok(SegmentRegister {
                base: vm.read_vmcs(base, Width::Bits64)?,
                limit: vm.read_vmcs(limit, Width::Bits32)? as u32,
                selector: vm.read_vmcs(sel, Width::Bits16)? as u16,
                attributes: (vm.read_vmcs(ar, Width::Bits32)? as u16).into(),
            })
        };
        self.cached = CachedState {
            segments: [
                segment(
                    f::GUEST_ES_SEL,
                    f::GUEST_ES_BASE,
                    f::GUEST_ES_LIMIT,
                    f::GUEST_ES_AR,
                )?,
                segment(
                    f::GUEST_CS_SEL,
                    f::GUEST_CS_BASE,
                    f::GUEST_CS_LIMIT,
                    f::GUEST_CS_AR,
                )?,
                segment(
                    f::GUEST_SS_SEL,
                    f::GUEST_SS_BASE,
                    f::GUEST_SS_LIMIT,
                    f::GUEST_SS_AR,
                )?,
                segment(
                    f::GUEST_DS_SEL,
                    f::GUEST_DS_BASE,
                    f::GUEST_DS_LIMIT,
                    f::GUEST_DS_AR,
                )?,
                segment(
                    f::GUEST_FS_SEL,
                    f::GUEST_FS_BASE,
                    f::GUEST_FS_LIMIT,
                    f::GUEST_FS_AR,
                )?,
                segment(
                    f::GUEST_GS_SEL,
                    f::GUEST_GS_BASE,
                    f::GUEST_GS_LIMIT,
                    f::GUEST_GS_AR,
                )?,
            ],
            efer: vm.read_vmcs(f::GUEST_IA32_EFER, Width::Bits64)?,
            cr0: vm.read_vmcs(f::GUEST_CR0, Width::Bits64)?,
            cr3: vm.read_vmcs(f::GUEST_CR3, Width::Bits64)?,
        };
        Ok(())
    }

    /// Translate a guest virtual address through the guest's own page tables.
    ///
    /// The emulator addresses operands the way the instruction does, so the
    /// backend repeats the translation the hardware just did. Only four-level
    /// long mode is handled, which is the only mode an L2 here runs in.
    pub(crate) fn translate(&self, gva: u64) -> Option<u64> {
        const PTE_P: u64 = 1;
        const PTE_PS: u64 = 1 << 7;
        const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

        // Before paging is enabled, the emulator has already applied the
        // segment base and the resulting linear address is the physical one.
        // This is the path an AP's SIPI trampoline uses below 1 MiB.
        if self.cached.cr0 & (1 << 31) == 0 {
            return Some(gva);
        }

        let mut table = self.cached.cr3 & ADDR_MASK;
        for level in (0..4).rev() {
            let index = (gva >> (12 + level * 9)) & 511;
            let entry = self.read_ram_u64(table + index * 8)?;
            if entry & PTE_P == 0 {
                return None;
            }
            if level > 0 && entry & PTE_PS != 0 {
                let size = 1u64 << (12 + level * 9);
                return Some((entry & ADDR_MASK & !(size - 1)) | (gva & (size - 1)));
            }
            table = entry & ADDR_MASK;
        }
        Some(table | (gva & 0xfff))
    }

    /// Return the entries consulted while translating `gva`.
    ///
    /// This is diagnostic rather than a second translator: a trapped guest
    /// page fault otherwise reports only CR3 and the virtual address, which
    /// is not enough to distinguish a missing guest mapping from a bad L2
    /// physical-memory publication.
    fn page_walk(&self, cr3: u64, gva: u64) -> Vec<(u8, u64, u64, Option<u64>)> {
        const PTE_P: u64 = 1;
        const PTE_PS: u64 = 1 << 7;
        const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

        let mut walk = Vec::with_capacity(4);
        let mut table = cr3 & ADDR_MASK;
        for level in (0..4).rev() {
            let index = (gva >> (12 + level * 9)) & 511;
            let entry_gpa = table + index * 8;
            let entry = self.read_ram_u64(entry_gpa);
            walk.push((level as u8, table, index, entry));
            let Some(entry) = entry else { break };
            if entry & PTE_P == 0 || (level > 0 && entry & PTE_PS != 0) {
                break;
            }
            table = entry & ADDR_MASK;
        }
        walk
    }

    fn ram_offset(&self, gpa: u64, len: usize) -> Option<usize> {
        self.memory.backing_offset(gpa, len)
    }

    fn read_ram_u64(&self, gpa: u64) -> Option<u64> {
        let offset = self.ram_offset(gpa, 8)?;
        let bytes = &self.memory.region().as_slice()[offset..offset + 8];
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    }

    /// Fetch the instruction at RIP, one byte at a time.
    ///
    /// Byte by byte because an instruction may end just before an unmapped
    /// page, and faulting on the fetch of a byte the instruction does not need
    /// would be a failure of the VMM's own making.
    fn fetch_instruction(&mut self) {
        self.instruction.clear();
        let rip = self.cached.segments[Segment::CS as usize]
            .base
            .wrapping_add(self.regs.rip);
        for i in 0..16u64 {
            let Some(gpa) = self.translate(rip + i) else {
                break;
            };
            if let Some(offset) = self.ram_offset(gpa, 1) {
                self.instruction
                    .push(self.memory.region().as_slice()[offset]);
            } else if gpa < crate::lowmem::LOW_MEMORY_END {
                let Ok(byte) = self.guest_memory.read_plain::<u8>(gpa) else {
                    break;
                };
                self.instruction.push(byte);
            } else {
                break;
            }
        }
    }
}

impl InspectMut for TdpProcessor {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond().field("vp_index", self.vp_index.index());
    }
}

/// What one entry produced, once the three return classes have been sorted
/// out.
pub(crate) enum Exit {
    /// The guest exited and the L1 has to service it.
    Serviced {
        reason: u16,
        qualification: u64,
        instruction_length: u32,
    },
    /// Nothing retired; enter again without touching guest state.
    Resume,
}

impl TdpProcessor {
    /// Program the controls the L2 needs before its first entry.
    ///
    /// Almost none of them are the L1's to choose: the TDX module owns EPT,
    /// VPID, unrestricted guest, the MSR bitmaps and the pin controls, and
    /// forces APIC virtualization on. Reading its defaults and writing them
    /// back is the honest thing to do, and it proves the write masks are
    /// right. What is left is guest-owned state and the virtual-APIC page,
    /// which entry insists on even for a guest that never uses an APIC.
    fn initialize(&mut self) -> anyhow::Result<()> {
        use vmcs::field as f;
        // Every TDG.VP.* operation reaches the TDVPS of the issuing L1 CPU,
        // so feature controls must be programmed here after this VP's thread
        // is pinned rather than once on the partition-building thread.
        vmcs::disable_optional_l2_features(&self.vm())?;
        let apic_gpa = self.apic_page.gpa();
        Self::initialize_apic_page(self.apic_page.as_mut_slice());
        self.with_apic(|apic, client| apic.access(client).msr_write(msr::APIC_BASE, 0xfee0_0d00))
            .transpose()
            .map_err(|err| anyhow::anyhow!("enabling the x2APIC model failed: {err:?}"))?;
        let vm = self.vm();

        let proc_ctls = vm.read_vmcs(f::PROC_EXEC_CTLS, Width::Bits32)?;
        vm.write_vmcs(f::PROC_EXEC_CTLS, Width::Bits32, proc_ctls)?;
        let proc_ctls2 = vm.read_vmcs(f::PROC_EXEC_CTLS2, Width::Bits32)?;
        vm.write_vmcs(f::PROC_EXEC_CTLS2, Width::Bits32, proc_ctls2)?;
        tracing::info!(
            proc_ctls,
            proc_ctls2,
            tpr_shadow = proc_ctls & (1 << 21) != 0,
            virtualize_apic_accesses = proc_ctls2 & (1 << 0) != 0,
            virtualize_x2apic = proc_ctls2 & (1 << 4) != 0,
            apic_register_virtualization = proc_ctls2 & (1 << 8) != 0,
            virtual_interrupt_delivery = proc_ctls2 & (1 << 9) != 0,
            pin_ctls = vm.read_vmcs(f::PIN_BASED_CTLS, Width::Bits32).unwrap_or(0),
            "the controls the TDX module gives this L2"
        );

        vm.write_vmcs(f::VIRTUAL_APIC_PAGE, Width::Bits64, apic_gpa)?;
        vm.write_vmcs(f::TPR_THRESHOLD, Width::Bits32, 0)?;
        // TDVPS state survives closing the userspace owner and claiming the
        // same L2 slot again. Never inherit a valid event-injection field from
        // an earlier VM: a stale NMI can remain blocked for minutes and then
        // appear in an unrelated workload as Linux's "unknown reason" NMI.
        vm.write_vmcs(f::ENTRY_INTR_INFO, Width::Bits32, 0)?;
        vm.write_vmcs(f::ENTRY_EXCEPTION_EC, Width::Bits32, 0)?;
        vm.write_vmcs(f::ENTRY_INSTR_LEN, Width::Bits32, 0)?;
        // A guest that faults in a loop of its own making takes no exits, so
        // there is nothing to see: TDG.VP.ENTER simply does not return. Setting
        // this makes the loop visible. It is off by default because a guest
        // legitimately handles its own exceptions — the decompressor builds
        // page mappings from its own #PF handler — and trapping them all would
        // be both slow and wrong.
        let bitmap: u32 = std::env::var("VIRT_TDP_TRAP_EXCEPTIONS")
            .ok()
            .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
            .unwrap_or(0)
            | (1 << 2);
        drop(vm);
        self.exception_bitmap = bitmap;
        let vm = self.vm();
        vm.write_vmcs(f::EXCEPTION_BITMAP, Width::Bits32, bitmap.into())?;
        vm.write_vmcs(f::PF_EC_MASK, Width::Bits32, 0)?;
        vm.write_vmcs(f::PF_EC_MATCH, Width::Bits32, 0)?;
        vm.write_vmcs(f::CR3_TARGET_COUNT, Width::Bits32, 0)?;

        // The guest owns its control registers; nothing here shadows them.
        vm.write_vmcs(f::CR0_MASK, Width::Bits64, 0)?;
        vm.write_vmcs(f::CR4_MASK, Width::Bits64, 0)?;

        vm.write_vmcs(f::ENTRY_INTR_INFO, Width::Bits32, 0)?;
        vm.write_vmcs(f::ENTRY_EXCEPTION_EC, Width::Bits32, 0)?;
        vm.write_vmcs(f::ENTRY_INSTR_LEN, Width::Bits32, 0)?;

        // Power-on values for the state the guest has not set itself.
        vm.write_vmcs(f::GUEST_SYSENTER_CS, Width::Bits32, 0)?;
        vm.write_vmcs(f::GUEST_SYSENTER_ESP, Width::Bits64, 0)?;
        vm.write_vmcs(f::GUEST_SYSENTER_EIP, Width::Bits64, 0)?;
        vm.write_vmcs(f::GUEST_PENDING_DBG, Width::Bits64, 0)?;
        vm.write_vmcs(f::GUEST_IA32_DEBUGCTL, Width::Bits64, 0)?;
        vm.write_vmcs(f::GUEST_INTERRUPTIBILITY, Width::Bits32, 0)?;
        vm.write_vmcs(f::GUEST_IA32_PAT, Width::Bits64, 0x0007_0406_0007_0406)?;
        vm.write_vmcs(f::GUEST_DR7, Width::Bits64, 0x400)?;

        // What the guest reaches without an exit.
        //
        // The x2APIC range is the important part: the module virtualizes an
        // L2's APIC through these MSRs, and that only happens if they are not
        // intercepted. Intercepting them means answering them here, which
        // cannot be done correctly — the hardware owns the interrupt state
        // these registers describe.
        //
        // The rest is per-thread state. Intercepting those means inventing
        // values, and SWAPGS exchanges two of them with no exit at all, so an
        // intercepted read cannot be answered correctly either. The set
        // matches what the kernel driver swaps around every entry.
        // The module can offload the three registers OpenHCL uses directly.
        // Every other x2APIC register stays intercepted for the APIC model.
        for msr in [0x808, 0x80b, 0x83f]
            .into_iter()
            .chain(PASSTHROUGH_MSRS.iter().copied())
        {
            vm.passthrough_msr(msr)?;
        }

        vm.invept()?;
        Ok(())
    }

    /// Give the virtual-APIC page the values a real local APIC powers up with.
    ///
    /// The guest's APIC reads never exit, so whatever is in this page is what
    /// it sees — and a zeroed page says APIC id 0, version 0, and every local
    /// vector table entry unmasked and pointing at vector 0. No real APIC
    /// reports that, and a guest that believes it configures itself into
    /// something that never delivers.
    fn initialize_apic_page(page: &mut [u8]) {
        const ID: usize = 0x020;
        const VERSION: usize = 0x030;
        const TPR: usize = 0x080;
        const LDR: usize = 0x0d0;
        const DFR: usize = 0x0e0;
        const SPURIOUS: usize = 0x0f0;
        const LVT_FIRST: usize = 0x320;
        /// Version 0x14 with six local vector table entries, which is what a
        /// modern local APIC reports.
        const VERSION_VALUE: u32 = 0x0005_0014;
        const MASKED: u32 = 1 << 16;

        let mut put = |offset: usize, value: u32| {
            page[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        };
        put(ID, 0);
        put(VERSION, VERSION_VALUE);
        put(TPR, 0);
        put(LDR, 1 << 24);
        put(DFR, 0xffff_ffff);
        // Software-disabled at power-on, with the spurious vector Linux
        // expects to overwrite.
        put(SPURIOUS, 0xff);
        for i in 0..6 {
            put(LVT_FIRST + i * 0x10, MASKED);
        }
    }

    pub(crate) fn enter(&mut self) -> anyhow::Result<Exit> {
        if !self.initialized {
            self.partition
                .low_memory
                .prepare(&self.vm(), &self.guest_memory)?;
            self.initialize()?;
            self.initialized = true;
            let vm = self.vm();
            tracing::info!(
                vm_id = self.partition.vm_id,
                rip = self.regs.rip,
                rsi = self.regs.rsi,
                cr0 = vm
                    .read_vmcs(vmcs::field::GUEST_CR0, Width::Bits64)
                    .unwrap_or(0),
                cr3 = vm
                    .read_vmcs(vmcs::field::GUEST_CR3, Width::Bits64)
                    .unwrap_or(0),
                cr4 = vm
                    .read_vmcs(vmcs::field::GUEST_CR4, Width::Bits64)
                    .unwrap_or(0),
                efer = vm
                    .read_vmcs(vmcs::field::GUEST_IA32_EFER, Width::Bits64)
                    .unwrap_or(0),
                entry_ctls = vm
                    .read_vmcs(vmcs::field::ENTRY_CTLS, Width::Bits32)
                    .unwrap_or(0),
                cs_ar = vm
                    .read_vmcs(vmcs::field::GUEST_CS_AR, Width::Bits32)
                    .unwrap_or(0),
                "entering an L2 for the first time"
            );
        }
        let context_gpa = self.context.gpa();
        // Take back an event only after an entry actually accepted it. A
        // kernel kick may win before TDG.VP.ENTER, in which case the field
        // must remain armed for the next attempt.
        let clear = std::mem::take(&mut self.injection_armed);
        let vm = self.vm();
        if clear && !self.injection_pending {
            vm.write_vmcs(vmcs::field::ENTRY_INTR_INFO, Width::Bits32, 0)?;
        }

        // Trap unexplained external NMIs. An NMI deliberately queued by the
        // APIC model gets one entry with the trap removed so the guest can
        // consume it normally.
        if self.nmi_injection_pending {
            vm.write_vmcs(
                vmcs::field::EXCEPTION_BITMAP,
                Width::Bits32,
                u64::from(self.exception_bitmap & !(1 << 2)),
            )?;
        }

        let result = vm.enter(self.vp_index.index(), context_gpa)?;
        drop(vm);
        if !matches!(result, VpEnterResult::NoEntry) {
            if self.injection_pending {
                self.injection_pending = false;
                self.injection_armed = true;
            }
            if std::mem::take(&mut self.nmi_injection_pending) {
                self.vm().write_vmcs(
                    vmcs::field::EXCEPTION_BITMAP,
                    Width::Bits32,
                    self.exception_bitmap.into(),
                )?;
            }
        }

        let vm = self.vm();
        match result {
            VpEnterResult::NoEntry => Ok(Exit::Resume),
            VpEnterResult::HostRouted { reason } => {
                if reason == exit::EXTERNAL_INTERRUPT {
                    return Ok(Exit::Resume);
                }
                // The host took the exit and resumed the L1 rather than
                // servicing it. The exit information is in the VMCS.
                Ok(Exit::Serviced {
                    reason,
                    qualification: vm.read_vmcs(vmcs::field::EXIT_QUALIFICATION, Width::Bits64)?,
                    instruction_length: vm
                        .read_vmcs(vmcs::field::EXIT_INSTRUCTION_LEN, Width::Bits32)?
                        as u32,
                })
            }
            VpEnterResult::Exit {
                reason,
                qualification,
                instruction_length,
            } => Ok(Exit::Serviced {
                reason,
                qualification,
                instruction_length,
            }),
            VpEnterResult::Error(status) => {
                let err = vm
                    .read_vmcs(vmcs::field::VM_INSTRUCTION_ERROR, Width::Bits32)
                    .unwrap_or(0);
                anyhow::bail!("TDG.VP.ENTER failed with {status:#018x}, VM_INSTRUCTION_ERROR={err}")
            }
        }
    }
}

/// Register access for the instruction emulator, indexed the way a VMX exit
/// qualification names registers.
pub(crate) fn gp_mut(regs: &mut L2GprContext, reg: Gp) -> &mut u64 {
    match reg {
        Gp::RAX => &mut regs.rax,
        Gp::RCX => &mut regs.rcx,
        Gp::RDX => &mut regs.rdx,
        Gp::RBX => &mut regs.rbx,
        Gp::RSP => &mut regs.rsp,
        Gp::RBP => &mut regs.rbp,
        Gp::RSI => &mut regs.rsi,
        Gp::RDI => &mut regs.rdi,
        Gp::R8 => &mut regs.r8,
        Gp::R9 => &mut regs.r9,
        Gp::R10 => &mut regs.r10,
        Gp::R11 => &mut regs.r11,
        Gp::R12 => &mut regs.r12,
        Gp::R13 => &mut regs.r13,
        Gp::R14 => &mut regs.r14,
        Gp::R15 => &mut regs.r15,
    }
}

impl virt::Processor for &'_ mut TdpProcessor {
    type StateAccess<'a>
        = crate::vp_state::TdpVpState<'a>
    where
        Self: 'a;

    fn set_debug_state(
        &mut self,
        _vtl: hvdef::Vtl,
        _state: Option<&virt::x86::DebugState>,
    ) -> Result<(), crate::partition::TdpError> {
        // Single-stepping an L2 means the monitor trap flag, which is a VMCS
        // control the TDX module does not let an L1 set.
        Err(crate::partition::TdpError::Unsupported("debugging an L2"))
    }

    fn flush_async_requests(&mut self) {}

    fn access_state(&mut self, vtl: hvdef::Vtl) -> Self::StateAccess<'_> {
        assert_eq!(vtl, hvdef::Vtl::Vtl0, "an L2 has no VTLs");
        crate::vp_state::TdpVpState { processor: self }
    }

    async fn run_vp(
        &mut self,
        mut stop: StopVp<'_>,
        dev: &impl CpuIo,
    ) -> Result<Infallible, VpHaltReason> {
        // Devices need a way to end an entry this thread is blocked in.
        self.partition
            .interrupts
            .register_waker_thread(self.vp_index);
        // `enter` blocks synchronously, so arm StopVp's task waker before the
        // first entry. A controller stop then reaches Partition::request_yield,
        // which interrupts the entry and lets the check below observe it.
        stop.arm_waker().await?;

        let mut resumes: u64 = 0;
        loop {
            stop.check()?;

            // Let the APIC model work out what is pending — its timer runs on
            // the VM's clock — and publish the result where an L2's hardware
            // will look for it.
            //
            // The model is the authoritative APIC register file. The
            // hand-written path remains available only as a diagnostic aid.
            if std::env::var_os("VIRT_TDP_HAND_APIC").is_none() {
                // Tell the chipset what the guest has finished with, before
                // asking it what is pending. A level-triggered line — which
                // is how virtio-mmio is declared — is held in service by the
                // IOAPIC from the moment it is delivered until this arrives,
                // and is not delivered again in between. Skipping it does not
                // lose one interrupt, it loses every interrupt that device
                // will ever raise.
                for vector in std::mem::take(&mut self.pending_eois) {
                    dev.handle_eoi(vector.into());
                }

                // Devices raise interrupts on other threads; they go to the
                // model, not the page, because there can only be one owner of
                // the request register. The model holds them until the scan
                // below publishes everything it has accepted in one place.
                // Clearing the per-VP notification does not consume the APIC
                // request. LocalApic::scan pulls the targeted request from its
                // shared set and publishes it into this VP's offload page.
                self.partition.interrupts.drain(self.vp_index);
                let work = self
                    .scan_apic()
                    .map_err(|err| dev.fatal_error(err.into()))?;
                self.handle_apic_work(work)
                    .map_err(|err| dev.fatal_error(err.into()))?;
            } else {
                // Devices raise interrupts on other threads; deliver whatever
                // has accumulated before entering.
                for vector in self.partition.interrupts.drain(self.vp_index) {
                    self.request_interrupt(vector)
                        .map_err(|err| dev.fatal_error(err.into()))?;
                    if std::env::var_os("VIRT_TDP_TRACE_DELIVERY").is_some() {
                        // Whether hardware consumed the last one says whether
                        // this mechanism works at all: delivery clears the
                        // request bit and the requesting virtual interrupt.
                        let irr = {
                            let page = self.apic_page.as_slice();
                            let offset = 0x200 + (usize::from(vector) / 32) * 0x10;
                            u32::from_le_bytes(page[offset..offset + 4].try_into().expect("four"))
                        };
                        let status = self
                            .vm()
                            .read_vmcs(vmcs::field::GUEST_INTERRUPT_STATUS, Width::Bits16)
                            .unwrap_or(0);
                        tracing::info!(
                            vector,
                            irr_set = irr & (1 << (vector % 32)) != 0,
                            rvi = status as u8,
                            svi = (status >> 8) as u8,
                            "requested an interrupt"
                        );
                    }
                }
            }

            if self.mp_state == virt::x86::vp::MpState::WaitForSipi {
                std::future::poll_fn(|cx| {
                    self.partition
                        .interrupts
                        .wake_on_interrupt(self.vp_index, cx.waker());
                    if !self.partition.interrupts.is_empty(self.vp_index) {
                        std::task::Poll::Ready(())
                    } else {
                        std::task::Poll::Pending
                    }
                })
                .await;
                continue;
            }

            // Publish that the entry is underway before making it, so an
            // interrupt raised during it signals this thread out rather than
            // being slept through. One raised in the gap between this and the
            // TDCALL can still have its signal land early and be lost; the
            // vector stays queued and the next entry — forced by the host's
            // own timer if nothing else — picks it up.
            self.partition.interrupts.entry_begin(self.vp_index);
            let exit = self.enter().map_err(|err| dev.fatal_error(err.into()))?;
            self.partition.interrupts.entry_end(self.vp_index);

            // A guest that stops exiting is either working or spinning, and
            // the only way to tell is where it is. The host's own timer forces
            // a return often enough to sample it.
            resumes = if matches!(exit, Exit::Resume) {
                resumes + 1
            } else {
                0
            };
            let sample = std::env::var("VIRT_TDP_SAMPLE")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(20_000);
            if resumes % sample == sample - 1 {
                tracing::info!(rip = self.read_context().rip, resumes, "L2 has not exited");
            }
            let Exit::Serviced {
                reason,
                qualification,
                instruction_length,
            } = exit
            else {
                continue;
            };

            // An exit reason that repeats without the guest advancing is a
            // loop the VMM is failing to break: it is handled, so nothing is
            // logged, and re-entering reproduces it immediately. Name it.
            if reason == self.last_reason {
                self.same_reason += 1;
                if self.same_reason % 50_000 == 49_999 {
                    tracing::warn!(
                        reason,
                        qualification,
                        rip = self.read_context().rip,
                        count = self.same_reason,
                        "the same exit reason keeps repeating"
                    );
                }
            } else {
                self.last_reason = reason;
                self.same_reason = 0;
            }

            self.regs = self.read_context();
            tracing::trace!(reason, qualification, rip = self.regs.rip, "L2 exit");
            stop.until_stop(self.service(dev, reason, qualification, instruction_length))
                .await??;
            let regs = self.regs;
            self.write_context(&regs);
        }
    }
}

/// The x2APIC register file, which lives in MSRs when an L2's APIC is
/// virtualized.
const X2APIC_MSRS: std::ops::Range<u32> = 0x800..0x840;

/// MSRs the guest reaches directly, from OpenHCL's MSR_ALLOWED_READ_WRITE.
const PASSTHROUGH_MSRS: &[u32] = &[
    0x10, // TSC
    0x48, // SPEC_CTRL
    0x49, // PRED_CMD, the barrier a context switch issues
    0x174,
    0x175,
    0x176,       // SYSENTER CS/ESP/EIP
    0xda0,       // XSS
    0xc000_0080, // EFER
    0xc000_0081, // STAR
    0xc000_0082, // LSTAR
    0xc000_0084, // SFMASK
    0xc000_0100, // FS_BASE
    0xc000_0101, // GS_BASE
    0xc000_0102, // KERNEL_GS_BASE
    0xc000_0103, // TSC_AUX
];

/// MSRs the backend answers itself. Everything else is either passed through
/// to hardware by the L2's MSR bitmap or faults.
mod msr {
    pub const PLATFORM_ID: u32 = 0x17;
    pub const APIC_BASE: u32 = 0x1b;
    /// The microcode revision. Linux reads it unconditionally on an Intel
    /// processor, and faulting is not an option: the sequence is a write of
    /// zero, a CPUID, then a read, with no exception handling around it.
    pub const BIOS_SIGN_ID: u32 = 0x8b;
    pub const MTRR_CAP: u32 = 0xfe;
    pub const ARCH_CAPABILITIES: u32 = 0x10a;
    pub const MISC_ENABLE: u32 = 0x1a0;
    pub const PAT: u32 = 0x277;
    pub const MTRR_DEF_TYPE: u32 = 0x2ff;
    pub const PLATFORM_INFO: u32 = 0xce;
}

impl TdpProcessor {
    /// Returns false if the MSR is unknown, in which case the caller injects
    /// a #GP rather than inventing a value.
    fn emulate_msr(&mut self, is_write: bool) -> anyhow::Result<bool> {
        let index = self.regs.rcx as u32;
        let value = (self.regs.rdx << 32) | (self.regs.rax & 0xffff_ffff);

        if X2APIC_MSRS.contains(&index) {
            if is_write {
                let result =
                    self.with_apic(|apic, client| apic.access(client).msr_write(index, value));
                return result
                    .transpose()
                    .map(|value| value.is_some())
                    .map_err(|err| anyhow::anyhow!("x2APIC MSR {index:#x} write failed: {err:?}"));
            }
            let result = self.with_apic(|apic, client| apic.access(client).msr_read(index));
            let Some(value) = result
                .transpose()
                .map_err(|err| anyhow::anyhow!("x2APIC MSR {index:#x} read failed: {err:?}"))?
            else {
                return Ok(false);
            };
            self.regs.rax = value & 0xffff_ffff;
            self.regs.rdx = value >> 32;
            return Ok(true);
        }

        if is_write {
            match index {
                msr::PAT => {
                    self.vm()
                        .write_vmcs(vmcs::field::GUEST_IA32_PAT, Width::Bits64, value)?
                }
                // Writing zero to the revision register is how the read is
                // armed; there is nothing to store.
                msr::BIOS_SIGN_ID => {}
                msr::APIC_BASE | msr::MISC_ENABLE | msr::MTRR_DEF_TYPE => {}
                _ => return Ok(false),
            }
            return Ok(true);
        }

        let value = match index {
            // Enabled, x2APIC, bootstrap processor.
            //
            // x2APIC is reported as already on because Linux will not turn it
            // on by itself without interrupt remapping, and it has no choice
            // in the matter: the TDX module virtualizes an L2's APIC through
            // MSRs and leaves memory-mapped accesses alone, so xAPIC mode
            // talks to nothing. Firmware enabling it is exactly the case Linux
            // does accept.
            // ...but claiming it outright stops the guest booting at all, and
            // the reason is not yet known — in x2APIC mode the APIC lives in
            // MSRs 0x800..0x83f are split between hardware offload and the
            // local APIC model above.
            msr::APIC_BASE => 0xfee0_0d00,
            msr::MISC_ENABLE => 1,
            msr::PAT => self
                .vm()
                .read_vmcs(vmcs::field::GUEST_IA32_PAT, Width::Bits64)?,
            // A microcode revision in the high half, which is where Linux
            // reads it from. Any value will do; none is the same as faulting.
            msr::BIOS_SIGN_ID => 1 << 32,
            // Processor flags are a bitmask selecting a microcode variant, and
            // an L2 is never going to load microcode.
            msr::PLATFORM_ID => 0,
            // No variable ranges and no fixed ranges: memory typing for an L2
            // is the TDX module's, not the guest's.
            msr::MTRR_CAP => 0,
            // Write-back everything, MTRRs disabled.
            msr::MTRR_DEF_TYPE => 0x0c00,
            // Claiming immunity to the speculation issues would be a lie the
            // guest cannot check; claiming nothing lets it apply its own
            // mitigations.
            msr::ARCH_CAPABILITIES => 0,
            // The bus ratio fields a guest uses to derive a nominal frequency;
            // it calibrates against the PIT anyway.
            msr::PLATFORM_INFO => 0,
            _ => return Ok(false),
        };
        self.regs.rax = value & 0xffff_ffff;
        self.regs.rdx = value >> 32;
        Ok(true)
    }

    /// A #GP with an error code, valid, hardware exception, vector 13.
    fn inject_gp(&mut self) -> anyhow::Result<()> {
        let vm = self.vm();
        vm.write_vmcs(vmcs::field::ENTRY_INTR_INFO, Width::Bits32, 0x8000_0b0d)?;
        vm.write_vmcs(vmcs::field::ENTRY_EXCEPTION_EC, Width::Bits32, 0)?;
        self.injection_pending = true;
        Ok(())
    }

    /// Emulate a control-register access.
    ///
    /// The TDX module owns bits of CR0 and CR4 for an L2 and forces an exit
    /// when the guest writes them, whatever guest/host mask the L1 asks for.
    /// Both the register and its read shadow have to be updated, or the guest
    /// reads back a value it did not write.
    fn emulate_cr_access(&mut self, qualification: u64) -> anyhow::Result<()> {
        use vmcs::field as f;
        let cr = qualification & 0xf;
        let access_type = (qualification >> 4) & 3;
        let index = (qualification >> 8) & 0xf;
        let gp = match index {
            0 => Gp::RAX,
            1 => Gp::RCX,
            2 => Gp::RDX,
            3 => Gp::RBX,
            4 => Gp::RSP,
            5 => Gp::RBP,
            6 => Gp::RSI,
            7 => Gp::RDI,
            8 => Gp::R8,
            9 => Gp::R9,
            10 => Gp::R10,
            11 => Gp::R11,
            12 => Gp::R12,
            13 => Gp::R13,
            14 => Gp::R14,
            _ => Gp::R15,
        };

        let (reg, shadow) = match cr {
            0 => (f::GUEST_CR0, Some(f::CR0_SHADOW)),
            3 => (f::GUEST_CR3, None),
            4 => (f::GUEST_CR4, Some(f::CR4_SHADOW)),
            other => anyhow::bail!("access to CR{other} is not emulated"),
        };

        match access_type {
            0 => {
                let value = *gp_mut(&mut self.regs, gp);
                let vm = self.vm();
                vm.write_vmcs(reg, Width::Bits64, value)?;
                if let Some(shadow) = shadow {
                    vm.write_vmcs(shadow, Width::Bits64, value)?;
                }
            }
            1 => {
                let value = self.vm().read_vmcs(reg, Width::Bits64)?;
                *gp_mut(&mut self.regs, gp) = value;
            }
            other => anyhow::bail!("control register access type {other} is not emulated"),
        }
        Ok(())
    }

    /// Retire the interrupt currently in service, as an EOI does.
    ///
    /// Clears the highest set bit of the in-service register and republishes
    /// the servicing virtual interrupt, which is what the hardware compares
    /// the next request against.
    pub(crate) fn retire_in_service_interrupt(&mut self) {
        const ISR: usize = 0x100;

        let page = self.apic_page.as_mut_slice();
        let mut top = None;
        for index in (0..8).rev() {
            let offset = ISR + index * 0x10;
            let word = u32::from_le_bytes(page[offset..offset + 4].try_into().expect("four"));
            if word != 0 {
                let bit = 31 - word.leading_zeros();
                let cleared = word & !(1 << bit);
                page[offset..offset + 4].copy_from_slice(&cleared.to_le_bytes());
                top = Some((index as u32 * 32 + bit) as u8);
                break;
            }
        }
        let Some(_retired) = top else {
            return;
        };

        // The servicing virtual interrupt is now whatever is left in service.
        let mut svi = 0u8;
        for index in (0..8).rev() {
            let offset = ISR + index * 0x10;
            let word = u32::from_le_bytes(page[offset..offset + 4].try_into().expect("four"));
            if word != 0 {
                svi = (index as u32 * 32 + (31 - word.leading_zeros())) as u8;
                break;
            }
        }
        let mut regs = self.read_context();
        regs.interrupt_status = (regs.interrupt_status & 0x00ff) | (u16::from(svi) << 8);
        self.write_context(&regs);
        self.regs.interrupt_status = regs.interrupt_status;
    }

    /// Run something against the APIC model, with this processor as its
    /// client. The model is taken out for the duration because the client
    /// borrows the rest.
    pub(crate) fn with_apic<R>(
        &mut self,
        f: impl FnOnce(&mut virt_support_apic::LocalApic, &mut TdpApicClient<'_>) -> R,
    ) -> Option<R> {
        let mut apic = self.apic.take()?;
        let mut eois = Vec::new();
        let mut client = TdpApicClient {
            vmtime: &mut self.vmtime,
            apic_page: self.apic_page.as_mut_slice(),
            eois: &mut eois,
            interrupts: self.partition.interrupts.clone(),
        };
        let r = f(&mut apic, &mut client);
        self.apic = Some(apic);
        // Queued rather than dispatched here: the chipset is reached through
        // the `CpuIo` the run loop holds, and this runs underneath the
        // instruction emulator, which does not have it.
        self.pending_eois.extend(eois);
        Some(r)
    }

    /// Read the interrupt registers back out of the virtual-APIC page.
    ///
    /// Hardware owns them while the guest runs — it moves a request into
    /// service and clears it on end-of-interrupt — so the model has to be told
    /// what happened before it is asked anything.
    fn read_apic_registers(&self) -> ([u32; 8], [u32; 8]) {
        const IRR: usize = 0x200;
        const ISR: usize = 0x100;
        let page = self.apic_page.as_slice();
        let read = |base: usize| {
            std::array::from_fn(|i| {
                let o = base + i * 0x10;
                u32::from_le_bytes(page[o..o + 4].try_into().expect("four bytes"))
            })
        };
        (read(IRR), read(ISR))
    }

    /// Give the APIC model a turn, and publish what it decides.
    ///
    /// This is the part that stays specific to an L2. The model works out
    /// which interrupts are pending and which are in service; hardware will
    /// only look at the virtual-APIC page, so the answer is written there and
    /// the requesting and servicing vectors go into the entry state block.
    pub(crate) fn scan_apic(&mut self) -> anyhow::Result<virt_support_apic::ApicWork> {
        const IRR: usize = 0x200;
        const ISR: usize = 0x100;

        let Some(mut apic) = self.apic.take() else {
            return Ok(Default::default());
        };
        // `scan` wants the clock directly, and the client wants it too; the
        // processor owns it, so the borrow is split by scanning first.
        let work = apic.scan(&mut self.vmtime, true);

        let page = self.apic_page.as_mut_slice();
        let mut pushed = false;
        let mut eoi_exit_bitmap = [0u64; 4];
        let push = apic.push_to_offload(|irr, isr, tmr| {
            for (i, (irr, isr)) in irr.iter().zip(isr).enumerate() {
                let o = IRR + i * 0x10;
                let existing = u32::from_le_bytes(page[o..o + 4].try_into().expect("four bytes"));
                page[o..o + 4].copy_from_slice(&(existing | irr).to_le_bytes());
                let o = ISR + i * 0x10;
                let existing = u32::from_le_bytes(page[o..o + 4].try_into().expect("four bytes"));
                page[o..o + 4].copy_from_slice(&(existing | isr).to_le_bytes());
            }
            for (bitmap, words) in eoi_exit_bitmap.iter_mut().zip(tmr.chunks_exact(2)) {
                *bitmap = u64::from(words[0]) | (u64::from(words[1]) << 32);
            }
            pushed = true;
        });

        // Auto-EOI cannot be expressed through the offload, and the model
        // says so rather than pushing. Ignoring it leaves the model holding
        // interrupts it will never publish and the page describing state
        // nobody owns. There is no un-offloaded delivery path here yet, so
        // this cannot recover the way the reference does — but it must not
        // pass silently.
        if push.is_err() {
            tracing::error!("the APIC model refused to offload; auto-EOI is active");
        }

        // Hardware owns the requesting and servicing vectors while the guest
        // runs: delivery moves a request into service and recomputes both.
        // They are recomputed here only when something changed on this side —
        // a new interrupt pushed, an access that pulled the registers, an
        // end-of-interrupt retired — because rewriting them on every entry
        // races the delivery they describe.
        if pushed {
            // A virtualized EOI only exits for vectors selected here. The APIC
            // model's TMR identifies the level-triggered vectors whose EOI
            // must reach the IOAPIC so it can clear remote IRR and, if the
            // line remains asserted, deliver it again.
            let fields = [
                vmcs::field::EOI_EXIT_BITMAP_0,
                vmcs::field::EOI_EXIT_BITMAP_1,
                vmcs::field::EOI_EXIT_BITMAP_2,
                vmcs::field::EOI_EXIT_BITMAP_3,
            ];
            for (field, bitmap) in fields.into_iter().zip(eoi_exit_bitmap) {
                self.vm().write_vmcs(field, Width::Bits64, bitmap)?;
            }
            let (irr, isr) = self.read_apic_registers();
            let top = |regs: [u32; 8]| -> u8 {
                for (i, word) in regs.iter().enumerate().rev() {
                    if *word != 0 {
                        return (i as u32 * 32 + (31 - word.leading_zeros())) as u8;
                    }
                }
                0
            };
            let status = u16::from(top(irr)) | (u16::from(top(isr)) << 8);
            let mut regs = self.read_context();
            regs.interrupt_status = status;
            self.write_context(&regs);
            self.regs.interrupt_status = status;
        }

        // Does the state hardware is about to act on agree with itself? It
        // reads the requesting and servicing vectors out of the entry state
        // and everything else out of the virtual-APIC page, so a vector named
        // there whose bit is not set in the page is one hardware may deliver
        // out of a register file that no longer describes it.
        if std::env::var_os("VIRT_TDP_CHECK_APIC").is_some() {
            let (irr, isr) = self.read_apic_registers();
            let top = |regs: [u32; 8]| -> u8 {
                for (i, word) in regs.iter().enumerate().rev() {
                    if *word != 0 {
                        return (i as u32 * 32 + (31 - word.leading_zeros())) as u8;
                    }
                }
                0
            };
            let status = self.read_context().interrupt_status;
            let (rvi, svi) = (status as u8, (status >> 8) as u8);
            let set =
                |regs: [u32; 8], v: u8| v == 0 || regs[usize::from(v) / 32] & (1 << (v % 32)) != 0;
            if rvi != top(irr) || svi != top(isr) || !set(irr, rvi) || !set(isr, svi) {
                tracing::warn!(
                    rvi,
                    svi,
                    page_irr = top(irr),
                    page_isr = top(isr),
                    rvi_backed = set(irr, rvi),
                    svi_backed = set(isr, svi),
                    pushed,
                    offloaded = apic.is_offloaded(),
                    "the entry state and the virtual-APIC page disagree"
                );
            }
        }

        self.apic = Some(apic);
        Ok(work)
    }

    fn handle_apic_work(&mut self, work: virt_support_apic::ApicWork) -> anyhow::Result<()> {
        use virt::x86::vp::AccessVpState as _;

        if work.init {
            let vp_info = self.vp_info;
            let mut access = crate::vp_state::TdpVpState { processor: self };
            virt::x86::vp::x86_init(&mut access, &vp_info)?;
        }

        if let Some(vector) = work.sipi {
            if self.mp_state == virt::x86::vp::MpState::WaitForSipi {
                let mut access = crate::vp_state::TdpVpState { processor: self };
                let mut regs = access.registers()?;
                regs.cs.base = u64::from(vector) << 12;
                regs.cs.selector = u16::from(vector) << 8;
                regs.cs.limit = 0xffff;
                regs.cs.attributes = 0x9b;
                regs.rip = 0;
                access.set_registers(&regs)?;
                self.mp_state = virt::x86::vp::MpState::Running;
            }
        }

        if work.nmi {
            tracing::warn!(
                vp = self.vp_index.index(),
                ?work,
                "injecting an APIC-requested NMI"
            );
        }
        self.nmi_pending |= work.nmi;
        if self.nmi_pending && !self.injection_pending {
            // Unlike fixed interrupts, an NMI is not represented in the
            // virtual-APIC IRR/RVI offload. Publish it through the VM-entry
            // event field. If the previous NMI handler has not executed IRET
            // yet, retain the request and retry after the next host return.
            // VM entry also rejects NMI injection while STI or MOV SS is
            // blocking maskable interrupts, even though those states do not
            // themselves block an NMI after entry has completed.
            const BLOCKING_NMI_ENTRY: u64 = (1 << 0) | (1 << 1) | (1 << 3);
            let vm = self.vm();
            let interruptibility =
                vm.read_vmcs(vmcs::field::GUEST_INTERRUPTIBILITY, Width::Bits32)?;
            if interruptibility & BLOCKING_NMI_ENTRY == 0 {
                // Valid, NMI delivery type, architectural NMI vector 2.
                vm.write_vmcs(vmcs::field::ENTRY_INTR_INFO, Width::Bits32, 0x8000_0202)?;
                self.injection_pending = true;
                self.nmi_injection_pending = true;
                self.nmi_pending = false;
            }
        }

        if work.extint || work.interrupt.is_some() {
            tracing::warn!(?work, "an APIC event could not use the L2 offload path");
        }
        Ok(())
    }

    /// Make a vector pending in the guest.
    ///
    /// Not the VM-entry interruption field: the TDX module forces APIC
    /// virtualization on for an L2, so an interrupt is published by setting
    /// its bit in the virtual-APIC page's IRR and raising the requesting
    /// virtual interrupt, and hardware delivers it when the guest allows.
    fn request_interrupt(&mut self, vector: u8) -> anyhow::Result<()> {
        // Both halves are required. The interrupt-request register in the
        // virtual-APIC page is what the hardware evaluates against the guest's
        // priority and state; the requesting virtual interrupt is what tells
        // it to look. Raising only RVI delivers nothing, and the guest reports
        // it as a timer that never ticks rather than as anything to do with
        // the APIC.
        const IRR: usize = 0x200;
        let offset = IRR + (usize::from(vector) / 32) * 0x10;
        let page = self.apic_page.as_mut_slice();
        let mut word = u32::from_le_bytes(page[offset..offset + 4].try_into().expect("four bytes"));
        word |= 1 << (vector % 32);
        page[offset..offset + 4].copy_from_slice(&word.to_le_bytes());

        // The requesting virtual interrupt goes in the entry state block, not
        // in the VMCS. They are the same architectural field, but the module
        // loads it from the block on every entry, so a VMCS write is
        // overwritten before it can have any effect — and the guest reports
        // that as a device whose interrupts never arrive.
        // Only that one field. The register image belongs to the guest
        // between exits, and writing back a copy taken at the last serviced
        // exit throws away everything it has done since — which surfaces as
        // memory corruption in whatever the guest was in the middle of.
        let mut regs = self.read_context();
        regs.interrupt_status = (regs.interrupt_status & 0xff00) | u16::from(vector);
        self.write_context(&regs);
        // Keep the cached image in step. The run loop writes it back after
        // every serviced exit, and a copy taken before the interrupt was armed
        // puts the field back to what it was — clearing the request between
        // arming it and the guest being able to take it.
        self.regs.interrupt_status = regs.interrupt_status;
        if std::env::var_os("VIRT_TDP_TRACE_DELIVERY").is_some() {
            let page = self.apic_page.as_slice();
            let word = |o: usize| u32::from_le_bytes(page[o..o + 4].try_into().expect("four"));
            tracing::info!(
                vector,
                status = regs.interrupt_status,
                tpr = word(0x80),
                spurious = word(0xf0),
                isr1 = word(0x110),
                irr1 = word(0x210),
                rflags_if = regs.rflags & 0x200 != 0,
                "armed the requesting virtual interrupt"
            );
        }
        Ok(())
    }

    async fn service(
        &mut self,
        dev: &impl CpuIo,
        reason: u16,
        qualification: u64,
        instruction_length: u32,
    ) -> Result<(), VpHaltReason> {
        match reason {
            exit::CPUID => {
                let leaf = self.regs.rax as u32;
                let subleaf = self.regs.rcx as u32;
                let result = crate::cpuid::for_l2(
                    leaf,
                    subleaf,
                    self.vp_index.index(),
                    self.partition.vp_count,
                );
                self.regs.rax = result[0].into();
                self.regs.rbx = result[1].into();
                self.regs.rcx = result[2].into();
                self.regs.rdx = result[3].into();
                self.regs.rip += u64::from(instruction_length);
            }
            exit::IO_INSTRUCTION => {
                let size = ((qualification & 7) + 1) as usize;
                let is_in = qualification & 8 != 0;
                let port = (qualification >> 16) as u16;
                let mut data = [0u8; 4];
                if is_in {
                    dev.read_io(self.vp_index, port, &mut data[..size]).await;
                    let mask = match size {
                        1 => 0xff,
                        2 => 0xffff,
                        _ => 0xffff_ffff,
                    };
                    let value = u32::from_le_bytes(data) as u64;
                    self.regs.rax = (self.regs.rax & !mask) | (value & mask);
                } else {
                    data.copy_from_slice(&(self.regs.rax as u32).to_le_bytes());
                    dev.write_io(self.vp_index, port, &data[..size]).await;
                }
                self.regs.rip += u64::from(instruction_length);
            }
            exit::HLT => {
                // The instruction retires, and then this waits.
                //
                // Re-entering immediately looks harmless — the guest simply
                // halts again — but it is a busy loop at the speed of a TDCALL
                // round trip, and this thread is pinned. The device backends
                // that would produce the very interrupt the guest is waiting
                // for get starved by it, so the wait never ends. Sleeping
                // until the APIC's next deadline, or until a device raises
                // something, is both cheaper and the only way out.
                self.regs.rip += u64::from(instruction_length);

                let regs = self.regs;
                self.write_context(&regs);

                // Two things end the wait: the APIC's next deadline, and a
                // device raising an interrupt. The second has to be arranged
                // for — queueing a vector would otherwise be invisible to a
                // future — and doing it by polling instead makes the guest as
                // slow as the poll interval, which a boot notices.
                std::future::poll_fn(|cx| {
                    self.partition
                        .interrupts
                        .wake_on_interrupt(self.vp_index, cx.waker());
                    if !self.partition.interrupts.is_empty(self.vp_index) {
                        return std::task::Poll::Ready(());
                    }
                    self.vmtime.poll_timeout(cx).map(|_| ())
                })
                .await;
            }
            exit::EXCEPTION_NMI => {
                // Bit 2 is always trapped. Deliberate APIC-model NMIs get one
                // entry with this bit cleared, so reaching here identifies an
                // external NMI that escaped TD Partitioning's NMI-exiting
                // control. Contain it instead of injecting unexplained host
                // state into the tenant.
                let (info, code, cr3) = {
                    let vm = self.vm();
                    (
                        vm.read_vmcs(vmcs::field::EXIT_INTR_INFO, Width::Bits32)
                            .unwrap_or(0),
                        vm.read_vmcs(vmcs::field::EXIT_INTR_ERROR_CODE, Width::Bits32)
                            .unwrap_or(0),
                        vm.read_vmcs(vmcs::field::GUEST_CR3, Width::Bits64)
                            .unwrap_or(0),
                    )
                };
                if info & 0xff == 2 {
                    tracing::warn!(
                        vp = self.vp_index.index(),
                        rip = self.regs.rip,
                        cr3,
                        "contained an unexpected external L2 NMI"
                    );
                } else {
                    let first = self.exceptions == 0;
                    self.exceptions += 1;
                    if first {
                        tracing::info!(
                            vector = info & 0xff,
                            code,
                            rip = self.regs.rip,
                            fault_va = qualification,
                            cr3,
                            rax = self.regs.rax,
                            rbx = self.regs.rbx,
                            rcx = self.regs.rcx,
                            rdx = self.regs.rdx,
                            rsi = self.regs.rsi,
                            rdi = self.regs.rdi,
                            rsp = self.regs.rsp,
                            "L2 exception"
                        );
                        if info & 0xff == 14 {
                            for (level, table, index, entry) in self.page_walk(cr3, qualification) {
                                tracing::info!(level, table, index, entry, "L2 faulting page walk");
                            }
                        }
                    }
                    {
                        let vm = self.vm();
                        vm.write_vmcs(vmcs::field::ENTRY_INTR_INFO, Width::Bits32, info)
                            .map_err(|err| dev.fatal_error(err.into()))?;
                        vm.write_vmcs(vmcs::field::ENTRY_EXCEPTION_EC, Width::Bits32, code)
                            .map_err(|err| dev.fatal_error(err.into()))?;
                    }
                    self.injection_pending = true;
                }
            }
            exit::EXTERNAL_INTERRUPT | exit::INTERRUPT_WINDOW => {}
            exit::VIRTUALIZED_EOI => {
                // Hardware has already retired the vector from the virtual
                // APIC ISR. Complete the level-triggered interrupt handshake
                // in the chipset; otherwise the IOAPIC's remote-IRR bit stays
                // set and the device can never interrupt again.
                dev.handle_eoi((qualification & 0xff) as u32);
            }
            exit::MSR_READ | exit::MSR_WRITE => {
                let is_write = reason == exit::MSR_WRITE;
                if self
                    .emulate_msr(is_write)
                    .map_err(|err| dev.fatal_error(err.into()))?
                {
                    self.regs.rip += u64::from(instruction_length);
                } else {
                    // An unknown MSR is a #GP, which a guest's safe accessors
                    // expect and handle. RIP stays where it is — and a guest
                    // that reads it *without* an accessor spins here forever,
                    // so which one it was is worth saying out loud.
                    tracing::debug!(
                        msr = self.regs.rcx as u32,
                        write = is_write,
                        rip = self.regs.rip,
                        "refusing an MSR the backend does not implement"
                    );
                    self.inject_gp()
                        .map_err(|err| dev.fatal_error(err.into()))?;
                }
            }
            exit::CR_ACCESS => {
                self.emulate_cr_access(qualification)
                    .map_err(|err| dev.fatal_error(err.into()))?;
                self.regs.rip += u64::from(instruction_length);
            }
            exit::XSETBV => {
                // XCR0 for an L2 is the TDX module's business — XFAM is fixed
                // at TD creation — so there is nothing to program. Letting the
                // instruction retire keeps the guest moving.
                self.regs.rip += u64::from(instruction_length);
            }
            exit::WBINVD => {
                // The architectural caches are coherent with the memory the
                // L1 maps for this L2. There is no guest-assigned device with
                // a non-coherent view to flush, so the only observable effect
                // is that the instruction retires.
                self.regs.rip += u64::from(instruction_length);
            }
            exit::APIC_WRITE => {
                // The value is already in the virtual-APIC page; the
                // qualification only says which register moved. Nothing here
                // acts on it yet, because the only register that would matter
                // is the timer, and the guest's timer comes from the chipset.
            }
            exit::EPT_VIOLATION => {
                // An L2 has no memory outside the region the L1 gave it, so
                // this is either a device register or a piece of low memory
                // early Linux reads unconditionally. Both are answered by
                // decoding the instruction and asking the chipset.
                let gpa = self
                    .vm()
                    .read_vmcs(vmcs::field::GUEST_PHYSICAL_ADDRESS, Width::Bits64)
                    .map_err(|err| dev.fatal_error(err.into()))?;
                self.fault_gpa = Some(gpa);
                let vectoring = self
                    .vm()
                    .read_vmcs(vmcs::field::IDT_VECTORING_INFO, Width::Bits32)
                    .map_err(|err| dev.fatal_error(err.into()))?;
                self.exit_during_delivery = vectoring & (1 << 31) != 0;
                if gpa & !0xfff != 0xfee0_0000 {
                    tracing::info!(gpa, rip = self.regs.rip, qualification, "L2 memory fault");
                } else {
                    tracing::trace!(gpa, rip = self.regs.rip, qualification, "L2 memory fault");
                }
                self.cache_state()
                    .map_err(|err| dev.fatal_error(err.into()))?;
                self.fetch_instruction();

                // Diagnostic: which instructions actually reach the emulator.
                // An SSE one would be silently wrong, because the guest's XMM
                // registers are not in the L2 register image and the
                // emulator's accessors for them are stubs.
                if std::env::var_os("VIRT_TDP_TRACE_OPCODE").is_some() {
                    let bytes: Vec<String> = self
                        .instruction
                        .iter()
                        .take(6)
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    tracing::info!(gpa, opcode = bytes.join(" "), "emulating");
                }

                let guest_memory = self.guest_memory.clone();
                let access = virt_support_x86emu::emulate::EmulatorMemoryAccess {
                    gm: &guest_memory,
                    kx_gm: &guest_memory,
                    ux_gm: &guest_memory,
                };
                virt_support_x86emu::emulate::emulate(self, &access, dev).await?;
            }
            exit::TRIPLE_FAULT => {
                return Err(VpHaltReason::TripleFault {
                    vtl: hvdef::Vtl::Vtl0,
                });
            }
            other => {
                let err = anyhow::anyhow!(
                    "exit reason {other} is not serviced yet (qualification {qualification:#x}, rip {:#x})",
                    self.regs.rip
                );
                return Err(dev.fatal_error(err.into()));
            }
        }
        Ok(())
    }
}
