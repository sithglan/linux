// SPDX-License-Identifier: GPL-2.0-only OR MIT
//! Retained M5 application root leases, shared by render and compute queues,
//! with an explicitly admitted T8152 diagnostic compute lease.
//!
//! Native M5 compute has a private lower root and a distinct empty upper
//! root. Its firmware-global root is not copied into the application slot.
//! Firmware owns contexts 0..2. Application contexts are claimed before any
//! root becomes visible, and the lease retains both VM roots. The submission
//! owner must retain the lease until completion or confirmed processor stop.

use super::*;

struct M5ContextSlot {
    inner: Arc<UatInner>,
    context: usize,
}

impl Drop for M5ContextSlot {
    fn drop(&mut self) {
        // An unexpected published root is not ours to retract or recycle.
        // Keep its bit quarantined rather than hand it to another client.
        let inner = self.inner.lock();
        let slot = &inner.ttbs()[self.context];
        if slot.ttb0.load(Ordering::Acquire) == 0
            && slot.ttb1.load(Ordering::Acquire) == 0 {
            self.inner.t8142_app_contexts.fetch_and(
                !(1u64 << self.context), Ordering::AcqRel);
        }
    }
}

pub(crate) struct M5ExecutionContext {
    slot: M5ContextSlot,
    low: Vm,
    high: Vm,
    published: bool,
}

impl Uat {
    pub(crate) fn new_t8142_execution_context(&self) -> Result<M5ExecutionContext> {
        if self.cfg != UatConfig::T8142 {
            return Err(ENODEV);
        }
        let range = IOVA_USER_BASE..uat_geometry(self.cfg)?.user_va_top();
        let low = self.new_vm(0x8142_0003, range)?;
        let context = self.prepare_t8142_execution_context(&low)?;
        // The diagnostic compute queue still explicitly names context 3.
        // Application queues call the VM-based constructor directly and use
        // the returned context_id in every descriptor/channel record.
        if context.context_id() != 3 {
            return Err(EBUSY);
        }
        Ok(context)
    }

    /// Retain a DRM-created VM without copying its mappings into a diagnostic
    /// address space. Publication remains explicit and happens after the job
    /// has acquired all of its buffer and completion owners.
    pub(crate) fn prepare_t8142_execution_context(
        &self, vm: &Vm,
    ) -> Result<M5ExecutionContext> {
        self.prepare_native_execution_context_mask(vm, !0b111, UatConfig::T8142)
    }

    /// G18's 26A428 HAL400 exposes 64 context IDs (8c44c50..8c44c58).
    /// The T8152 UAT already reserves the native firmware roots in 0..2;
    /// use a separate, retained lower/empty-upper pair for compute. This
    /// claims a slot only. Root publication remains explicit and bounded by
    /// the owning G18 firmware session's lifetime.
    pub(crate) fn prepare_t8152_execution_context(
        &self, vm: &Vm,
    ) -> Result<M5ExecutionContext> {
        self.prepare_native_execution_context_mask(vm, !0b111, UatConfig::T8152)
    }

    /// Select a slot for the native-program regression test. Ordinary DRM
    /// queues use the allocator above and do not request a hardware ID.
    pub(crate) fn new_t8142_graphics_execution_context(
        &self, context: u8,
    ) -> Result<M5ExecutionContext> {
        if !(3..UAT_NUM_CTX as u8).contains(&context) {
            return Err(EINVAL);
        }
        // Exercise the same compact driver reservation as M5 Mesa. Captured
        // diagnostic inputs are kernel-owned fixed mappings, installed before
        // allocating private resources; they are not user VM_BIND mappings.
        let range = crate::m5_vm::DRIVER_BASE..crate::m5_vm::DRIVER_END;
        let vm = self.new_vm(0x8142_0000 + context as u64, range)?;
        self.prepare_native_execution_context_mask(&vm, 1u64 << context, UatConfig::T8142)
    }

    fn prepare_native_execution_context_mask(
        &self, vm: &Vm, allowed: u64, expected: UatConfig,
    ) -> Result<M5ExecutionContext> {
        if self.cfg != expected || !self.cfg.has_m5_native_topology() {
            return Err(ENODEV);
        }
        {
            let owner = vm.inner.exec_lock(None, false)?;
            if owner.is_kernel || !core::ptr::eq(
                Arc::as_ptr(&owner.uat_inner), Arc::as_ptr(&self.inner),
            ) {
                return Err(EINVAL);
            }
        }
        let context = loop {
            let used = self.inner.t8142_app_contexts.load(Ordering::Acquire);
            let free = !used & allowed & !0b111u64;
            if free == 0 {
                return Err(ENOSPC);
            }
            let index = free.trailing_zeros() as usize;
            if self.inner.t8142_app_contexts.compare_exchange_weak(
                used, used | (1u64 << index), Ordering::AcqRel, Ordering::Acquire,
            ).is_ok() {
                break index;
            }
        };
        // Construct the slot guard first so a failed private-root allocation
        // returns its claim. This high VM is never exposed for mappings.
        let slot = M5ContextSlot { inner: self.inner.clone(), context };
        let range = IOVA_USER_BASE..uat_geometry(self.cfg)?.user_va_top();
        let high = self.new_vm((u64::from(self.cfg.chip_id) << 16) + 0x1000 + context as u64, range)?;
        Ok(M5ExecutionContext { slot, low: vm.clone(), high, published: false })
    }
}

impl M5ExecutionContext {
    pub(crate) fn vm(&self) -> &Vm { &self.low }

    pub(crate) fn context_id(&self) -> u8 { self.slot.context as u8 }

    pub(crate) fn matches(&self, vm: &Vm) -> bool {
        core::ptr::eq(&*self.low.inner, &*vm.inner)
    }

    pub(crate) fn roots(&self) -> [u64; 2] {
        let tag = (self.slot.context as u64) << TTBR_ASID_SHIFT | TTBR_VALID;
        [self.low.ttb() | tag, self.high.ttb() | tag]
    }

    /// Publish only after all command-owned mappings have been prepared.
    /// An occupied slot is never replaced, even by a second M5 probe owner.
    pub(crate) fn publish(&mut self) -> Result {
        if self.published { return Err(EBUSY); }
        let roots = self.roots();
        {
            let inner = self.slot.inner.lock();
            let slot = &inner.ttbs()[self.slot.context];
            if slot.ttb0.load(Ordering::Acquire) != 0
                || slot.ttb1.load(Ordering::Acquire) != 0 {
                return Err(EBUSY);
            }
            slot.ttb0.store(roots[0], Ordering::Release);
            slot.ttb1.store(roots[1], Ordering::Release);
            self.published = true;
        }
        fence(Ordering::SeqCst);
        mem::tlbi_asid(self.context_id());
        mem::sync();
        Ok(())
    }

    pub(crate) fn is_current(&self) -> bool {
        if !self.published { return false; }
        let roots = self.roots();
        let inner = self.slot.inner.lock();
        let slot = &inner.ttbs()[self.slot.context];
        slot.ttb0.load(Ordering::Acquire) == roots[0]
            && slot.ttb1.load(Ordering::Acquire) == roots[1]
    }

    /// Retarget this lease at another application VM without giving up the
    /// hardware context slot. Any published roots are retracted and the ASID
    /// invalidated first, exactly as on drop; the caller has already verified
    /// that no work of the previous VM is outstanding. Publication of the new
    /// lower root is explicit again, once the new owner's graph is prepared.
    pub(crate) fn rebind(&mut self, vm: &Vm) -> Result {
        {
            let owner = vm.inner.exec_lock(None, false)?;
            if owner.is_kernel || !core::ptr::eq(
                Arc::as_ptr(&owner.uat_inner), Arc::as_ptr(&self.slot.inner),
            ) {
                return Err(EINVAL);
            }
        }
        self.retract();
        self.low = vm.clone();
        Ok(())
    }

    fn retract(&mut self) {
        if !self.published { return; }
        let roots = self.roots();
        {
            let inner = self.slot.inner.lock();
            let slot = &inner.ttbs()[self.slot.context];
            // Only our exact pair may be retracted. VM owners remain alive
            // through root invalidation and the final ASID synchronization.
            if slot.ttb0.load(Ordering::Acquire) == roots[0]
                && slot.ttb1.load(Ordering::Acquire) == roots[1] {
                slot.ttb0.store(0, Ordering::Release);
                slot.ttb1.store(0, Ordering::Release);
            }
        }
        fence(Ordering::SeqCst);
        mem::tlbi_asid(self.context_id());
        mem::sync();
        self.published = false;
    }
}

impl Drop for M5ExecutionContext {
    fn drop(&mut self) {
        self.retract();
    }
}

impl VmInner {
    /// M5 application roots are leased independently of the legacy binding
    /// slot. Flush every currently published view of this VM after changing
    /// its leaves, including cached negative translations after a new bind.
    /// The VM exec lock is held by all callers; publication does its own full
    /// ASID invalidation if it races this snapshot.
    pub(super) fn flush_m5_app_range(&self, address: u64, size: usize) {
        if self.is_kernel || size == 0 {
            return;
        }
        let used = self.uat_inner.t8142_app_contexts.load(Ordering::Acquire) & !0b111;
        if used == 0 {
            return;
        }
        let root = self.page_table.ttb();
        let contexts = {
            let inner = self.uat_inner.lock();
            let mut contexts = 0u64;
            for context in 3..UAT_NUM_CTX {
                let tagged = root | TTBR_VALID | (context as u64) << TTBR_ASID_SHIFT;
                if used & (1u64 << context) != 0
                    && inner.ttbs()[context].ttb0.load(Ordering::Acquire) == tagged {
                    contexts |= 1u64 << context;
                }
            }
            contexts
        };
        if contexts != 0 {
            fence(Ordering::SeqCst);
            for context in 3..UAT_NUM_CTX {
                if contexts & (1u64 << context) != 0 {
                    mem::tlbi_range(context as u8, address as usize, size);
                }
            }
            mem::sync();
        }
    }
}
