// SPDX-License-Identifier: GPL-2.0-only OR MIT
//! Bounded, private 25G76 graphics reference input for the first M5 draw.
//!
//! The bundle holds user GPU addresses and page contents, never captured
//! physical mappings or kernel command objects. Parsing does not publish work.
//! Version 1 deliberately accepts only the qualified 256x256 reference.

#[cfg(test)]
#[path = "m5_graphics_bundle_test.rs"]
mod tests;

pub(crate) const MAGIC: &[u8; 8] = b"M5GFXR01";
pub(crate) const HEADER_BYTES: usize = 64;
pub(crate) const PAGE_BYTES: usize = 0x4000;
pub(crate) const RECORD_BYTES: usize = PAGE_BYTES + 16;
pub(crate) const MAX_PAGES: usize = 384;
pub(crate) const PIXEL_ADDRESS: u64 = 0x100_0005_8000;
pub(crate) const WIDTH: usize = 256;
pub(crate) const HEIGHT: usize = 256;
pub(crate) const STRIDE: usize = WIDTH * 4;
pub(crate) const PIXEL_BYTES: usize = STRIDE * HEIGHT;
pub(crate) const PIXEL_PAGES: usize = PIXEL_BYTES / PAGE_BYTES;
pub(crate) const READ: u32 = 1;
pub(crate) const READ_WRITE: u32 = 3;
pub(crate) const SENTINEL: u8 = 0xa5;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BundleError {
    Header,
    Length,
    Checksum,
    Address,
    Permissions,
    Output,
}

pub(crate) struct GraphicsBundle<'a> {
    bytes: &'a [u8],
    page_count: usize,
}

pub(crate) struct Page<'a> {
    pub(crate) address: u64,
    pub(crate) writable: bool,
    pub(crate) bytes: &'a [u8],
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

/// Corruption detection for the private transfer; not an authenticity check.
pub(crate) fn fnv64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3);
    }
    hash
}

pub(crate) fn user_page(address: u64) -> bool {
    address & (PAGE_BYTES as u64 - 1) == 0
        && [0x10_0000_0000, 0x100_0000_0000]
            .iter()
            .any(|base| *base <= address && address < *base + 0x40_0000)
}

impl<'a> GraphicsBundle<'a> {
    pub(crate) fn parse(bytes: &'a [u8]) -> Result<Self, BundleError> {
        if bytes.len() < HEADER_BYTES {
            return Err(BundleError::Length);
        }
        if &bytes[..8] != MAGIC
            || u32_at(bytes, 8) != 1
            || u32_at(bytes, 12) != HEADER_BYTES as u32
            || u32_at(bytes, 16) != PAGE_BYTES as u32
            || u32_at(bytes, 24) != WIDTH as u32
            || u32_at(bytes, 28) != HEIGHT as u32
            || u32_at(bytes, 32) != STRIDE as u32
            || u32_at(bytes, 36) != 3
            || u64_at(bytes, 40) != PIXEL_ADDRESS
            || u64_at(bytes, 56) != 0
        {
            return Err(BundleError::Header);
        }
        let page_count = u32_at(bytes, 20) as usize;
        if !(PIXEL_PAGES..=MAX_PAGES).contains(&page_count)
            || bytes.len() != HEADER_BYTES + page_count * RECORD_BYTES
        {
            return Err(BundleError::Length);
        }
        if fnv64(&bytes[HEADER_BYTES..]) != u64_at(bytes, 48) {
            return Err(BundleError::Checksum);
        }
        let bundle = Self { bytes, page_count };
        let mut previous = 0;
        let mut output_pages = 0;
        for record in bytes[HEADER_BYTES..].chunks_exact(RECORD_BYTES) {
            let address = u64_at(record, 0);
            if !user_page(address) || address <= previous {
                return Err(BundleError::Address);
            }
            previous = address;
            let flags = u32_at(record, 8);
            if flags != READ && flags != READ_WRITE {
                return Err(BundleError::Permissions);
            }
            if u32_at(record, 12) != PAGE_BYTES as u32 {
                return Err(BundleError::Length);
            }
            if (PIXEL_ADDRESS..PIXEL_ADDRESS + PIXEL_BYTES as u64).contains(&address) {
                if flags != READ_WRITE || record[16..].iter().any(|byte| *byte != SENTINEL) {
                    return Err(BundleError::Output);
                }
                output_pages += 1;
            }
        }
        if output_pages != PIXEL_PAGES {
            return Err(BundleError::Output);
        }
        Ok(bundle)
    }

    pub(crate) fn page_count(&self) -> usize {
        self.page_count
    }

    pub(crate) fn page(&self, index: usize) -> Option<Page<'a>> {
        if index >= self.page_count {
            return None;
        }
        let offset = HEADER_BYTES + index * RECORD_BYTES;
        Some(Page {
            address: u64_at(self.bytes, offset),
            writable: u32_at(self.bytes, offset + 8) == READ_WRITE,
            bytes: &self.bytes[offset + 16..offset + RECORD_BYTES],
        })
    }
}

/// Evaluate pixels read from Linux-owned output after queue completion.
/// Edge pixels are counted but omitted from the geometric tolerance check.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TriangleResult {
    pub(crate) foreground: usize,
    pub(crate) background: usize,
    pub(crate) unexpected: usize,
    pub(crate) untouched: usize,
    pub(crate) checked: usize,
    pub(crate) wrong: usize,
}

impl TriangleResult {
    pub(crate) fn add(&mut self, index: usize, rgba: [u8; 4]) {
        let close = |expected: [u8; 4]| rgba.iter().zip(expected).all(|(a, b)| a.abs_diff(b) <= 1);
        let foreground = close([64, 128, 191, 255]);
        let background = close([16, 16, 16, 255]);
        self.foreground += usize::from(foreground);
        self.background += usize::from(background);
        self.unexpected += usize::from(!foreground && !background);
        self.untouched += usize::from(rgba == [SENTINEL; 4]);
        let x = (index % WIDTH) as i32 * 2 + 1;
        let y = (index / WIDTH) as i32 * 2 + 1;
        let inside = y > 68 && y < 444 && 2 * x + y > 584 && 2 * x - y < 440;
        let outside = y < 60 || y > 452 || 2 * x + y < 568 || 2 * x - y > 456;
        self.checked += usize::from(inside || outside);
        self.wrong += usize::from((inside && !foreground) || (outside && !background));
    }

    pub(crate) fn verified(&self) -> bool {
        self.foreground == 18432
            && self.background == 47104
            && self.unexpected == 0
            && self.untouched == 0
            && self.checked == 63234
            && self.wrong == 0
    }
}
