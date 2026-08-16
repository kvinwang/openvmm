// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Instruction emulation for accesses an L2 cannot complete on its own.
//!
//! An L2 has no memory outside the region the L1 gave it, because aliasing
//! cannot move a page to a different guest physical address. Two kinds of
//! access land here: device registers, which is the ordinary MMIO case, and
//! low memory, which early Linux reads unconditionally — the EBDA pointer at
//! 0x40e, the iBFT scan across 0x80000..0x100000, the DMI scan at 0xf0000.
//! Both are answered by decoding the instruction and asking the chipset, which
//! is what OpenVMM's emulator is for.
//!
//! The one thing this has to supply that other backends get from their
//! hypervisor is translation: the emulator addresses operands the way the
//! instruction does, so the backend repeats the page walk the hardware just
//! did.

use crate::vp::TdpProcessor;

/// Where a local APIC appears in memory.
const APIC_BASE: u64 = 0xfee0_0000;
use virt::VpIndex;
use virt_support_x86emu::emulate::EmuCheckVtlAccessError;
use virt_support_x86emu::emulate::EmuTranslateError;
use virt_support_x86emu::emulate::EmuTranslateResult;
use virt_support_x86emu::emulate::EmulatorSupport;
use virt_support_x86emu::emulate::InitialTranslation;
use virt_support_x86emu::emulate::TranslateMode;
use x86defs::RFlags;
use x86defs::SegmentRegister;
use x86emu::Gp;
use x86emu::Segment;

impl EmulatorSupport for TdpProcessor {
    fn vp_index(&self) -> VpIndex {
        self.vp_index
    }

    fn vendor(&self) -> x86defs::cpuid::Vendor {
        x86defs::cpuid::Vendor::INTEL
    }

    fn gp(&mut self, index: Gp) -> u64 {
        *crate::vp::gp_mut(&mut self.regs, index)
    }

    fn set_gp(&mut self, reg: Gp, v: u64) {
        *crate::vp::gp_mut(&mut self.regs, reg) = v;
    }

    fn rip(&mut self) -> u64 {
        self.regs.rip
    }

    fn set_rip(&mut self, v: u64) {
        self.regs.rip = v;
    }

    fn segment(&mut self, index: Segment) -> SegmentRegister {
        self.cached.segments[index as usize]
    }

    fn efer(&mut self) -> u64 {
        self.cached.efer
    }

    fn cr0(&mut self) -> u64 {
        self.cached.cr0
    }

    fn rflags(&mut self) -> RFlags {
        self.regs.rflags.into()
    }

    fn set_rflags(&mut self, v: RFlags) {
        self.regs.rflags = v.into();
    }

    fn xmm(&mut self, _reg: usize) -> u128 {
        // The guest's SSE state stays in the CPU across an L2 exit and is not
        // part of the register image, so an instruction that needs it cannot
        // be emulated. Nothing on the paths this serves uses one.
        0
    }

    fn set_xmm(&mut self, _reg: usize, _value: u128) {}

    fn flush(&mut self) {
        self.flush_registers();
    }

    fn instruction_bytes(&self) -> &[u8] {
        &self.instruction
    }

    fn physical_address(&self) -> Option<u64> {
        self.fault_gpa
    }

    fn initial_gva_translation(&mut self) -> Option<InitialTranslation> {
        // The exit reports the faulting physical address but not the
        // translation that produced it, so the emulator repeats the walk.
        None
    }

    fn interruption_pending(&self) -> bool {
        // What the emulator is really asking is whether this exit happened in
        // the middle of delivering an event — a page walk through MMIO on the
        // way to a handler — because then there is no instruction to emulate.
        // The exit says so directly, in the IDT-vectoring information. The
        // injection flags are the wrong answer: they stay set across the exit
        // *after* an injected exception is delivered, and a guest whose
        // handler touches a device register immediately would be killed for
        // it.
        self.exit_during_delivery
    }

    fn check_vtl_access(
        &mut self,
        _gpa: u64,
        _mode: TranslateMode,
    ) -> Result<(), EmuCheckVtlAccessError> {
        // An L2 has no VTLs; the only privilege boundary is the one the TDX
        // module enforces between it and the L1.
        Ok(())
    }

    fn translate_gva(
        &mut self,
        gva: u64,
        _mode: TranslateMode,
    ) -> Result<EmuTranslateResult, EmuTranslateError> {
        match self.translate(gva) {
            Some(gpa) => Ok(EmuTranslateResult {
                gpa,
                overlay_page: None,
            }),
            None => Err(EmuTranslateError {
                code: hvdef::hypercall::TranslateGvaResultCode::GPA_UNMAPPED,
                event_info: None,
            }),
        }
    }

    fn inject_pending_event(&mut self, event_info: hvdef::HvX64PendingEvent) {
        // The emulator asks for an exception when the guest's own access would
        // have faulted. Recording it here; the run loop programs the entry
        // field and clears it afterwards, because that field is not
        // self-clearing.
        self.pending_event = Some(event_info);
    }

    fn is_gpa_mapped(&self, gpa: u64, _write: bool) -> bool {
        // Low memory counts: it is not the guest's, but the VMM backs it and
        // the emulator can reach it, which is the only sense in which "mapped"
        // matters here.
        self.memory.contains_range(gpa..gpa + 1) || gpa < crate::lowmem::LOW_MEMORY_END
    }

    fn lapic_base_address(&self) -> Option<u64> {
        // An L2's APIC accesses *do* reach the emulator, and that is the one
        // thing about its APIC that is not obvious. The TDX module gives an L2
        // APIC-register virtualization and virtual-interrupt delivery, but not
        // APIC-*access* virtualization — the control that redirects memory
        // accesses to this address into the virtual-APIC page. So a guest in
        // xAPIC mode faults here instead, and the VMM has to do the redirect
        // itself.
        Some(APIC_BASE)
    }

    fn lapic_read(&mut self, address: u64, data: &mut [u8]) {
        // Through the APIC model, not out of the page: the page is what
        // hardware reads, and it holds only the registers hardware cares
        // about. Everything else — the timer's current count, the version,
        // the local vector table as the model understands it — is the model's.
        if std::env::var_os("VIRT_TDP_HAND_APIC").is_none() {
            self.with_apic(|apic, client| apic.access(client).mmio_read(address, data));
            return;
        }
        let offset = (address - APIC_BASE) as usize;
        let page = self.apic_page.as_slice();
        if let Some(source) = page.get(offset..offset + data.len()) {
            data.copy_from_slice(source);
        }
    }

    fn lapic_write(&mut self, address: u64, data: &[u8]) {
        const EOI: usize = 0x0b0;

        if std::env::var_os("VIRT_TDP_HAND_APIC").is_none() {
            self.with_apic(|apic, client| apic.access(client).mmio_write(address, data));
            return;
        }

        let offset = (address - APIC_BASE) as usize;
        if offset == EOI {
            // End of interrupt. Hardware would do this itself if it were
            // virtualizing these accesses; forgetting leaves the highest
            // priority interrupt in service forever, and every later one is
            // blocked on priority.
            self.retire_in_service_interrupt();
            return;
        }
        let page = self.apic_page.as_mut_slice();
        if let Some(target) = page.get_mut(offset..offset + data.len()) {
            target.copy_from_slice(data);
        }
        if data.len() == 4 {
            let value = u32::from_le_bytes(data.try_into().expect("four bytes"));
            self.apic_timer.on_write(offset, value);
        }
    }
}
