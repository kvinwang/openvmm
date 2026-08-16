// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Partition-wide state.
//!
//! All of it is Hyper-V enlightenment state — the hypercall page, the
//! reference time counter, the reference TSC page — and an L2 here is told it
//! is bare metal, so there is nothing of it to save. The methods exist because
//! save and restore go through them; they report the default, which is what a
//! guest that never used an enlightenment would have.

use crate::partition::TdpError;
use crate::partition::TdpPartition;
use virt::x86::vm;
use virt::x86::vm::AccessVmState;

impl AccessVmState for &'_ TdpPartition {
    type Error = TdpError;

    fn caps(&self) -> &virt::PartitionCapabilities {
        &self.inner.caps
    }

    fn commit(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn hypercall(&mut self) -> Result<vm::HypercallMsrs, Self::Error> {
        Ok(vm::HypercallMsrs::default())
    }

    fn set_hypercall(&mut self, _value: &vm::HypercallMsrs) -> Result<(), Self::Error> {
        Ok(())
    }

    fn reftime(&mut self) -> Result<vm::ReferenceTime, Self::Error> {
        Ok(vm::ReferenceTime::default())
    }

    fn set_reftime(&mut self, _value: &vm::ReferenceTime) -> Result<(), Self::Error> {
        Ok(())
    }

    fn reference_tsc_page(&mut self) -> Result<vm::ReferenceTscPage, Self::Error> {
        Ok(vm::ReferenceTscPage::default())
    }

    fn set_reference_tsc_page(&mut self, _value: &vm::ReferenceTscPage) -> Result<(), Self::Error> {
        Ok(())
    }
}
