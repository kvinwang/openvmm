// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The guest-side TDX interface an L1 VMM under TD partitioning needs.
//!
//! Eight TDCALL leaves and one field-identifier encoding cover everything:
//! reading TDCS, reading and writing the L2 VMCS, aliasing memory into an L2,
//! entering it, and invalidating translations. The encodings follow Intel's
//! TDX Module ABI and were validated against their reference L1 VMM.

use crate::driver::TdcallArgs;
use crate::driver::TdcallDevice;
use anyhow::Result;

pub const LEAF_VP_INFO: u64 = 1;
pub const LEAF_VM_RD: u64 = 7;
pub const LEAF_VM_WR: u64 = 8;
pub const LEAF_VP_RD: u64 = 9;
pub const LEAF_VP_WR: u64 = 10;
pub const LEAF_MEM_PAGE_ATTR_WR: u64 = 24;
pub const LEAF_VP_ENTER: u64 = 25;
pub const LEAF_VP_INVEPT: u64 = 26;

pub const TDX_SUCCESS: u64 = 0;
pub const TDX_OPERAND_BUSY: u64 = 0x8000_0200_0000_0000;

/// TDCS field holding the `TD_PARAMS.NUM_L2_VMS` the TD was created with.
pub const MD_TDCS_NUM_L2_VMS: u64 = 0x9010_0001_0000_0005;
/// Per-L2 control flags; add the VM id.
pub const MD_TDVPS_L2_CTLS: u64 = 0xA020_0003_0000_0050;

const MD_CONTEXT_VP: u64 = 2;
const MD_CLASS_TDVPS_VMCS_1: u64 = 36;
const MD_CLASS_TDVPS_MSR_BITMAP_1: u64 = 37;

/// GPA attribute bits for `TDG.MEM.PAGE.ATTR.WR`.
pub mod gpa_attr {
    pub const R: u16 = 0x0001;
    pub const W: u16 = 0x0002;
    pub const XS: u16 = 0x0004;
    pub const XU: u16 = 0x0008;
    pub const VALID: u16 = 0x8000;
    pub const RWX: u16 = R | W | XS;
}

fn page_attr_operands(vm_id: u8, attr: u16, mask: u16, invept: bool) -> (u64, u64) {
    let shift = u32::from(vm_id) * 16;
    let attributes = u64::from(attr | gpa_attr::VALID) << shift;
    let flags = u64::from(mask | if invept { 1 << 15 } else { 0 }) << shift;
    (attributes, flags)
}

/// How a `TDG.VP.ENTER` returned.
///
/// Only [`VpEnterResult::Exit`] means the L2 ran and stopped for the L1 to
/// service. The other two mean no instruction retired, so the VMM must resume
/// without advancing RIP — getting this wrong silently skips a guest
/// instruction and the guest misbehaves much later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VpEnterResult {
    /// The L2 exited to the L1.
    Exit {
        reason: u16,
        qualification: u64,
        instruction_length: u32,
    },
    /// The host took the exit; resume. The reason says what the L2 did, which
    /// matters when the host cannot actually service it.
    HostRouted { reason: u16 },
    /// No entry happened, an interrupt was pending in the L1; resume.
    NoEntry,
    /// A TDCALL-level failure rather than an L2 exit.
    Error(u64),
}

const CLASS_MASK: u64 = 0xFFFF_FFFF_0000_0000;
const L2_EXIT_HOST_ROUTED_ASYNC: u64 = 0x0000_1100_0000_0000;
const L2_EXIT_HOST_ROUTED_TDVMCALL: u64 = 0x0000_1101_0000_0000;
const L2_EXIT_PENDING_INTERRUPT: u64 = 0x0000_1102_0000_0000;
const PENDING_INTERRUPT: u64 = 0x0000_1120_0000_0000;

/// The GPR image `TDG.VP.ENTER` consumes and writes back. The order is the
/// architectural GPR encoding, not the pushad order.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct L2GprContext {
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rbx: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rflags: u64,
    pub rip: u64,
    pub ssp: u64,
    pub interrupt_status: u16,
}

/// Field widths, as the metadata encoding expresses them.
#[derive(Clone, Copy)]
pub enum Width {
    Bits16,
    Bits32,
    Bits64,
}

impl Width {
    fn elem_size(self) -> u64 {
        match self {
            Width::Bits16 => 1,
            Width::Bits32 => 2,
            Width::Bits64 => 3,
        }
    }

    fn default_mask(self) -> u64 {
        match self {
            Width::Bits16 => 0xffff,
            Width::Bits32 => 0xffff_ffff,
            Width::Bits64 => !0,
        }
    }
}

fn build_field_id(class: u64, context: u64, write: bool, elem_size: u64, field: u64) -> u64 {
    ((class & 0x3f) << 56)
        | ((context & 0x7) << 52)
        | ((write as u64) << 51)
        | ((elem_size & 0x3) << 32)
        | (field & 0xffff_ffff)
}

/// The write mask the TDX module requires for a given L2 VMCS field.
///
/// The module rejects a write whose mask covers a bit the L1 does not own, so
/// the mask belongs to the field rather than to the caller. Fields absent from
/// this table are fully writable at their natural width.
fn vmcs_write_mask(field: u32, width: Width) -> u64 {
    match field {
        0x201a => 0x80,                  // EPT pointer
        0x2012 => 0xFFFF_FFFF_FFFF_F000, // virtual-APIC page address
        0x2040 => 0xFFFF_FFFF_F018,      // HLAT pointer
        0x2802 => 0xFFC1,                // guest IA32_DEBUGCTL
        0x2806 => 0x501,                 // guest IA32_EFER
        0x4002 => 0x48F9_9A04,           // primary processor-based controls
        0x4004 => 0xFFFF_FFFF_FFFB_FFFF, // exception bitmap
        0x4012 => 0x200,                 // VM-entry controls
        0x401e => 0xC51_3F0C,            // secondary processor-based controls
        0x2034 => 0xE,                   // tertiary processor-based controls
        0x6800 => 0x8005_001F,           // guest CR0
        0x6804 => 0x3FF_1FBF,            // guest CR4
        _ => width.default_mask(),
    }
}

/// An L2 VM, addressed through the TDVPS of the L1 vCPU this runs on.
///
/// Every `TDG.VP.*` leaf acts on the issuing vCPU's TDVPS, so an instance is
/// bound to one L1 vCPU: the thread that programs the VMCS must be the thread
/// that enters, and it must not migrate.
pub struct L2Vm<'a> {
    dev: &'a TdcallDevice,
    vm_id: u8,
}

impl<'a> L2Vm<'a> {
    pub fn new(dev: &'a TdcallDevice, vm_id: u8) -> Self {
        Self { dev, vm_id }
    }

    pub fn vm_id(&self) -> u8 {
        self.vm_id
    }

    /// How many L2 VMs this TD was created with. Zero means the TD is not an
    /// L1 VMM and nothing else here will work.
    pub fn num_l2_vms(dev: &TdcallDevice) -> Result<u64> {
        let mut args = TdcallArgs {
            rax: LEAF_VM_RD,
            rdx: MD_TDCS_NUM_L2_VMS,
            ..Default::default()
        };
        let status = dev.tdcall(&mut args)?;
        anyhow::ensure!(
            status == TDX_SUCCESS,
            "TDG.VM.RD(NUM_L2_VMS) failed: {status:#018x}"
        );
        Ok(args.r8)
    }

    pub fn read_vmcs(&self, field: u32, width: Width) -> Result<u64> {
        let id = build_field_id(
            MD_CLASS_TDVPS_VMCS_1 + u64::from(self.vm_id - 1) * 8,
            MD_CONTEXT_VP,
            false,
            width.elem_size(),
            field.into(),
        );
        let mut args = TdcallArgs {
            rax: LEAF_VP_RD,
            rdx: id,
            ..Default::default()
        };
        let status = self.dev.tdcall(&mut args)?;
        anyhow::ensure!(
            status == TDX_SUCCESS,
            "TDG.VP.RD of VMCS field {field:#06x} failed: {status:#018x}"
        );
        Ok(args.r8)
    }

    pub fn write_vmcs(&self, field: u32, width: Width, value: u64) -> Result<()> {
        let id = build_field_id(
            MD_CLASS_TDVPS_VMCS_1 + u64::from(self.vm_id - 1) * 8,
            MD_CONTEXT_VP,
            true,
            width.elem_size(),
            field.into(),
        );
        let mut args = TdcallArgs {
            rax: LEAF_VP_WR,
            rdx: id,
            r8: value,
            r9: vmcs_write_mask(field, width),
            ..Default::default()
        };
        let status = self.dev.tdcall(&mut args)?;
        anyhow::ensure!(
            status == TDX_SUCCESS,
            "TDG.VP.WR of VMCS field {field:#06x} <- {value:#x} failed: {status:#018x}"
        );
        Ok(())
    }

    /// Write a TDVPS field that is not part of the VMCS, such as `L2_CTLS`.
    pub fn write_tdvps(&self, field_id: u64, value: u64, mask: u64) -> Result<()> {
        let mut args = TdcallArgs {
            rax: LEAF_VP_WR,
            rdx: field_id,
            r8: value,
            r9: mask,
            ..Default::default()
        };
        let status = self.dev.tdcall(&mut args)?;
        anyhow::ensure!(
            status == TDX_SUCCESS,
            "TDG.VP.WR of {field_id:#x} failed: {status:#018x}"
        );
        Ok(())
    }

    /// Alias one 4 KiB page of this TD's private memory into the L2.
    ///
    /// The module can return without creating the alias — it needs the host to
    /// supply an L2 Secure EPT page first — and signals that by returning
    /// attributes other than the ones requested, so the call is retried. The
    /// bound matters: an unbounded loop would wedge the vCPU if the host never
    /// cooperated.
    ///
    /// `level` selects the mapping size: 0 for 4 KiB, 1 for 2 MiB, 2 for
    /// 1 GiB. A large level costs one call instead of 512 or 262144, but the
    /// module only accepts it if it already has a mapping of that size.
    pub fn add_page_alias_level(&self, gpa: u64, level: u64, attr: u16) -> Result<u32> {
        let (want, flags) = page_attr_operands(self.vm_id, attr, attr, false);

        for tries in 1..=4096 {
            let mut args = TdcallArgs {
                rax: LEAF_MEM_PAGE_ATTR_WR,
                rcx: gpa | level,
                rdx: want,
                r8: flags,
                ..Default::default()
            };
            let status = self.dev.tdcall(&mut args)?;
            if status == TDX_OPERAND_BUSY {
                continue;
            }
            anyhow::ensure!(
                status == TDX_SUCCESS,
                "TDG.MEM.PAGE.ATTR.WR for gpa {gpa:#x} failed: {status:#018x}"
            );
            if args.rdx == want {
                return Ok(tries);
            }
        }
        anyhow::bail!("TDG.MEM.PAGE.ATTR.WR for gpa {gpa:#x} never took effect")
    }

    /// Alias one 4 KiB page.
    pub fn add_page_alias(&self, gpa: u64, attr: u16) -> Result<u32> {
        self.add_page_alias_level(gpa, 0, attr)
    }

    /// Remove this L2's access to one 4 KiB page.
    ///
    /// Revocation is deliberately different from granting access: the
    /// module must invalidate the L2 translation while narrowing the page's
    /// attributes, otherwise a cached R+W+X translation can survive after
    /// the page has gone back to the L1.
    pub fn drop_page_alias(&self, gpa: u64) -> Result<()> {
        let (want, flags) = page_attr_operands(self.vm_id, 0, gpa_attr::RWX, true);
        let mut args = TdcallArgs {
            rax: LEAF_MEM_PAGE_ATTR_WR,
            rcx: gpa,
            rdx: want,
            // Bits 0..14 are the attribute mask and bit 15 requests INVEPT.
            r8: flags,
            ..Default::default()
        };
        let status = self.dev.tdcall(&mut args)?;
        anyhow::ensure!(
            status == TDX_SUCCESS,
            "TDG.MEM.PAGE.ATTR.WR revoking gpa {gpa:#x} failed: {status:#018x}"
        );
        anyhow::ensure!(
            args.rdx == want,
            "TDG.MEM.PAGE.ATTR.WR did not revoke gpa {gpa:#x}: got attributes {:#018x}",
            args.rdx
        );
        Ok(())
    }

    /// Read one 64-bit word of the L2's MSR intercept bitmap.
    ///
    /// Used to check the field encoding before writing it: a wrong class or
    /// index would corrupt some other part of the TDVPS, and the symptom of
    /// that is the L1 itself hanging with nothing to inspect.
    pub fn read_msr_bitmap(&self, qword: u64) -> Result<u64> {
        let class = MD_CLASS_TDVPS_MSR_BITMAP_1 + u64::from(self.vm_id - 1) * 8;
        let field_id = build_field_id(class, MD_CONTEXT_VP, false, 3, qword);
        let mut args = TdcallArgs {
            rax: LEAF_VP_RD,
            rdx: field_id,
            ..Default::default()
        };
        let status = self.dev.tdcall(&mut args)?;
        anyhow::ensure!(
            status == TDX_SUCCESS,
            "reading MSR bitmap word {qword} failed: {status:#018x}"
        );
        Ok(args.r8)
    }

    /// Stop intercepting an MSR, letting the hardware serve the L2 directly.
    ///
    /// Emulating an MSR means inventing a value, and for the ones that hold
    /// per-CPU state — `KERNEL_GS_BASE` above all — an invented value is
    /// indistinguishable from memory corruption a long way from the cause. The
    /// TDX module context-switches these for an L2 already; the L1's only job
    /// is to get out of the way, which is what the L2's MSR bitmap is for.
    pub fn passthrough_msr(&self, msr: u32) -> Result<()> {
        // The bitmap is four 1 KiB regions: read intercepts for the low and
        // high MSR ranges, then write intercepts for the same two.
        let index = if msr < 0x2000 {
            msr as u64
        } else if (0xc000_0000..0xc000_2000).contains(&msr) {
            128 * 64 + u64::from(msr - 0xc000_0000)
        } else {
            anyhow::bail!("msr {msr:#x} is outside the bitmap")
        };

        let class = MD_CLASS_TDVPS_MSR_BITMAP_1 + u64::from(self.vm_id - 1) * 8;
        for write_region in [0, 256] {
            let qword = write_region + index / 64;
            let field_id = build_field_id(class, MD_CONTEXT_VP, true, 3, qword);
            let mut args = TdcallArgs {
                rax: LEAF_VP_WR,
                rdx: field_id,
                r8: 0,
                r9: 1 << (index % 64),
                ..Default::default()
            };
            let status = self.dev.tdcall(&mut args)?;
            anyhow::ensure!(
                status == TDX_SUCCESS,
                "clearing the intercept for msr {msr:#x} failed: {status:#018x}"
            );
        }
        Ok(())
    }

    /// Invalidate the L2's cached translations.
    pub fn invept(&self) -> Result<()> {
        let mut args = TdcallArgs {
            rax: LEAF_VP_INVEPT,
            rcx: 1 << self.vm_id,
            ..Default::default()
        };
        self.dev.tdcall(&mut args)?;
        Ok(())
    }

    /// Run the L2 until it exits. `context_gpa` is the GPR image the module
    /// loads on entry and writes back on exit.
    pub fn enter(&self, context_gpa: u64) -> Result<VpEnterResult> {
        let mut args = TdcallArgs {
            rax: LEAF_VP_ENTER,
            rcx: u64::from(self.vm_id) << 52,
            rdx: context_gpa,
            ..Default::default()
        };
        // Interruptible, so a device raising an interrupt on another thread
        // can end an entry that would otherwise sleep through it.
        let Some(status) = self.dev.tdcall_interruptible(&mut args)? else {
            return Ok(VpEnterResult::NoEntry);
        };

        Ok(match status & CLASS_MASK {
            PENDING_INTERRUPT => VpEnterResult::NoEntry,
            L2_EXIT_HOST_ROUTED_ASYNC | L2_EXIT_HOST_ROUTED_TDVMCALL => VpEnterResult::HostRouted {
                reason: status as u16,
            },
            TDX_SUCCESS | L2_EXIT_PENDING_INTERRUPT => VpEnterResult::Exit {
                reason: status as u16,
                qualification: args.rcx,
                instruction_length: (args.r11 >> 32) as u32,
            },
            _ => VpEnterResult::Error(status),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_sets_attributes_without_invalidation() {
        let (attributes, flags) = page_attr_operands(1, gpa_attr::RWX, gpa_attr::RWX, false);
        assert_eq!(attributes, 0x8007_0000);
        assert_eq!(flags, 0x0007_0000);
    }

    #[test]
    fn revoke_clears_permissions_and_requests_invalidation() {
        let (attributes, flags) = page_attr_operands(1, 0, gpa_attr::RWX, true);
        assert_eq!(attributes, 0x8000_0000);
        assert_eq!(flags, 0x8007_0000);
    }
}

/// Pin the calling thread to one CPU.
///
/// An L2 vCPU lives in the TDVPS of a specific L1 vCPU. If the VMM thread
/// migrates between programming the VMCS and entering, it enters a VMCS still
/// at its power-on defaults.
pub fn pin_to_cpu(cpu: usize) -> Result<()> {
    // SAFETY: writing a zeroed cpu_set_t and setting one bit in it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        anyhow::ensure!(
            libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) == 0,
            "pinning to cpu {cpu} failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}
