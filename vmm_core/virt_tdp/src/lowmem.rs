// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Memory below 1 MiB, where Linux places its AP startup trampoline.
//!
//! The kernel reserves a real-mode trampoline under 1 MiB before it will
//! continue, even with one processor that will never run it. An L2's guest
//! physical addresses are the L1's own: aliasing publishes an address, it
//! does not choose one. Ordinary allocated memory therefore cannot execute at
//! the required address. The L1 reserves 512..640 KiB in its boot memory map,
//! and the restricted driver exclusively maps those exact private pages for
//! OpenVMM. The remaining legacy holes stay emulated.
//!
//! Declaring the range in e820 without physical backing is insufficient: the
//! kernel writes and then executes the trampoline. Register-only instructions
//! do not provide an MMIO operand for OpenVMM's device emulator, so executable
//! same-GPA pages are the architectural solution rather than an expanding
//! Linux-trampoline interpreter.

/// Start of the boot-reserved window used by the L2. Keeping the first 512
/// KiB for the L1 lets a normal multiprocessor Linux host retain its own
/// real-mode trampoline.
pub const LOW_MEMORY_BASE: u64 = 512 * 1024;

/// End of conventional RAM below the VGA aperture. The L1 must reserve
/// `LOW_MEMORY_BASE..LOW_MEMORY_RESERVED_END` at boot and configure the
/// restricted driver with the same range.
pub const LOW_MEMORY_RESERVED_END: u64 = 640 * 1024;
pub const LOW_MEMORY_SIZE: usize = (LOW_MEMORY_RESERVED_END - LOW_MEMORY_BASE) as usize;

/// End of the complete legacy window that OpenVMM still backs for BIOS-era
/// probes. Only conventional RAM above `LOW_MEMORY_BASE` is directly aliased;
/// the VGA/ROM holes continue to be emulated.
pub const LOW_MEMORY_END: u64 = 1 << 20;
