// SPDX-License-Identifier: MIT OR Apache-2.0
use crate::hv::RamMapper;
use applevisor_sys::{hv_return_t, hv_vm_map, HV_MEMORY_EXEC, HV_MEMORY_READ, HV_MEMORY_WRITE};
use std::num::NonZero;
use thiserror::Error;

/// Implementation of [`RamMapper`] for Hypervisor Framework.
pub struct HvfMapper;

impl RamMapper for HvfMapper {
    type Err = HvfMapperError;

    fn map(&self, host: *mut u8, vm: usize, len: NonZero<usize>) -> Result<(), Self::Err> {
        let ret = unsafe {
            hv_vm_map(
                host.cast(),
                vm.try_into().unwrap(),
                len.get(),
                HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC,
            )
        };

        match NonZero::new(ret) {
            Some(ret) => Err(HvfMapperError::MapFailed(ret)),
            None => Ok(()),
        }
    }
}

/// Implementation of [`RamMapper::Err`] for Hypervisor Framework.
#[derive(Debug, Error)]
pub enum HvfMapperError {
    #[error("hv_vm_map failed ({0:#x})")]
    MapFailed(NonZero<hv_return_t>),
}
