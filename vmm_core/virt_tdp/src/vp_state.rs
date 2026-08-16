// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-processor state.
//!
//! Registers are the part that matters: the loader sets the guest's initial
//! state through this interface, so a guest cannot start without it. They come
//! from two places — the general-purpose registers live in the image the TDX
//! module exchanges on every entry, and everything else is VMCS fields reached
//! by `TDG.VP.RD` and `TDG.VP.WR`, one TDCALL each.
//!
//! The rest is state this backend does not model yet. Snapshot reads fail
//! explicitly. Writes are accepted only before first entry, while OpenVMM's
//! initial-state loader is constructing a fresh VP; once the VP has run, a
//! restore attempt fails. Returning reset for live state, or silently
//! discarding a restore into a running VP, would create a plausible-looking
//! snapshot that resumes incorrectly.

use crate::partition::TdpError;
use crate::tdx::Width;
use crate::vmcs;
use crate::vp::TdpProcessor;
use virt::x86::SegmentRegister;
use virt::x86::vp;
use virt::x86::vp::AccessVpState;

pub struct TdpVpState<'a> {
    pub(crate) processor: &'a mut TdpProcessor,
}

/// State elements this backend does not model and therefore cannot snapshot.
/// Initial writes are required during fresh VM construction. The TDX module
/// owns these elements' effective reset state; the backend cannot later read
/// or restore them.
macro_rules! unmodelled {
    ($($get:ident, $set:ident, $ty:ident;)*) => {
        $(
            fn $get(&mut self) -> Result<vp::$ty, Self::Error> {
                Err(TdpError::Unsupported(concat!(
                    "snapshotting VP state element ",
                    stringify!($ty),
                )))
            }

            fn $set(&mut self, value: &vp::$ty) -> Result<(), Self::Error> {
                if !self.processor.initialized {
                    return Ok(());
                }
                let reset = <vp::$ty as virt::state::StateElement<_, _>>::at_reset(
                    &self.processor.partition.caps,
                    &self.processor.vp_info,
                );
                if value == &reset {
                    Ok(())
                } else {
                    Err(TdpError::Unsupported(concat!(
                        "restoring non-reset VP state element ",
                        stringify!($ty),
                    )))
                }
            }
        )*
    };
}

impl TdpVpState<'_> {
    fn read_segment(
        &self,
        sel: u32,
        base: u32,
        limit: u32,
        ar: u32,
    ) -> Result<SegmentRegister, TdpError> {
        let vm = self.processor.vm();
        Ok(SegmentRegister {
            base: vm.read_vmcs(base, Width::Bits64)?,
            limit: vm.read_vmcs(limit, Width::Bits32)? as u32,
            selector: vm.read_vmcs(sel, Width::Bits16)? as u16,
            attributes: (vm.read_vmcs(ar, Width::Bits32)? as u16).into(),
        })
    }

    fn write_segment(
        &self,
        sel: u32,
        base: u32,
        limit: u32,
        ar: u32,
        value: &SegmentRegister,
    ) -> Result<(), TdpError> {
        let vm = self.processor.vm();
        vm.write_vmcs(sel, Width::Bits16, value.selector.into())?;
        vm.write_vmcs(base, Width::Bits64, value.base)?;
        vm.write_vmcs(limit, Width::Bits32, value.limit.into())?;
        vm.write_vmcs(ar, Width::Bits32, u16::from(value.attributes).into())?;
        Ok(())
    }
}

impl AccessVpState for TdpVpState<'_> {
    type Error = TdpError;

    fn caps(&self) -> &virt::PartitionCapabilities {
        &self.processor.partition.caps
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn activity(&mut self) -> Result<vp::Activity, Self::Error> {
        Ok(vp::Activity {
            mp_state: self.processor.mp_state,
            ..Default::default()
        })
    }

    fn set_activity(&mut self, value: &vp::Activity) -> Result<(), Self::Error> {
        if value.nmi_pending
            || value.nmi_masked
            || value.interrupt_shadow
            || value.pending_event.is_some()
            || value.pending_interruption.is_some()
        {
            return Err(TdpError::Unsupported("pending activity state"));
        }
        self.processor.mp_state = value.mp_state;
        Ok(())
    }

    fn registers(&mut self) -> Result<vp::Registers, Self::Error> {
        use vmcs::field as f;
        let regs = self.processor.regs;
        let vm = self.processor.vm();
        Ok(vp::Registers {
            rax: regs.rax,
            rcx: regs.rcx,
            rdx: regs.rdx,
            rbx: regs.rbx,
            rsp: regs.rsp,
            rbp: regs.rbp,
            rsi: regs.rsi,
            rdi: regs.rdi,
            r8: regs.r8,
            r9: regs.r9,
            r10: regs.r10,
            r11: regs.r11,
            r12: regs.r12,
            r13: regs.r13,
            r14: regs.r14,
            r15: regs.r15,
            rip: regs.rip,
            rflags: regs.rflags,
            cs: self.read_segment(
                f::GUEST_CS_SEL,
                f::GUEST_CS_BASE,
                f::GUEST_CS_LIMIT,
                f::GUEST_CS_AR,
            )?,
            ds: self.read_segment(
                f::GUEST_DS_SEL,
                f::GUEST_DS_BASE,
                f::GUEST_DS_LIMIT,
                f::GUEST_DS_AR,
            )?,
            es: self.read_segment(
                f::GUEST_ES_SEL,
                f::GUEST_ES_BASE,
                f::GUEST_ES_LIMIT,
                f::GUEST_ES_AR,
            )?,
            fs: self.read_segment(
                f::GUEST_FS_SEL,
                f::GUEST_FS_BASE,
                f::GUEST_FS_LIMIT,
                f::GUEST_FS_AR,
            )?,
            gs: self.read_segment(
                f::GUEST_GS_SEL,
                f::GUEST_GS_BASE,
                f::GUEST_GS_LIMIT,
                f::GUEST_GS_AR,
            )?,
            ss: self.read_segment(
                f::GUEST_SS_SEL,
                f::GUEST_SS_BASE,
                f::GUEST_SS_LIMIT,
                f::GUEST_SS_AR,
            )?,
            tr: self.read_segment(
                f::GUEST_TR_SEL,
                f::GUEST_TR_BASE,
                f::GUEST_TR_LIMIT,
                f::GUEST_TR_AR,
            )?,
            ldtr: self.read_segment(
                f::GUEST_LDTR_SEL,
                f::GUEST_LDTR_BASE,
                f::GUEST_LDTR_LIMIT,
                f::GUEST_LDTR_AR,
            )?,
            gdtr: virt::x86::TableRegister {
                base: vm.read_vmcs(f::GUEST_GDTR_BASE, Width::Bits64)?,
                limit: vm.read_vmcs(f::GUEST_GDTR_LIMIT, Width::Bits32)? as u16,
            },
            idtr: virt::x86::TableRegister {
                base: vm.read_vmcs(f::GUEST_IDTR_BASE, Width::Bits64)?,
                limit: vm.read_vmcs(f::GUEST_IDTR_LIMIT, Width::Bits32)? as u16,
            },
            cr0: vm.read_vmcs(f::GUEST_CR0, Width::Bits64)?,
            // CR2 belongs to the L2 but has no VMCS field; the kernel driver
            // carries it across the L1's execution, so it is not readable here.
            cr2: 0,
            cr3: vm.read_vmcs(f::GUEST_CR3, Width::Bits64)?,
            cr4: vm.read_vmcs(f::GUEST_CR4, Width::Bits64)?,
            cr8: 0,
            efer: vm.read_vmcs(f::GUEST_IA32_EFER, Width::Bits64)?,
        })
    }

    fn set_registers(&mut self, value: &vp::Registers) -> Result<(), Self::Error> {
        use vmcs::field as f;
        {
            let regs = &mut self.processor.regs;
            regs.rax = value.rax;
            regs.rcx = value.rcx;
            regs.rdx = value.rdx;
            regs.rbx = value.rbx;
            regs.rsp = value.rsp;
            regs.rbp = value.rbp;
            regs.rsi = value.rsi;
            regs.rdi = value.rdi;
            regs.r8 = value.r8;
            regs.r9 = value.r9;
            regs.r10 = value.r10;
            regs.r11 = value.r11;
            regs.r12 = value.r12;
            regs.r13 = value.r13;
            regs.r14 = value.r14;
            regs.r15 = value.r15;
            regs.rip = value.rip;
            regs.rflags = value.rflags;
        }
        self.processor.flush_registers();

        self.write_segment(
            f::GUEST_CS_SEL,
            f::GUEST_CS_BASE,
            f::GUEST_CS_LIMIT,
            f::GUEST_CS_AR,
            &value.cs,
        )?;
        self.write_segment(
            f::GUEST_DS_SEL,
            f::GUEST_DS_BASE,
            f::GUEST_DS_LIMIT,
            f::GUEST_DS_AR,
            &value.ds,
        )?;
        self.write_segment(
            f::GUEST_ES_SEL,
            f::GUEST_ES_BASE,
            f::GUEST_ES_LIMIT,
            f::GUEST_ES_AR,
            &value.es,
        )?;
        self.write_segment(
            f::GUEST_FS_SEL,
            f::GUEST_FS_BASE,
            f::GUEST_FS_LIMIT,
            f::GUEST_FS_AR,
            &value.fs,
        )?;
        self.write_segment(
            f::GUEST_GS_SEL,
            f::GUEST_GS_BASE,
            f::GUEST_GS_LIMIT,
            f::GUEST_GS_AR,
            &value.gs,
        )?;
        self.write_segment(
            f::GUEST_SS_SEL,
            f::GUEST_SS_BASE,
            f::GUEST_SS_LIMIT,
            f::GUEST_SS_AR,
            &value.ss,
        )?;
        self.write_segment(
            f::GUEST_TR_SEL,
            f::GUEST_TR_BASE,
            f::GUEST_TR_LIMIT,
            f::GUEST_TR_AR,
            &value.tr,
        )?;
        self.write_segment(
            f::GUEST_LDTR_SEL,
            f::GUEST_LDTR_BASE,
            f::GUEST_LDTR_LIMIT,
            f::GUEST_LDTR_AR,
            &value.ldtr,
        )?;

        let vm = self.processor.vm();
        vm.write_vmcs(f::GUEST_GDTR_BASE, Width::Bits64, value.gdtr.base)?;
        vm.write_vmcs(f::GUEST_GDTR_LIMIT, Width::Bits32, value.gdtr.limit.into())?;
        vm.write_vmcs(f::GUEST_IDTR_BASE, Width::Bits64, value.idtr.base)?;
        vm.write_vmcs(f::GUEST_IDTR_LIMIT, Width::Bits32, value.idtr.limit.into())?;
        vm.write_vmcs(f::GUEST_CR0, Width::Bits64, value.cr0)?;
        vm.write_vmcs(f::CR0_SHADOW, Width::Bits64, value.cr0)?;
        vm.write_vmcs(f::GUEST_CR3, Width::Bits64, value.cr3)?;
        vm.write_vmcs(f::GUEST_CR4, Width::Bits64, value.cr4)?;
        vm.write_vmcs(f::CR4_SHADOW, Width::Bits64, value.cr4)?;
        vm.write_vmcs(f::GUEST_IA32_EFER, Width::Bits64, value.efer)?;

        // Long mode has to be declared in the entry controls as well as in
        // EFER, or entry fails its guest-state consistency checks.
        const ENTRY_CTL_IA32E_MODE: u64 = 1 << 9;
        const EFER_LMA: u64 = 1 << 10;
        let entry = vm.read_vmcs(f::ENTRY_CTLS, Width::Bits32)?;
        let entry = if value.efer & EFER_LMA != 0 {
            entry | ENTRY_CTL_IA32E_MODE
        } else {
            entry & !ENTRY_CTL_IA32E_MODE
        };
        vm.write_vmcs(f::ENTRY_CTLS, Width::Bits32, entry)?;
        Ok(())
    }

    fn apic(&mut self) -> Result<vp::Apic, Self::Error> {
        self.processor
            .with_apic(|apic, client| {
                let (irr, isr) = virt_support_apic::ApicClient::pull_offload(client);
                apic.disable_offload(&irr, &isr);
                let state = apic.save();
                apic.enable_offload();
                state
            })
            .ok_or(TdpError::Unsupported("accessing an unavailable L2 APIC"))
    }

    fn set_apic(&mut self, value: &vp::Apic) -> Result<(), Self::Error> {
        self.processor
            .with_apic(|apic, client| {
                let (irr, isr) = virt_support_apic::ApicClient::pull_offload(client);
                apic.disable_offload(&irr, &isr);
                let result = apic.restore(value);
                apic.enable_offload();
                result
            })
            .ok_or(TdpError::Unsupported("accessing an unavailable L2 APIC"))?
            .map_err(|err| TdpError::Tdx(err.into()))
    }

    unmodelled! {
        xsave, set_xsave, Xsave;
        xcr, set_xcr, Xcr0;
        xss, set_xss, Xss;
        mtrrs, set_mtrrs, Mtrrs;
        pat, set_pat, Pat;
        virtual_msrs, set_virtual_msrs, VirtualMsrs;
        debug_regs, set_debug_regs, DebugRegisters;
        tsc, set_tsc, Tsc;
        cet, set_cet, Cet;
        cet_ss, set_cet_ss, CetSs;
        tsc_aux, set_tsc_aux, TscAux;
        synic_msrs, set_synic_msrs, SyntheticMsrs;
        synic_message_page, set_synic_message_page, SynicMessagePage;
        synic_event_flags_page, set_synic_event_flags_page, SynicEventFlagsPage;
        synic_message_queues, set_synic_message_queues, SynicMessageQueues;
        synic_timers, set_synic_timers, SynicTimers;
        nested_state, set_nested_state, NestedState;
    }
}
