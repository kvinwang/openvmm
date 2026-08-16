// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TDX L2 hypervisor backend.
//!
//! Available only inside a TD that was created as an L1 VMM — that is, with
//! `TD_PARAMS.NUM_L2_VMS` greater than zero — and only with dstack's TDCALL
//! passthrough driver loaded. Both are checked by the probe, because the
//! alternative is a partition that fails much later with a TDCALL status code.

#![cfg(all(target_os = "linux", feature = "virt_tdp", guest_arch = "x86_64"))]

use anyhow::Context as _;
use hypervisor_resources::HypervisorKind;
use hypervisor_resources::TdpHandle;
use vm_resource::IntoResource;
use vm_resource::Resource;

/// One gibibyte, the hugepage size an L2's memory is carved from.
const GIB: u64 = 1024 * 1024 * 1024;

/// Reserve the guest's memory and describe it to the worker.
///
/// Reserving here rather than in the resolver is not an optimisation: the
/// memory layout is built from the address these pages landed at, and that
/// happens before the worker exists.
fn reserve(memory_size: u64) -> anyhow::Result<Resource<HypervisorKind>> {
    let (ranges, file) = virt_tdp::reserve_guest_memory(memory_size as usize)?;
    Ok(TdpHandle {
        memory_size,
        memory_ranges: ranges
            .into_iter()
            .map(|range| (range.start, range.end - range.start))
            .collect(),
        memory: Some(file.into()),
    }
    .into_resource())
}

/// TDX L2 probe for auto-detection.
pub struct TdpProbe;

impl hypervisor_resources::HypervisorProbe for TdpProbe {
    fn name(&self) -> &str {
        "tdp"
    }

    fn try_new_resource(&self) -> anyhow::Result<Option<Resource<HypervisorKind>>> {
        if !virt_tdp::Tdp::is_available() {
            return Ok(None);
        }
        Ok(Some(reserve(GIB)?))
    }

    fn new_resource(&self, params: &[(&str, &str)]) -> anyhow::Result<Resource<HypervisorKind>> {
        let mut memory_size = GIB;
        for &(key, val) in params {
            match key {
                // The guest's memory has to be reserved before the VM exists,
                // so its size is a property of the backend rather than of the
                // VM configuration.
                "memory" => {
                    memory_size = parse_size(val)?;
                }
                _ => anyhow::bail!("unknown tdp parameter: {key}"),
            }
        }
        anyhow::ensure!(
            virt_tdp::Tdp::is_available(),
            "this process cannot run L2 VMs: either the TD was not created with \
             num-l2-vms, or dstack's TDCALL driver is not loaded"
        );
        reserve(memory_size)
    }
}

fn parse_size(value: &str) -> anyhow::Result<u64> {
    let (number, unit) = value
        .trim()
        .split_at_checked(
            value
                .trim()
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(value.trim().len()),
        )
        .context("expected a byte count with an optional KiB, MiB, or GiB suffix")?;
    let number: u64 = number.parse().context("expected a memory size")?;
    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "" | "g" | "gib" => GIB,
        "m" | "mib" => 1024 * 1024,
        "k" | "kib" => 1024,
        _ => anyhow::bail!("unsupported memory size suffix: {unit}"),
    };
    let size = number
        .checked_mul(multiplier)
        .context("memory size overflow")?;
    anyhow::ensure!(
        size != 0 && size % 4096 == 0,
        "memory size must be 4 KiB aligned"
    );
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flexible_page_aligned_sizes() {
        assert_eq!(parse_size("4").unwrap(), 4 * GIB);
        assert_eq!(parse_size("3584MiB").unwrap(), 3584 * 1024 * 1024);
        assert_eq!(parse_size("3147788KiB").unwrap(), 3147788 * 1024);
        assert!(parse_size("3KiB").is_err());
    }
}
