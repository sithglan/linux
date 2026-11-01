// SPDX-License-Identifier: GPL-2.0-only OR MIT
//! Owned M5 diagnostic program pages; no work publication.

use super::MappedObject;
use crate::{driver::AsahiDevice, m5_compute_program as program, mmu};
use kernel::prelude::*;

#[path = "m5_compute_job.rs"]
mod compute_job;
pub(super) use compute_job::{DESCRIPTOR_HIGH, DESCRIPTOR_LOW, EVENT};
use compute_job::M5ComputeJob;

pub(super) struct M5ProgramMemory {
    // Retract the roots before releasing any page mappings.
    context: mmu::M5ExecutionContext,
    pages: KVec<MappedObject>,
    job: Option<M5ComputeJob>,
    triangle: bool,
}

impl M5ProgramMemory {
    pub(super) fn new(dev: &AsahiDevice, uat: &mmu::Uat, with_job: bool, triangle: bool) -> Result<Self> {
        let mut context = uat.new_t8142_execution_context()?;
        let mut pages = KVec::new();
        for (address, kind) in [
            (program::SHADER_PAGE, program::PageKind::Shader),
            (program::PIPELINE_PAGE, program::PageKind::Pipeline),
            (program::STREAM_PAGE, program::PageKind::Stream),
            (program::PREEMPT_PAGE, program::PageKind::Preempt),
            (program::BUFFER_PAGE, program::PageKind::Buffers),
            (program::ROBUSTNESS_PAGE, program::PageKind::Robustness),
        ] {
            // Native 25G76 context walks distinguish the shader and CDM
            // stream (leaf attributes 0x80000000000c8b) from writable data
            // (0xc0000000000c8b). CPU initialization uses the separate WC
            // mapping; GPU write permission is not required for these pages.
            let protection = match kind {
                program::PageKind::Shader | program::PageKind::Stream => mmu::PROT_GPU_SHARED_RO,
                _ => mmu::PROT_GPU_SHARED_RW,
            };
            let mut page = MappedObject::new_wc_at(dev, context.vm(), address,
                program::PAGE_BYTES, protection)?;
            page.with_bytes_mut(|out| program::initialize_page_for(kind, out, triangle).map_err(|_| EINVAL))?;
            // Validate each installed leaf before publishing the root pair.
            context.vm().translate_iova(address)?;
            pages.push(page, GFP_KERNEL)?;
        }
        if triangle {
            let mut constants = MappedObject::new_wc_at(dev, context.vm(), program::CONSTANTS_PAGE,
                program::PAGE_BYTES, mmu::PROT_GPU_SHARED_RO)?;
            constants.with_bytes_mut(|out| program::initialize_page_for(
                program::PageKind::Constants, out, true).map_err(|_| EINVAL))?;
            context.vm().translate_iova(program::CONSTANTS_PAGE)?;
            pages.push(constants, GFP_KERNEL)?;
        }
        let job = if with_job { Some(M5ComputeJob::new(dev, uat, context.vm())?) } else { None };
        context.publish()?;
        Ok(Self { context, pages, job, triangle })
    }

    pub(super) fn uma_descriptor(&self) -> Result<[u64; 4]> {
        Ok(self.job.as_ref().ok_or(EINVAL)?.uma_descriptor())
    }

    pub(super) fn pool_state(&mut self) -> Result<[u64; 5]> {
        self.job.as_mut().ok_or(EINVAL)?.pool_state()
    }

    pub(super) fn pool_ready(state: [u64; 5]) -> bool { M5ComputeJob::pool_ready(state) }

    pub(super) fn progress(&mut self) -> Result<[u32; 5]> {
        self.job.as_mut().ok_or(EINVAL)?.progress()
    }

    pub(super) fn result(&mut self) -> Result<program::ComputeResult> {
        let triangle = self.triangle;
        self.pages[4].with_bytes_mut(|bytes| {
            let mut result = program::ComputeResult {
                inputs_changed: 0, outputs_wrong: 0, outputs_untouched: 0,
            };
            for index in 0..program::ELEMENTS {
                // SAFETY: these two aligned 1024-word spans are inside the
                // retained 16-KiB WC mapping. Firmware may update output.
                let read = |base: usize| unsafe { core::ptr::read_volatile(
                    bytes.as_ptr().add(base + index * 4).cast::<u32>()) };
                result.inputs_changed += usize::from(read(program::INPUT_OFFSET) != index as u32 * 7);
                let value = read(program::OUTPUT_OFFSET);
                result.outputs_wrong += usize::from(value != program::expected_output(index, triangle));
                result.outputs_untouched += usize::from(value == program::SENTINEL);
            }
            Ok(result)
        })
    }

    pub(super) fn validate_unsubmitted(&mut self) -> Result<[u64; 2]> {
        if !self.context.is_current() || self.pages.len() != 6 + usize::from(self.triangle) {
            return Err(EFAULT);
        }
        let result = self.pages[4].with_bytes_mut(|out| {
            program::check_result(out).map_err(|_| EINVAL)
        })?;
        if result.inputs_changed != 0 || result.outputs_wrong != program::ELEMENTS
            || result.outputs_untouched != program::ELEMENTS {
            return Err(EIO);
        }
        Ok(self.context.roots())
    }
}
