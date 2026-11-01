// SPDX-License-Identifier: GPL-2.0-only OR MIT

//! Asahi driver GEM object implementation
//!
//! Basic wrappers and adaptations between generic GEM shmem objects and this driver's
//! view of what a GPU buffer object is. It is in charge of keeping track of all mappings for
//! each GEM object so we can remove them when a client (File) or a Vm are destroyed, as well as
//! implementing RTKit buffers on top of GEM objects for firmware use.

use kernel::{
    drm,
    drm::gem::{
        shmem,
        shmem::VMap,
        BaseObject,
        DriverObject, //
    },
    error::Result,
    iosys_map::IoSysMapRef,
    prelude::*,
    sync::aref::ARef,
    uapi, //
};

use core::ops::Range;
use core::sync::atomic::{
    AtomicU64,
    Ordering, //
};

use crate::{
    debug::*,
    driver::{
        AsahiDevice,
        AsahiDriver, //
    },
    file,
    mmu,
    util::*, //
};

const DEBUG_CLASS: DebugFlags = DebugFlags::Gem;

/// Represents the inner data of a GEM object for this driver.
#[pin_data(PinnedDrop)]
pub(crate) struct AsahiObject {
    /// ID for debug
    id: u64,
    /// Object size in bytes, for live-object accounting.
    size: usize,
    /// Object creation flags.
    flags: u32,
    /// Whether this object can be exported.
    exportable: bool,
    /// Whether this is a kernel-created object.
    kernel: bool,
}

impl AsahiObject {
    /// Guard-tail aliases are restricted to driver-owned shared backing.
    pub(crate) fn is_kernel_object(&self) -> bool { self.kernel }
}

/// Type alias for the shmem GEM object type for this driver.
pub(crate) type Object = shmem::Object<AsahiObject>;

unsafe impl Send for AsahiObject {}
unsafe impl Sync for AsahiObject {}

// /// Type alias for the SGTable type for this driver.
// pub(crate) type SGTable = shmem::SGTable<AsahiObject>;

/// A shared reference to a GEM object for this driver.
pub(crate) struct ObjectRef {
    /// The underlying GEM object reference
    pub(crate) gem: ARef<Object>,
    /// The kernel-side VMap of this object, if needed
    vmap: Option<VMap<AsahiObject, u8>>,
}

crate::no_debug!(ObjectRef);

static GEM_ID: AtomicU64 = AtomicU64::new(0);

impl ObjectRef {
    /// Create a new wrapper for a raw GEM object reference.
    pub(crate) fn new(gem: ARef<Object>) -> ObjectRef {
        ObjectRef { gem, vmap: None }
    }

    /// Return the `VMap` for this object, creating it if necessary.
    ///
    /// The owned mapping lives as long as this reference, so hand out a view
    /// of it directly. Going back through `gem.vmap()` took and dropped the
    /// object's reservation lock and vmap refcount on every access, which the
    /// G17P firmware-memory accessors do hundreds of times per submission.
    pub(crate) fn vmap(&mut self) -> Result<IoSysMapRef<'_, u8>> {
        if self.vmap.is_none() {
            self.vmap = Some(self.gem.owned_vmap()?);
        }
        Ok(self.vmap.as_ref().ok_or(EINVAL)?.get())
    }

    /// Returns the size of an object in bytes
    pub(crate) fn size(&self) -> usize {
        self.gem.size()
    }

    /// Maps an object into a given `Vm` at any free address within a given range.
    pub(crate) fn map_into_range(
        &mut self,
        vm: &crate::mmu::Vm,
        range: Range<u64>,
        alignment: u64,
        prot: mmu::Prot,
        guard: bool,
    ) -> Result<crate::mmu::KernelMapping> {
        // Only used for kernel objects now
        if !self.gem.kernel {
            return Err(EINVAL);
        }
        vm.map_in_range(&self.gem, 0..self.gem.size(), alignment, range, prot, guard)
    }

    /// Maps a range within an object into a given `Vm` at any free address within a given range.
    pub(crate) fn map_range_into_range(
        &mut self,
        vm: &crate::mmu::Vm,
        obj_range: Range<usize>,
        range: Range<u64>,
        alignment: u64,
        prot: mmu::Prot,
        guard: bool,
    ) -> Result<crate::mmu::KernelMapping> {
        if obj_range.end > self.gem.size() {
            return Err(EINVAL);
        }
        if self.gem.flags & uapi::drm_asahi_gem_flags_DRM_ASAHI_GEM_VM_PRIVATE != 0
            && vm.is_extobj(&*self.gem)
        {
            return Err(EINVAL);
        }
        vm.map_in_range(&self.gem, obj_range, alignment, range, prot, guard)
    }

    /// Maps an object into a given `Vm` at a specific address.
    ///
    /// Returns Err(ENOSPC) if the requested address is already busy.
    pub(crate) fn map_at(
        &mut self,
        vm: &crate::mmu::Vm,
        addr: u64,
        prot: mmu::Prot,
        guard: bool,
    ) -> Result<crate::mmu::KernelMapping> {
        if self.gem.flags & uapi::drm_asahi_gem_flags_DRM_ASAHI_GEM_VM_PRIVATE != 0
            && vm.is_extobj(&*self.gem)
        {
            return Err(EINVAL);
        }

        vm.map_at(addr, self.gem.size(), self.gem.clone(), prot, guard)
    }

    /// Map at a specific address, reserving `guard_pages` unmapped pages
    /// after the object in that Vm's allocator.
    pub(crate) fn map_at_with_guards(
        &mut self,
        vm: &crate::mmu::Vm,
        addr: u64,
        prot: mmu::Prot,
        guard_pages: usize,
    ) -> Result<crate::mmu::KernelMapping> {
        if self.gem.flags & uapi::drm_asahi_gem_flags_DRM_ASAHI_GEM_VM_PRIVATE != 0
            && vm.is_extobj(&*self.gem)
        {
            return Err(EINVAL);
        }

        vm.map_at_with_guards(addr, self.gem.size(), self.gem.clone(), prot, guard_pages)
    }
}

#[pinned_drop]
impl PinnedDrop for AsahiObject {
    fn drop(self: Pin<&mut Self>) {
        crate::gem_stats::note_free(self.kernel, self.size);
    }
}

pub(crate) struct AsahiObjConfig {
    flags: u32,
    exportable: bool,
    kernel: bool,
}

fn new_kernel_object_with_cpu_mapping(
    dev: &AsahiDevice,
    size: usize,
    map_wc: bool,
) -> Result<ObjectRef> {
    let gem = shmem::Object::<AsahiObject>::new(
        dev,
        align(size, mmu::UAT_PGSZ),
        shmem::ObjectConfig::<AsahiObject> {
            map_wc,
            parent_resv_obj: None,
        },
        AsahiObjConfig {
            flags: 0,
            exportable: false,
            kernel: true,
        },
    )?;

    mod_pr_debug!("AsahiObject new kernel object id={}\n", gem.id);
    Ok(ObjectRef::new(gem))
}

/// Create a new kernel-owned GEM object with a write-back CPU mapping.
pub(crate) fn new_kernel_object(dev: &AsahiDevice, size: usize) -> Result<ObjectRef> {
    new_kernel_object_with_cpu_mapping(dev, size, false)
}

/// Create a new kernel-owned GEM object with a write-combined CPU mapping.
///
/// Firmware-visible producer rings use this form when the hardware consumes
/// host stores immediately after an ordering barrier. It matches the
/// uncached channel-memory allocation used by Apple's producer and avoids a
/// dirty write-back alias surviving past the device doorbell.
pub(crate) fn new_kernel_object_wc(dev: &AsahiDevice, size: usize) -> Result<ObjectRef> {
    new_kernel_object_with_cpu_mapping(dev, size, true)
}

/// Create a new user-owned GEM object with the given flags.
pub(crate) fn new_object(
    dev: &AsahiDevice,
    size: usize,
    flags: u32,
    parent_object: Option<&shmem::Object<AsahiObject>>,
) -> Result<ARef<Object>> {
    if (flags & uapi::drm_asahi_gem_flags_DRM_ASAHI_GEM_VM_PRIVATE != 0) != parent_object.is_some()
    {
        return Err(EINVAL);
    }

    let gem = shmem::Object::<AsahiObject>::new(
        dev,
        align(size, mmu::UAT_PGSZ),
        shmem::ObjectConfig::<AsahiObject> {
            map_wc: flags & uapi::drm_asahi_gem_flags_DRM_ASAHI_GEM_WRITEBACK == 0,
            parent_resv_obj: parent_object,
        },
        AsahiObjConfig {
            flags,
            exportable: parent_object.is_none(),
            kernel: false,
        },
    )?;

    mod_pr_debug!("AsahiObject new user object: id={}\n", gem.id);
    Ok(gem)
}

#[vtable]
impl DriverObject for AsahiObject {
    type Driver = AsahiDriver;
    type Args = AsahiObjConfig;

    const HAS_EXPORT: bool = true;
    const PRIME_IMPORT: Option<drm::gem::PrimeImportFn> =
        Some(crate::g18_drm::asahi_gem_prime_import);

    /// Callback to create the inner data of a GEM object
    fn new(_dev: &AsahiDevice, size: usize, args: Self::Args) -> impl PinInit<Self, Error> {
        let id = GEM_ID.fetch_add(1, Ordering::Relaxed);
        mod_pr_debug!("AsahiObject::new id={}\n", id);
        try_pin_init!(AsahiObject {
            id,
            // Counted inside the initializer, so an object whose allocation fails before
            // this runs is never counted; once it has run, PinnedDrop balances it.
            size: {
                crate::gem_stats::note_new(args.kernel, size);
                size
            },
            flags: args.flags,
            exportable: args.exportable,
            kernel: args.kernel,
        })
    }

    /// Callback to drop all mappings for a GEM object owned by a given `File`
    fn close(obj: &<Self::Driver as drm::Driver>::Object, file: &drm::gem::DriverFile<Self>) {
        // fn close(obj: &Object, file: &DrmFile) {
        mod_pr_debug!("AsahiObject::close id={}\n", obj.id);
        if file::File::unbind_gem_object(file, obj).is_err() {
            pr_err!("AsahiObject::close: Failed to unbind GEM object\n");
        }
    }

    /// Optional handle for exporting a gem object.
    fn export(
        obj: &<Self::Driver as drm::Driver>::Object,
        flags: u32,
    ) -> Result<drm::gem::DmaBuf<Object>> {
        if !obj.exportable {
            return Err(EINVAL);
        }

        obj.prime_export(flags)
    }
}
