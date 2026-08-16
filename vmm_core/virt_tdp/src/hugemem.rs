// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Segmented private memory for an L2.
//!
//! TD Partitioning aliases the L1's GPA directly into the L2. The allocator
//! therefore preserves the GPA of every backing extent instead of pretending
//! fragmented pages form one physical range. It uses 1 GiB hugetlb pages for
//! the main body, 2 MiB hugetlb pages for the remainder, and ordinary 4 KiB
//! pages only for the final sub-2-MiB tail.

use crate::TdcallDevice;
use anyhow::Context;
use anyhow::Result;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::Arc;

pub const HUGE_1G: usize = 1024 * 1024 * 1024;
pub const HUGE_2M: usize = 2 * 1024 * 1024;
pub const PAGE_4K: usize = 4096;
const MAX_GUEST_RAM_SEGMENTS: usize = 96;

fn allocation_counts(size: usize) -> (usize, usize, usize) {
    let one_gib = size / HUGE_1G;
    let after_1g = size % HUGE_1G;
    let two_mib = after_1g / HUGE_2M;
    let base_pages = (after_1g % HUGE_2M) / PAGE_4K;
    (one_gib, two_mib, base_pages)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemorySegment {
    pub gpa: u64,
    pub len: usize,
}

struct SourceMapping {
    ptr: *mut u8,
    len: usize,
    _file: File,
}

impl Drop for SourceMapping {
    fn drop(&mut self) {
        // SAFETY: this type owns the complete mapping.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

struct Candidate {
    gpa: u64,
    ptr: Option<*mut u8>,
    len: usize,
}

/// Driver-backed linear mapping plus the physical extents it represents.
pub struct HugeRegion {
    ptr: *mut u8,
    len: usize,
    segments: Vec<MemorySegment>,
    device: Arc<TdcallDevice>,
}

// The mapping is owned exclusively by this struct, and mutation needs &mut.
unsafe impl Send for HugeRegion {}
// SAFETY: as above.
unsafe impl Sync for HugeRegion {}

impl HugeRegion {
    pub fn alloc(size: usize) -> Result<Self> {
        anyhow::ensure!(
            size != 0 && size % PAGE_4K == 0,
            "L2 memory size must be 4 KiB aligned"
        );
        let device = Arc::new(TdcallDevice::open()?);
        let mut sources = Vec::new();
        let mut candidates = Vec::new();

        let (one_gib, two_mib, base_pages) = allocation_counts(size);
        if one_gib != 0 {
            let source = Self::map_hugetlb(one_gib * HUGE_1G, libc::MFD_HUGE_1GB)?;
            for index in 0..one_gib {
                // SAFETY: every candidate lies within `source`.
                let ptr = unsafe { source.ptr.add(index * HUGE_1G) };
                candidates.push(Candidate {
                    gpa: device.query_huge_1g(ptr, HUGE_1G)?,
                    ptr: Some(ptr),
                    len: HUGE_1G,
                });
            }
            sources.push(source);
        }

        if two_mib != 0 {
            let source = Self::map_hugetlb(two_mib * HUGE_2M, libc::MFD_HUGE_2MB)?;
            for index in 0..two_mib {
                // SAFETY: every candidate lies within `source`.
                let ptr = unsafe { source.ptr.add(index * HUGE_2M) };
                candidates.push(Candidate {
                    gpa: device.query_huge_2m(ptr, HUGE_2M)?,
                    ptr: Some(ptr),
                    len: HUGE_2M,
                });
            }
            sources.push(source);
        }

        let tail = base_pages * PAGE_4K;
        if tail != 0 {
            let mut remaining = tail;
            while remaining != 0 {
                let len = 1_usize << (usize::BITS - 1 - remaining.leading_zeros());
                let allocation = device.alloc_unmapped(len, 0)?;
                candidates.push(Candidate {
                    gpa: allocation.gpa,
                    ptr: None,
                    len: allocation.len,
                });
                remaining -= len;
            }
        }

        candidates.sort_unstable_by_key(|candidate| candidate.gpa);
        for pair in candidates.windows(2) {
            let first_end = pair[0]
                .gpa
                .checked_add(pair[0].len as u64)
                .context("L2 backing GPA overflow")?;
            anyhow::ensure!(
                first_end <= pair[1].gpa,
                "overlapping L2 backing at {:#x} and {:#x}",
                pair[0].gpa,
                pair[1].gpa
            );
        }

        // Registration order is also the file-offset order exported by the
        // driver. Keeping it GPA-sorted lets one linear mappable back a sorted
        // list of sparse guest RAM ranges.
        for candidate in &candidates {
            if let Some(ptr) = candidate.ptr {
                let actual = device.register_memory(ptr, candidate.len)?;
                anyhow::ensure!(
                    actual == candidate.gpa,
                    "L2 backing GPA changed during registration"
                );
            }
        }
        device.finalize_memory()?;

        let mut segments: Vec<MemorySegment> = Vec::new();
        for candidate in &candidates {
            if let Some(last) = segments.last_mut()
                && last.gpa + last.len as u64 == candidate.gpa
            {
                last.len += candidate.len;
            } else {
                segments.push(MemorySegment {
                    gpa: candidate.gpa,
                    len: candidate.len,
                });
            }
        }

        anyhow::ensure!(
            segments.len() <= MAX_GUEST_RAM_SEGMENTS,
            "L2 backing produced {} physical extents; at most {} fit safely in the x86 boot memory map. Reserve 2 MiB hugepages at L1 boot so they are physically clustered",
            segments.len(),
            MAX_GUEST_RAM_SEGMENTS
        );
        tracing::info!(size, ?segments, "allocated segmented L2 memory");

        let ptr = Self::map_driver(&device, size)?;
        // The driver pins the source mappings before they are dropped. Clear
        // through the canonical composite mapping so reused L2 pages cannot
        // retain tenant data.
        unsafe { std::ptr::write_bytes(ptr, 0, size) };
        drop(sources);

        Ok(Self {
            ptr,
            len: size,
            segments,
            device,
        })
    }

    pub fn from_registered(
        device: Arc<TdcallDevice>,
        size: usize,
        segments: Vec<MemorySegment>,
    ) -> Result<Self> {
        anyhow::ensure!(
            size != 0 && size % PAGE_4K == 0,
            "L2 memory size must be 4 KiB aligned"
        );
        anyhow::ensure!(
            !segments.is_empty()
                && segments.iter().all(|segment| {
                    segment.gpa % PAGE_4K as u64 == 0
                        && segment.len != 0
                        && segment.len % PAGE_4K == 0
                        && segment.gpa.checked_add(segment.len as u64).is_some()
                })
                && segments
                    .windows(2)
                    .all(|pair| { pair[0].gpa + pair[0].len as u64 <= pair[1].gpa }),
            "invalid L2 segment metadata"
        );
        anyhow::ensure!(
            segments
                .iter()
                .try_fold(0_usize, |total, segment| total.checked_add(segment.len))
                == Some(size),
            "L2 segment lengths do not match the requested memory size"
        );
        anyhow::ensure!(
            segments.len() <= MAX_GUEST_RAM_SEGMENTS,
            "too many L2 RAM segments"
        );
        let ptr = Self::map_driver(&device, size)?;
        Ok(Self {
            ptr,
            len: size,
            segments,
            device,
        })
    }

    fn create_memfd(name: &'static std::ffi::CStr, flags: libc::c_uint) -> Result<File> {
        // SAFETY: `name` is NUL terminated and flags are defined by Linux.
        let fd = unsafe { libc::syscall(libc::SYS_memfd_create, name.as_ptr(), flags) };
        anyhow::ensure!(
            fd >= 0,
            "creating L2 backing failed: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: this function owns the returned descriptor.
        Ok(unsafe { File::from_raw_fd(fd as i32) })
    }

    fn map_hugetlb(size: usize, page_flag: libc::c_uint) -> Result<SourceMapping> {
        let file = Self::create_memfd(
            c"tdp-guest-hugetlb",
            libc::MFD_CLOEXEC | libc::MFD_HUGETLB | page_flag,
        )?;
        file.set_len(size as u64)
            .context("sizing hugetlb L2 backing")?;
        // SAFETY: mapping the complete private hugetlb file.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE | libc::MAP_LOCKED,
                file.as_raw_fd(),
                0,
            )
        };
        anyhow::ensure!(
            ptr != libc::MAP_FAILED,
            "mapping hugetlb L2 backing failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(SourceMapping {
            ptr: ptr.cast(),
            len: size,
            _file: file,
        })
    }

    fn map_driver(device: &TdcallDevice, size: usize) -> Result<*mut u8> {
        // SAFETY: the driver exposes the registered segments consecutively at
        // file offset zero.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                device.as_raw_fd(),
                0,
            )
        };
        anyhow::ensure!(
            ptr != libc::MAP_FAILED,
            "mapping segmented L2 memory failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(ptr.cast())
    }

    pub fn segments(&self) -> &[MemorySegment] {
        &self.segments
    }

    pub fn device(&self) -> &Arc<TdcallDevice> {
        &self.device
    }

    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: this struct owns a mapping of `len` bytes.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above, and &mut excludes competing references.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    pub fn write_at(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        let end = offset.checked_add(data.len()).context("offset overflow")?;
        anyhow::ensure!(end <= self.len, "write exceeds the L2 memory region");
        self.as_mut_slice()[offset..end].copy_from_slice(data);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_uses_small_pages_only_for_the_tail() {
        assert_eq!(allocation_counts(4 * HUGE_1G), (4, 0, 0));
        assert_eq!(allocation_counts(3 * HUGE_1G + HUGE_1G / 2), (3, 256, 0));
        assert_eq!(
            allocation_counts(HUGE_1G + HUGE_2M + 3 * PAGE_4K),
            (1, 1, 3)
        );
    }
}

impl Drop for HugeRegion {
    fn drop(&mut self) {
        // SAFETY: unmapping the mapping owned by this struct.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}
