// SPDX-License-Identifier: GPL-2.0-only OR MIT

//! UAT Page Table management
//!
//! AGX GPUs use an MMU called the UAT, which is largely compatible with the ARM64 page table
//! format. This module manages the actual page tables by allocating raw memory pages from
//! the kernel page allocator.

use core::fmt::Debug;
use core::mem::size_of;
use core::ops::Range;
#[cfg(CONFIG_DEV_COREDUMP)]
use core::ptr::NonNull;
use core::sync::atomic::{
    AtomicU64,
    Ordering, //
};

use kernel::{
    addr::PhysicalAddr,
    error::Result,
    page::Page,
    prelude::*, //
};
#[cfg(CONFIG_DEV_COREDUMP)]
use kernel::{
    bindings,
    ffi::c_void,
    types::Owned,
    uapi::{
        PF_R,
        PF_W,
        PF_X, //
    },
};

use crate::debug::*;
use crate::{uat::UatGeometry, util::align};

pub(crate) use crate::uat::{UAT_PGBIT, UAT_PGMSK, UAT_PGSZ};

const DEBUG_CLASS: DebugFlags = DebugFlags::PgTable;

type Pte = AtomicU64;

const PTE_BIT: usize = 3; // log2(sizeof(Pte))
const PTE_SIZE: usize = 1 << PTE_BIT;

/// Number of PTEs per page.
const UAT_NPTE: usize = UAT_PGSZ / size_of::<Pte>();

/// Number of address bits to address a level
const UAT_LVBIT: usize = crate::uat::UAT_LVBIT;
/// Number of entries per level
const UAT_LVSZ: usize = UAT_NPTE;
/// Mask of level bits
const UAT_LVMSK: u64 = (UAT_LVSZ - 1) as u64;

const UAT_LEVELS: usize = crate::uat::UAT_LEVELS;

const PTE_TYPE_BITS: u64 = 3;
const PTE_TYPE_LEAF_TABLE: u64 = 3;

const UAT_NON_GLOBAL: u64 = 1 << 11;
const UAT_AP_SHIFT: u32 = 6;
const UAT_AP_BITS: u64 = 3 << UAT_AP_SHIFT;
const UAT_HIGH_BITS_SHIFT: u32 = 52;
const UAT_HIGH_BITS: u64 = 0xfff << UAT_HIGH_BITS_SHIFT;
const UAT_MEMATTR_SHIFT: u32 = 2;
const UAT_MEMATTR_BITS: u64 = 7 << UAT_MEMATTR_SHIFT;

const UAT_PROT_BITS: u64 = UAT_AP_BITS | UAT_MEMATTR_BITS | UAT_HIGH_BITS;

const UAT_AF: u64 = 1 << 10;

const MEMATTR_CACHED: u8 = 0;
const MEMATTR_DEV: u8 = 1;
const MEMATTR_UNCACHED: u8 = 2;

const AP_FW_GPU: u8 = 0;
const AP_FW: u8 = 1;
const AP_GPU: u8 = 2;

const HIGH_BITS_PXN: u16 = 1 << 1;
const HIGH_BITS_UXN: u16 = 1 << 2;
const HIGH_BITS_GPU_ACCESS: u16 = 1 << 3;

const fn complete_coverage(expected: u64, visited: u64, permitted: bool) -> bool {
    permitted && visited == expected
}

/// One maximal run of leaf-mapped pages found by
/// [`UatPageTable::collect_mapped_ranges`].
///
/// `pte` is the run's first leaf entry, kept so a caller can tell a
/// GPU-readable mapping apart from a firmware-only one without walking again.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub(crate) struct MappedRange {
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) pte: u64,
}

fn push_mapped_range(
    out: &mut [MappedRange],
    count: &mut usize,
    truncated: &mut bool,
    run: MappedRange,
) {
    match out.get_mut(*count) {
        Some(slot) => {
            *slot = run;
            *count += 1;
        }
        None => *truncated = true,
    }
}

#[cfg(CONFIG_DEV_COREDUMP)]
pub(crate) const PTE_ADDR_BITS: u64 = (!UAT_PGMSK as u64) & (!UAT_HIGH_BITS);

#[derive(Debug, Copy, Clone)]
pub(crate) struct Prot {
    memattr: u8,
    ap: u8,
    high_bits: u16,
}

// Firmware + GPU access
const PROT_FW_GPU_NA: Prot = Prot::from_bits(AP_FW_GPU, 0, 0);
const _PROT_FW_GPU_RO: Prot = Prot::from_bits(AP_FW_GPU, 0, 1);
const _PROT_FW_GPU_WO: Prot = Prot::from_bits(AP_FW_GPU, 1, 0);
const PROT_FW_GPU_RW: Prot = Prot::from_bits(AP_FW_GPU, 1, 1);

// Firmware only access
const PROT_FW_RO: Prot = Prot::from_bits(AP_FW, 0, 0);
const _PROT_FW_NA: Prot = Prot::from_bits(AP_FW, 0, 1);
const PROT_FW_RW: Prot = Prot::from_bits(AP_FW, 1, 0);
const PROT_FW_RW_GPU_RO: Prot = Prot::from_bits(AP_FW, 1, 1);

// GPU only access
const PROT_GPU_RO: Prot = Prot::from_bits(AP_GPU, 0, 0);
const PROT_GPU_WO: Prot = Prot::from_bits(AP_GPU, 0, 1);
const PROT_GPU_RW: Prot = Prot::from_bits(AP_GPU, 1, 0);
const _PROT_GPU_NA: Prot = Prot::from_bits(AP_GPU, 1, 1);

#[cfg(CONFIG_DEV_COREDUMP)]
const PF_RW: u32 = PF_R | PF_W;
#[cfg(CONFIG_DEV_COREDUMP)]
const PF_RX: u32 = PF_R | PF_X;

// For crash dumps
#[cfg(CONFIG_DEV_COREDUMP)]
const PROT_TO_PERMS_FW: [[u32; 4]; 4] = [
    [0, 0, 0, PF_RW],
    [0, PF_RW, 0, PF_RW],
    [PF_RX, PF_RX, 0, PF_R],
    [PF_RX, PF_RW, 0, PF_R],
];
#[cfg(CONFIG_DEV_COREDUMP)]
const PROT_TO_PERMS_OS: [[u32; 4]; 4] = [
    [0, PF_R, PF_W, PF_RW],
    [PF_R, 0, PF_RW, PF_RW],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
];

pub(crate) mod prot {
    pub(crate) use super::Prot;
    use super::*;

    /// Firmware MMIO R/W
    pub(crate) const PROT_FW_MMIO_RW: Prot = PROT_FW_RW.memattr(MEMATTR_DEV);
    /// Firmware MMIO R/O
    pub(crate) const PROT_FW_MMIO_RO: Prot = PROT_FW_RO.memattr(MEMATTR_DEV);
    /// Firmware shared (uncached) RW
    pub(crate) const PROT_FW_SHARED_RW: Prot = PROT_FW_RW.memattr(MEMATTR_UNCACHED);
    /// Firmware shared (uncached) RO
    pub(crate) const PROT_FW_SHARED_RO: Prot = PROT_FW_RO.memattr(MEMATTR_UNCACHED);
    /// Firmware private (cached) RW
    pub(crate) const PROT_FW_PRIV_RW: Prot = PROT_FW_RW.memattr(MEMATTR_CACHED);
    /// Firmware/GPU shared (uncached) RW
    pub(crate) const PROT_GPU_FW_SHARED_RW: Prot = PROT_FW_GPU_RW.memattr(MEMATTR_UNCACHED);
    /// Firmware/GPU shared (private) RW
    pub(crate) const PROT_GPU_FW_PRIV_RW: Prot = PROT_FW_GPU_RW.memattr(MEMATTR_CACHED);
    /// Firmware-RW/GPU-RO shared (private) RW
    pub(crate) const PROT_GPU_RO_FW_PRIV_RW: Prot = PROT_FW_RW_GPU_RO.memattr(MEMATTR_CACHED);
    /// GPU shared/coherent RW
    pub(crate) const PROT_GPU_SHARED_RW: Prot = PROT_GPU_RW.memattr(MEMATTR_UNCACHED);
    /// GPU shared/coherent RO
    pub(crate) const PROT_GPU_SHARED_RO: Prot = PROT_GPU_RO.memattr(MEMATTR_UNCACHED);
    /// GPU shared/coherent WO
    pub(crate) const PROT_GPU_SHARED_WO: Prot = PROT_GPU_WO.memattr(MEMATTR_UNCACHED);
}

impl Prot {
    const fn from_bits(ap: u8, uxn: u16, pxn: u16) -> Self {
        assert!(uxn <= 1);
        assert!(pxn <= 1);
        assert!(ap <= 3);

        Prot {
            high_bits: HIGH_BITS_GPU_ACCESS | (pxn * HIGH_BITS_PXN) | (uxn * HIGH_BITS_UXN),
            memattr: 0,
            ap,
        }
    }

    pub(crate) const fn from_pte(pte: u64) -> Self {
        Prot {
            high_bits: (pte >> UAT_HIGH_BITS_SHIFT) as u16,
            ap: ((pte & UAT_AP_BITS) >> UAT_AP_SHIFT) as u8,
            memattr: ((pte & UAT_MEMATTR_BITS) >> UAT_MEMATTR_SHIFT) as u8,
        }
    }

    #[cfg(CONFIG_DEV_COREDUMP)]
    pub(crate) const fn elf_flags(&self) -> u32 {
        let ap = (self.ap & 3) as usize;
        let uxn = if self.high_bits & HIGH_BITS_UXN != 0 {
            1
        } else {
            0
        };
        let pxn = if self.high_bits & HIGH_BITS_PXN != 0 {
            1
        } else {
            0
        };
        let gpu = self.high_bits & HIGH_BITS_GPU_ACCESS != 0;

        // Format:
        // [12 top bits of PTE] [12 bottom bits of PTE] [5 bits pad] [ELF RWX]
        let mut perms = if gpu {
            PROT_TO_PERMS_OS[ap][(uxn << 1) | pxn]
        } else {
            PROT_TO_PERMS_FW[ap][(uxn << 1) | pxn]
        };

        perms |= ((self.as_pte() >> 52) << 20) as u32;
        perms |= ((self.as_pte() & 0xfff) << 8) as u32;

        perms
    }

    const fn memattr(&self, memattr: u8) -> Self {
        Self { memattr, ..*self }
    }

    const fn as_pte(&self) -> u64 {
        (self.ap as u64) << UAT_AP_SHIFT
            | (self.high_bits as u64) << UAT_HIGH_BITS_SHIFT
            | (self.memattr as u64) << UAT_MEMATTR_SHIFT
            | UAT_AF
    }

    pub(crate) const fn allows_gpu(&self, need_read: bool, need_write: bool) -> bool {
        let uxn = self.high_bits & HIGH_BITS_UXN != 0;
        let pxn = self.high_bits & HIGH_BITS_PXN != 0;
        let (readable, writable) = match self.ap {
            AP_FW_GPU => (pxn, uxn),
            AP_FW => (uxn && pxn, false),
            AP_GPU => (!pxn, uxn != pxn),
            _ => (false, false),
        };
        (!need_read || readable) && (!need_write || writable)
    }

    pub(crate) const fn is_cached_noncoherent(&self) -> bool {
        self.ap != AP_GPU && self.memattr == MEMATTR_CACHED
    }

    pub(crate) const fn as_uncached(&self) -> Self {
        self.memattr(MEMATTR_UNCACHED)
    }
}

impl Default for Prot {
    fn default() -> Self {
        PROT_FW_GPU_NA
    }
}

const _: () = {
    assert!(PROT_GPU_RO.allows_gpu(true, false));
    assert!(!PROT_GPU_RO.allows_gpu(false, true));
    assert!(!PROT_GPU_WO.allows_gpu(true, false));
    assert!(PROT_GPU_WO.allows_gpu(false, true));
    assert!(PROT_GPU_RW.allows_gpu(true, false));
    assert!(PROT_GPU_RW.allows_gpu(false, true));
    assert!(PROT_GPU_RW.allows_gpu(true, true));
    assert!(!PROT_FW_RW.allows_gpu(true, false));
    assert!(!PROT_FW_RW.allows_gpu(false, true));
    assert!(!Prot::from_pte(0).allows_gpu(true, false));
    assert!(complete_coverage(3, 3, true));
    assert!(!complete_coverage(3, 2, true));
    assert!(!complete_coverage(3, 3, false));
};

#[cfg(CONFIG_DEV_COREDUMP)]
pub(crate) struct DumpedPage {
    pub(crate) iova: u64,
    pub(crate) pte: u64,
    pub(crate) data: Option<Owned<Page>>,
}

pub(crate) struct UatPageTable {
    ttb: PhysicalAddr,
    ttb_owned: bool,
    va_range: Range<u64>,
    geometry: UatGeometry,
    oas_mask: u64,
}

impl UatPageTable {
    pub(crate) fn new(ias: u8, oas: u32) -> Result<Self> {
        mod_pr_debug!("UATPageTable::new: ias={} oas={}\n", ias, oas);
        let geometry = UatGeometry::new(ias).ok_or(EINVAL)?;
        let ttb_page = Page::alloc_page(GFP_KERNEL | __GFP_ZERO)?;
        let ttb = Page::into_phys(ttb_page);
        Ok(UatPageTable {
            ttb,
            ttb_owned: true,
            va_range: 0..geometry.root_size(),
            geometry,
            oas_mask: (1u64 << oas) - 1,
        })
    }

    /// Resolve one mapped IOVA through this page table without touching the
    /// mapped memory. This is used to prove that separately named aliases
    /// refer to the same backing page before firmware can consume them.
    pub(crate) fn translate_iova(&mut self, iova: u64) -> Result<PhysicalAddr> {
        let page_mask = UAT_PGMSK as u64;
        let page = iova & !page_mask;
        let end = page.checked_add(UAT_PGSZ as u64).ok_or(EOVERFLOW)?;
        let offset = iova & page_mask;
        let oas_mask = self.oas_mask;
        let mut translated = None;
        self.with_pages(page..end, false, false, |_, ptes| {
            let pte = ptes.first().ok_or(EFAULT)?.load(Ordering::Acquire);
            if pte & PTE_TYPE_BITS != PTE_TYPE_LEAF_TABLE {
                return Err(EFAULT);
            }
            translated = Some((pte & oas_mask & !page_mask).checked_add(offset).ok_or(EOVERFLOW)?);
            Ok(())
        })?;
        translated.ok_or(EFAULT)
    }

    pub(crate) fn new_with_ttb(
        ttb: PhysicalAddr,
        va_range: Range<u64>,
        ias: u8,
        oas: u32,
    ) -> Result<Self> {
        mod_pr_debug!(
            "UATPageTable::new_with_ttb: ttb={:#x} range={:#x?} ias={} oas={}\n",
            ttb,
            va_range,
            ias,
            oas
        );
        let geometry = UatGeometry::new(ias).ok_or(EINVAL)?;
        if ttb & (UAT_PGMSK as PhysicalAddr) != 0 {
            return Err(EINVAL);
        }
        if (va_range.start | va_range.end) & (UAT_PGMSK as u64) != 0 {
            return Err(EINVAL);
        }
        // SAFETY: The TTB is should remain valid (if properly mapped), as it is bootloader-managed.
        if unsafe { Page::borrow_phys(&ttb) }.is_none() {
            pr_err!(
                "UATPageTable::new_with_ttb: ttb at {:#x} is not mapped (DT using no-map?)\n",
                ttb
            );
            return Err(EIO);
        }

        Ok(UatPageTable {
            ttb,
            ttb_owned: false,
            va_range,
            geometry,
            oas_mask: (1u64 << oas) - 1,
        })
    }

    pub(crate) fn ttb(&self) -> PhysicalAddr {
        self.ttb
    }

    fn with_pages<F>(
        &mut self,
        iova_range: Range<u64>,
        alloc: bool,
        free: bool,
        mut cb: F,
    ) -> Result
    where
        F: FnMut(u64, &[Pte]) -> Result,
    {
        mod_pr_debug!(
            "UATPageTable::with_pages: {:#x?} alloc={} free={}\n",
            iova_range,
            alloc,
            free
        );
        if (iova_range.start | iova_range.end) & (UAT_PGMSK as u64) != 0 {
            pr_err!(
                "UATPageTable::with_pages: iova range not aligned: {:#x?}\n",
                iova_range
            );
            return Err(EINVAL);
        }

        if iova_range.is_empty() {
            return Ok(());
        }

        let ias_mask = self.geometry.root_mask();
        let mut iova = iova_range.start & ias_mask;
        let mut last_iova = iova;
        // Handle the case where iova_range.end is just at the top boundary of the IAS
        let end = ((iova_range.end - 1) & ias_mask) + 1;

        let mut pt_addr: [Option<PhysicalAddr>; UAT_LEVELS] = Default::default();
        pt_addr[UAT_LEVELS - 1] = Some(self.ttb);

        'outer: while iova < end {
            mod_pr_debug!("UATPageTable::with_pages: iova={:#x}\n", iova);
            let addr_diff = last_iova ^ iova;
            for level in (0..UAT_LEVELS - 1).rev() {
                // If the iova has changed at this level or above, invalidate the physaddr
                if addr_diff & !((1 << (UAT_PGBIT + (level + 1) * UAT_LVBIT)) - 1) != 0 {
                    if let Some(phys) = pt_addr[level].take() {
                        if free {
                            mod_pr_debug!(
                                "UATPageTable::with_pages: free level {} {:#x?}\n",
                                level,
                                phys
                            );
                            // SAFETY: Page tables for our VA ranges always come from Page::into_phys().
                            unsafe { Page::from_phys(phys) };
                        }
                        mod_pr_debug!("UATPageTable::with_pages: invalidate level {}\n", level);
                    }
                }
            }
            last_iova = iova;
            for level in (0..UAT_LEVELS - 1).rev() {
                // Fetch the page table base address for this level
                if pt_addr[level].is_none() {
                    let phys = pt_addr[level + 1].unwrap();
                    mod_pr_debug!(
                        "UATPageTable::with_pages: need level {}, parent phys {:#x}\n",
                        level,
                        phys
                    );
                    let upidx = ((iova >> (UAT_PGBIT + (level + 1) * UAT_LVBIT) as u64) & UAT_LVMSK)
                        as usize;
                    // SAFETY: Page table addresses are either allocated by us, or
                    // firmware-managed and safe to borrow a struct page from.
                    let upt = unsafe { Page::borrow_phys_unchecked(&phys) };
                    mod_pr_debug!("UATPageTable::with_pages: borrowed phys {:#x}\n", phys);
                    pt_addr[level] =
                        upt.with_pointer_into_page(upidx * PTE_SIZE, PTE_SIZE, |p| {
                            let uptep = p as *const _ as *const Pte;
                            // SAFETY: with_pointer_into_page() ensures the pointer is valid,
                            // and our index is aligned so it is safe to deref as an AtomicU64.
                            let upte = unsafe { &*uptep };
                            let mut upte_val = upte.load(Ordering::Relaxed);
                            // Allocate if requested
                            if upte_val == 0 && alloc {
                                let pt_page = Page::alloc_page(GFP_KERNEL | __GFP_ZERO)?;
                                mod_pr_debug!("UATPageTable::with_pages: alloc PT at {:#x}\n", pt_page.phys());
                                let pt_paddr = Page::into_phys(pt_page);
                                upte_val = pt_paddr | PTE_TYPE_LEAF_TABLE;
                                upte.store(upte_val, Ordering::Relaxed);
                            }
                            if upte_val & PTE_TYPE_BITS == PTE_TYPE_LEAF_TABLE {
                                Ok(Some(upte_val & self.oas_mask & (!UAT_PGMSK as u64)))
                            } else if upte_val == 0 || (!alloc && !free) {
                                mod_pr_debug!("UATPageTable::with_pages: no level {}\n", level);
                                Ok(None)
                            } else {
                                pr_err!("UATPageTable::with_pages: Unexpected Table PTE value {:#x} at iova {:#x} index {} phys {:#x}\n", upte_val,
                                        iova, level + 1, phys + ((upidx * PTE_SIZE) as PhysicalAddr));
                                Ok(None)
                            }
                        })?;
                    mod_pr_debug!(
                        "UATPageTable::with_pages: level {} PT {:#x?}\n",
                        level,
                        pt_addr[level]
                    );
                }
                // If we don't have a page table, skip this entire level
                if pt_addr[level].is_none() {
                    let block = 1 << (UAT_PGBIT + UAT_LVBIT * (level + 1));
                    let old = iova;
                    iova = align(iova + 1, block);
                    mod_pr_debug!(
                        "UATPageTable::with_pages: skip {:#x} {:#x} -> {:#x}\n",
                        block,
                        old,
                        iova
                    );
                    continue 'outer;
                }
            }

            let idx = ((iova >> UAT_PGBIT as u64) & UAT_LVMSK) as usize;
            let max_count = UAT_NPTE - idx;
            let count = (((end - iova) >> UAT_PGBIT) as usize).min(max_count);
            let phys = pt_addr[0].unwrap();
            mod_pr_debug!(
                "UATPageTable::with_pages: leaf PT at {:#x} idx {:#x} count {:#x} iova {:#x}\n",
                phys,
                idx,
                count,
                iova
            );
            // SAFETY: Page table addresses are either allocated by us, or
            // firmware-managed and safe to borrow a struct page from.
            let pt = unsafe { Page::borrow_phys_unchecked(&phys) };
            pt.with_pointer_into_page(idx * PTE_SIZE, count * PTE_SIZE, |p| {
                let ptep = p as *const _ as *const Pte;
                // SAFETY: We know this is a valid pointer to PTEs and the range is valid and
                // checked by with_pointer_into_page().
                let ptes = unsafe { core::slice::from_raw_parts(ptep, count) };
                cb(iova, ptes)?;
                Ok(())
            })?;

            let block = 1 << (UAT_PGBIT + UAT_LVBIT);
            iova = align(iova + 1, block);
        }

        if free {
            for level in (0..UAT_LEVELS - 1).rev() {
                if let Some(phys) = pt_addr[level] {
                    mod_pr_debug!(
                        "UATPageTable::with_pages: free level {} {:#x?}\n",
                        level,
                        phys
                    );
                    // SAFETY: Page tables for our VA ranges always come from Page::into_phys().
                    unsafe { Page::from_phys(phys) };
                }
            }
        }

        Ok(())
    }

    pub(crate) fn alloc_pages(&mut self, iova_range: Range<u64>) -> Result {
        mod_pr_debug!("UATPageTable::alloc_pages: {:#x?}\n", iova_range);
        self.with_pages(iova_range, true, false, |_, _| Ok(()))
    }

    /// Reserve every intermediate table required by a new leaf mapping and
    /// prove that the complete destination span is empty before any leaf is
    /// installed.  GPUVM and the driver's fixed-mapping allocator are
    /// intentionally separate address managers, so neither allocator alone
    /// can detect ownership held by the other one.  The page table is the
    /// authoritative final arbiter.
    pub(crate) fn prepare_map(&mut self, iova_range: Range<u64>) -> Result {
        if iova_range.is_empty()
            || (iova_range.start | iova_range.end) & UAT_PGMSK as u64 != 0
        {
            return Err(EINVAL);
        }

        let mut occupied = None;
        self.with_pages(iova_range.clone(), false, false, |iova, ptes| {
            for (index, pte) in ptes.iter().enumerate() {
                let value = pte.load(Ordering::Acquire);
                if value != 0 && occupied.is_none() {
                    occupied = Some((iova + (index * UAT_PGSZ) as u64, value));
                }
            }
            Ok(())
        })?;
        if let Some((iova, pte)) = occupied {
            pr_err!(
                "UATPageTable::prepare_map: Page at IOVA {:#x} is already owned (PTE: {:#x})\n",
                iova,
                pte
            );
            return Err(EBUSY);
        }

        // Do all fallible page-table allocation before installing any leaf.
        // A failed allocation can leave empty intermediate tables behind, but
        // it can never leave a partially visible object mapping.
        self.alloc_pages(iova_range)
    }

    /// Check that every page in an aligned range is mapped with the requested
    /// GPU permissions. Missing intermediate page tables are detected by the
    /// visited-page count because `with_pages` skips those holes.
    pub(crate) fn covers_range(
        &mut self,
        iova_range: Range<u64>,
        need_read: bool,
        need_write: bool,
    ) -> Result<bool> {
        if iova_range.is_empty() || (iova_range.start | iova_range.end) & UAT_PGMSK as u64 != 0 {
            return Ok(false);
        }

        let expected = (iova_range.end - iova_range.start) >> UAT_PGBIT;
        let mut visited = 0u64;
        let mut permitted = true;
        self.with_pages(iova_range, false, false, |_, ptes| {
            visited = visited.checked_add(ptes.len() as u64).ok_or(EOVERFLOW)?;
            for pte in ptes {
                let value = pte.load(Ordering::Acquire);
                if value & PTE_TYPE_BITS != PTE_TYPE_LEAF_TABLE
                    || !Prot::from_pte(value).allows_gpu(need_read, need_write)
                {
                    permitted = false;
                }
            }
            Ok(())
        })?;
        Ok(complete_coverage(expected, visited, permitted))
    }

    /// Collect the maximal runs of leaf-mapped pages inside `iova_range`.
    ///
    /// This reads page-table memory only -- ordinary DRAM, never GPU MMIO --
    /// so it is safe to call with the GPU cores power-gated, which is the
    /// state a faulted submission leaves them in.  Holes are skipped at
    /// whole-table granularity by `with_pages`, so walking the full user
    /// window costs one pass over the tables that actually exist.
    ///
    /// Returns the number of runs written to `out` and whether `out` was too
    /// small to hold them all.
    pub(crate) fn collect_mapped_ranges(
        &mut self,
        iova_range: Range<u64>,
        out: &mut [MappedRange],
    ) -> Result<(usize, bool)> {
        if (iova_range.start | iova_range.end) & UAT_PGMSK as u64 != 0 {
            return Err(EINVAL);
        }

        let page = UAT_PGSZ as u64;
        let mut count = 0usize;
        let mut truncated = false;
        let mut current: Option<MappedRange> = None;

        self.with_pages(iova_range, false, false, |iova, ptes| {
            for (idx, ppte) in ptes.iter().enumerate() {
                let pte = ppte.load(Ordering::Acquire);
                let addr = iova + (idx as u64) * page;
                if pte & PTE_TYPE_BITS == PTE_TYPE_LEAF_TABLE {
                    if let Some(run) = current.as_mut() {
                        if run.end == addr {
                            run.end = addr + page;
                            continue;
                        }
                    }
                    if let Some(run) = current.take() {
                        push_mapped_range(out, &mut count, &mut truncated, run);
                    }
                    current = Some(MappedRange {
                        start: addr,
                        end: addr + page,
                        pte,
                    });
                } else if let Some(run) = current.take() {
                    push_mapped_range(out, &mut count, &mut truncated, run);
                }
            }
            Ok(())
        })?;

        if let Some(run) = current.take() {
            push_mapped_range(out, &mut count, &mut truncated, run);
        }
        Ok((count, truncated))
    }

    fn pte_bits(&self) -> u64 {
        if self.ttb_owned {
            // Owned page tables are userspace, so non-global
            PTE_TYPE_LEAF_TABLE | UAT_NON_GLOBAL
        } else {
            // The sole non-owned page table is kernelspace, so global
            PTE_TYPE_LEAF_TABLE
        }
    }

    pub(crate) fn map_pages(
        &mut self,
        iova_range: Range<u64>,
        mut phys: PhysicalAddr,
        prot: Prot,
        one_page: bool,
    ) -> Result {
        mod_pr_debug!(
            "UATPageTable::map_pages: {:#x?} {:#x?} {:?}\n",
            iova_range,
            phys,
            prot
        );
        if phys & (UAT_PGMSK as PhysicalAddr) != 0 {
            pr_err!("UATPageTable::map_pages: phys not aligned: {:#x?}\n", phys);
            return Err(EINVAL);
        }

        let pte_bits = self.pte_bits();

        // Callers prepare the complete object range while holding the VM
        // execution lock.  Do not allocate here: mapping one scatterlist run
        // and then failing to allocate tables for a later run would create an
        // ownerless partial mapping.
        let expected = (iova_range.end - iova_range.start) >> UAT_PGBIT;
        let mut visited = 0u64;
        let mut occupied = None;
        self.with_pages(iova_range.clone(), false, false, |iova, ptes| {
            visited = visited.checked_add(ptes.len() as u64).ok_or(EOVERFLOW)?;
            for (idx, pte) in ptes.iter().enumerate() {
                let value = pte.load(Ordering::Acquire);
                if value != 0 && occupied.is_none() {
                    occupied = Some((iova + (idx * UAT_PGSZ) as u64, value));
                }
            }
            Ok(())
        })?;
        if visited != expected {
            pr_err!(
                "UATPageTable::map_pages: destination tables were not prepared ({}/{} pages)\n",
                visited,
                expected
            );
            return Err(EFAULT);
        }
        if let Some((iova, pte)) = occupied {
            pr_err!(
                "UATPageTable::map_pages: refusing to overwrite owned page at IOVA {:#x} (PTE: {:#x})\n",
                iova,
                pte
            );
            return Err(EBUSY);
        }
        self.with_pages(iova_range, false, false, |_, ptes| {
            for pte in ptes {
                pte.store(phys | prot.as_pte() | pte_bits, Ordering::Relaxed);
                if !one_page {
                    phys += UAT_PGSZ as PhysicalAddr;
                }
            }
            Ok(())
        })
    }

    pub(crate) fn reprot_pages(&mut self, iova_range: Range<u64>, prot: Prot) -> Result {
        mod_pr_debug!(
            "UATPageTable::reprot_pages: {:#x?} {:?}\n",
            iova_range,
            prot
        );
        // Preserve the established zero-length range semantics.  In
        // particular, GPUVM can produce an empty remap hole when the retained
        // prefix and suffix exactly cover the old VA.
        if iova_range.is_empty() {
            return Ok(());
        }
        if !self.covers_range(iova_range.clone(), false, false)? {
            pr_err!(
                "UATPageTable::reprot_pages: refusing partially unmapped range {:#x?}\n",
                iova_range
            );
            return Err(EFAULT);
        }
        self.with_pages(iova_range, false, false, |iova, ptes| {
            for (idx, pte) in ptes.iter().enumerate() {
                let ptev = pte.load(Ordering::Relaxed);
                if ptev & PTE_TYPE_BITS != PTE_TYPE_LEAF_TABLE {
                    pr_err!(
                        "UATPageTable::reprot_pages: Page at IOVA {:#x} is unmapped (PTE: {:#x})\n",
                        iova + (idx * UAT_PGSZ) as u64,
                        ptev
                    );
                    return Err(EFAULT);
                }
                pte.store((ptev & !UAT_PROT_BITS) | prot.as_pte(), Ordering::Relaxed);
            }
            Ok(())
        })
    }

    pub(crate) fn unmap_pages(&mut self, iova_range: Range<u64>) -> Result {
        mod_pr_debug!("UATPageTable::unmap_pages: {:#x?}\n", iova_range);
        // An empty GPUVM remap hole is a valid no-op, not evidence of a stale
        // owner.  covers_range() intentionally reports empty ranges as false,
        // so handle that case before the ownership preflight.
        if iova_range.is_empty() {
            return Ok(());
        }
        // Keep the ownership preflight in this frame. `KernelMapping::drop`
        // runs from deeply nested GPUVM constructor unwind paths, and calling
        // the generic `covers_range` walker here stacked two independently
        // large page-table traversal frames. A routine two-root descriptor
        // allocation collision then overflowed the 32-KiB task stack before
        // either walker touched a PTE. This is the same two-pass invariant as
        // `covers_range`: count every expected leaf and reject any hole before
        // the second pass clears a single entry, but without nesting walkers.
        let expected = (iova_range.end - iova_range.start) >> UAT_PGBIT;
        let mut visited = 0u64;
        let mut fully_mapped = true;
        self.with_pages(iova_range.clone(), false, false, |_, ptes| {
            visited = visited.checked_add(ptes.len() as u64).ok_or(EOVERFLOW)?;
            for pte in ptes {
                if pte.load(Ordering::Acquire) & PTE_TYPE_BITS != PTE_TYPE_LEAF_TABLE {
                    fully_mapped = false;
                }
            }
            Ok(())
        })?;
        if !complete_coverage(expected, visited, fully_mapped) {
            pr_err!(
                "UATPageTable::unmap_pages: refusing partially unmapped range {:#x?}\n",
                iova_range
            );
            return Err(EFAULT);
        }
        self.with_pages(iova_range, false, false, |iova, ptes| {
            for (idx, pte) in ptes.iter().enumerate() {
                if pte.load(Ordering::Relaxed) & PTE_TYPE_LEAF_TABLE == 0 {
                    pr_err!(
                        "UATPageTable::unmap_pages: Page at IOVA {:#x} already unmapped\n",
                        iova + (idx * UAT_PGSZ) as u64
                    );
                    return Err(EFAULT);
                }
                pte.store(0, Ordering::Relaxed);
            }
            Ok(())
        })
    }

    #[cfg(CONFIG_DEV_COREDUMP)]
    pub(crate) fn dump_pages(&mut self, iova_range: Range<u64>) -> Result<KVVec<DumpedPage>> {
        let mut pages = KVVec::new();
        let oas_mask = self.oas_mask;
        let iova_base = self.va_range.start & !self.geometry.root_mask();
        self.with_pages(iova_range, false, false, |iova, ptes| {
            let iova = iova | iova_base;
            for (idx, ppte) in ptes.iter().enumerate() {
                let pte = ppte.load(Ordering::Relaxed);
                if (pte & PTE_TYPE_LEAF_TABLE) != PTE_TYPE_LEAF_TABLE {
                    continue;
                }
                let memattr = ((pte & UAT_MEMATTR_BITS) >> UAT_MEMATTR_SHIFT) as u8;

                if !(memattr == MEMATTR_CACHED || memattr == MEMATTR_UNCACHED) {
                    pages.push(
                        DumpedPage {
                            iova: iova + (idx * UAT_PGSZ) as u64,
                            pte,
                            data: None,
                        },
                        GFP_KERNEL,
                    )?;
                    continue;
                }
                let phys = pte & oas_mask & (!UAT_PGMSK as u64);
                // SAFETY: GPU pages are either firmware/preallocated pages
                // (which the kernel isn't concerned with and are either in
                // the page map or not, and if they aren't, borrow_phys()
                // will fail), or GPU page table pages (which we own),
                // or GEM buffer pages (which are locked while they are
                // mapped in the page table), so they should be safe to
                // borrow.
                //
                // This does trust the firmware not to have any weird
                // mappings in its own internal page tables, but since
                // those are managed by the uPPL which is privileged anyway,
                // this trust does not actually extend any trust boundary.
                let src_page = match unsafe { Page::borrow_phys(&phys) } {
                    Some(page) => page,
                    None => {
                        pages.push(
                            DumpedPage {
                                iova: iova + (idx * UAT_PGSZ) as u64,
                                pte,
                                data: None,
                            },
                            GFP_KERNEL,
                        )?;
                        continue;
                    }
                };
                let dst_page = Page::alloc_page(GFP_KERNEL)?;
                src_page.with_page_mapped(|psrc| -> Result {
                    // SAFETY: This could technically still have a data race with the firmware
                    // or other driver code (or even userspace with timestamp buffers), but while
                    // the Rust language technically says this is UB, in the real world, using
                    // atomic reads for this is guaranteed to never cause any harmful effects
                    // other than possibly reading torn/unreliable data. At least on ARM64 anyway.
                    //
                    // (Yes, I checked with Rust people about this. ~~ Lina)
                    //
                    let src_items = unsafe {
                        core::slice::from_raw_parts(
                            psrc as *const AtomicU64,
                            UAT_PGSZ / core::mem::size_of::<AtomicU64>(),
                        )
                    };
                    dst_page.with_page_mapped(|pdst| -> Result {
                        // SAFETY: We own the destination page, so it is safe to view its contents
                        // as a u64 slice.
                        let dst_items = unsafe {
                            core::slice::from_raw_parts_mut(
                                pdst as *mut u64,
                                UAT_PGSZ / core::mem::size_of::<u64>(),
                            )
                        };
                        for (si, di) in src_items.iter().zip(dst_items.iter_mut()) {
                            *di = si.load(Ordering::Relaxed);
                        }
                        Ok(())
                    })?;
                    Ok(())
                })?;
                pages.push(
                    DumpedPage {
                        iova: iova + (idx * UAT_PGSZ) as u64,
                        pte,
                        data: Some(dst_page),
                    },
                    GFP_KERNEL,
                )?;
            }
            Ok(())
        })?;
        Ok(pages)
    }

    /// Copy a firmware-owned DVA through a physical UAT root which is not
    /// represented by Linux `struct page` ownership.
    ///
    /// This is intentionally a bounded read-only walker. It temporarily maps
    /// one 16 KiB page-table or payload page at a time and never changes PTEs
    /// or reclaims firmware-owned physical memory.
    #[cfg(CONFIG_DEV_COREDUMP)]
    pub(crate) fn copy_from_phys_root(
        ttb: PhysicalAddr,
        address: u64,
        out: &mut [u8],
        ias: u8,
        oas: u32,
    ) -> Result {
        Self::copy_from_phys_root_inner(ttb, address, out, ias, oas, false)
    }

    /// Copy a live firmware-owned DVA after invalidating every page-table and
    /// payload cacheline before it is read. This is the dynamic-pointer form
    /// of [`Self::copy_live_firmware_physical_range`].
    #[cfg(CONFIG_DEV_COREDUMP)]
    pub(crate) fn copy_live_from_phys_root(
        ttb: PhysicalAddr,
        address: u64,
        out: &mut [u8],
        ias: u8,
        oas: u32,
    ) -> Result {
        Self::copy_from_phys_root_inner(ttb, address, out, ias, oas, true)
    }

    #[cfg(CONFIG_DEV_COREDUMP)]
    fn copy_from_phys_root_inner(
        ttb: PhysicalAddr,
        address: u64,
        out: &mut [u8],
        ias: u8,
        oas: u32,
        live: bool,
    ) -> Result {
        if out.is_empty() || (ttb | address | out.len() as u64) & 7 != 0 {
            return Err(EINVAL);
        }
        let geometry = UatGeometry::new(ias).ok_or(EINVAL)?;
        let end = address.checked_add(out.len() as u64).ok_or(EOVERFLOW)?;
        if end > geometry.root_size() {
            return Err(EFAULT);
        }
        let oas_mask = (1u64 << oas) - 1;
        let page_mask = UAT_PGMSK as u64;
        let mut copied = 0usize;

        while copied < out.len() {
            let iova = address.checked_add(copied as u64).ok_or(EOVERFLOW)?;
            let iova_page = iova & !page_mask;
            let mut table_phys = ttb;

            for level in (0..UAT_LEVELS - 1).rev() {
                let shift = UAT_PGBIT + (level + 1) * UAT_LVBIT;
                let index = ((iova_page >> shift) & UAT_LVMSK) as usize;
                let table = PhysPageMapping::new(table_phys)?;
                if live {
                    table.invalidate_live_range(index * PTE_SIZE, PTE_SIZE)?;
                }
                let pte = table.read_u64(index * PTE_SIZE)?;
                if pte & PTE_TYPE_BITS != PTE_TYPE_LEAF_TABLE {
                    pr_err!(
                        "UAT crash walker: level {} IOVA {:#x} PTE {:#x} at {:#x}\n",
                        level + 1,
                        iova_page,
                        pte,
                        table_phys + (index * PTE_SIZE) as u64
                    );
                    return Err(EFAULT);
                }
                table_phys = pte & oas_mask & !page_mask;
            }

            let leaf_index = ((iova_page >> UAT_PGBIT) & UAT_LVMSK) as usize;
            let leaf_table = PhysPageMapping::new(table_phys)?;
            if live {
                leaf_table.invalidate_live_range(leaf_index * PTE_SIZE, PTE_SIZE)?;
            }
            let leaf = leaf_table.read_u64(leaf_index * PTE_SIZE)?;
            if leaf & PTE_TYPE_BITS != PTE_TYPE_LEAF_TABLE {
                pr_err!(
                    "UAT crash walker: leaf IOVA {:#x} PTE {:#x} at {:#x}\n",
                    iova_page,
                    leaf,
                    table_phys + (leaf_index * PTE_SIZE) as u64
                );
                return Err(EFAULT);
            }
            let memattr = ((leaf & UAT_MEMATTR_BITS) >> UAT_MEMATTR_SHIFT) as u8;
            if memattr != MEMATTR_CACHED && memattr != MEMATTR_UNCACHED {
                return Err(EFAULT);
            }
            let payload_phys = leaf & oas_mask & !page_mask;
            let payload = PhysPageMapping::new(payload_phys)?;
            let offset = (iova & page_mask) as usize;
            let length = (UAT_PGSZ - offset).min(out.len() - copied);
            if live {
                payload.invalidate_live_range(offset, length)?;
            }
            payload.copy_atomic(offset, &mut out[copied..copied + length])?;
            copied = copied.checked_add(length).ok_or(EOVERFLOW)?;
        }
        Ok(())
    }

    /// Copy a bounded range from a firmware carveout identified by physical
    /// address. G17P's endpoint-1 crash addresses are not client UAT DVAs:
    /// they are `gfx-data + 0xa0000` and `gfx1-data + 0xa0000`, and the
    /// output-positive m1n1 postmortem reads them as physical memory.
    #[cfg(CONFIG_DEV_COREDUMP)]
    pub(crate) fn copy_firmware_physical_range(
        physical: PhysicalAddr,
        size: usize,
    ) -> Result<KVVec<u8>> {
        if size == 0 || (physical | size as u64) & 7 != 0 {
            return Err(EINVAL);
        }
        physical.checked_add(size as u64).ok_or(EOVERFLOW)?;
        let page_mask = UAT_PGMSK as u64;
        let mut data = KVVec::from_elem(0, size, GFP_KERNEL)?;
        let mut copied = 0usize;
        while copied < size {
            let address = physical.checked_add(copied as u64).ok_or(EOVERFLOW)?;
            let page = PhysPageMapping::new(address & !page_mask)?;
            let offset = (address & page_mask) as usize;
            let length = (UAT_PGSZ - offset).min(size - copied);
            page.copy_atomic(offset, &mut data[copied..copied + length])?;
            copied = copied.checked_add(length).ok_or(EOVERFLOW)?;
        }
        Ok(data)
    }

    /// Write a bounded range back into a firmware carveout identified by
    /// physical address, cleaning the AP cache behind every page.
    ///
    /// This is the one path in the driver that writes firmware-owned memory.
    /// It exists so a restarted ASC can be given back the `__DATA` segment
    /// iBoot placed for it, which the previous firmware instance has been
    /// running out of: an ASC coming out of a real block reset re-enters at
    /// `IORVBAR` with whatever its predecessor left in its own globals.
    #[cfg(CONFIG_DEV_COREDUMP)]
    pub(crate) fn restore_firmware_physical_range(
        physical: PhysicalAddr,
        data: &[u8],
    ) -> Result {
        if data.is_empty() || (physical | data.len() as u64) & 7 != 0 {
            return Err(EINVAL);
        }
        physical.checked_add(data.len() as u64).ok_or(EOVERFLOW)?;
        let page_mask = UAT_PGMSK as u64;
        let mut written = 0usize;
        while written < data.len() {
            let address = physical.checked_add(written as u64).ok_or(EOVERFLOW)?;
            let page = PhysPageMapping::new(address & !page_mask)?;
            let offset = (address & page_mask) as usize;
            let length = (UAT_PGSZ - offset).min(data.len() - written);
            page.write_atomic(offset, &data[written..written + length])?;
            written = written.checked_add(length).ok_or(EOVERFLOW)?;
        }
        Ok(())
    }

    /// Copy a live firmware-data range after invalidating the AP cache alias.
    ///
    /// Unlike crashlog storage, scheduler state can change between two host
    /// samples. A write-back `memremap()` can therefore return an AP cacheline
    /// retained from the preceding sample. Invalidate each covered cacheline
    /// before loading it, matching the physical-read cache maintenance used by
    /// the output-positive m1n1 host. This path never writes firmware memory.
    #[cfg(CONFIG_DEV_COREDUMP)]
    pub(crate) fn copy_live_firmware_physical_range(
        physical: PhysicalAddr,
        size: usize,
    ) -> Result<KVVec<u8>> {
        if size == 0 || (physical | size as u64) & 7 != 0 {
            return Err(EINVAL);
        }
        physical.checked_add(size as u64).ok_or(EOVERFLOW)?;
        let page_mask = UAT_PGMSK as u64;
        let mut data = KVVec::from_elem(0, size, GFP_KERNEL)?;
        let mut copied = 0usize;
        while copied < size {
            let address = physical.checked_add(copied as u64).ok_or(EOVERFLOW)?;
            let page = PhysPageMapping::new(address & !page_mask)?;
            let offset = (address & page_mask) as usize;
            let length = (UAT_PGSZ - offset).min(size - copied);
            page.invalidate_live_range(offset, length)?;
            page.copy_atomic(offset, &mut data[copied..copied + length])?;
            copied = copied.checked_add(length).ok_or(EOVERFLOW)?;
        }
        Ok(data)
    }
}

#[cfg(CONFIG_DEV_COREDUMP)]
struct PhysPageMapping {
    ptr: NonNull<c_void>,
}

/// A bounded read-only probe for one qword in live firmware-owned physical
/// memory.
///
/// Construction maps the containing page once. [`Self::observe_mask`] then
/// invalidates only the sampled AP cacheline before each load, so a short
/// firmware transition can be observed without remapping, writing, or holding
/// any firmware lock.
#[cfg(CONFIG_DEV_COREDUMP)]
pub(crate) struct LiveFirmwareU64Probe {
    page: PhysPageMapping,
    offset: usize,
}

/// Result of one bounded live-mask observation.
#[cfg(CONFIG_DEV_COREDUMP)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct LiveFirmwareMaskObservation {
    pub(crate) first: u64,
    pub(crate) last: u64,
    pub(crate) observed_or: u64,
    pub(crate) mask_seen: bool,
    pub(crate) polls: usize,
}

#[cfg(CONFIG_DEV_COREDUMP)]
impl LiveFirmwareU64Probe {
    const MAX_POLLS: usize = 256;

    pub(crate) fn new(physical: PhysicalAddr) -> Result<Self> {
        if physical & 7 != 0 {
            return Err(EINVAL);
        }
        let page_mask = UAT_PGMSK as u64;
        let page = PhysPageMapping::new(physical & !page_mask)?;
        let offset = (physical & page_mask) as usize;
        if offset.checked_add(size_of::<u64>()).ok_or(EOVERFLOW)? > UAT_PGSZ {
            return Err(EINVAL);
        }
        Ok(Self { page, offset })
    }

    /// Observe one mask for at most `polls` cache-invalidated qword loads.
    /// DIAGNOSTIC. Set bits in the probed qword; returns (before, after).
    pub(crate) fn set_bits(&self, bits: u64) -> Result<(u64, u64)> {
        self.page.set_bits_u64(self.offset, bits)
    }

    /// DIAGNOSTIC (v93r). `set_bits` on a freshly invalidated line, then
    /// clean it to the point of coherency so the coprocessor sees the bits.
    pub(crate) fn set_bits_visible(&self, bits: u64) -> Result<(u64, u64)> {
        self.page.invalidate_live_range(self.offset, size_of::<u64>())?;
        let result = self.page.set_bits_u64(self.offset, bits)?;
        self.page.clean_written_range(self.offset, size_of::<u64>())?;
        Ok(result)
    }

    /// DIAGNOSTIC. Read `out.len()` consecutive qwords from the probed qword
    /// onwards, within its page, after invalidating their AP cachelines. Not
    /// an atomic snapshot of the run.
    pub(crate) fn read_run(&self, out: &mut [u64]) -> Result {
        let size = out.len().checked_mul(size_of::<u64>()).ok_or(EOVERFLOW)?;
        self.page.invalidate_live_range(self.offset, size)?;
        for (index, value) in out.iter_mut().enumerate() {
            *value = self.page.read_u64(self.offset + index * size_of::<u64>())?;
        }
        Ok(())
    }

    pub(crate) fn observe_mask(
        &self,
        mask: u64,
        polls: usize,
    ) -> Result<LiveFirmwareMaskObservation> {
        if mask == 0 || polls == 0 || polls > Self::MAX_POLLS {
            return Err(EINVAL);
        }

        let mut first = 0;
        let mut last = 0;
        let mut observed_or = 0;
        let mut mask_seen = false;
        let mut completed = 0;
        for index in 0..polls {
            self.page.invalidate_live_range(self.offset, size_of::<u64>())?;
            let value = self.page.read_u64(self.offset)?;
            if index == 0 {
                first = value;
            }
            last = value;
            observed_or |= value;
            completed = index + 1;
            if value & mask != 0 {
                mask_seen = true;
                break;
            }
            core::hint::spin_loop();
        }

        Ok(LiveFirmwareMaskObservation {
            first,
            last,
            observed_or,
            mask_seen,
            polls: completed,
        })
    }
}

#[cfg(CONFIG_DEV_COREDUMP)]
impl PhysPageMapping {
    fn new(phys: PhysicalAddr) -> Result<Self> {
        if phys & UAT_PGMSK as u64 != 0 {
            return Err(EINVAL);
        }
        let ptr = unsafe {
            // SAFETY: the caller supplies a page-aligned physical address
            // obtained from a live firmware UAT PTE. The mapping is used only
            // for bounded reads and is released by Drop.
            bindings::memremap(
                phys,
                UAT_PGSZ,
                bindings::MEMREMAP_WB as _,
            )
        };
        let ptr = NonNull::new(ptr).ok_or_else(|| {
            pr_err!("UAT crash walker: memremap failed for {:#x}\n", phys);
            ENOMEM
        })?;
        Ok(Self { ptr })
    }

    fn invalidate_live_range(&self, offset: usize, size: usize) -> Result {
        let end = offset.checked_add(size).ok_or(EOVERFLOW)?;
        if size == 0 || end > UAT_PGSZ {
            return Err(EINVAL);
        }
        let ctr_el0: u64;
        unsafe {
            // SAFETY: CTR_EL0 is readable at EL1 and reports DminLine as
            // log2(words). The resulting line size controls only bounded cache
            // maintenance on this read-only mapping.
            core::arch::asm!(
                "mrs {ctr}, ctr_el0",
                ctr = out(reg) ctr_el0,
                options(nomem, nostack, preserves_flags),
            );
        }
        let line_size = 4usize
            .checked_shl(((ctr_el0 >> 16) & 0xf) as u32)
            .ok_or(EOVERFLOW)?;
        if !line_size.is_power_of_two() {
            return Err(EINVAL);
        }
        let start_address = self.ptr.as_ptr() as usize + offset;
        let end_address = self.ptr.as_ptr() as usize + end;
        let mut address = start_address & !(line_size - 1);
        unsafe {
            // SAFETY: the page remains mapped for the whole loop. `dc ivac`
            // invalidates AP cachelines only; this diagnostic never writes the
            // firmware-owned page. The barriers order invalidation before the
            // atomic loads in `copy_atomic()`.
            core::arch::asm!("dsb osh", options(nostack, preserves_flags));
            while address < end_address {
                core::arch::asm!(
                    "dc ivac, {address}",
                    address = in(reg) address,
                    options(nostack, preserves_flags),
                );
                address = address.checked_add(line_size).ok_or(EOVERFLOW)?;
            }
            core::arch::asm!("dsb osh", options(nostack, preserves_flags));
        }
        Ok(())
    }

    fn read_u64(&self, offset: usize) -> Result<u64> {
        let end = offset.checked_add(8).ok_or(EOVERFLOW)?;
        if offset & 7 != 0 || end > UAT_PGSZ {
            return Err(EINVAL);
        }
        let pointer = unsafe {
            // SAFETY: offset is qword-aligned and bounded to the mapped page.
            self.ptr.as_ptr().cast::<u8>().add(offset).cast::<AtomicU64>()
        };
        Ok(unsafe { &*pointer }.load(Ordering::Acquire))
    }

    /// DIAGNOSTIC. Set bits in one firmware-DRAM qword.
    ///
    /// The b1 image gates KTrace emission on class bytes at 0x17780c/0x17780d:
    /// bit 0 is the only one this driver has ever had set, which is why the
    /// halt-decision records (0x909 with the scan's own inputs, 0x113 at the
    /// thread-4 wake, 0x105, 0x110) have never appeared in a capture. Arming
    /// them is a read-modify-write of a debug-enable byte the firmware only
    /// reads; it steers no scheduling decision.
    fn set_bits_u64(&self, offset: usize, bits: u64) -> Result<(u64, u64)> {
        let end = offset.checked_add(8).ok_or(EOVERFLOW)?;
        if offset & 7 != 0 || end > UAT_PGSZ {
            return Err(EINVAL);
        }
        let pointer = unsafe {
            // SAFETY: offset is qword-aligned and bounded to the mapped page,
            // exactly as read_u64 checks above.
            self.ptr.as_ptr().cast::<u8>().add(offset).cast::<AtomicU64>()
        };
        let cell = unsafe { &*pointer };
        let before = cell.load(Ordering::Acquire);
        cell.store(before | bits, Ordering::Release);
        let after = cell.load(Ordering::Acquire);
        Ok((before, after))
    }

    /// Clean AP cachelines covering a range this mapping has just written.
    ///
    /// The mapping is write-back, so a store can sit in an AP cacheline the
    /// coprocessor's fetch path will not see. `invalidate_live_range()` above
    /// is the read-side mirror of this; keep the two symmetric.
    fn clean_written_range(&self, offset: usize, size: usize) -> Result {
        let end = offset.checked_add(size).ok_or(EOVERFLOW)?;
        if size == 0 || end > UAT_PGSZ {
            return Err(EINVAL);
        }
        let ctr_el0: u64;
        unsafe {
            // SAFETY: CTR_EL0 is readable at EL1 and reports DminLine as
            // log2(words); the value bounds cache maintenance only.
            core::arch::asm!(
                "mrs {ctr}, ctr_el0",
                ctr = out(reg) ctr_el0,
                options(nomem, nostack, preserves_flags),
            );
        }
        let line_size = 4usize
            .checked_shl(((ctr_el0 >> 16) & 0xf) as u32)
            .ok_or(EOVERFLOW)?;
        if !line_size.is_power_of_two() {
            return Err(EINVAL);
        }
        let start_address = self.ptr.as_ptr() as usize + offset;
        let end_address = self.ptr.as_ptr() as usize + end;
        let mut address = start_address & !(line_size - 1);
        unsafe {
            // SAFETY: the page stays mapped for the whole loop. `dc cvac`
            // cleans AP cachelines to the point of coherency; the barriers
            // order the preceding stores before the clean and the clean before
            // anything the coprocessor is subsequently told to do.
            core::arch::asm!("dsb osh", options(nostack, preserves_flags));
            while address < end_address {
                core::arch::asm!(
                    "dc cvac, {address}",
                    address = in(reg) address,
                    options(nostack, preserves_flags),
                );
                address = address.checked_add(line_size).ok_or(EOVERFLOW)?;
            }
            core::arch::asm!("dsb osh", options(nostack, preserves_flags));
        }
        Ok(())
    }

    fn write_atomic(&self, offset: usize, data: &[u8]) -> Result {
        let end = offset.checked_add(data.len()).ok_or(EOVERFLOW)?;
        if (offset | data.len()) & 7 != 0 || end > UAT_PGSZ {
            return Err(EINVAL);
        }
        for (index, source) in data.chunks_exact(size_of::<u64>()).enumerate() {
            let at = offset + index * size_of::<u64>();
            let pointer = unsafe {
                // SAFETY: `at` is qword-aligned and bounded to the mapped page
                // by the check above.
                self.ptr.as_ptr().cast::<u8>().add(at).cast::<AtomicU64>()
            };
            let mut qword = [0u8; size_of::<u64>()];
            qword.copy_from_slice(source);
            unsafe { &*pointer }.store(u64::from_le_bytes(qword), Ordering::Release);
        }
        self.clean_written_range(offset, data.len())
    }

    fn copy_atomic(&self, offset: usize, out: &mut [u8]) -> Result {
        let end = offset.checked_add(out.len()).ok_or(EOVERFLOW)?;
        if (offset | out.len()) & 7 != 0 || end > UAT_PGSZ {
            return Err(EINVAL);
        }
        for (index, destination) in out.chunks_exact_mut(size_of::<u64>()).enumerate() {
            destination.copy_from_slice(
                &self
                    .read_u64(offset + index * size_of::<u64>())?
                    .to_le_bytes(),
            );
        }
        Ok(())
    }
}

#[cfg(CONFIG_DEV_COREDUMP)]
impl Drop for PhysPageMapping {
    fn drop(&mut self) {
        unsafe {
            // SAFETY: ptr was returned by memremap and has not been unmapped.
            bindings::memunmap(self.ptr.as_ptr());
        }
    }
}

impl Drop for UatPageTable {
    fn drop(&mut self) {
        mod_pr_debug!("UATPageTable::drop range: {:#x?}\n", &self.va_range);
        if self
            .with_pages(self.va_range.clone(), false, true, |iova, ptes| {
                for (idx, pte) in ptes.iter().enumerate() {
                    if pte.load(Ordering::Relaxed) != 0 {
                        pr_err!(
                            "UATPageTable::drop: Leaked page at IOVA {:#x}\n",
                            iova + (idx * UAT_PGSZ) as u64
                        );
                    }
                }
                Ok(())
            })
            .is_err()
        {
            pr_err!("UATPageTable::drop failed to free page tables\n",);
        }
        if self.ttb_owned {
            mod_pr_debug!("UATPageTable::drop: Free TTB {:#x}\n", self.ttb);
            // SAFETY: If we own the ttb, it was allocated with Page::into_phys().
            unsafe {
                Page::from_phys(self.ttb);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_permissions_match_ro_wo_and_rw_pte_encodings() {
        assert!(PROT_GPU_RO.allows_gpu(true, false));
        assert!(!PROT_GPU_RO.allows_gpu(false, true));
        assert!(!PROT_GPU_RO.allows_gpu(true, true));

        assert!(!PROT_GPU_WO.allows_gpu(true, false));
        assert!(PROT_GPU_WO.allows_gpu(false, true));
        assert!(!PROT_GPU_WO.allows_gpu(true, true));

        assert!(PROT_GPU_RW.allows_gpu(true, false));
        assert!(PROT_GPU_RW.allows_gpu(false, true));
        assert!(PROT_GPU_RW.allows_gpu(true, true));

        assert!(!PROT_FW_RW.allows_gpu(true, false));
        assert!(!PROT_FW_RW.allows_gpu(false, true));
        assert!(!Prot::from_pte(0).allows_gpu(true, false));
    }

    #[test]
    fn skipped_page_table_hole_never_counts_as_coverage() {
        assert!(complete_coverage(3, 3, true));
        assert!(!complete_coverage(3, 2, true));
        assert!(!complete_coverage(3, 3, false));
    }
}
