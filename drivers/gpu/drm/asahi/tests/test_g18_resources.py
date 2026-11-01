#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Build the real g18_resources.rs against host stubs and run the G18G graph.

The stubs mirror the signatures g18_resources.rs uses from g17_resources.rs
(MappedObject), mmu.rs (Uat, Vm, KernelMapping, PROT_*), driver.rs and the
kernel prelude. The stub Vm is an in-memory page table, so the test can read
the constructed graph exactly as the firmware would: through its own
translations, following the root pointers. No hardware is involved.
"""
from pathlib import Path
import json
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]


def path(name):
    return json.dumps(str(root / name))



kernel_source = r'''
#![allow(dead_code)]
use std::sync::Mutex;
pub static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
pub fn log(line: String) { LOG.lock().unwrap().push(line); }

#[macro_export]
macro_rules! dev_info {
    ($dev:expr, $($arg:tt)*) => {{
        let _device = $dev;
        $crate::log(format!($($arg)*));
    }};
}
#[macro_export]
macro_rules! try_init {
    ($($body:tt)*) => { $($body)* };
}

pub mod prelude {
    pub use crate::{dev_info, try_init};
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Error(pub i32);
    pub type Result<T = (), E = Error> = core::result::Result<T, E>;
    pub const EIO: Error = Error(5);
    pub const EFAULT: Error = Error(14);
    pub const EBUSY: Error = Error(16);
    pub const ENODEV: Error = Error(19);
    pub const EINVAL: Error = Error(22);
    pub const ENOSPC: Error = Error(28);
    pub const EOVERFLOW: Error = Error(75);
    impl From<core::num::TryFromIntError> for Error {
        fn from(_: core::num::TryFromIntError) -> Self { EINVAL }
    }
    #[derive(Debug)]
    pub struct AllocError;
    impl From<AllocError> for Error { fn from(_: AllocError) -> Self { Error(12) } }
    #[derive(Clone, Copy)]
    pub struct Flags;
    pub const GFP_KERNEL: Flags = Flags;
    pub struct KVec<T>(Vec<T>);
    impl<T> KVec<T> {
        pub fn new() -> Self { Self(Vec::new()) }
        pub fn push(&mut self, value: T, _: Flags) -> core::result::Result<(), AllocError> {
            self.0.push(value);
            Ok(())
        }
        pub fn len(&self) -> usize { self.0.len() }
    }
    impl<T> core::ops::Deref for KVec<T> {
        type Target = [T];
        fn deref(&self) -> &[T] { &self.0 }
    }
    impl<T> core::ops::DerefMut for KVec<T> {
        fn deref_mut(&mut self) -> &mut [T] { &mut self.0 }
    }
    pub struct KBox<T>(Box<T>);
    impl<T> KBox<T> {
        pub fn init(value: T, _: Flags) -> Result<Self> { Ok(Self(Box::new(value))) }
    }
    impl<T> core::ops::Deref for KBox<T> {
        type Target = T;
        fn deref(&self) -> &T { &self.0 }
    }
    impl<T> core::ops::DerefMut for KBox<T> {
        fn deref_mut(&mut self) -> &mut T { &mut self.0 }
    }
}
pub mod sync { pub use std::sync::Arc; }
'''

source = r'''
#![allow(dead_code, non_upper_case_globals, unused_imports, unused_variables, unused_mut)]

mod module_parameters {
    pub struct Param(u32);
    impl Param { pub fn value(&self) -> &u32 { &self.0 } }
    pub static g17p_fw_log: Param = Param(0);
    pub static g17p_progress_watchdog: Param = Param(1);
    pub static g17p_idle_selector: Param = Param(0);
}
#[path = @@G17_CONFIG@@] mod g17_config;
#[path = @@G17_INITDATA@@] mod g17_initdata;
#[path = @@G18_UMA@@] mod g18_uma;

mod hw { pub mod agx3 {
    pub struct Registry { pub num_cores: u32 }
    pub const T8152_REGISTRY: Registry = Registry { num_cores: 12 };
} }

mod driver {
    pub struct Device;
    pub struct AsahiDevice(pub Device);
    impl AsRef<Device> for AsahiDevice {
        fn as_ref(&self) -> &Device { &self.0 }
    }
}

/// Simulated physical memory: page frame -> (backing, byte offset).
mod physmem {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    pub type Backing = Arc<Mutex<Vec<u8>>>;
    pub static FRAMES: Mutex<BTreeMap<u64, (Backing, usize)>> = Mutex::new(BTreeMap::new());
    pub static NEXT: Mutex<u64> = Mutex::new(0x100_0000_0000);
    pub fn allocate(size: usize) -> (Backing, u64) {
        let pages = (size + 0x3fff) / 0x4000;
        let backing: Backing = Arc::new(Mutex::new(vec![0u8; pages * 0x4000]));
        let mut next = NEXT.lock().unwrap();
        let base = *next;
        *next += pages as u64 * 0x4000 + 0x4000;
        let mut frames = FRAMES.lock().unwrap();
        for page in 0..pages {
            frames.insert(base + page as u64 * 0x4000, (backing.clone(), page * 0x4000));
        }
        (backing, base)
    }
}

mod mmu {
    use kernel::prelude::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    pub const UAT_PGSZ: usize = 0x4000;
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Prot(pub &'static str);
    pub const PROT_FW_MMIO_RO: Prot = Prot("mmio-ro");
    pub const PROT_FW_MMIO_RW: Prot = Prot("mmio-rw");
    pub const PROT_FW_SHARED_RW: Prot = Prot("shared-rw");
    pub const PROT_FW_SHARED_RO: Prot = Prot("shared-ro");
    pub const PROT_FW_PRIV_RW: Prot = Prot("priv-rw");
    pub const PROT_GPU_SHARED_RW: Prot = Prot("gpu-shared-rw");
    pub struct KernelMapping {
        pub iova: u64, pub size: usize, pub prot: Prot,
        pages: Arc<Mutex<BTreeMap<u64, (u64, Prot)>>>,
        reservations: Arc<Mutex<BTreeMap<u64, u64>>>,
    }
    impl Drop for KernelMapping {
        fn drop(&mut self) {
            let mut pages = self.pages.lock().unwrap();
            for page in (0..self.size as u64).step_by(UAT_PGSZ) {
                pages.remove(&(self.iova + page));
            }
            self.reservations.lock().unwrap().remove(&self.iova);
        }
    }
    pub struct Vm {
        pub pages: Arc<Mutex<BTreeMap<u64, (u64, Prot)>>>,
        pub reservations: Arc<Mutex<BTreeMap<u64, u64>>>,
    }
    impl Vm {
        pub fn new() -> Self { Self {
            pages: Arc::new(Mutex::new(BTreeMap::new())),
            reservations: Arc::new(Mutex::new(BTreeMap::new())),
        } }
        pub(crate) fn map_pages(&self, iova: u64, phys: u64, size: usize, prot: Prot) -> Result<KernelMapping> {
            self.map_pages_with_guards(iova, phys, size, prot, 0)
        }
        pub(crate) fn map_pages_with_guards(&self, iova: u64, phys: u64, size: usize,
                                            prot: Prot, guards: usize) -> Result<KernelMapping> {
            if (iova | phys | size as u64) & 0x3fff != 0 || size == 0 { return Err(EINVAL); }
            let end = iova + size as u64 + guards as u64 * UAT_PGSZ as u64;
            let mut reservations = self.reservations.lock().unwrap();
            if reservations.iter().any(|(&start, &limit)| start < end && iova < limit) {
                return Err(EBUSY);
            }
            let mut pages = self.pages.lock().unwrap();
            for page in (0..size as u64).step_by(UAT_PGSZ) {
                if pages.contains_key(&(iova + page)) { return Err(EBUSY); }
            }
            for page in (0..size as u64).step_by(UAT_PGSZ) {
                pages.insert(iova + page, (phys + page, prot));
            }
            reservations.insert(iova, end);
            Ok(KernelMapping { iova, size, prot,
                pages: self.pages.clone(), reservations: self.reservations.clone() })
        }
        pub(crate) fn map_io(&self, iova: u64, phys: usize, size: usize, prot: Prot) -> Result<KernelMapping> {
            self.map_pages(iova, phys as u64, size, prot)
        }
        pub(crate) fn translate_iova(&self, address: u64) -> Result<usize> {
            let pages = self.pages.lock().unwrap();
            let (phys, _) = pages.get(&(address & !0x3fff)).ok_or(EFAULT)?;
            Ok((phys + (address & 0x3fff)) as usize)
        }
        pub(crate) fn prot(&self, address: u64) -> Option<Prot> {
            self.pages.lock().unwrap().get(&(address & !0x3fff)).map(|(_, prot)| *prot)
        }
    }
    pub struct Uat { pub t8152: bool, pub kernel: Vm, pub lower: Vm }
    impl Uat {
        pub(crate) fn is_t8152_geometry(&self) -> bool { self.t8152 }
        pub(crate) fn kernel_vm(&self) -> &Vm { &self.kernel }
        pub(crate) fn kernel_lower_vm(&self) -> &Vm { &self.lower }
    }
}

mod g17_resources {
    use kernel::prelude::*;
    use crate::{driver::AsahiDevice, mmu, physmem};
    use core::ops::Range;

    pub(crate) struct MappedObject {
        backing: physmem::Backing,
        phys: u64,
        iova: u64,
        logical_size: usize,
        mapping: mmu::KernelMapping,
    }

    impl MappedObject {
        fn new_wc_at(_dev: &AsahiDevice, vm: &mmu::Vm, address: u64, size: usize, prot: mmu::Prot) -> Result<Self> {
            let (backing, phys) = physmem::allocate(size);
            let span = (size + 0x3fff) & !0x3fff;
            let mapping = vm.map_pages(address, phys, span, prot)?;
            Ok(Self { backing, phys, iova: address, logical_size: size, mapping })
        }
        fn new_wc_in_range(dev: &AsahiDevice, vm: &mmu::Vm, range: Range<u64>, size: usize,
                           alignment: u64, prot: mmu::Prot) -> Result<Self> {
            let span = ((size + 0x3fff) & !0x3fff) as u64;
            let mut address = (range.start + alignment - 1) & !(alignment - 1);
            while address + span <= range.end {
                let free = (0..span).step_by(0x4000).all(|page| vm.translate_iova(address + page).is_err());
                if free { return Self::new_wc_at(dev, vm, address, size, prot); }
                address += alignment;
            }
            Err(EBUSY)
        }
        fn iova(&self) -> u64 { self.iova }
        fn with_bytes_mut<R>(&mut self, f: impl FnOnce(&mut [u8]) -> Result<R>) -> Result<R> {
            let mut bytes = self.backing.lock().unwrap();
            f(&mut bytes[..self.logical_size])
        }
        fn read_u32(&mut self, address: u64) -> Result<u32> {
            let offset = address.checked_sub(self.iova()).ok_or(EINVAL)? as usize;
            let end = offset.checked_add(4).ok_or(EINVAL)?;
            if end > self.logical_size { return Err(EINVAL); }
            self.with_bytes_mut(|bytes| Ok(u32::from_le_bytes(bytes[offset..end].try_into().unwrap())))
        }
        fn read_u64(&mut self, address: u64) -> Result<u64> {
            let offset = address.checked_sub(self.iova()).ok_or(EINVAL)? as usize;
            let end = offset.checked_add(8).ok_or(EINVAL)?;
            if end > self.logical_size { return Err(EINVAL); }
            self.with_bytes_mut(|bytes| Ok(u64::from_le_bytes(bytes[offset..end].try_into().unwrap())))
        }
        fn write_u32(&mut self, address: u64, value: u32) -> Result {
            let offset = address.checked_sub(self.iova()).ok_or(EINVAL)? as usize;
            let end = offset.checked_add(4).ok_or(EINVAL)?;
            if end > self.logical_size { return Err(EINVAL); }
            self.with_bytes_mut(|bytes| { bytes[offset..end].copy_from_slice(&value.to_le_bytes()); Ok(()) })
        }
        fn map_alias_at(&mut self, vm: &mmu::Vm, address: u64, prot: mmu::Prot) -> Result<mmu::KernelMapping> {
            vm.map_pages(address, self.phys, (self.logical_size + 0x3fff) & !0x3fff, prot)
        }
        fn map_alias_at_with_guards(&mut self, vm: &mmu::Vm, address: u64, prot: mmu::Prot,
                                   guards: usize) -> Result<mmu::KernelMapping> {
            vm.map_pages_with_guards(address, self.phys, (self.logical_size + 0x3fff) & !0x3fff,
                                    prot, guards)
        }
    }

    #[path = @@G18_RESOURCES@@]
    pub(crate) mod g18;
    #[path = @@G18_POOL@@]
    pub(crate) mod g18_pool;
}

use g17_initdata as init;
use g17_resources::g18::G18FirmwareMemory;
use kernel::sync::Arc;

/// Read `len` bytes at firmware VA `va` through the firmware-high page table.
fn fw_read(uat: &mmu::Uat, va: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    for index in 0..len as u64 {
        let phys = uat.kernel_vm().translate_iova(va + index).unwrap_or_else(|_| panic!("unmapped FW VA {:#x}", va + index)) as u64;
        let frames = physmem::FRAMES.lock().unwrap();
        let (backing, offset) = frames.get(&(phys & !0x3fff)).unwrap_or_else(|| panic!("no RAM behind {:#x}", va + index));
        out.push(backing.lock().unwrap()[offset + (phys & 0x3fff) as usize]);
    }
    out
}
fn fw_write(uat: &mmu::Uat, va: u64, bytes: &[u8]) {
    for (index, byte) in bytes.iter().enumerate() {
        let phys = uat.kernel_vm().translate_iova(va + index as u64).unwrap() as u64;
        let frames = physmem::FRAMES.lock().unwrap();
        let (backing, offset) = frames.get(&(phys & !0x3fff)).unwrap();
        backing.lock().unwrap()[offset + (phys & 0x3fff) as usize] = *byte;
    }
}
fn u32_at(uat: &mmu::Uat, va: u64) -> u32 { u32::from_le_bytes(fw_read(uat, va, 4).try_into().unwrap()) }
fn u64_at(uat: &mmu::Uat, va: u64) -> u64 { u64::from_le_bytes(fw_read(uat, va, 8).try_into().unwrap()) }

fn uat(t8152: bool) -> Arc<mmu::Uat> {
    Arc::new(mmu::Uat { t8152, kernel: mmu::Vm::new(), lower: mmu::Vm::new() })
}

fn main() {
    let dev = driver::AsahiDevice(driver::Device);
    assert_eq!(G18FirmwareMemory::new_with_uat(&dev, uat(false), 1).err(), Some(kernel::prelude::ENODEV));

    // Fail halfway through the sparse alias graph. All earlier reservations
    // and high mappings must unwind; the unrelated conflicting owner stays.
    let failure_uat = uat(true);
    let conflict = failure_uat.lower.map_io(0x70_0200_0000 + 5 * 0x158000,
        0x100_0040_0000, 0x4000, mmu::PROT_GPU_SHARED_RW).unwrap();
    assert_eq!(G18FirmwareMemory::new_with_uat(&dev, failure_uat.clone(), 1).err(),
               Some(kernel::prelude::EBUSY));
    assert!(failure_uat.kernel.pages.lock().unwrap().is_empty());
    assert!(failure_uat.kernel.reservations.lock().unwrap().is_empty());
    assert_eq!(failure_uat.lower.pages.lock().unwrap().len(), 1);
    assert_eq!(failure_uat.lower.reservations.lock().unwrap().len(), 1);
    drop(conflict);
    assert!(failure_uat.lower.reservations.lock().unwrap().is_empty());

    let uat = uat(true);
    let timestamp = 0x0123_4567_89ab_cdef;
    let mut memory = G18FirmwareMemory::new_with_uat(&dev, uat.clone(), timestamp).unwrap();
    memory.log_summary(&dev);

    let pool = memory.uma_pool();
    let view = pool.view();
    assert_eq!((view.directory_base, view.reservation_bytes, view.stride, view.page_count,
                view.gpu_base_offset), (0x70_0200_0000, 0x1024000, 0x158000, 13, 0x3c00));
    let mut mapped = 0;
    for offset in (0..view.reservation_bytes).step_by(0x4000) {
        if offset % view.stride == 0 {
            let index = (offset / view.stride) as usize;
            let high = pool.directory_page_high(index).unwrap();
            assert_eq!(uat.kernel_vm().translate_iova(high),
                       uat.kernel_lower_vm().translate_iova(view.directory_base + offset));
            assert!(fw_read(&uat, high, 0x4000).iter().all(|&v| v == 0));
            mapped += 1;
        } else {
            assert_eq!(uat.kernel_lower_vm().translate_iova(view.directory_base + offset),
                       Err(kernel::prelude::EFAULT));
        }
    }
    assert_eq!(mapped, 13);
    assert_eq!(uat.lower.map_io(view.directory_base + view.reservation_bytes - 0x8000,
        0x100_0040_0000, 0x4000, mmu::PROT_GPU_SHARED_RW).err(), Some(kernel::prelude::EBUSY));
    assert_eq!(uat.kernel_vm().translate_iova(view.block_list_high),
               uat.kernel_lower_vm().translate_iova(view.block_list_low));
    assert_eq!(u32_at(&uat, view.state_high + 8), u32::MAX);
    assert!(fw_read(&uat, view.block_list_high, 0x4000).iter().all(|&v| v == 0));
    assert!(pool.directory_page_high(13).is_none());

    // Walk the graph from the two roots, as each firmware image would.
    let private = 0xffff_fc20_0002_0000u64;
    for (secondary, role) in [(false, 0u32), (true, 1)] {
        let root = memory.root(secondary);
        let bytes = fw_read(&uat, root, init::G18G_26A428_ROOT_SIZE);
        assert_eq!(u64::from_le_bytes(bytes[0..8].try_into().unwrap()), 0x0c8b_4339_83a0_04c0);
        init::check_firmware_root_gates_for_profile(init::RootProfile::J873G18G26A428, &bytes).unwrap();
        assert_eq!(u32_at(&uat, root + 0x28), role);
        let main = u64_at(&uat, root + 0x18);
        let hw = u64_at(&uat, main);
        assert_eq!(u32_at(&uat, hw + 0xfb0), 0x8152, "chip id through root {role}");
        assert_eq!(u32_at(&uat, hw + 0x10e0), 13);
        assert_eq!(u32_at(&uat, main + 0x4c0), if secondary { 0x2a } else { 0x15 });
        let region_c = u64_at(&uat, root + 0x20);
        assert_eq!(u32_at(&uat, region_c + 0x40), 0x78);
        assert_eq!(u32_at(&uat, region_c + 0x7c8), 1500);
        let status_a = u64_at(&uat, root + 0xa8);
        assert_eq!(u32_at(&uat, status_a + 4), 1);
        fw_read(&uat, u64_at(&uat, root + 0x08), 0x1000);
        // Channel table: every populated state address and ring is mapped.
        for entry in 0..init::CHANNEL_TABLE_ENTRIES {
            for word in 0..4 {
                let address = u64_at(&uat, main + 0x20 + entry as u64 * 0x20 + word * 8);
                if address != 0 {
                    fw_read(&uat, address, 4);
                }
            }
        }
        if secondary {
            let config = u64_at(&uat, root + 0xb8);
            assert_eq!(config, private + 0xea80);
            assert_eq!(u32_at(&uat, config + 0x6c4), 1);
            let extra = u64_at(&uat, root + 0xc0);
            assert_eq!(u32_at(&uat, extra + 0x10), 2);
            let policy = u64_at(&uat, main + 0x481);
            assert_eq!(policy, hw + 0x5380);
            assert_eq!(u64_at(&uat, policy + 0x6c14), timestamp);
            assert_eq!(u32_at(&uat, policy + 0xb38), 75);
            fw_read(&uat, u64_at(&uat, policy + 0x3d30), 8);
            let aon = u64_at(&uat, policy + 0x64d8);
            assert_eq!(u32_at(&uat, aon), 0x6730_5452);
        } else {
            let status_b = u64_at(&uat, root + 0xb0);
            assert_eq!(status_b, private + 0x340);
            assert_eq!(u32_at(&uat, status_b + 0xe714), 1);
            // Follow the new cross-link pair both ways.
            let link = u64_at(&uat, status_b + 0xe720);
            assert_eq!(u32_at(&uat, link), 1);
            assert_eq!(u64_at(&uat, link + 4), status_b + 0xe71c);
            fw_read(&uat, u64_at(&uat, status_b + 0x4ac0), 0x20);
            fw_read(&uat, u64_at(&uat, status_b + 0x4ac8), 0x1800);
            assert_eq!(u64_at(&uat, root + 0xb8), 0);
            // Native M6 allocation: PB 16 KiB, UMA 32 KiB (256 x 128-byte
            // descriptors). Walk both complete aliases, including slot 255.
            for (high, low, size) in [(0x2e0u64, 0x2d8u64, 0x4000usize),
                                      (0x2f0, 0x2e8, 0x8000)] {
                let high = u64_at(&uat, main + high);
                let low = u64_at(&uat, main + low);
                for page in (0..size).step_by(0x4000) {
                    assert_eq!(uat.kernel_vm().translate_iova(high + page as u64).unwrap(),
                               uat.kernel_lower_vm().translate_iova(low + page as u64).unwrap());
                    assert!(fw_read(&uat, high + page as u64, 0x4000).iter().all(|b| *b == 0),
                            "descriptor tables must start empty");
                }
                assert_eq!(u64_at(&uat, high + size as u64 - 8), 0);
            }
            assert!(uat.kernel_vm().translate_iova(u64_at(&uat, main + 0x2d0)).is_err(), "sentinel stays unmapped");
            // QoS block and the four completion queues.
            fw_read(&uat, u64_at(&uat, hw + 0x27cc) + 0xe00, 8);
            for ordinal in 0..4u64 {
                let descriptor = hw + 0x2734 + ordinal * 0x20;
                let (low, high) = (u64_at(&uat, descriptor), u64_at(&uat, descriptor + 8));
                assert_eq!(uat.kernel_vm().translate_iova(high).unwrap(),
                           uat.kernel_lower_vm().translate_iova(low).unwrap());
                assert_eq!(u32_at(&uat, descriptor + 0x18), ordinal as u32);
            }
            // Register windows: device VA -> the slot's physical page, with
            // read-only mappings exactly for flag-0 windows.
            let mut windows = 0;
            for slot in 0..init::G18G_26A428_HW_REGISTER_MAP_SLOTS as u64 {
                let entry = hw + 0x640 + slot * 0x28;
                let (phys, va, flag) = (u64_at(&uat, entry), u64_at(&uat, entry + 8), u32_at(&uat, entry + 0x20));
                if va == 0 {
                    assert_eq!(phys, 0, "slot {slot}");
                    continue;
                }
                windows += 1;
                assert_eq!(uat.kernel_vm().translate_iova(va).unwrap() as u64, phys, "slot {slot}");
                let prot = uat.kernel_vm().prot(va).unwrap();
                assert_eq!(prot, if flag == 2 { mmu::PROT_FW_MMIO_RW } else { mmu::PROT_FW_MMIO_RO });
            }
            assert_eq!(windows, init::G18G_26A428_REGISTER_WINDOWS.len());
            assert_eq!(u64_at(&uat, hw + 0x640 + 41 * 0x28 + 8), 0);
        }
    }

    // Opening records staged, counters untouched by the host so far.
    assert_eq!(memory.control_counters().unwrap(), [[0, 0, 1], [0, 0, 1]]);
    assert_eq!(memory.publish_idle_wake().err(), Some(kernel::prelude::EBUSY));
    // Simulate both roles consuming their opening record.
    let grids = init::G18G_26A428_PRIVATE_STATE_GRIDS;
    for grid in grids {
        for index in 0..2u64 {
            fw_write(&uat, private + grid as u64 + 0x300 + index * 0x10, &1u32.to_le_bytes());
        }
    }
    assert_eq!(memory.control_counters().unwrap(), [[1; 3]; 2]);
    memory.publish_idle_wake().unwrap();
    let main0 = u64_at(&uat, memory.root(false) + 0x18);
    assert_eq!(u32_at(&uat, main0 + 0x4c0 + 0x40), 0x0a);
    assert_eq!(u32_at(&uat, main0 + 0x4c0 + 0x44), 0);
    assert_eq!(memory.control_counters().unwrap()[0], [1, 1, 2]);
    assert_eq!(memory.publish_idle_wake().err(), Some(kernel::prelude::EBUSY));

    assert_eq!(memory.completion_cursors().unwrap(), [0; 4]);
    let hw = u64_at(&uat, main0);
    fw_write(&uat, hw + 0x2734 + 2 * 0x20 + 0x1c, &7u32.to_le_bytes());
    assert_eq!(memory.completion_cursors().unwrap(), [0, 0, 7, 0]);

    let links = memory.status_links().unwrap();
    assert_eq!(links.status_b_header, 1);
    assert_eq!(links.status_b_word, 0);
    assert_eq!(links.status_b_pointer, private + 0xea80 + 0x6c4);
    assert_eq!(links.status_config_word, 1);
    assert_eq!(links.status_config_pointer, private + 0x340 + 0xe71c);

    // Heartbeat drain: one role-0 event (type 4) and one role-1 trace record.
    let status_a = init::G18G_26A428_PRIVATE_STATUS_A.map(|offset| private + offset as u64);
    let mut event = [0u8; 0x48];
    event[0..4].copy_from_slice(&4u32.to_le_bytes());
    fw_write(&uat, status_a[0] + 0x2c0, &event);
    fw_write(&uat, status_a[0] + 0x40 + 0x20, &1u32.to_le_bytes());
    let mut trace = [0u8; 0x48];
    trace[0..4].copy_from_slice(&5u32.to_le_bytes());
    trace[4..12].copy_from_slice(&0x1234u64.to_le_bytes());
    trace[0x2c..0x30].copy_from_slice(&0x0300_0041u32.to_le_bytes());
    fw_write(&uat, status_a[1] + 0xa6ac0, &trace);
    fw_write(&uat, status_a[1] + 0x240 + 0x20, &1u32.to_le_bytes());
    assert!(memory.drain_reports(&dev).unwrap());
    assert_eq!(u32_at(&uat, status_a[0] + 0x40), 1);
    assert_eq!(u32_at(&uat, status_a[1] + 0x240), 1);
    assert_eq!(memory.report_counts(), [[1, 0], [0, 1]]);
    assert!(!memory.drain_reports(&dev).unwrap());
    let log = kernel::LOG.lock().unwrap();
    assert!(log.iter().any(|line| line.starts_with("G18_EVENT role=0 ordinal=0 type=4")), "{log:?}");
    assert!(log.iter().any(|line| line.starts_with("G18_TRACE role=1 ordinal=0 timestamp=4660 type=5 id=0x3000041")), "{log:?}");
    assert!(log.iter().any(|line| line.starts_with("G18G initdata: roots")), "{log:?}");
    // A corrupt producer index fails closed.
    fw_write(&uat, status_a[0] + 0x40 + 0x20, &300u32.to_le_bytes());
    drop(log);
    assert_eq!(memory.drain_reports(&dev).err(), Some(kernel::prelude::EIO));
    println!("G18_RESOURCES_OK windows={} log_lines={}", init::G18G_26A428_REGISTER_WINDOWS.len(),
             kernel::LOG.lock().unwrap().len());
    drop(memory);
    assert!(uat.kernel.pages.lock().unwrap().is_empty());
    assert!(uat.kernel.reservations.lock().unwrap().is_empty());
    assert!(uat.lower.pages.lock().unwrap().is_empty());
    assert!(uat.lower.reservations.lock().unwrap().is_empty());
    println!("G18_SPARSE_OWNER_OK mapped_pages=13 holes=1020 rollback=verified teardown=verified");
}
'''

source = (source.replace('@@G17_CONFIG@@', path('g17_config.rs'))
          .replace('@@G17_INITDATA@@', path('g17_initdata.rs'))
          .replace('@@G18_UMA@@', path('g18_uma.rs'))
          .replace('@@G18_POOL@@', path('g18_pool_resources.rs'))
          .replace('@@G18_RESOURCES@@', path('g18_resources.rs')))

with tempfile.TemporaryDirectory(prefix='g18-resources-') as tmp:
    kernel = Path(tmp) / 'kernel.rs'
    kernel.write_text(kernel_source)
    subprocess.run(['rustc', '--edition=2021', '--crate-type=rlib', '--crate-name=kernel',
                    str(kernel), '--out-dir', tmp], check=True)
    test = Path(tmp) / 'test.rs'
    test.write_text(source)
    exe = Path(tmp) / 'test'
    subprocess.run(['rustc', '--edition=2021', '-D', 'warnings', '-A', 'dead_code', str(test),
                    '--extern', 'kernel=' + str(Path(tmp) / 'libkernel.rlib'), '-o', str(exe)],
                   check=True)
    output = subprocess.check_output([str(exe)], text=True)
    print(output, end='')
    assert 'G18_RESOURCES_OK' in output
