// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A `virt` backend that runs guests as Intel TDX **L2 VMs**.
//!
//! dstack runs its guest OS as a TDX L1 and wants the tenant workload in an
//! L2, so that a workload with root is no longer co-equal with the guest
//! agent. Running an L2 means driving a guest through `TDG.VP.ENTER` rather
//! than through KVM, which is the one thing OpenVMM does not already have:
//! the TDX support in this tree lives in the paravisor, wired to a Hyper-V
//! host interface that does not exist here.
//!
//! Everything else — the device models, the instruction emulator, the ACPI
//! tables, the Linux loader — is why this is a backend and not another VMM.
//!
//! # What is different about an L2
//!
//! Three constraints shape this backend, and none of them apply to the other
//! backends in this directory.
//!
//! **The guest's physical addresses are the L1's.** Aliasing a page into an L2
//! publishes the L1's own guest physical address; there is no second
//! translation. So a partition cannot be given an arbitrary memory layout, and
//! the layout has to be built around wherever the L1's memory actually is.
//!
//! **The TDX module owns most of the VMCS.** EPT, VPID, unrestricted guest,
//! the MSR bitmaps and the pin-based controls are not the L1's to choose, and
//! APIC virtualization is forced on. The guest's APIC reads never exit, so the
//! backend writes the virtual-APIC page rather than emulating reads, and
//! delivers interrupts by setting IRR and raising RVI.
//!
//! **Some guest state has no VMCS field.** `KERNEL_GS_BASE`, the SYSCALL MSRs,
//! `XSS`, `TSC_AUX`, `CR2` and the x87 state live in the physical registers
//! where the L1 and the L2 overwrite each other, so they are swapped around
//! every entry — by the kernel driver, since the swap has to be atomic with
//! respect to preemption.

#![cfg(target_os = "linux")]
#![expect(missing_docs)]
// The kernel interface is ioctl and mmap, and the guest's register image is
// memory the TDX module writes behind our back.
#![expect(unsafe_code)]

mod cpuid;
pub mod driver;
mod emulate;
pub mod hugemem;
mod hypervisor;
pub(crate) mod lowmem;
mod partition;
pub mod tdx;
mod traits;
mod vm_state;
pub mod vmcs;
mod vp;
mod vp_state;

pub use driver::TdcallDevice;
pub use hugemem::HugeRegion;
pub use partition::Tdp;
pub use partition::TdpError;
pub use partition::TdpMemory;
pub use partition::TdpPartition;
pub use partition::reserve_guest_memory;
pub use partition::reserved_guest_memory;
pub use tdx::L2GprContext;
pub use tdx::L2Vm;
pub use tdx::VpEnterResult;
pub use vp::TdpProcessor;
