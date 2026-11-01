// SPDX-License-Identifier: GPL-2.0-only OR MIT
//! Retained M5 render VM and private storage, with an optional native test program.
//!
//! Allocation is separate from root publication and queue submission. A future
//! job owner keeps this graph alive through queue completion. After a fault,
//! ownership remains retained until both firmware CPUs stop. Captured physical
//! backing is never installed.

use super::MappedObject;
use super::graphics_usc::M5GraphicsUsc;
use crate::{driver::AsahiDevice, m5_graphics_bundle as bundle, mmu};
use kernel::{firmware::Firmware, prelude::*};

pub(super) struct M5GraphicsMemory {
    // Retract any published roots before dropping their mapping owners.
    context: mmu::M5ExecutionContext,
    // Keep user mappings and GEM backing across handle close/VM teardown.
    // This guard's bookkeeping is generation-neutral despite its old name.
    user_mappings: Option<mmu::T8140VmJobGuard>,
    pages: KVec<MappedObject>,
    usc: M5GraphicsUsc,
    output_first: Option<usize>,
}

impl M5GraphicsMemory {
    /// Keep the actual submitting VM and allocate only driver-owned storage.
    /// Application shader/input/output mappings remain in that VM; no native
    /// program firmware or captured pages are requested by this constructor.
    pub(super) fn from_vm(dev: &AsahiDevice, uat: &mmu::Uat, vm: &mmu::Vm) -> Result<Self> {
        crate::m5_vm::compact_range(&vm.kernel_range()).ok_or(EINVAL)?;
        let user_mappings = vm.retain_t8140_job()?;
        let context = uat.prepare_t8142_execution_context(vm)?;
        let usc = M5GraphicsUsc::new_application(dev, context.vm())?;
        Ok(Self { context, user_mappings: Some(user_mappings), pages: KVec::new(), usc, output_first: None })
    }

    /// Hand the retained USC pool and this context slot to another
    /// application VM. The previous roots are retracted first (with ASID
    /// invalidation), the same hardware context is re-leased for `vm`, and
    /// the pool is aliased at unchanged addresses, so every firmware record
    /// that names the context or the pool keeps its value. The caller has
    /// verified that the previous owner's work is completely retired and
    /// publishes the new roots explicitly afterwards.
    pub(super) fn rebind(&mut self, dev: &AsahiDevice, vm: &mmu::Vm)
        -> Result<KVec<mmu::KernelMapping>> {
        crate::m5_vm::compact_range(&vm.kernel_range()).ok_or(EINVAL)?;
        // Only application owners hand over; the native diagnostic program
        // keeps captured pages in its private VM.
        if !self.pages.is_empty() || self.output_first.is_some() { return Err(ENOTSUPP); }
        let user_mappings = vm.retain_t8140_job()?;
        self.context.rebind(vm)?;
        // Release the previous VM's mapping hold only after its roots are gone.
        self.user_mappings = Some(user_mappings);
        let aliases = self.usc.alias_into(vm)?;
        dev_dbg!(dev.as_ref(), "M5_GRAPHICS_CONTEXT id={} rebound vm={} roots={:x?}; USC pool aliased\n",
            self.context.context_id(), vm.id(), self.context.roots());
        Ok(aliases)
    }

    /// Return an owning allocation in the compact TA resource arena. The
    /// render job retains it until completion or confirmed processor stop.
    pub(super) fn allocate_compact(
        &self, dev: &AsahiDevice, size: usize, alignment: u64, prot: mmu::Prot,
    ) -> Result<MappedObject> {
        let range = crate::m5_vm::compact_range(&self.vm().kernel_range()).ok_or(EINVAL)?;
        MappedObject::new_wc_in_range_with_guards(
            dev, self.vm(), range, size, alignment, prot, 2,
        )
    }

    pub(super) fn load(dev: &AsahiDevice, uat: &mmu::Uat) -> Result<Self> {
        let firmware = Firmware::request(c"asahi/m5-25g76-graphics.bin", dev.as_ref())?;
        let reference = bundle::GraphicsBundle::parse(firmware.data()).map_err(|_| EINVAL)?;
        Self::new(dev, uat, &reference)
    }

    fn new(
        dev: &AsahiDevice,
        uat: &mmu::Uat,
        reference: &bundle::GraphicsBundle<'_>,
    ) -> Result<Self> {
        let id = crate::module_parameters::m5_graphics_context.value();
        if !(3..64).contains(&id) { return Err(EINVAL); }
        let context = uat.new_t8142_graphics_execution_context(id as u8)?;
        let mut pages = KVec::new();
        let mut output_first = None;
        for index in 0..reference.page_count() {
            let input = reference.page(index).ok_or(EINVAL)?;
            let protection = if input.writable {
                mmu::PROT_GPU_SHARED_RW
            } else {
                mmu::PROT_GPU_SHARED_RO
            };
            let mut page = MappedObject::new_wc_at(
                dev,
                context.vm(),
                input.address,
                bundle::PAGE_BYTES,
                protection,
            )?;
            page.with_bytes_mut(|bytes| {
                bytes.copy_from_slice(input.bytes);
                Ok(())
            })?;
            context.vm().translate_iova(input.address)?;
            if input.address == bundle::PIXEL_ADDRESS {
                output_first = Some(index);
            }
            pages.push(page, GFP_KERNEL)?;
        }
        let usc = M5GraphicsUsc::new_application(dev, context.vm())?;
        dev_info!(dev.as_ref(), "M5_GRAPHICS_CONTEXT id={} roots={:x?}; mappings owned\n",
            context.context_id(), context.roots());
        Ok(Self {
            context,
            user_mappings: None,
            pages,
            usc,
            output_first: Some(output_first.ok_or(EINVAL)?),
        })
    }

    /// Job construction can add its own objects before root publication.
    pub(super) fn vm(&self) -> &mmu::Vm {
        self.context.vm()
    }

    pub(super) fn context_id(&self) -> u8 { self.context.context_id() }

    pub(super) fn roots(&self) -> [u64; 2] {
        self.context.roots()
    }

    pub(super) fn usc(&self) -> &M5GraphicsUsc {
        &self.usc
    }

    /// A stop request is not proof that hardware has released these mappings.
    /// Close VM_BIND admission before any future error fence can wake userspace.
    pub(super) fn quarantine(&self) {
        if let Some(guard) = &self.user_mappings {
            guard.quarantine();
        }
    }

    /// Alias existing owned user pages into the firmware VM. PM page lists
    /// and scene counters must share backing with their user GPU views.
    /// The returned mappings belong to the eventual retained job owner.
    pub(super) fn alias_kernel_pages(
        &mut self,
        uat: &mmu::Uat,
        user_address: u64,
        kernel_address: u64,
        bytes: usize,
    ) -> Result<KVec<mmu::KernelMapping>> {
        if self.context.is_current() {
            return Err(EBUSY);
        }
        if bytes == 0
            || bytes > bundle::MAX_PAGES * bundle::PAGE_BYTES
            || bytes % bundle::PAGE_BYTES != 0
            || (user_address | kernel_address) & (bundle::PAGE_BYTES as u64 - 1) != 0
        {
            return Err(EINVAL);
        }
        user_address.checked_add(bytes as u64).ok_or(EOVERFLOW)?;
        kernel_address.checked_add(bytes as u64).ok_or(EOVERFLOW)?;
        let mut indices = KVec::new();
        for offset in (0..bytes).step_by(bundle::PAGE_BYTES) {
            let address = user_address + offset as u64;
            let index = self
                .pages
                .iter()
                .position(|page| page.iova() == address)
                .ok_or(EFAULT)?;
            indices.push(index, GFP_KERNEL)?;
        }
        let mut aliases = KVec::new();
        for (ordinal, index) in indices.iter().enumerate() {
            let high = kernel_address + (ordinal * bundle::PAGE_BYTES) as u64;
            let alias =
                self.pages[*index].map_alias_at(uat.kernel_vm(), high, mmu::PROT_FW_SHARED_RW)?;
            if uat.kernel_vm().translate_iova(high)?
                != self
                    .context
                    .vm()
                    .translate_iova(self.pages[*index].iova())?
            {
                return Err(EFAULT);
            }
            aliases.push(alias, GFP_KERNEL)?;
        }
        Ok(aliases)
    }

    /// Read a word from a retained reference page while preparing its job.
    pub(super) fn read_word(&mut self, address: u64) -> Result<u32> {
        if address & 3 != 0 {
            return Err(EINVAL);
        }
        let base = address & !(bundle::PAGE_BYTES as u64 - 1);
        let page = self
            .pages
            .iter_mut()
            .find(|page| page.iova() == base)
            .ok_or(EFAULT)?;
        page.read_u32(address)
    }

    /// Reset owned PM outputs before exposing the context to either firmware.
    pub(super) fn clear_pages(&mut self, address: u64, bytes: usize) -> Result {
        if self.context.is_current() || bytes == 0 || bytes % bundle::PAGE_BYTES != 0
            || address & (bundle::PAGE_BYTES as u64 - 1) != 0 {
            return Err(EINVAL);
        }
        let end = address.checked_add(bytes as u64).ok_or(EOVERFLOW)?;
        let count = self.pages.iter().filter(|p| p.iova() >= address && p.iova() < end).count();
        if count != bytes / bundle::PAGE_BYTES { return Err(EFAULT); }
        for page in self.pages.iter_mut().filter(|p| p.iova() >= address && p.iova() < end) {
            page.with_bytes_mut(|out| { out.fill(0); Ok(()) })?;
        }
        Ok(())
    }

    pub(super) fn write_word(&mut self, address: u64, value: u32) -> Result {
        if self.context.is_current() || address & 3 != 0 { return Err(EINVAL); }
        let base = address & !(bundle::PAGE_BYTES as u64 - 1);
        let offset = (address - base) as usize;
        let page = self.pages.iter_mut().find(|p| p.iova() == base).ok_or(EFAULT)?;
        page.with_bytes_mut(|out| {
            out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            Ok(())
        })
    }

    /// Call only after the complete TA/3D resource graph is prepared.
    pub(super) fn publish(&mut self) -> Result {
        self.context.publish()
    }

    pub(super) fn is_current(&self) -> bool {
        self.context.is_current()
    }

    /// Read only after a stopped/unsubmitted graph or confirmed completion.
    /// Pixel success alone does not prove queue retirement or presentation.
    pub(super) fn result(&mut self) -> Result<bundle::TriangleResult> {
        let first = self.output_first.ok_or(ENOTSUPP)?;
        let mut result = bundle::TriangleResult::default();
        for page_index in 0..bundle::PIXEL_PAGES {
            self.pages[first + page_index].with_bytes_mut(|bytes| {
                for word in 0..bundle::PAGE_BYTES / 4 {
                    // SAFETY: word stays within this aligned, retained WC
                    // mapping. The GPU may have updated these output bytes.
                    let value = unsafe {
                        core::ptr::read_volatile(bytes.as_ptr().add(word * 4).cast::<u32>())
                    };
                    result.add(
                        page_index * (bundle::PAGE_BYTES / 4) + word,
                        value.to_le_bytes(),
                    );
                }
                Ok(())
            })?;
        }
        Ok(result)
    }

    /// The parent must first verify both queues, stamps and scheduler unlink.
    /// Keep the published context and its allocations; only erase its output.
    pub(super) fn reset_completed_output(&mut self) -> Result {
        let first = self.output_first.ok_or(ENOTSUPP)?;
        for page_index in 0..bundle::PIXEL_PAGES {
            self.pages[first + page_index].with_bytes_mut(|bytes| {
                bytes.fill(0xa5);
                Ok(())
            })?;
        }
        Ok(())
    }
}
