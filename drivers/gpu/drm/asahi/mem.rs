// SPDX-License-Identifier: GPL-2.0-only OR MIT

//! ARM64 low level memory operations.
//!
//! This GPU uses CPU-side `tlbi` outer-shareable instructions to manage its TLBs.
//! Yes, really. Even though the VA address spaces are unrelated.
//!
//! Right now we pick our own ASIDs and don't coordinate with the CPU. This might result
//! in needless TLB shootdowns on the CPU side... TODO: fix this.

use core::arch::asm;
use core::cmp::min;

use crate::debug::*;
use crate::mmu;
use kernel::page::PAGE_SIZE;

type Asid = u8;

/// Invalidate the entire GPU TLB.
#[inline(always)]
pub(crate) fn tlbi_all() {
    // SAFETY: tlbi is always safe by definition
    unsafe {
        asm!(".arch armv8.4-a", "tlbi vmalle1os",);
    }
}

/// Invalidate all TLB entries for a given ASID.
#[inline(always)]
pub(crate) fn tlbi_asid(asid: Asid) {
    if debug_enabled(DebugFlags::ConservativeTlbi) {
        tlbi_all();
        sync();
        return;
    }

    // SAFETY: tlbi is always safe by definition
    unsafe {
        asm!(
            ".arch armv8.4-a",
            "tlbi aside1os, {x}",
            x = in(reg) ((asid as u64) << 48)
        );
    }
}

/// Invalidate a single page for a given ASID.
#[inline(always)]
pub(crate) fn tlbi_page(asid: Asid, va: usize) {
    if debug_enabled(DebugFlags::ConservativeTlbi) {
        tlbi_all();
        sync();
        return;
    }

    let val: u64 = ((asid as u64) << 48) | ((va as u64 >> 12) & 0xffffffffffc);
    // SAFETY: tlbi is always safe by definition
    unsafe {
        asm!(
            ".arch armv8.4-a",
            "tlbi vae1os, {x}",
            x = in(reg) val
        );
    }
}

/// Invalidate a range of pages for a given ASID.
#[inline(always)]
pub(crate) fn tlbi_range(asid: Asid, va: usize, len: usize) {
    if debug_enabled(DebugFlags::ConservativeTlbi) {
        tlbi_all();
        sync();
        return;
    }

    if len == 0 {
        return;
    }

    let start_pg = va >> mmu::UAT_PGBIT;
    let end_pg = (va + len + mmu::UAT_PGMSK) >> mmu::UAT_PGBIT;

    let mut val: u64 = ((asid as u64) << 48) | (2 << 46) | (start_pg as u64 & 0x1fffffffff);
    let pages = end_pg - start_pg;

    // Guess? It's possible that the page count is in terms of 4K pages
    // when the CPU is in 4K mode...
    #[cfg(CONFIG_ARM64_4K_PAGES)]
    let pages = 4 * pages;

    if pages == 1 {
        tlbi_page(asid, va);
        return;
    }

    // Page count is always in units of 2
    let num = ((pages + 1) >> 1) as u64;
    // base: 5 bits
    // exp: 2 bits
    // pages = (base + 1) << (5 * exp + 1)
    let exp = min(3, (64 - num.leading_zeros()) / 5);
    let bits = 5 * exp;
    let mut base = (num + (1 << bits) - 1) >> bits;

    val |= (exp as u64) << 44;

    while base > 32 {
        // SAFETY: tlbi is always safe by definition
        unsafe {
            asm!(
                ".arch armv8.4-a",
                "tlbi rvae1os, {x}",
                x = in(reg) val | (31 << 39)
            );
        }
        base -= 32;
    }

    // SAFETY: tlbi is always safe by definition
    unsafe {
        asm!(
            ".arch armv8.4-a",
            "tlbi rvae1os, {x}",
            x = in(reg) val | ((base - 1) << 39)
        );
    }
}

/// Invalidate a T8140/G17P range using the native PI_300 producer's encoding.
///
/// Keep this separate from `tlbi_range`: the legacy encoder is hardware-proven
/// on already-conformant M1/M2 systems, while this formula is qualified only on
/// T8140/G17P.
#[inline(always)]
pub(crate) fn tlbi_range_t8140(asid: Asid, va: usize, len: usize) {
    if debug_enabled(DebugFlags::ConservativeTlbi) {
        tlbi_all();
        sync();
        return;
    }

    if len == 0 {
        return;
    }

    // The single-page decision follows the host CPU page size; range mode
    // still uses the GPU's 16K granule (TG=2, BADDR=VA>>14).
    if va == 0 || len >= 0x0200_0000 {
        tlbi_asid(asid);
        return;
    }
    if len <= PAGE_SIZE {
        tlbi_page(asid, va);
        return;
    }

    let (scale, unit_shift) = if len >= 0x0010_0000 {
        (1u64, 20usize)
    } else {
        (0u64, 15usize)
    };
    let count = ((len - 1) >> unit_shift) + 1;
    let num = (count - 1) as u64;
    debug_assert!(num <= 31);

    // Native inserts VA[51:14] into operand[37:0]. Bit 37 is
    // Apple/GPU-specific here; match the inspected PI_300 producer exactly.
    let val = ((asid as u64) << 48)
        | (2 << 46)
        | (scale << 44)
        | (num << 39)
        | (((va >> mmu::UAT_PGBIT) as u64) & 0x3fffffffff);

    // SAFETY: tlbi is always safe by definition
    unsafe {
        asm!(
            ".arch armv8.4-a",
            "tlbi rvae1os, {x}",
            x = in(reg) val
        );
    }
}

/// Issue a memory barrier (`dsb sy`).
#[inline(always)]
pub(crate) fn sync() {
    // SAFETY: Barriers are always safe
    unsafe {
        asm!("dsb sy");
    }
}
