// SPDX-License-Identifier: GPL-2.0-only OR MIT
//! Retained resources for one bounded M5 shader experiment.
//!
//! The native capture establishes the process pool layout and register
//! program. Its initial page directory is populated by the host; opcode 20
//! appends later growth records and is not an initial registration command.
//! No completion or rendering support is implied by constructing this graph.

use super::MappedObject;
use crate::{driver::AsahiDevice, m5_compute, m5_compute_program as program, mmu};
use kernel::prelude::*;

pub(crate) const DESCRIPTOR_HIGH: u64 = 0xffff_fc20_c040_0000;
pub(crate) const DESCRIPTOR_LOW: u64 = 0x70_0038_0000;
pub(crate) const EVENT: u64 = DESCRIPTOR_HIGH + 0x5000;
const INNER: u64 = DESCRIPTOR_HIGH + 0x6000;
const ZERO: u64 = DESCRIPTOR_HIGH + 0x8000;
const SCHEDULER: u64 = DESCRIPTOR_HIGH + 0x4000;
const SUBMITTED: u64 = DESCRIPTOR_HIGH + 0x4104;
const DIRECTORY: u64 = 0x70_0000_0000;
const FREELIST: u64 = 0x70_0040_8000;
const OPERAND: u64 = 0x70_0042_0000;
const POOL_BASE: u64 = 0x70_0044_8000;
const POOL_COUNT: usize = 14;
const POOL_BYTES: usize = 0x20_0000;
const POOL_STRIDE: u64 = 0x20_8000;
const DIRECTORY_ENTRIES: usize = 0x80000;
const POOL_PAGES: usize = POOL_COUNT * POOL_BYTES / 0x1000;
// The sole probe owns hardware UMA slot zero. The register program and
// shared-control record must select that same populated slot.
const PRIVATE_POOL_HW_ID: u32 = 0;

pub(super) struct M5ComputeJob {
    descriptor_alias: mmu::KernelMapping,
    graph: MappedObject,
    directory: MappedObject,
    freelist: MappedObject,
    operand: MappedObject,
    blocks: KVec<MappedObject>,
}

impl M5ComputeJob {
    pub(super) fn new(dev: &AsahiDevice, uat: &mmu::Uat, vm: &mmu::Vm) -> Result<Self> {
        let mut graph = MappedObject::new_wc_at(dev, uat.kernel_vm(), DESCRIPTOR_HIGH,
            0x10000, mmu::PROT_FW_PRIV_RW)?;
        let descriptor_alias = graph.map_alias_at(uat.kernel_lower_vm(), DESCRIPTOR_LOW,
            mmu::PROT_GPU_SHARED_RW)?;
        for offset in (0..0x10000).step_by(mmu::UAT_PGSZ) {
            if uat.kernel_vm().translate_iova(DESCRIPTOR_HIGH + offset as u64)?
                != uat.kernel_lower_vm().translate_iova(DESCRIPTOR_LOW + offset as u64)? {
                return Err(EFAULT);
            }
        }
        // Native directory holds one GPU VA for every 4-KiB allocation unit.
        // Native context3 maps the full 4-MiB table and leaves the following
        // two 16-KiB pages unmapped. Its first 7168 entries are ready before
        // the controlled compute submission, without a pool-growth request.
        let mut directory = MappedObject::new_wc_at(dev, vm, DIRECTORY, DIRECTORY_ENTRIES * 8,
            mmu::PROT_GPU_SHARED_RW)?;
        let mut freelist = MappedObject::new_wc_at(dev, vm, FREELIST, 0x4000,
            mmu::PROT_GPU_SHARED_RW)?;
        let operand = MappedObject::new_wc_at(dev, vm, OPERAND, 0x20000,
            mmu::PROT_GPU_SHARED_RW)?;
        let mut blocks = KVec::new();
        for index in 0..POOL_COUNT {
            let address = POOL_BASE + index as u64 * POOL_STRIDE;
            blocks.push(MappedObject::new_wc_at(dev, vm, address, POOL_BYTES,
                mmu::PROT_GPU_SHARED_RW)?, GFP_KERNEL)?;
            // Preserve the observed 32-KiB inter-block guard.
            if vm.translate_iova(address + POOL_BYTES as u64).is_ok() {
                return Err(EFAULT);
            }
        }
        directory.with_bytes_mut(|out| {
            for page in 0..POOL_PAGES {
                let block = page / (POOL_BYTES / 0x1000);
                let within = page % (POOL_BYTES / 0x1000);
                let address = POOL_BASE + block as u64 * POOL_STRIDE + within as u64 * 0x1000;
                out[page * 8..page * 8 + 8].copy_from_slice(&address.to_le_bytes());
            }
            Ok(())
        })?;
        for offset in [0x400000u64, 0x404000] {
            if vm.translate_iova(DIRECTORY + offset).is_ok() { return Err(EFAULT); }
        }
        freelist.with_bytes_mut(|out| {
            for index in 0..POOL_COUNT {
                let address = POOL_BASE + index as u64 * POOL_STRIDE;
                out[index * 0x40..index * 0x40 + 8]
                    .copy_from_slice(&(address | (1u64 << 61)).to_le_bytes());
            }
            Ok(())
        })?;
        graph.with_bytes_mut(|out| {
            // Independent scheduler count, not a pointer to the UAT roots.
            out[0x4000..0x4008].copy_from_slice(&SUBMITTED.to_le_bytes());
            out[0x4010..0x4014].copy_from_slice(&0x50u32.to_le_bytes());
            out[0x4104..0x4108].copy_from_slice(&1u32.to_le_bytes());
            // Both native pre-submit owner captures retain an enabled link.
            out[0x6000..0x6004].copy_from_slice(&1u32.to_le_bytes());
            let event = &mut out[0x5000..0x5070];
            for (offset, value) in [(0, 1u32), (8, PRIVATE_POOL_HW_ID), (0x10, 2), (0x1c, 0x80000),
                (0x24, 0x1c00), (0x2c, 0x1c00), (0x40, 4), (0x48, 0x70),
                (0x5c, 1), (0x60, 2)] {
                event[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            }
            for (offset, address) in [(0x14, DIRECTORY), (0x30, FREELIST), (0x4c, INNER)] {
                event[offset..offset + 8].copy_from_slice(&address.to_le_bytes());
            }
            // Index 0 is owned by this sole probe. Native
            // populateInitialFirmwareState writes the host-owned enabled
            // flag at +0x5c (25G76, 0x8a2922c); it is 1 in the compute pool.
            // The firmware event link remains separate from the populated
            // page directory and the hardware's UMA descriptor table.
            m5_compute::encode_m5_compute_descriptor(m5_compute::M5ComputeDescriptor {
                registers: m5_compute::M5ComputeParameters {
                    preempt_base: program::PREEMPT_PAGE, cdm_base: program::STREAM_PAGE,
                    dispatch_identity: 0x0000_0001_0000_0001, context_id: 3, work_ordinal: 0,
                    robustness: program::ROBUSTNESS_PAGE, operand_state_base: OPERAND,
                    private_pool_hw_id: u64::from(PRIVATE_POOL_HW_ID), descriptor_control_530: 0x54b,
                    descriptor_control_538: 0xf5, descriptor_flag_60a: false,
                    descriptor_flag_60b: false,
                },
                submit_sequence: 0, scheduler_record: SCHEDULER,
                descriptor_low_alias: DESCRIPTOR_LOW, cdm_terminator: program::STREAM_PAGE + 44,
                dispatch_stamps: [DESCRIPTOR_HIGH + 0xc000, DESCRIPTOR_HIGH + 0xc040],
                event_machine_stamp: 0x100, queue_id: 11, queue_ordinal: 0,
                has_event: false, submission_index: 1,
                status: [DESCRIPTOR_HIGH + 0xc080, DESCRIPTOR_HIGH + 0xc088],
                shared_control: EVENT, shared_control_enabled: true,
                // Zero-page +8 is zero: the native pressure-accounting helper
                // returns before using these opaque memory-pressure values.
                event_metadata: [0; 16], zero_page: ZERO, owner_id: 1,
                descriptor_work_ordinal: 0,
            }, &mut out[..0x1000]).map_err(|_| EINVAL)
        })?;
        Ok(Self { descriptor_alias, graph, directory, freelist, operand, blocks })
    }

    pub(super) fn uma_descriptor(&self) -> [u64; 4] {
        // M5 25G76 halLoadUMADesc (89de718..89de740), not the Neo packer.
        // Hardware buffer ID0 is owned by this sole probe, matching EVENT+8.
        [DIRECTORY >> 7, (POOL_PAGES as u64) << 32,
            (DIRECTORY_ENTRIES as u64) << 32 | POOL_PAGES as u64, 0]
    }

    pub(super) fn pool_state(&mut self) -> Result<[u64; 5]> {
        Ok([u64::from(self.graph.read_u32(EVENT + 8)?),
            u64::from(self.graph.read_u32(EVENT + 0x5c)?),
            u64::from(self.graph.read_u32(INNER)?),
            self.directory.read_u64(DIRECTORY)?,
            self.directory.read_u64(DIRECTORY + (0x1c00 - 1) * 8)?])
    }

    pub(super) fn pool_ready(state: [u64; 5]) -> bool {
        state[0] == u64::from(PRIVATE_POOL_HW_ID) && state[1] == 1 && state[2] == 1
            && state[3] == POOL_BASE
            && state[4] == POOL_BASE + (POOL_COUNT - 1) as u64 * POOL_STRIDE + 0x1ff000
    }

    pub(super) fn progress(&mut self) -> Result<[u32; 5]> {
        Ok([self.graph.read_u32(DESCRIPTOR_HIGH + 0xc000)?,
            self.graph.read_u32(DESCRIPTOR_HIGH + 0xc040)?,
            self.graph.read_u32(DESCRIPTOR_HIGH + 0xc080)?,
            self.graph.read_u32(SUBMITTED)?, self.graph.read_u32(EVENT)?])
    }
}
