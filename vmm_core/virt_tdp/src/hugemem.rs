// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Large, physically contiguous guest memory for an L2.
//!
//! TD partitioning aliases the L1's GPA directly into the L2, so guest RAM
//! must be physically contiguous. Memory comes from the L1's reserved 1 GiB
//! hugetlb pool. The TDCALL driver pins every page, verifies contiguity, and
//! reports the GPA; `/proc/self/pagemap` is not part of the trusted path.

use anyhow::Context;
use anyhow::Result;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};

pub const HUGE_1G: usize = 1024 * 1024 * 1024;

/// A driver-validated, 1 GiB-aligned physically contiguous region.
pub struct HugeRegion {
    ptr: *mut u8,
    len: usize,
    gpa: u64,
    file: File,
}

// The mapping is owned exclusively by this struct, and mutation needs &mut.
unsafe impl Send for HugeRegion {}
// SAFETY: as above.
unsafe impl Sync for HugeRegion {}

impl HugeRegion {
    /// Reserve `size` bytes from the 1 GiB hugepage pool.
    pub fn alloc(size: usize) -> Result<Self> {
        anyhow::ensure!(size % HUGE_1G == 0, "{size:#x} is not a multiple of 1 GiB");
        // SAFETY: memfd_create receives a valid NUL-terminated name.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_memfd_create,
                c"tdp-guest-memory".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_HUGETLB | libc::MFD_HUGE_1GB,
            )
        };
        anyhow::ensure!(
            fd >= 0,
            "reserving {} GiB of 1 GiB hugepages failed: {}. Boot the L1 with \
             `default_hugepagesz=1G hugepagesz=1G hugepages=N`.",
            size / HUGE_1G,
            std::io::Error::last_os_error()
        );
        // SAFETY: this function owns the returned descriptor.
        let file = unsafe { File::from_raw_fd(fd as i32) };
        file.set_len(size as u64)
            .context("sizing the hugepage region")?;
        let mut ptr = Self::map_populate(&file, size)?;
        let device = crate::TdcallDevice::open()?;

        // Hugetlbfs assigns reserved hugepages in free-list order, not GPA
        // order. Ask the driver for each hugepage's GPA and punch them back
        // out in descending order until file offsets are physically ordered.
        for attempt in 0..4 {
            let mut gpas = Vec::with_capacity(size / HUGE_1G);
            for i in 0..size / HUGE_1G {
                // SAFETY: i is within the mapping and each address is aligned.
                let address = unsafe { ptr.add(i * HUGE_1G) };
                gpas.push(device.query_huge_1g(address, HUGE_1G)?);
            }
            let mut sorted = gpas.clone();
            sorted.sort_unstable();
            anyhow::ensure!(
                sorted.windows(2).all(|w| w[1] == w[0] + HUGE_1G as u64),
                "the reserved hugepage pool is not physically contiguous: {sorted:x?}"
            );
            if gpas == sorted {
                break;
            }
            anyhow::ensure!(attempt < 3, "could not order hugepages by GPA: {gpas:x?}");
            // SAFETY: unmapping the mapping owned here; it is recreated below.
            unsafe { libc::munmap(ptr.cast(), size) };
            let mut order: Vec<usize> = (0..gpas.len()).collect();
            order.sort_unstable_by_key(|&i| std::cmp::Reverse(gpas[i]));
            for i in order {
                // SAFETY: punching a complete hugepage from our private file.
                let rc = unsafe {
                    libc::fallocate(
                        file.as_raw_fd(),
                        libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                        (i * HUGE_1G) as i64,
                        HUGE_1G as i64,
                    )
                };
                anyhow::ensure!(
                    rc == 0,
                    "returning hugepage {i} failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            ptr = Self::map_populate(&file, size)?;
        }

        // A TD Partitioning VM slot can be reused after either an orderly
        // shutdown or a killed VMM. Do not depend on hugetlbfs allocation
        // policy to sanitize pages returned by the previous L2: clear the
        // complete region before the driver can publish any alias to it.
        // SAFETY: `ptr` owns a writable mapping of exactly `size` bytes.
        unsafe { std::ptr::write_bytes(ptr, 0, size) };

        // This registration is the authoritative GPA lookup. The
        // memfd retains the hugepages after the descriptor closes; the worker
        // registers and pins the same file again before it may create aliases.
        let gpa = device.register_memory(ptr, size)?;
        Ok(Self {
            ptr,
            len: size,
            gpa,
            file,
        })
    }

    fn map_populate(file: &File, size: usize) -> Result<*mut u8> {
        // SAFETY: mapping a file owned by the caller.
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
            "mapping {} GiB of L2 memory failed: {}",
            size / HUGE_1G,
            std::io::Error::last_os_error()
        );
        Ok(ptr.cast())
    }

    /// Map and register a region passed into the VMM worker.
    pub fn from_file(file: File, size: usize, device: &crate::TdcallDevice) -> Result<Self> {
        let ptr = Self::map_populate(&file, size)?;
        let gpa = device.register_memory(ptr, size)?;
        Ok(Self {
            ptr,
            len: size,
            gpa,
            file,
        })
    }

    pub fn gpa(&self) -> u64 {
        self.gpa
    }
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }
    pub fn file(&self) -> &File {
        &self.file
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

impl Drop for HugeRegion {
    fn drop(&mut self) {
        // SAFETY: unmapping the mapping owned by this struct.
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}
