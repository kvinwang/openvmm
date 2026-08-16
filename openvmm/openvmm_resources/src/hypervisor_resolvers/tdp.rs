// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TDX L2 resource resolver.

#![cfg(all(target_os = "linux", feature = "virt_tdp", guest_arch = "x86_64"))]

use hypervisor_resources::HypervisorKind;
use hypervisor_resources::TdpHandle;
use openvmm_core::hypervisor_backend::ResolvedHypervisorBackend;
use std::sync::Arc;

/// TDX L2 resource resolver.
pub struct TdpResolver;

impl vm_resource::ResolveResource<HypervisorKind, TdpHandle> for TdpResolver {
    type Output = ResolvedHypervisorBackend;
    type Error = virt_tdp::TdpError;

    fn resolve(&self, resource: TdpHandle, _input: ()) -> Result<Self::Output, Self::Error> {
        // The memory was reserved before this process existed, because the
        // memory layout was built from the address it landed at. Mapping the
        // same descriptor here gives the same physical pages.
        let segments = resource
            .memory_ranges
            .into_iter()
            .map(|(gpa, len)| virt_tdp::hugemem::MemorySegment {
                gpa,
                len: len as usize,
            })
            .collect::<Vec<_>>();
        let region = match resource.memory {
            Some(fd) => {
                let device = Arc::new(
                    virt_tdp::TdcallDevice::from_file(fd.into())
                        .map_err(virt_tdp::TdpError::NoDevice)?,
                );
                virt_tdp::HugeRegion::from_registered(
                    device,
                    resource.memory_size as usize,
                    segments,
                )?
            }
            None => virt_tdp::HugeRegion::alloc(resource.memory_size as usize)?,
        };
        let mut backend = virt_tdp::Tdp::from_device(region.device().clone())?;
        backend.set_memory(Arc::new(virt_tdp::TdpMemory::new(region)));
        Ok(ResolvedHypervisorBackend::new(backend))
    }
}

vm_resource::declare_static_resolver!(TdpResolver, (HypervisorKind, TdpHandle),);
