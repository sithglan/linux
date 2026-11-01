// SPDX-License-Identifier: GPL-2.0-only OR MIT
#![allow(clippy::unusual_byte_groupings)]

//! File implementation, which represents a single DRM client.
//!
//! This is in charge of managing the resources associated with one GPU client, including an
//! arbitrary number of submission queues and Vm objects, and reporting hardware/driver
//! information to userspace and accepting submissions.

use crate::debug::*;
use crate::driver::AsahiDevice;
use crate::{
    alloc,
    buffer,
    driver,
    gem,
    mmu,
    module_parameters,
    queue,
    util::{
        align,
        align_down,
        gcd,
        AnyBitPattern,
        RangeExt,
        Reader, //
    }, //
};
use core::mem::MaybeUninit;
use core::ops::Deref;
use core::ops::Range;
use core::ptr::addr_of_mut;
use kernel::bindings;
use kernel::dma_fence::RawDmaFence;
use kernel::drm::gem::BaseObject;
use kernel::error::code::*;
use kernel::new_mutex;
use kernel::prelude::*;
use kernel::sync::{
    aref::ARef,
    Arc,
    Mutex, //
};
use kernel::time::NSEC_PER_SEC;
use kernel::uaccess::{
    UserPtr,
    UserSlice, //
};
use kernel::{
    c_str,
    dma_fence,
    drm,
    of,
    uapi,
    xarray, //
};

const DEBUG_CLASS: DebugFlags = DebugFlags::File;

pub(crate) const MAX_COMMANDS_PER_SUBMISSION: u32 = 64;

/// Resolve the exact visible iBoot framebuffer aperture from DT.
///
/// This deliberately accepts only the native Neo simplefb layout and requires
/// it to be contained in a `no-map` reserved-memory framebuffer node. The
/// returned size is the page-aligned visible image, never the larger DCP
/// reservation. The last page may extend past the byte-exact simplefb aperture,
/// but only when that complete page remains inside the no-map framebuffer
/// reservation. No CPU mapping or display/DCP operation is performed here.
fn t8140_simplefb_phys_range() -> Result<(usize, usize)> {
    let chosen = of::chosen().ok_or(ENOENT)?;
    let mut candidate = None;

    for node in chosen.children() {
        let compatible: KVec<u8> = match node.get_property(c_str!("compatible")) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if compatible.as_slice()
            != b"apple,simple-framebuffer\0simple-framebuffer\0"
        {
            continue;
        }
        if candidate.is_some() {
            return Err(EINVAL);
        }

        let reg: KVec<u64> = node.get_property(c_str!("reg"))?;
        let width: u32 = node.get_property(c_str!("width"))?;
        let height: u32 = node.get_property(c_str!("height"))?;
        let stride: u32 = node.get_property(c_str!("stride"))?;
        let format: KVec<u8> = node.get_property(c_str!("format"))?;
        if reg.len() != 2
            || format.as_slice() != b"x8r8g8b8\0"
            || width == 0
            || height == 0
            || stride < width.checked_mul(4).ok_or(EOVERFLOW)?
        {
            return Err(EINVAL);
        }

        let visible = usize::try_from(stride)?
            .checked_mul(usize::try_from(height)?)
            .ok_or(EOVERFLOW)?;
        let mapped = align(visible as u64, mmu::UAT_PGSZ as u64);
        let base = usize::try_from(reg[0])?;
        let size = usize::try_from(reg[1])?;
        let mapped = usize::try_from(mapped)?;
        if base & mmu::UAT_PGMSK != 0 || visible > size {
            return Err(EINVAL);
        }
        candidate = Some((base, mapped));
    }

    let (base, mapped) = candidate.ok_or(ENOENT)?;
    let root = of::root().ok_or(ENOENT)?;
    let mut reserved = None;
    for node in root.children() {
        let name: KVec<u8> = match node.get_property(c_str!("name")) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if name.as_slice() == b"reserved-memory\0" {
            if reserved.replace(node).is_some() {
                return Err(EINVAL);
            }
        }
    }
    let reserved = reserved.ok_or(ENOENT)?;
    let target_end = base.checked_add(mapped).ok_or(EOVERFLOW)?;
    let mut covered = false;
    for node in reserved.children() {
        let compatible: KVec<u8> = match node.get_property(c_str!("compatible")) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let no_map: KVec<u8> = match node.get_property(c_str!("no-map")) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if compatible.as_slice() != b"framebuffer\0" || !no_map.is_empty() {
            continue;
        }
        let reg: KVec<u64> = node.get_property(c_str!("reg"))?;
        if reg.len() != 2 {
            return Err(EINVAL);
        }
        let reserve_base = usize::try_from(reg[0])?;
        let reserve_size = usize::try_from(reg[1])?;
        let reserve_end = reserve_base.checked_add(reserve_size).ok_or(EOVERFLOW)?;
        if reserve_base <= base && target_end <= reserve_end {
            if covered {
                return Err(EINVAL);
            }
            covered = true;
        }
    }
    if !covered {
        return Err(EINVAL);
    }
    Ok((base, mapped))
}

/// A client instance of an `mmu::Vm` address space.
struct Vm {
    ualloc: Arc<Mutex<alloc::DefaultAllocator>>,
    ualloc_priv: Arc<Mutex<alloc::DefaultAllocator>>,
    vm: mmu::Vm,
    kernel_range: Range<u64>,
    /// The usable user VA range for this device's UAT geometry
    /// (`mmu::iova_user_usable_range`), captured at creation.
    user_range: Range<u64>,
    _dummy_mapping: mmu::KernelMapping,
}

enum PreparedGemBindOp {
    Map {
        op: uapi::drm_asahi_gem_bind_op,
        bo: ARef<gem::Object>,
    },
    Unmap {
        op: uapi::drm_asahi_gem_bind_op,
    },
}

impl Drop for Vm {
    fn drop(&mut self) {
        if self
            .vm
            .unmap_user_ranges(self.user_range.clone(), self.kernel_range.clone())
            .is_err()
        {
            pr_err!("Vm::Drop: vm.unmap_user_ranges() failed\n");
        }

        self.vm.bo_deferred_cleanup();
    }
}

/// Sync object from userspace.
pub(crate) struct SyncItem {
    pub(crate) syncobj: drm::syncobj::SyncObj,
    pub(crate) fence: Option<dma_fence::Fence>,
    pub(crate) chain_fence: Option<dma_fence::FenceChain>,
    pub(crate) timeline_value: u64,
}

impl SyncItem {
    fn parse_one(file: &DrmFile, data: uapi::drm_asahi_sync, out: bool) -> Result<SyncItem> {
        match data.sync_type {
            uapi::drm_asahi_sync_type_DRM_ASAHI_SYNC_SYNCOBJ => {
                if data.timeline_value != 0 {
                    cls_pr_debug!(Errors, "Non-timeline sync object with a nonzero value\n");
                    return Err(EINVAL);
                }
                let syncobj = drm::syncobj::SyncObj::lookup_handle(file, data.handle)?;

                Ok(SyncItem {
                    fence: if out {
                        None
                    } else {
                        Some(syncobj.fence_get().ok_or_else(|| {
                            cls_pr_debug!(Errors, "Failed to get fence from sync object\n");
                            EINVAL
                        })?)
                    },
                    syncobj,
                    chain_fence: None,
                    timeline_value: data.timeline_value,
                })
            }
            uapi::drm_asahi_sync_type_DRM_ASAHI_SYNC_TIMELINE_SYNCOBJ => {
                let syncobj = drm::syncobj::SyncObj::lookup_handle(file, data.handle)?;
                let fence = if out {
                    None
                } else {
                    syncobj
                        .fence_get()
                        .ok_or_else(|| {
                            cls_pr_debug!(
                                Errors,
                                "Failed to get fence from timeline sync object\n"
                            );
                            EINVAL
                        })?
                        .chain_find_seqno(data.timeline_value)?
                };

                Ok(SyncItem {
                    fence,
                    syncobj,
                    chain_fence: if out {
                        Some(dma_fence::FenceChain::new()?)
                    } else {
                        None
                    },
                    timeline_value: data.timeline_value,
                })
            }
            _ => {
                cls_pr_debug!(Errors, "Invalid sync type {}\n", data.sync_type);
                Err(EINVAL)
            }
        }
    }

    fn parse_array(
        file: &DrmFile,
        ptr: u64,
        in_count: u32,
        out_count: u32,
    ) -> Result<KVec<SyncItem>> {
        let count = in_count + out_count;
        let mut vec = KVec::with_capacity(count as usize, GFP_KERNEL)?;

        const STRIDE: usize = core::mem::size_of::<uapi::drm_asahi_sync>();
        let size = STRIDE * count as usize;

        // SAFETY: We only read this once, so there are no TOCTOU issues.
        let mut reader = UserSlice::new(UserPtr::from_addr(ptr as _), size).reader();

        for i in 0..count {
            let mut sync: MaybeUninit<uapi::drm_asahi_sync> = MaybeUninit::uninit();

            // SAFETY: The size of `sync` is STRIDE
            reader.read_raw(unsafe {
                core::slice::from_raw_parts_mut(sync.as_mut_ptr() as *mut MaybeUninit<u8>, STRIDE)
            })?;

            // SAFETY: All bit patterns in the struct are valid
            let sync = unsafe { sync.assume_init() };

            vec.push(SyncItem::parse_one(file, sync, i >= in_count)?, GFP_KERNEL)?;
        }

        Ok(vec)
    }
}

#[derive(Clone)]
pub(crate) enum Object {
    TimestampBuffer(Arc<mmu::KernelMapping>),
}

/// State associated with a client.
// #[pin_data]
pub(crate) struct File {
    id: u64,
    // #[pin]
    vms: xarray::XArray<KBox<Vm>>,
    // #[pin]
    queues: xarray::XArray<Arc<Mutex<KBox<dyn queue::Queue>>>>,
    // #[pin]
    objects: xarray::XArray<KBox<Object>>,
}

/// Convenience type alias for our DRM `File` type.
pub(crate) type DrmFile = drm::File<File>;

// The DRM adapter supplies the per-device user range and reserved dummy page.

/// Minimum reserved AS for kernel mappings
const VM_KERNEL_MIN_SIZE: u64 = 0x20000000;

impl drm::file::DriverFile for File {
    type Driver = driver::AsahiDriver;

    /// Create a new `File` instance for a fresh client.
    fn open(device: &AsahiDevice) -> Result<Pin<KBox<Self>>> {
        debug::update_debug_flags();

        let gpu = &device.gpu;
        let id = gpu.ids().file.next();

        mod_dev_dbg!(device, "[File {}]: DRM device opened\n", id);
        Ok(KBox::pin_init(File::new(id), GFP_KERNEL)?)
    }

    fn as_raw(&self) -> *mut bindings::drm_file {
        todo!()
    }
}

// SAFETY: All bit patterns are valid by construction.
unsafe impl AnyBitPattern for uapi::drm_asahi_gem_bind_op {}

impl File {
    fn new(id: u64) -> impl PinInit<Self, Error> {
        unsafe {
            pin_init::pin_init_from_closure(move |slot: *mut Self| {
                let raw_vms = addr_of_mut!((*slot).vms);
                xarray::XArray::<KBox<Vm>>::new(xarray::AllocKind::Alloc1)
                    .__init(raw_vms)?;

                let raw_queues = addr_of_mut!((*slot).queues);
                xarray::XArray::<Arc<Mutex<KBox<dyn queue::Queue>>>>::new(
                    xarray::AllocKind::Alloc1,
                )
                .__init(raw_queues)?;

                let raw_objects = addr_of_mut!((*slot).objects);
                xarray::XArray::<KBox<Object>>::new(xarray::AllocKind::Alloc1)
                    .__init(raw_objects)?;

                (*slot).id = id;
                Ok(())
            })
        }
    }

    fn vms(self: Pin<&Self>) -> Pin<&xarray::XArray<KBox<Vm>>> {
        // SAFETY: Structural pinned projection for vms.
        // We never move out of this field.
        unsafe { self.map_unchecked(|s| &s.vms) }
    }

    #[allow(clippy::type_complexity)]
    fn queues(self: Pin<&Self>) -> Pin<&xarray::XArray<Arc<Mutex<KBox<dyn queue::Queue>>>>> {
        // SAFETY: Structural pinned projection for queues.
        // We never move out of this field.
        unsafe { self.map_unchecked(|s| &s.queues) }
    }

    fn objects(self: Pin<&Self>) -> Pin<&xarray::XArray<KBox<Object>>> {
        // SAFETY: Structural pinned projection for objects.
        // We never move out of this field.
        unsafe { self.map_unchecked(|s| &s.objects) }
    }

    /// IOCTL: get_param: Get a driver parameter value.
    pub(crate) fn get_params(
        device: &AsahiDevice,
        data: &uapi::drm_asahi_get_params,
        file: &DrmFile,
    ) -> Result<u32> {
        mod_dev_dbg!(device, "[File {}]: IOCTL: get_params\n", file.inner().id);

        let gpu = &device.gpu;

        if data.param_group == crate::g17_status::PARAM_GROUP_VM_STATUS {
            return Self::get_vm_status(device, data, file);
        }
        if data.param_group == crate::g17_status::PARAM_GROUP_VM_STATUS_MIRROR {
            return Self::register_vm_status_mirror(device, data, file);
        }
        if data.param_group == crate::g17_queue_limits::PARAM_GROUP_QUEUE_LIMITS {
            if data.pad != 0 || data.size < crate::g17_queue_limits::QUEUE_LIMITS_SIZE as u64 {
                return Err(EINVAL);
            }
            let bytes = gpu.independent_queue_limits().ok_or(EINVAL)?;
            UserSlice::new(UserPtr::from_addr(data.pointer as _), bytes.len())
                .writer().write_slice(&bytes)?;
            return Ok(0);
        }
        if data.param_group != 0 || data.pad != 0 {
            cls_pr_debug!(Errors, "get_params: Invalid arguments\n");
            return Err(EINVAL);
        }

        if gpu.is_crashed() {
            return Err(ENODEV);
        }

        let facts = gpu.params()?;
        let mut params = uapi::drm_asahi_params_global {
            features: 0,

            gpu_generation: facts.gpu_generation,
            gpu_variant: facts.gpu_variant,
            gpu_revision: facts.gpu_revision,
            chip_id: facts.chip_id,

            num_dies: facts.num_dies,
            num_clusters_total: facts.num_clusters_total,
            num_cores_per_cluster: facts.num_cores_per_cluster,
            core_masks: facts.core_masks.map(|mask| mask.into()),

            vm_start: gpu.user_range()?.start,
            vm_end: gpu.user_range()?.end,
            vm_kernel_min_size: VM_KERNEL_MIN_SIZE,

            max_commands_per_submission: facts.max_commands_per_submission,
            max_attachments: crate::microseq::MAX_ATTACHMENTS as u32,
            max_frequency_khz: facts.max_frequency_khz,

            command_timestamp_frequency_hz: 1_000_000_000, // User timestamps always in nanoseconds

            usc_generation: facts.usc_generation,
            gpu_hal_generation: facts.gpu_hal_generation,
        };

        if gpu.supports_vm_status() {
            params.features |= crate::g17_status::FEATURE_VM_STATUS;
            if gpu.supports_vm_status_mirror() {
                params.features |= crate::g17_status::FEATURE_VM_STATUS_MIRROR;
            }
        }
        if gpu.supports_per_command_input_syncs() {
            params.features |= crate::g17_queue_limits::FEATURE_PER_COMMAND_INPUT_SYNCS;
        }
        if gpu.supports_exact_prior_barriers() {
            params.features |=
                uapi::drm_asahi_feature_DRM_ASAHI_FEATURE_EXACT_PRIOR_BARRIERS as u64;
        }
        if gpu.independent_queue_limits().is_some() {
            params.features |= crate::g17_queue_limits::FEATURE_INDEPENDENT_QUEUES;
        }
        if gpu.supports_g18_graphics_scratch() {
            // DRM_ASAHI_FEATURE_G18_GRAPHICS_SCRATCH, spelled as its value:
            // the incremental module build reuses pinned bindgen output.
            params.features |= 1u64 << 8;
        }
        if gpu.supports_g18_native_tile_samples() {
            // DRM_ASAHI_FEATURE_G18_NATIVE_TILE_SAMPLES, spelled as its value.
            params.features |= 1u64 << 9;
        }
        if module_parameters::fault_control.value() == 0xb {
            params.features |= uapi::drm_asahi_feature_DRM_ASAHI_FEATURE_SOFT_FAULTS as u64;
        }

        let size = core::mem::size_of::<uapi::drm_asahi_params_global>().min(data.size.try_into()?);

        // SAFETY: We only write to this userptr once, so there are no TOCTOU issues.
        let mut params_writer =
            UserSlice::new(UserPtr::from_addr(data.pointer as _), size).writer();

        // SAFETY: `size` is at most the sizeof of `params`
        params_writer.write_slice(unsafe {
            core::slice::from_raw_parts(&params as *const _ as *const u8, size)
        })?;

        Ok(0)
    }

    /// Status queries never enter firmware recovery or wait for a GPU job.
    /// File-local lookup enforces VM ownership. Status survives queue teardown.
    fn get_vm_status(device: &AsahiDevice, data: &uapi::drm_asahi_get_params,
                     file: &DrmFile) -> Result<u32> {
        use crate::g17_status::{decode_request, encode_response, VM_STATUS_SIZE};
        if !device.gpu.supports_vm_status() { return Err(EINVAL); }
        if data.pad != 0 || data.size < VM_STATUS_SIZE as u64 { return Err(EINVAL); }
        let user = UserPtr::from_addr(data.pointer as _);
        let mut bytes = [0u8; VM_STATUS_SIZE];
        UserSlice::new(user, VM_STATUS_SIZE).reader().read_slice(&mut bytes)?;
        let vm_id = decode_request(&bytes).ok_or(EINVAL)?;
        let status = {
            let vms = file.inner().vms();
            let guard = vms.lock();
            let vm = guard.get(vm_id as usize).ok_or(ENOENT)?;
            vm.vm.status().clone()
        };
        /* A status QUERY must not be what kills a healthy VM. Name it when it
         * does: `submission_error()` is device-wide, so recording it here
         * poisons every client that merely asks whether it is alive, and the
         * poison then surfaces as `stage=vm-status` on the next submit with no
         * other kernel diagnostic. Measured on G17P 2026-09-18: a brand-new
         * client reported DEVICE_LOST with zero kernel lines attributable to
         * it. */
        let device_error = device.gpu.submission_error();
        let before = status.get();
        status.record(device_error);
        if before == 0 && status.get() != 0 {
            dev_err!(
                device.as_ref(),
                "G17PMARK vm-status poisoned by QUERY: vm {} was clean, device submission_error={} -- this client never submitted anything\n",
                vm_id,
                device_error,
            );
        }
        let bytes = encode_response(vm_id, status.get());
        UserSlice::new(user, VM_STATUS_SIZE).writer().write_slice(&bytes)?;
        Ok(0)
    }

    /// v87: register a userspace word mirroring this file's VM status, so a
    /// successful syncobj wait can be checked with a load instead of a group
    /// 1 query per wait. Refused (EOPNOTSUPP) unless the mirror is exact and
    /// `g17p_host_path` bit8 is set; userspace then keeps querying.
    fn register_vm_status_mirror(device: &AsahiDevice, data: &uapi::drm_asahi_get_params,
                                 file: &DrmFile) -> Result<u32> {
        use crate::g17_status::{decode_mirror_request, VM_STATUS_MIRROR_SIZE};
        if !device.gpu.supports_vm_status() || !device.gpu.supports_vm_status_mirror()
            || !crate::g17_host_path::enabled(crate::g17_host_path::VM_STATUS_MIRROR)
        {
            // EOPNOTSUPP; the kernel crate declares no constant for it.
            return Err(Error::from_errno(-95));
        }
        if data.pad != 0 || data.size < VM_STATUS_MIRROR_SIZE as u64 { return Err(EINVAL); }
        let mut bytes = [0u8; VM_STATUS_MIRROR_SIZE];
        UserSlice::new(UserPtr::from_addr(data.pointer as _), VM_STATUS_MIRROR_SIZE)
            .reader().read_slice(&mut bytes)?;
        let (vm_id, handle, offset) = decode_mirror_request(&bytes).ok_or(EINVAL)?;
        let status = {
            let vms = file.inner().vms();
            let guard = vms.lock();
            let vm = guard.get(vm_id as usize).ok_or(ENOENT)?;
            vm.vm.status().clone()
        };
        let mut owner = gem::ObjectRef::new(gem::Object::lookup_handle(file, handle)?);
        let offset = offset as usize;
        // The kernel keeps this object vmapped for the status lifetime, so
        // bound it (userspace needs one word; Mesa passes a 16 KiB object).
        const MIRROR_MAX_OBJECT: usize = 64 * 1024;
        if owner.size() > MIRROR_MAX_OBJECT
            || offset.checked_add(4).is_none_or(|end| end > owner.size())
        {
            return Err(EINVAL);
        }
        // The owned kernel mapping lives in `owner`, which the status retains;
        // moving `owner` does not move the mapping.
        let base = owner.vmap()?.as_mut_ptr();
        // SAFETY: offset + 4 lies within the object, checked above.
        let word = unsafe { base.add(offset) }.cast::<i32>();
        status.attach_mirror(owner, word)?;
        crate::g17_host_path::count(&crate::g17_host_path::MIRRORS);
        Ok(0)
    }

    /// IOCTL: vm_create: Create a new `Vm`.
    pub(crate) fn vm_create(
        device: &AsahiDevice,
        data: &mut uapi::drm_asahi_vm_create,
        file: &DrmFile,
    ) -> Result<u32> {
        let kernel_range = data.kernel_start..data.kernel_end;
        let user_range = device.gpu.user_range()?;

        // Validate requested kernel range
        if !user_range.is_superset(kernel_range.clone())
            || kernel_range.range() < VM_KERNEL_MIN_SIZE
            || kernel_range.start & (mmu::UAT_PGMSK as u64) != 0
            || kernel_range.end & (mmu::UAT_PGMSK as u64) != 0
        {
            cls_pr_debug!(Errors, "vm_create: Invalid kernel range\n");
            return Err(EINVAL);
        }

        // Align to buffer::PAGE_SIZE so the allocators are happy
        let kernel_range = align(kernel_range.start, buffer::PAGE_SIZE as u64)
            ..align_down(kernel_range.end, buffer::PAGE_SIZE as u64);

        let kernel_half_size = align_down(kernel_range.range() >> 1, buffer::PAGE_SIZE as u64);
        let kernel_gpu_range = kernel_range.start..(kernel_range.start + kernel_half_size);
        let kernel_gpufw_range = kernel_gpu_range.end..kernel_range.end;

        let gpu = &device.gpu;
        let file_id = file.inner().id;
        let vm = gpu.new_vm(kernel_range.clone())?;
        if module_parameters::g17p_fault_report.value() & 2 != 0 {
            dev_info!(
                device.as_ref(),
                "G17P diagnostic VM owner: vm={} pid={} file={}\n",
                vm.id(),
                kernel::current!().group_leader().pid(),
                file_id,
            );
        }

        let simplefb_iova = module_parameters::g17p_simplefb_iova.value();
        if simplefb_iova != 0 {
            if gpu.params()?.chip_id != 0x8140 {
                return Err(ENOTSUPP);
            }
            let (simplefb_phys, simplefb_size) = t8140_simplefb_phys_range()?;
            let simplefb_end = simplefb_iova
                .checked_add(simplefb_size as u64)
                .ok_or(EOVERFLOW)?;
            let simplefb_range = simplefb_iova..simplefb_end;
            if simplefb_iova & mmu::UAT_PGMSK as u64 != 0
                || !user_range.is_superset(simplefb_range.clone())
                || kernel_range.overlaps(simplefb_range.clone())
                || vm.driver_range_overlaps(simplefb_range)
            {
                return Err(EINVAL);
            }
            vm.install_simplefb_mapping(simplefb_iova, simplefb_phys, simplefb_size)?;
            dev_info!(
                device,
                "G17P simplefb: retained GPU alias {:#x}:{:#x} -> phys {:#x}\n",
                simplefb_iova,
                simplefb_size,
                simplefb_phys,
            );
        }

        let vm_xa = file.inner().vms();
        let resv = vm_xa.lock().reserve_limit(1..=u32::MAX, GFP_KERNEL)?;
        let id: u32 = resv.index().try_into()?;

        mod_dev_dbg!(device, "[File {} VM {}]: VM Create\n", file_id, id);
        mod_dev_dbg!(
            device,
            "[File {} VM {}]: Creating allocators\n",
            file_id,
            id
        );
        let ualloc = Arc::pin_init(
            new_mutex!(alloc::DefaultAllocator::new(
                device,
                &vm,
                kernel_gpu_range,
                buffer::PAGE_SIZE,
                mmu::PROT_GPU_SHARED_RW,
                512 * 1024,
                true,
                fmt!("File {} VM {} GPU Shared", file_id, id),
                false,
            )?),
            GFP_KERNEL,
        )?;
        let ualloc_priv = Arc::pin_init(
            new_mutex!(alloc::DefaultAllocator::new(
                device,
                &vm,
                kernel_gpufw_range,
                buffer::PAGE_SIZE,
                mmu::PROT_GPU_FW_PRIV_RW,
                64 * 1024,
                true,
                fmt!("File {} VM {} GPU FW Private", file_id, id),
                false,
            )?),
            GFP_KERNEL,
        )?;

        mod_dev_dbg!(
            device,
            "[File {} VM {}]: Creating dummy object\n",
            file_id,
            id
        );
        let mut dummy_obj = gem::new_kernel_object(device, 0x4000)?;
        dummy_obj.vmap()?.memset(0);
        let dummy_mapping = dummy_obj.map_at(
            &vm,
            device.gpu.unknown_page()?,
            mmu::PROT_GPU_SHARED_RW,
            true,
        )?;

        mod_dev_dbg!(device, "[File {} VM {}]: VM created\n", file_id, id);
        resv.fill(KBox::new(
            Vm {
                ualloc,
                ualloc_priv,
                vm,
                kernel_range,
                user_range,
                _dummy_mapping: dummy_mapping,
            },
            GFP_KERNEL,
        )?)?;

        data.vm_id = id;

        Ok(0)
    }

    /// IOCTL: vm_destroy: Destroy a `Vm`.
    pub(crate) fn vm_destroy(
        _device: &AsahiDevice,
        data: &mut uapi::drm_asahi_vm_destroy,
        file: &DrmFile,
    ) -> Result<u32> {
        let vm = file.inner().vms().remove(data.vm_id as usize);
        if vm.is_none() {
            Err(ENOENT)
        } else {
            Ok(0)
        }
    }

    /// IOCTL: gem_create: Create a new GEM object.
    pub(crate) fn gem_create(
        device: &AsahiDevice,
        data: &mut uapi::drm_asahi_gem_create,
        file: &DrmFile,
    ) -> Result<u32> {
        mod_dev_dbg!(
            device,
            "[File {}]: IOCTL: gem_create size={:#x?}\n",
            file.inner().id,
            data.size
        );

        if (data.flags
            & !(uapi::drm_asahi_gem_flags_DRM_ASAHI_GEM_WRITEBACK
                | uapi::drm_asahi_gem_flags_DRM_ASAHI_GEM_VM_PRIVATE))
            != 0
            || (data.flags & uapi::drm_asahi_gem_flags_DRM_ASAHI_GEM_VM_PRIVATE == 0
                && data.vm_id != 0)
        {
            cls_pr_debug!(Errors, "gem_create: Invalid arguments\n");
            return Err(EINVAL);
        }

        let resv_gem;
        let resv_obj = if data.flags & uapi::drm_asahi_gem_flags_DRM_ASAHI_GEM_VM_PRIVATE != 0 {
            resv_gem = file
                .inner()
                .vms()
                .lock()
                .get(data.vm_id.try_into()?)
                .ok_or(ENOENT)?
                .vm
                .get_resv_obj();
            Some(resv_gem.deref())
        } else {
            None
        };

        let create_started = crate::g17_mem_path::stamp();
        let gem = gem::new_object(device, data.size.try_into()?, data.flags, resv_obj)?;

        let handle = gem.create_handle(file)?;
        data.handle = handle;
        crate::g17_mem_path::GEM_CREATE.since(create_started);
        crate::g17_mem_path::count(&crate::g17_mem_path::GEM_BYTES, data.size);

        mod_dev_dbg!(
            device,
            "[File {}]: IOCTL: gem_create size={:#x} handle={:#x?}\n",
            file.inner().id,
            data.size,
            data.handle
        );

        Ok(0)
    }

    /// IOCTL: gem_mmap_offset: Assign an mmap offset to a GEM object.
    pub(crate) fn gem_mmap_offset(
        device: &AsahiDevice,
        data: &mut uapi::drm_asahi_gem_mmap_offset,
        file: &DrmFile,
    ) -> Result<u32> {
        mod_dev_dbg!(
            device,
            "[File {}]: IOCTL: gem_mmap_offset handle={:#x?}\n",
            file.inner().id,
            data.handle
        );

        if data.flags != 0 {
            cls_pr_debug!(Errors, "gem_mmap_offset: Unexpected flags\n");
            return Err(EINVAL);
        }

        let gem = gem::Object::lookup_handle(file, data.handle)?;
        data.offset = gem.create_mmap_offset()?;
        Ok(0)
    }

    /// IOCTL: vm_bind: Map or unmap memory into a Vm.
    pub(crate) fn vm_bind(
        device: &AsahiDevice,
        data: &uapi::drm_asahi_vm_bind,
        file: &DrmFile,
    ) -> Result<u32> {
        mod_dev_dbg!(
            device,
            "[File {} VM {}]: IOCTL: vm_bind\n",
            file.inner().id,
            data.vm_id,
        );

        if data.stride == 0 || data.pad != 0 {
            cls_pr_debug!(Errors, "vm_bind: Unexpected headers\n");
            return Err(EINVAL);
        }
        let bind_started = crate::g17_mem_path::stamp();

        let vm_id = data.vm_id.try_into()?;
        let count = usize::try_from(data.num_binds)?;
        let stride = usize::try_from(data.stride)?;
        let size = stride.checked_mul(count).ok_or(EOVERFLOW)?;
        let mut vec = KVec::new();
        let reader = UserSlice::new(UserPtr::from_addr(data.userptr as _), size).reader();
        reader.read_all(&mut vec, GFP_KERNEL)?;
        let mut reader = Reader::new(&vec);
        let mut ops = KVec::with_capacity(count, GFP_KERNEL)?;
        for _i in 0..count {
            let bind: uapi::drm_asahi_gem_bind_op = reader.read_up_to(stride)?;
            // Vulkan faults on an unmapped USC exec window. Log every bind so we
            // can tell whether Mesa asks for that range and we drop it, or never
            // asks at all.
            if crate::util::g17p_verbose() {
                pr_info!(
                    "G17P bind: vm={} addr={:#x} range={:#x} offset={:#x} flags={:#x} handle={}\n",
                    data.vm_id,
                    bind.addr,
                    bind.range,
                    bind.offset,
                    bind.flags,
                    bind.handle,
                );
            }
            ops.push(bind, GFP_KERNEL)?;
        }

        let (vm, kernel_range, user_range) = {
            let vms_xa = file.inner().vms();
            let guard = vms_xa.lock();
            let guarded_vm = guard.get(vm_id).ok_or(ENOENT)?;
            (
                guarded_vm.vm.clone(),
                guarded_vm.kernel_range.clone(),
                guarded_vm.user_range.clone(),
            )
        };

        // Validate every operation and resolve every handle before the
        // established interruptible per-operation drm_exec commit path begins.
        let mut plans = KVec::with_capacity(count, GFP_KERNEL)?;
        let mut batch = mmu::PreparedUserBindBatch::new(count)?;
        let mut unmap_count = 0usize;
        let prepare_started = crate::g17_mem_path::stamp();
        crate::g17_mem_path::count(&crate::g17_mem_path::BIND_OPS, count as u64);
        for op in ops {
            crate::g17_mem_path::count(&crate::g17_mem_path::BIND_BYTES, op.range);
            if op.range == 0
                || (op.addr | op.range | op.offset) & mmu::UAT_PGMSK as u64 != 0
            {
                return Err(EINVAL);
            }
            let end = op.addr.checked_add(op.range).ok_or(EINVAL)?;
            let range = op.addr..end;
            if !user_range.is_superset(range.clone())
                || kernel_range.overlaps(range.clone())
                || vm.driver_range_overlaps(range.clone())
            {
                return Err(EINVAL);
            }
            if op.flags & uapi::drm_asahi_bind_flags_DRM_ASAHI_BIND_UNBIND != 0 {
                if op.offset != 0
                    || op.flags != uapi::drm_asahi_bind_flags_DRM_ASAHI_BIND_UNBIND
                    || op.handle != 0
                {
                    return Err(EINVAL);
                }
                let prepared = vm.prepare_user_unmap(op.addr, op.range)?;
                batch.push_unmap(prepared)?;
                plans.push(PreparedGemBindOp::Unmap { op }, GFP_KERNEL)?;
                unmap_count += 1;
                continue;
            }

            vm.wait_for_user_map_admission()?;
            if op.flags
                & !(uapi::drm_asahi_bind_flags_DRM_ASAHI_BIND_READ
                    | uapi::drm_asahi_bind_flags_DRM_ASAHI_BIND_WRITE
                    | uapi::drm_asahi_bind_flags_DRM_ASAHI_BIND_SINGLE_PAGE)
                != 0
            {
                return Err(EINVAL);
            }
            let read = op.flags & uapi::drm_asahi_bind_flags_DRM_ASAHI_BIND_READ != 0;
            let write = op.flags & uapi::drm_asahi_bind_flags_DRM_ASAHI_BIND_WRITE != 0;
            if !read && !write {
                return Err(EINVAL);
            }
            let single_page =
                op.flags & uapi::drm_asahi_bind_flags_DRM_ASAHI_BIND_SINGLE_PAGE != 0;
            let bo = gem::Object::lookup_handle(file, op.handle)?;
            let accessed_size = if single_page {
                mmu::UAT_PGSZ as u64
            } else {
                op.range
            };
            let end_off = op.offset.checked_add(accessed_size).ok_or(EINVAL)?;
            if end_off > u64::try_from(bo.size())? {
                return Err(EINVAL);
            }
            vm.validate_t8140_native_context_binding(op.addr, op.range, single_page)?;
            let prot = match (read, write) {
                (true, true) => mmu::PROT_GPU_SHARED_RW,
                (true, false) => mmu::PROT_GPU_SHARED_RO,
                (false, true) => mmu::PROT_GPU_SHARED_WO,
                (false, false) => unreachable!(),
            };
            let prepared = vm.prepare_bind_object(
                &bo, op.addr, op.range, op.offset, prot, single_page,
            )?;
            batch.push_map(prepared)?;
            plans.push(PreparedGemBindOp::Map { op, bo }, GFP_KERNEL)?;
        }

        crate::g17_mem_path::BIND_PREPARE.since(prepare_started);
        vm.reserve_deferred_user_unmaps(unmap_count)?;
        let commit_started = crate::g17_mem_path::stamp();
        vm.commit_prepared_user_bind_batch(&mut batch)?;
        crate::g17_mem_path::BIND_COMMIT.since(commit_started);

        // Replay registry changes only after the page-table batch commits.
        for plan in &plans {
            match plan {
                PreparedGemBindOp::Map { op, bo, .. } => {
                    vm.track_t8140_native_context_binding(
                        bo.clone(), op.addr, op.range, op.offset,
                    )?;
                    vm.track_shared_binding(
                        bo, op.addr, op.range, op.offset,
                        op.flags & uapi::drm_asahi_bind_flags_DRM_ASAHI_BIND_SINGLE_PAGE != 0,
                    );
                }
                PreparedGemBindOp::Unmap { op, .. } => {
                    vm.untrack_t8140_native_context_range(op.addr..op.addr + op.range);
                    vm.untrack_shared_range(op.addr..op.addr + op.range);
                }
            }
        }
        vm.bo_deferred_cleanup();
        crate::g17_mem_path::BIND_CALL.since(bind_started);
        Ok(0)
    }

    pub(crate) fn unbind_gem_object(file: &DrmFile, bo: &gem::Object) -> Result {
        // TODO: use iter()
        let mut index = 0;
        loop {
            let vms = file.inner().vms();
            let item = vms.find(index, usize::MAX);
            match item {
                Some((idx, file_vm)) => {
                    // Clone since we can't hold the xarray spinlock while
                    // calling drop_mappings()
                    let vm = file_vm.borrow().vm.clone();
                    core::mem::drop(file_vm);
                    vm.drop_mappings(bo)?;
                    vm.untrack_shared_object(bo);
                    if idx == usize::MAX {
                        break;
                    }
                    index = idx + 1;
                }
                None => break,
            }
        }
        Ok(())
    }

    /// IOCTL: gem_bind_object: Map or unmap a GEM object as a special object.
    pub(crate) fn gem_bind_object(
        device: &AsahiDevice,
        data: &mut uapi::drm_asahi_gem_bind_object,
        file: &DrmFile,
    ) -> Result<u32> {
        mod_dev_dbg!(
            device,
            "[File {} VM {}]: IOCTL: gem_bind_object op={:?} handle={:#x?} flags={:#x?} {:#x?}:{:#x?} object_handle={:#x?}\n",
            file.inner().id,
            data.vm_id,
            data.op,
            data.handle,
            data.flags,
            data.offset,
            data.range,
            data.object_handle
        );

        if data.pad != 0 {
            cls_pr_debug!(Errors, "gem_bind_object: Unexpected pad\n");
            return Err(EINVAL);
        }

        if data.vm_id != 0 {
            cls_pr_debug!(Errors, "gem_bind_object: Unexpected vm_id\n");
            return Err(EINVAL);
        }

        match data.op {
            uapi::drm_asahi_bind_object_op_DRM_ASAHI_BIND_OBJECT_OP_BIND => {
                Self::do_gem_bind_object(device, data, file)
            }
            uapi::drm_asahi_bind_object_op_DRM_ASAHI_BIND_OBJECT_OP_UNBIND => {
                Self::do_gem_unbind_object(device, data, file)
            }
            _ => {
                cls_pr_debug!(Errors, "gem_bind_object: Invalid op {}\n", data.op);
                Err(EINVAL)
            }
        }
    }

    pub(crate) fn do_gem_bind_object(
        device: &AsahiDevice,
        data: &mut uapi::drm_asahi_gem_bind_object,
        file: &DrmFile,
    ) -> Result<u32> {
        if (data.range | data.offset) as usize & mmu::UAT_PGMSK != 0 {
            cls_pr_debug!(
                Errors,
                "gem_bind_object: Range/offset not page aligned: {:#x} {:#x}\n",
                data.range,
                data.offset
            );
            return Err(EINVAL); // Must be page aligned
        }

        if data.flags != uapi::drm_asahi_bind_object_flags_DRM_ASAHI_BIND_OBJECT_USAGE_TIMESTAMPS {
            cls_pr_debug!(Errors, "gem_bind_object: Invalid flags {:#x}\n", data.flags);
            return Err(EINVAL);
        }

        let offset = data.offset.try_into()?;
        let end_offset = data
            .offset
            .checked_add(data.range)
            .ok_or(EINVAL)?
            .try_into()?;
        let bo = gem::ObjectRef::new(gem::Object::lookup_handle(file, data.handle)?);

        let mapping = Arc::new(
            device.gpu.map_timestamp_buffer(bo, offset..end_offset)?,
            GFP_KERNEL,
        )?;
        let obj = KBox::new(Object::TimestampBuffer(mapping), GFP_KERNEL)?;
        let handle = file
            .inner()
            .objects()
            .lock()
            .insert_limit(1..=u32::MAX, obj, GFP_KERNEL)? as u64;

        data.object_handle = handle as u32;
        Ok(0)
    }

    pub(crate) fn do_gem_unbind_object(
        _device: &AsahiDevice,
        data: &mut uapi::drm_asahi_gem_bind_object,
        file: &DrmFile,
    ) -> Result<u32> {
        if data.range != 0 || data.offset != 0 {
            cls_pr_debug!(
                Errors,
                "gem_unbind_object: Range/offset not zero: {:#x} {:#x}\n",
                data.range,
                data.offset
            );
            return Err(EINVAL);
        }

        if data.flags != 0 {
            cls_pr_debug!(
                Errors,
                "gem_unbind_object: Invalid flags {:#x}\n",
                data.flags
            );
            return Err(EINVAL);
        }

        if data.handle != 0 {
            cls_pr_debug!(
                Errors,
                "gem_unbind_object: Invalid handle {}\n",
                data.handle
            );
            return Err(EINVAL);
        }

        let object = file.inner().objects().remove(data.object_handle as usize);
        if object.is_none() {
            Err(ENOENT)
        } else {
            Ok(0)
        }
    }

    /// IOCTL: queue_create: Create a new command submission queue of a given type.
    pub(crate) fn queue_create(
        device: &AsahiDevice,
        data: &mut uapi::drm_asahi_queue_create,
        file: &DrmFile,
    ) -> Result<u32> {
        let file_id = file.inner().id;

        mod_dev_dbg!(
            device,
            "[File {} VM {}]: Creating queue prio={:?} flags={:#x?}\n",
            file_id,
            data.vm_id,
            data.priority,
            data.flags,
        );

        if data.flags != 0 || data.priority > uapi::drm_asahi_priority_DRM_ASAHI_PRIORITY_REALTIME {
            cls_pr_debug!(Errors, "queue_create: Invalid arguments\n");
            return Err(EINVAL);
        }

        // TODO: Allow with CAP_SYS_NICE
        if data.priority >= uapi::drm_asahi_priority_DRM_ASAHI_PRIORITY_HIGH {
            cls_pr_debug!(Errors, "queue_create: Invalid priority\n");
            return Err(EINVAL);
        }

        let queues_xa = file.inner().queues();
        let resv = queues_xa.lock().reserve_limit(1..=u32::MAX, GFP_KERNEL)?;
        let vms_xa = file.inner().vms();
        let guard = vms_xa.lock();
        let file_vm = guard.get(data.vm_id.try_into()?).ok_or(ENOENT)?;
        let vm = file_vm.vm.clone();
        let ualloc = file_vm.ualloc.clone();
        let ualloc_priv = file_vm.ualloc_priv.clone();
        // Drop the vms lock eagerly
        let _ = file_vm;
        core::mem::drop(guard);

        let queue = device.gpu.new_queue(
            vm,
            ualloc,
            ualloc_priv,
            // TODO: Plumb deeper the enum
            uapi::drm_asahi_priority_DRM_ASAHI_PRIORITY_REALTIME - data.priority,
            data.usc_exec_base,
        )?;

        data.queue_id = resv.index().try_into()?;
        resv.fill(Arc::pin_init(new_mutex!(queue), GFP_KERNEL)?)?;

        Ok(0)
    }

    /// IOCTL: queue_destroy: Destroy a command submission queue.
    pub(crate) fn queue_destroy(
        _device: &AsahiDevice,
        data: &mut uapi::drm_asahi_queue_destroy,
        file: &DrmFile,
    ) -> Result<u32> {
        // grab the queue so the xarray spinlock is dropped first
        let queue = file.inner().queues().remove(data.queue_id as usize);
        if queue.is_none() {
            Err(ENOENT)
        } else {
            Ok(0)
        }
    }

    /// IOCTL: submit: Submit GPU work to a command submission queue.
    pub(crate) fn submit(
        device: &AsahiDevice,
        data: &mut uapi::drm_asahi_submit,
        file: &DrmFile,
    ) -> Result<u32> {
        // v81 e2e diagnostic: 0 (no counter read) unless the ring is armed.
        let e2e_entry = crate::g17_e2e::stamp();
        // v92 PCORE_NUDGE (diagnostic, default off): one relaxed load unless
        // enabled and the feed-perf request is applied.
        crate::g17_host_path::nudge_feeding_thread();
        debug::update_debug_flags();

        if data.flags != 0 || data.pad != 0 {
            cls_pr_debug!(Errors, "submit: Invalid arguments\n");
            return Err(EINVAL);
        }

        let gpu = &device.gpu;
        gpu.update_globals();

        // Upgrade to Arc<T> to drop the XArray lock early
        let queue: Arc<Mutex<KBox<dyn queue::Queue>>> = file
            .inner()
            .queues()
            .lock()
            .get(data.queue_id.try_into()?)
            .ok_or(ENOENT)?
            .into();

        let id = gpu.ids().submission.next();
        mod_dev_dbg!(
            device,
            "[File {} Queue {}]: IOCTL: submit (submission ID: {})\n",
            file.inner().id,
            data.queue_id,
            id
        );

        mod_dev_dbg!(
            device,
            "[File {} Queue {}]: IOCTL: submit({}): Parsing commands\n",
            file.inner().id,
            data.queue_id,
            id
        );

        let mut vec = KVec::new();

        // Copy the command buffer into the kernel. Because we need to iterate
        // the command buffer twice, we do this in one big copy_from_user to
        // avoid TOCTOU issues.
        //
        // This runs BEFORE the syncobjs are parsed on purpose. Once an output
        // syncobj has been looked up, userspace is already waiting on it, and
        // the driver owes it a fence on every path. `queue.submit()` guarantees
        // that for itself; a failure between the lookup and the call would not,
        // and a copy_from_user of a userspace-supplied pointer and length is
        // exactly such a failure. Ordering the fallible copy first leaves no
        // gap: after `parse_array` succeeds the only remaining step is the
        // guarded submit.
        let reader = UserSlice::new(
            UserPtr::from_addr(data.cmdbuf as _),
            data.cmdbuf_size as usize,
        )
        .reader();
        reader.read_all(&mut vec, GFP_KERNEL)?;

        mod_dev_dbg!(
            device,
            "[File {} Queue {}]: IOCTL: submit({}): Parsing syncs\n",
            file.inner().id,
            data.queue_id,
            id
        );
        let e2e_copied = crate::g17_e2e::stamp();
        let syncs =
            SyncItem::parse_array(file, data.syncs, data.in_sync_count, data.out_sync_count)?;

        let objects = file.inner().objects();
        let e2e_parsed = crate::g17_e2e::stamp();
        if e2e_entry != 0 {
            let e2e_pid = kernel::current!().group_leader().pid() as u64;
            crate::g17_e2e::record_at(e2e_entry, crate::g17_e2e::K_IOCTL_ENTER, data.queue_id, id,
                [u64::from(data.in_sync_count) | u64::from(data.out_sync_count) << 32,
                 u64::from(data.cmdbuf_size), e2e_pid, e2e_copied, e2e_parsed]);
        }
        let (ret, context_unbound) = {
            let mut queue = queue.lock();
            let ret = queue.submit(id, syncs, data.in_sync_count as usize, &vec, objects);
            let refused = ret == Err(ENOENT) && queue.context_unbound_refusal(id);
            (ret, refused)
        };
        // The queue lock is dropped and only accepted work refreshes this
        // client's short-lived wait hint. Errors cannot extend a boost.
        if ret.is_ok() {
            let wait_util_min = crate::g17_host_path::feeder_wait_util_min();
            if wait_util_min != 0 {
                file.set_syncobj_wait_hint(wait_util_min, 8);
            }
        }
        if e2e_entry != 0 {
            crate::g17_e2e::record(crate::g17_e2e::K_IOCTL_EXIT, 0, id,
                [ret.as_ref().err().map_or(0, |error| error.to_errno() as i64 as u64), 0, 0, 0, 0]);
        }

        match ret {
            Err(ERESTARTSYS) => Err(ERESTARTSYS),
            Err(ENOENT) if context_unbound => {
                crate::g17_queue_slots::note_context_unbound_refusal();
                dev_info!(device.as_ref(),
                    "[File {} Queue {}]: IOCTL: submit refused: context-unbound (submission ID: {} err: ENOENT)\n",
                    file.inner().id, data.queue_id, id);
                Err(ENOENT)
            }
            Err(e) => {
                crate::g17_queue_slots::note_submit_failure();
                dev_info!(
                    device.as_ref(),
                    "[File {} Queue {}]: IOCTL: submit failed! (submission ID: {} err: {:?})\n",
                    file.inner().id,
                    data.queue_id,
                    id,
                    e
                );
                Err(e)
            }
            Ok(()) => Ok(0),
        }
    }

    /// IOCTL: get_time: Get the current GPU timer value.
    pub(crate) fn get_time(
        device: &AsahiDevice,
        data: &mut uapi::drm_asahi_get_time,
        _file: &DrmFile,
    ) -> Result<u32> {
        if data.flags != 0 {
            cls_pr_debug!(Errors, "get_time: Unexpected flags\n");
            return Err(EINVAL);
        }

        // TODO: Do this on device-init for perf.
        let gpu = &device.gpu;
        let frequency_hz = gpu.base_clock_hz() as u64;
        let ts_gcd = gcd(frequency_hz, NSEC_PER_SEC as u64);

        let num = (NSEC_PER_SEC as u64) / ts_gcd;
        let den = frequency_hz / ts_gcd;

        let raw: u64;

        // SAFETY: Assembly only loads the timer
        unsafe {
            core::arch::asm!(
                "mrs {x}, CNTPCT_EL0",
                x = out(reg) raw
            );
        }

        data.gpu_timestamp = (raw * num) / den;

        Ok(0)
    }
}

impl Drop for File {
    fn drop(&mut self) {
        mod_pr_debug!("[File {}]: Closing...\n", self.id);
    }
}
