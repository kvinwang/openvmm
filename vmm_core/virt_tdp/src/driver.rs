// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bindings for dstack's restricted `/dev/dstack_tdcall` device, used by the
//! OpenVMM TD Partitioning backend.
//!
//! TDCALL is a ring-0 instruction and the kernel exports no helper for it, so
//! dstack ships a small out-of-tree driver. This module is the whole kernel
//! interface: restricted VM ownership and TDCALL operations, memory
//! allocation or registration, and mmap access to driver-owned memory.

use anyhow::Context;
use anyhow::Result;
use std::fs::File;
use std::fs::OpenOptions;
use std::os::fd::AsRawFd;

/// The register set the TDX module reads and writes. Every field is in-out:
/// results, including the L2 exit information of `TDG.VP.ENTER`, come back in
/// the same registers.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct TdcallArgs {
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
}

#[repr(C)]
#[derive(Default)]
struct TdAlloc {
    size: u64,
    flags: u64,
    gpa: u64,
    offset: u64,
}

#[repr(C)]
#[derive(Default)]
struct TdVmClaim {
    vm_id: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Default)]
struct TdRegisterMemory {
    address: u64,
    size: u64,
    flags: u64,
    gpa: u64,
}

#[repr(C)]
#[derive(Default)]
struct TdApi {
    abi_version: u32,
    struct_size: u32,
    features: u64,
    reserved: [u64; 2],
}

#[repr(C)]
#[derive(Default)]
struct TdVpEnter {
    vp_index: u32,
    flags: u32,
    args: TdcallArgs,
}

#[repr(C)]
#[derive(Default)]
struct TdVpKick {
    vp_index: u32,
    flags: u32,
}

/// Keep the allocation below 4 GiB. Needed only for a guest that starts
/// outside 64-bit mode, where segment bases must fit in 32 bits.
pub const ALLOC_BELOW_4G: u64 = 1 << 0;
/// Claim the administrator-configured boot-reserved physical range.
pub const ALLOC_RESERVED: u64 = 1 << 1;

const IOC_MAGIC: u8 = b'T';

// _IOWR(magic, nr, size): direction 3, size in bits 16..30.
const fn iowr(nr: u8, size: usize) -> libc::c_ulong {
    (3 << 30)
        | ((size as libc::c_ulong) << 16)
        | ((IOC_MAGIC as libc::c_ulong) << 8)
        | nr as libc::c_ulong
}

// _IOW(magic, nr, size): direction 1, size in bits 16..30.
const fn iow(nr: u8, size: usize) -> libc::c_ulong {
    (1 << 30)
        | ((size as libc::c_ulong) << 16)
        | ((IOC_MAGIC as libc::c_ulong) << 8)
        | nr as libc::c_ulong
}

const fn ior(nr: u8, size: usize) -> libc::c_ulong {
    (2 << 30)
        | ((size as libc::c_ulong) << 16)
        | ((IOC_MAGIC as libc::c_ulong) << 8)
        | nr as libc::c_ulong
}

const IOCTL_EXEC: libc::c_ulong = iowr(1, size_of::<TdcallArgs>());
const IOCTL_ALLOC: libc::c_ulong = iowr(2, size_of::<TdAlloc>());
const IOCTL_CLAIM_VM: libc::c_ulong = iow(3, size_of::<TdVmClaim>());
const IOCTL_REGISTER_MEMORY: libc::c_ulong = iowr(4, size_of::<TdRegisterMemory>());
const IOCTL_GET_API: libc::c_ulong = ior(5, size_of::<TdApi>());
const IOCTL_VP_ENTER: libc::c_ulong = iowr(6, size_of::<TdVpEnter>());
const IOCTL_KICK_VP: libc::c_ulong = iow(7, size_of::<TdVpKick>());
const ABI_VERSION: u32 = 1;
const FEATURE_VP_ENTER: u64 = 1 << 0;
const FEATURE_VP_KICK: u64 = 1 << 1;
const REGISTER_QUERY_HUGE_1G: u64 = 1 << 0;

/// Private memory shared between this process and an L2.
pub struct TdMemory {
    ptr: *mut u8,
    len: usize,
    gpa: u64,
}

// The mapping is owned exclusively by this struct and the kernel keeps the
// pages alive for the lifetime of the file descriptor.
unsafe impl Send for TdMemory {}
// SAFETY: mutating the mapping requires &mut, as for any slice.
unsafe impl Sync for TdMemory {}

impl TdMemory {
    /// Guest physical address of the first byte. Inside a TD this is also the
    /// address an L2 will see, because aliasing publishes the L1's own GPA.
    pub fn gpa(&self) -> u64 {
        self.gpa
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: the mapping covers `len` bytes and lives as long as `self`.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` excludes other references.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    /// Write at an offset from the start of the region.
    pub fn write_at(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        let end = offset.checked_add(data.len()).context("offset overflow")?;
        anyhow::ensure!(
            end <= self.len,
            "write of {} bytes at {offset:#x} exceeds the {:#x}-byte region",
            data.len(),
            self.len
        );
        self.as_mut_slice()[offset..end].copy_from_slice(data);
        Ok(())
    }
}

impl Drop for TdMemory {
    fn drop(&mut self) {
        // SAFETY: unmapping a mapping this struct owns.
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

/// An open handle to the TDCALL device.
pub struct TdcallDevice {
    file: File,
}

impl TdcallDevice {
    pub fn open() -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/dstack_tdcall")
            .context("opening /dev/dstack_tdcall; is the dstack_tdcall module loaded?")?;
        let device = Self { file };
        device.check_api()?;
        Ok(device)
    }

    fn check_api(&self) -> Result<()> {
        let mut api = TdApi::default();
        // SAFETY: the ioctl writes exactly one TdApi.
        let rc = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                IOCTL_GET_API,
                std::ptr::from_mut(&mut api),
            )
        };
        anyhow::ensure!(
            rc == 0,
            "querying dstack_tdcall API failed: {}",
            std::io::Error::last_os_error()
        );
        anyhow::ensure!(
            api.abi_version == ABI_VERSION && api.struct_size as usize >= size_of::<TdApi>(),
            "unsupported dstack_tdcall ABI {} (expected {ABI_VERSION})",
            api.abi_version
        );
        let required = FEATURE_VP_ENTER | FEATURE_VP_KICK;
        anyhow::ensure!(
            api.features & required == required,
            "dstack_tdcall lacks required VP enter/kick capabilities ({:#x})",
            api.features
        );
        Ok(())
    }

    /// Exclusively claim an L2 VM slot for this descriptor.
    ///
    /// The restricted kernel interface rejects every VP, entry, memory
    /// attribute, and invalidation operation that does not address this slot.
    /// Closing the descriptor releases it only after kernel-owned alias
    /// cleanup has completed.
    pub fn claim_vm(&self, vm_id: u8) -> Result<()> {
        let mut claim = TdVmClaim {
            vm_id: vm_id.into(),
            ..Default::default()
        };
        // SAFETY: the ioctl reads exactly one initialized TdVmClaim.
        let rc = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                IOCTL_CLAIM_VM,
                std::ptr::from_mut(&mut claim),
            )
        };
        anyhow::ensure!(
            rc == 0,
            "claiming L2 VM slot {vm_id} failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }

    /// Execute one TDCALL. The returned status is `args.rax`; TDCALL leaves
    /// report failure there rather than through errno, so this only fails when
    /// the ioctl itself does.
    pub fn tdcall(&self, args: &mut TdcallArgs) -> Result<u64> {
        // SAFETY: the ioctl reads and writes exactly one TdcallArgs.
        let rc =
            unsafe { libc::ioctl(self.file.as_raw_fd(), IOCTL_EXEC, std::ptr::from_mut(args)) };
        anyhow::ensure!(
            rc == 0,
            "TDCALL ioctl for leaf {} failed: {}",
            args.rax,
            std::io::Error::last_os_error()
        );
        Ok(args.rax)
    }

    /// Execute one TDCALL, letting a signal cut it short.
    ///
    /// An entry is a blocking TDCALL, and a device raising an interrupt on
    /// another thread has to be able to end it: the entry state is read on
    /// the way in, so an interrupt published after that is invisible until
    /// the next one. Returns `None` when a signal got there first — the
    /// caller goes back around its loop, picks the interrupt up, and enters
    /// again.
    pub fn vp_enter(&self, vp_index: u32, args: &mut TdcallArgs) -> Result<Option<u64>> {
        let mut enter = TdVpEnter {
            vp_index,
            args: *args,
            ..Default::default()
        };
        // SAFETY: the ioctl reads and writes exactly one TdVpEnter.
        let rc = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                IOCTL_VP_ENTER,
                std::ptr::from_mut(&mut enter),
            )
        };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                return Ok(None);
            }
            anyhow::bail!("TDCALL ioctl for leaf {} failed: {err}", args.rax);
        }
        *args = enter.args;
        Ok(Some(args.rax))
    }

    /// Safely force a concurrent VP entry to return through a kernel IPI.
    pub fn kick_vp(&self, vp_index: u32) -> Result<()> {
        let mut kick = TdVpKick {
            vp_index,
            ..Default::default()
        };
        // SAFETY: the ioctl reads exactly one TdVpKick.
        let rc = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                IOCTL_KICK_VP,
                std::ptr::from_mut(&mut kick),
            )
        };
        anyhow::ensure!(
            rc == 0,
            "kicking L2 VP {vp_index} failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }

    /// Allocate physically contiguous private memory and map it.
    ///
    /// User space cannot learn the guest physical address of its own mappings,
    /// and `TDG.MEM.PAGE.ATTR.WR` needs one, so allocation has to go through
    /// the driver rather than through mmap of anonymous memory.
    pub fn alloc(&self, size: usize, flags: u64) -> Result<TdMemory> {
        let mut alloc = TdAlloc {
            size: size as u64,
            flags,
            ..Default::default()
        };
        // SAFETY: the ioctl reads and writes exactly one TdAlloc.
        let rc = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                IOCTL_ALLOC,
                std::ptr::from_mut(&mut alloc),
            )
        };
        anyhow::ensure!(
            rc == 0,
            "allocating {size:#x} bytes of L2 memory failed: {}",
            std::io::Error::last_os_error()
        );

        let len = alloc.size as usize;
        // SAFETY: mapping the region the driver just reported, at its offset.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.file.as_raw_fd(),
                alloc.offset as libc::off_t,
            )
        };
        anyhow::ensure!(
            ptr != libc::MAP_FAILED,
            "mapping L2 memory failed: {}",
            std::io::Error::last_os_error()
        );

        Ok(TdMemory {
            ptr: ptr.cast(),
            len,
            gpa: alloc.gpa,
        })
    }

    /// Claim and map the complete boot-reserved range configured in the
    /// driver. The zero size and GPA deliberately leave the range selection
    /// to the trusted kernel interface rather than to the VMM.
    pub fn alloc_reserved(&self) -> Result<TdMemory> {
        self.alloc(0, ALLOC_RESERVED)
    }

    /// Pin and validate an existing mapping as physically contiguous L2 RAM.
    pub fn register_memory(&self, address: *mut u8, size: usize) -> Result<u64> {
        self.register_memory_with_flags(address, size, 0)
    }

    pub(crate) fn query_huge_1g(&self, address: *mut u8, size: usize) -> Result<u64> {
        self.register_memory_with_flags(address, size, REGISTER_QUERY_HUGE_1G)
    }

    fn register_memory_with_flags(&self, address: *mut u8, size: usize, flags: u64) -> Result<u64> {
        let mut registration = TdRegisterMemory {
            address: address as usize as u64,
            size: size as u64,
            flags,
            ..Default::default()
        };
        // SAFETY: the ioctl reads and writes exactly one initialized value.
        let rc = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                IOCTL_REGISTER_MEMORY,
                std::ptr::from_mut(&mut registration),
            )
        };
        anyhow::ensure!(
            rc == 0,
            "registering {size:#x} bytes of L2 memory failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(registration.gpa)
    }
}
