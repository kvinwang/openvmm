// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Initial L2 VMCS state for a 64-bit guest.
//!
//! Most of an L2's VMCS is not the L1 VMM's to choose. The TDX module owns
//! EPT, VPID, unrestricted guest, the MSR bitmaps and the pin-based controls,
//! and it forces on APIC virtualization; attempts to clear those bits fail the
//! write with "field value not valid" even where the documented write mask
//! covers them. What is left to the L1 is the guest state and a handful of
//! controls, which is what this module programs.

use crate::tdx::L2Vm;
use crate::tdx::Width;
use anyhow::Result;

// Field encodings, from the architectural VMCS layout.
pub mod field {
    pub const GUEST_ES_SEL: u32 = 0x0800;
    pub const GUEST_CS_SEL: u32 = 0x0802;
    pub const GUEST_SS_SEL: u32 = 0x0804;
    pub const GUEST_DS_SEL: u32 = 0x0806;
    pub const GUEST_FS_SEL: u32 = 0x0808;
    pub const GUEST_GS_SEL: u32 = 0x080a;
    pub const GUEST_LDTR_SEL: u32 = 0x080c;
    pub const GUEST_TR_SEL: u32 = 0x080e;
    pub const GUEST_INTERRUPT_STATUS: u32 = 0x0810;

    pub const VIRTUAL_APIC_PAGE: u32 = 0x2012;
    pub const EOI_EXIT_BITMAP_0: u32 = 0x201c;
    pub const EOI_EXIT_BITMAP_1: u32 = 0x201e;
    pub const EOI_EXIT_BITMAP_2: u32 = 0x2020;
    pub const EOI_EXIT_BITMAP_3: u32 = 0x2022;
    pub const GUEST_PHYSICAL_ADDRESS: u32 = 0x2400;
    pub const GUEST_IA32_DEBUGCTL: u32 = 0x2802;
    pub const GUEST_IA32_PAT: u32 = 0x2804;
    pub const GUEST_IA32_EFER: u32 = 0x2806;

    /// Pin-based controls, which the TDX module owns for an L2.
    pub const PIN_BASED_CTLS: u32 = 0x4000;
    pub const PROC_EXEC_CTLS: u32 = 0x4002;
    pub const EXCEPTION_BITMAP: u32 = 0x4004;
    pub const PF_EC_MASK: u32 = 0x4006;
    pub const PF_EC_MATCH: u32 = 0x4008;
    pub const CR3_TARGET_COUNT: u32 = 0x400a;
    pub const ENTRY_CTLS: u32 = 0x4012;
    pub const ENTRY_INTR_INFO: u32 = 0x4016;
    pub const ENTRY_EXCEPTION_EC: u32 = 0x4018;
    pub const ENTRY_INSTR_LEN: u32 = 0x401a;
    pub const TPR_THRESHOLD: u32 = 0x401c;
    pub const PROC_EXEC_CTLS2: u32 = 0x401e;
    pub const VM_INSTRUCTION_ERROR: u32 = 0x4400;
    pub const EXIT_REASON: u32 = 0x4402;
    /// What the exception was, when the exception bitmap caused the exit.
    pub const EXIT_INTR_INFO: u32 = 0x4404;
    pub const EXIT_INTR_ERROR_CODE: u32 = 0x4406;
    pub const IDT_VECTORING_INFO: u32 = 0x4408;
    pub const EXIT_INSTRUCTION_LEN: u32 = 0x440c;
    pub const EXIT_QUALIFICATION: u32 = 0x6400;

    pub const GUEST_ES_LIMIT: u32 = 0x4800;
    pub const GUEST_CS_LIMIT: u32 = 0x4802;
    pub const GUEST_SS_LIMIT: u32 = 0x4804;
    pub const GUEST_DS_LIMIT: u32 = 0x4806;
    pub const GUEST_FS_LIMIT: u32 = 0x4808;
    pub const GUEST_GS_LIMIT: u32 = 0x480a;
    pub const GUEST_LDTR_LIMIT: u32 = 0x480c;
    pub const GUEST_TR_LIMIT: u32 = 0x480e;
    pub const GUEST_GDTR_LIMIT: u32 = 0x4810;
    pub const GUEST_IDTR_LIMIT: u32 = 0x4812;
    pub const GUEST_ES_AR: u32 = 0x4814;
    pub const GUEST_CS_AR: u32 = 0x4816;
    pub const GUEST_SS_AR: u32 = 0x4818;
    pub const GUEST_DS_AR: u32 = 0x481a;
    pub const GUEST_FS_AR: u32 = 0x481c;
    pub const GUEST_GS_AR: u32 = 0x481e;
    pub const GUEST_LDTR_AR: u32 = 0x4820;
    pub const GUEST_TR_AR: u32 = 0x4822;
    pub const GUEST_INTERRUPTIBILITY: u32 = 0x4824;
    pub const GUEST_SYSENTER_CS: u32 = 0x482a;

    pub const CR0_MASK: u32 = 0x6000;
    pub const CR4_MASK: u32 = 0x6002;
    pub const CR0_SHADOW: u32 = 0x6004;
    pub const CR4_SHADOW: u32 = 0x6006;

    pub const GUEST_CR0: u32 = 0x6800;
    pub const GUEST_CR3: u32 = 0x6802;
    pub const GUEST_CR4: u32 = 0x6804;
    pub const GUEST_ES_BASE: u32 = 0x6806;
    pub const GUEST_CS_BASE: u32 = 0x6808;
    pub const GUEST_SS_BASE: u32 = 0x680a;
    pub const GUEST_DS_BASE: u32 = 0x680c;
    pub const GUEST_FS_BASE: u32 = 0x680e;
    pub const GUEST_GS_BASE: u32 = 0x6810;
    pub const GUEST_LDTR_BASE: u32 = 0x6812;
    pub const GUEST_TR_BASE: u32 = 0x6814;
    pub const GUEST_GDTR_BASE: u32 = 0x6816;
    pub const GUEST_IDTR_BASE: u32 = 0x6818;
    pub const GUEST_DR7: u32 = 0x681a;
    pub const GUEST_RSP: u32 = 0x681c;
    pub const GUEST_PENDING_DBG: u32 = 0x6822;
    pub const GUEST_SYSENTER_ESP: u32 = 0x6824;
    pub const GUEST_SYSENTER_EIP: u32 = 0x6826;
}

/// Long-mode control register state: PG | NE | ET | PE.
pub const CR0_LONG_MODE: u64 = 0x8000_0031;
/// PAE, which long mode requires.
pub const CR4_LONG_MODE: u64 = 0x20;
/// LMA | LME.
pub const EFER_LONG_MODE: u64 = 0x500;
const ENTRY_CTL_IA32E_MODE: u64 = 1 << 9;

const CS_AR_LONG: u64 = 0xa09b; // present, code, execute/read, L=1, G=1
const DS_AR_LONG: u64 = 0xc093; // present, data, read/write, D/B=1, G=1
const TR_AR: u64 = 0x8b;
const LDTR_AR_UNUSABLE: u64 = 0x10000;
const PAT_POWER_ON: u64 = 0x0007_0406_0007_0406;
const DR7_INIT: u64 = 0x400;

/// Where the guest starts and what it starts with.
pub struct GuestState {
    pub cr3: u64,
    pub rip: u64,
    pub rsp: u64,
    /// Page holding the virtual APIC. The module leaves "use TPR shadow" set
    /// in the L2's controls, and VM entry then requires this to be valid even
    /// for a guest that never touches the APIC.
    pub virtual_apic_gpa: u64,
}

fn set_segment(
    vm: &L2Vm<'_>,
    sel_f: u32,
    base_f: u32,
    limit_f: u32,
    ar_f: u32,
    sel: u64,
    base: u64,
    limit: u64,
    ar: u64,
) -> Result<()> {
    vm.write_vmcs(sel_f, Width::Bits16, sel)?;
    vm.write_vmcs(base_f, Width::Bits64, base)?;
    vm.write_vmcs(limit_f, Width::Bits32, limit)?;
    vm.write_vmcs(ar_f, Width::Bits32, ar)?;
    Ok(())
}

/// Program an L2 VMCS for a flat 64-bit guest.
pub fn configure_64bit(vm: &L2Vm<'_>, state: &GuestState) -> Result<()> {
    use field as f;

    // Read the module's control defaults and write them back unchanged. The
    // L1 cannot pick these, and writing them proves the masks are right.
    let proc_ctls = vm.read_vmcs(f::PROC_EXEC_CTLS, Width::Bits32)?;
    vm.write_vmcs(f::PROC_EXEC_CTLS, Width::Bits32, proc_ctls)?;
    let proc_ctls2 = vm.read_vmcs(f::PROC_EXEC_CTLS2, Width::Bits32)?;
    vm.write_vmcs(f::PROC_EXEC_CTLS2, Width::Bits32, proc_ctls2)?;

    vm.write_vmcs(f::VIRTUAL_APIC_PAGE, Width::Bits64, state.virtual_apic_gpa)?;
    vm.write_vmcs(f::TPR_THRESHOLD, Width::Bits32, 0)?;
    // Intercepting every exception is a diagnostic, not a design: an
    // exception the guest cannot handle otherwise loops forever with no exit
    // and no console output, which is indistinguishable from a hang.
    let bitmap = if std::env::var_os("TDP_TRAP_EXCEPTIONS").is_some() {
        0xfffb_ffff
    } else {
        0
    };
    vm.write_vmcs(f::EXCEPTION_BITMAP, Width::Bits32, bitmap)?;
    vm.write_vmcs(f::PF_EC_MASK, Width::Bits32, 0)?;
    vm.write_vmcs(f::PF_EC_MATCH, Width::Bits32, 0)?;
    vm.write_vmcs(f::CR3_TARGET_COUNT, Width::Bits32, 0)?;

    // The guest owns its control registers; nothing here shadows them.
    vm.write_vmcs(f::CR0_MASK, Width::Bits64, 0)?;
    vm.write_vmcs(f::CR4_MASK, Width::Bits64, 0)?;
    vm.write_vmcs(f::CR0_SHADOW, Width::Bits64, CR0_LONG_MODE)?;
    vm.write_vmcs(f::CR4_SHADOW, Width::Bits64, CR4_LONG_MODE)?;

    // IA32E_MODE is the one entry control an L1 may set, and it has to agree
    // with CR0.PG and EFER.LMA or entry fails its guest-state checks.
    let entry_ctls = vm.read_vmcs(f::ENTRY_CTLS, Width::Bits32)?;
    vm.write_vmcs(
        f::ENTRY_CTLS,
        Width::Bits32,
        entry_ctls | ENTRY_CTL_IA32E_MODE,
    )?;
    vm.write_vmcs(f::ENTRY_INTR_INFO, Width::Bits32, 0)?;
    vm.write_vmcs(f::ENTRY_EXCEPTION_EC, Width::Bits32, 0)?;
    vm.write_vmcs(f::ENTRY_INSTR_LEN, Width::Bits32, 0)?;

    vm.write_vmcs(f::GUEST_CR0, Width::Bits64, CR0_LONG_MODE)?;
    vm.write_vmcs(f::GUEST_CR4, Width::Bits64, CR4_LONG_MODE)?;
    vm.write_vmcs(f::GUEST_CR3, Width::Bits64, state.cr3)?;
    vm.write_vmcs(f::GUEST_IA32_EFER, Width::Bits64, EFER_LONG_MODE)?;

    // Flat segments. Long mode ignores most bases, but VM entry still checks
    // the access-rights bytes, so they must be architecturally sane.
    set_segment(
        vm,
        f::GUEST_CS_SEL,
        f::GUEST_CS_BASE,
        f::GUEST_CS_LIMIT,
        f::GUEST_CS_AR,
        0x10,
        0,
        0xffff_ffff,
        CS_AR_LONG,
    )?;
    for (sel_f, base_f, limit_f, ar_f) in [
        (
            f::GUEST_DS_SEL,
            f::GUEST_DS_BASE,
            f::GUEST_DS_LIMIT,
            f::GUEST_DS_AR,
        ),
        (
            f::GUEST_ES_SEL,
            f::GUEST_ES_BASE,
            f::GUEST_ES_LIMIT,
            f::GUEST_ES_AR,
        ),
        (
            f::GUEST_SS_SEL,
            f::GUEST_SS_BASE,
            f::GUEST_SS_LIMIT,
            f::GUEST_SS_AR,
        ),
        (
            f::GUEST_FS_SEL,
            f::GUEST_FS_BASE,
            f::GUEST_FS_LIMIT,
            f::GUEST_FS_AR,
        ),
        (
            f::GUEST_GS_SEL,
            f::GUEST_GS_BASE,
            f::GUEST_GS_LIMIT,
            f::GUEST_GS_AR,
        ),
    ] {
        set_segment(
            vm,
            sel_f,
            base_f,
            limit_f,
            ar_f,
            0x18,
            0,
            0xffff_ffff,
            DS_AR_LONG,
        )?;
    }
    set_segment(
        vm,
        f::GUEST_TR_SEL,
        f::GUEST_TR_BASE,
        f::GUEST_TR_LIMIT,
        f::GUEST_TR_AR,
        0x20,
        0,
        0xffff,
        TR_AR,
    )?;
    set_segment(
        vm,
        f::GUEST_LDTR_SEL,
        f::GUEST_LDTR_BASE,
        f::GUEST_LDTR_LIMIT,
        f::GUEST_LDTR_AR,
        0,
        0,
        0xffff,
        LDTR_AR_UNUSABLE,
    )?;

    vm.write_vmcs(f::GUEST_GDTR_BASE, Width::Bits64, 0)?;
    vm.write_vmcs(f::GUEST_GDTR_LIMIT, Width::Bits32, 0xffff)?;
    vm.write_vmcs(f::GUEST_IDTR_BASE, Width::Bits64, 0)?;
    vm.write_vmcs(f::GUEST_IDTR_LIMIT, Width::Bits32, 0xffff)?;

    vm.write_vmcs(f::GUEST_SYSENTER_CS, Width::Bits32, 0)?;
    vm.write_vmcs(f::GUEST_SYSENTER_ESP, Width::Bits64, 0)?;
    vm.write_vmcs(f::GUEST_SYSENTER_EIP, Width::Bits64, 0)?;
    vm.write_vmcs(f::GUEST_PENDING_DBG, Width::Bits64, 0)?;
    vm.write_vmcs(f::GUEST_IA32_DEBUGCTL, Width::Bits64, 0)?;
    vm.write_vmcs(f::GUEST_INTERRUPTIBILITY, Width::Bits32, 0)?;
    vm.write_vmcs(f::GUEST_IA32_PAT, Width::Bits64, PAT_POWER_ON)?;
    vm.write_vmcs(f::GUEST_DR7, Width::Bits64, DR7_INIT)?;
    vm.write_vmcs(f::GUEST_RSP, Width::Bits64, state.rsp)?;

    Ok(())
}

/// Turn off the optional L2 features this backend does not offer.
///
/// `L2_CTLS` selects shared memory, TDVMCALLs from the L2 and extended
/// virtualization exceptions. A guest here has none of them: it is told it is
/// bare metal, and every TDCALL it might make would exit to the VMM anyway.
/// Clearing them explicitly beats inheriting whatever the module left behind.
pub fn disable_optional_l2_features(vm: &L2Vm<'_>) -> anyhow::Result<()> {
    vm.write_tdvps(crate::tdx::MD_TDVPS_L2_CTLS + u64::from(vm.vm_id()), 0, 0x7)
}
