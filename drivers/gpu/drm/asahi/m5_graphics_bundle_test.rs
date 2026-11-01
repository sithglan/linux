// SPDX-License-Identifier: MIT
//! Synthetic parser inputs; no captured shader or data pages.
use super::*;

fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn checksum(bytes: &mut [u8]) {
    let hash = fnv64(&bytes[HEADER_BYTES..]);
    put64(bytes, 48, hash);
}

fn synthetic() -> Vec<u8> {
    let count = PIXEL_PAGES + 1;
    let mut bytes = vec![0; HEADER_BYTES + count * RECORD_BYTES];
    bytes[..8].copy_from_slice(MAGIC);
    for (offset, value) in [
        (8, 1),
        (12, 64),
        (16, PAGE_BYTES),
        (20, count),
        (24, WIDTH),
        (28, HEIGHT),
        (32, STRIDE),
        (36, 3),
    ] {
        put32(&mut bytes, offset, value as u32);
    }
    put64(&mut bytes, 40, PIXEL_ADDRESS);
    for index in 0..count {
        let record = HEADER_BYTES + index * RECORD_BYTES;
        let address = if index == 0 {
            0x10_0000_0000
        } else {
            PIXEL_ADDRESS + ((index - 1) * PAGE_BYTES) as u64
        };
        put64(&mut bytes, record, address);
        put32(
            &mut bytes,
            record + 8,
            if index == 0 { READ } else { READ_WRITE },
        );
        put32(&mut bytes, record + 12, PAGE_BYTES as u32);
        if index != 0 {
            bytes[record + 16..record + RECORD_BYTES].fill(SENTINEL);
        }
    }
    checksum(&mut bytes);
    bytes
}

#[test]
fn accepts_owned_user_pages_and_preserves_permissions() {
    let bytes = synthetic();
    let bundle = GraphicsBundle::parse(&bytes).unwrap();
    assert_eq!(bundle.page_count(), 17);
    assert!(!bundle.page(0).unwrap().writable);
    let output = bundle.page(1).unwrap();
    assert!(output.writable);
    assert_eq!(output.address, PIXEL_ADDRESS);
    assert_eq!(output.bytes, &[SENTINEL; PAGE_BYTES]);
    assert!(bundle.page(17).is_none());
    assert!(bundle.page(usize::MAX).is_none());
    assert_eq!(fnv64(b"hello"), 0xa430_d846_80aa_bd0b);
}

#[test]
fn rejects_truncation_trailing_data_and_invalid_header() {
    let bytes = synthetic();
    for length in (0..HEADER_BYTES).chain([HEADER_BYTES, bytes.len() - 1]) {
        assert!(matches!(
            GraphicsBundle::parse(&bytes[..length]),
            Err(BundleError::Length)
        ));
    }
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(matches!(
        GraphicsBundle::parse(&extra),
        Err(BundleError::Length)
    ));
    for offset in [0, 8, 12, 16, 24, 28, 32, 36, 40, 56] {
        let mut invalid = bytes.clone();
        invalid[offset] ^= 1;
        assert!(
            matches!(GraphicsBundle::parse(&invalid), Err(BundleError::Header)),
            "{offset}"
        );
    }
    for count in [0, 15, 385, u32::MAX] {
        let mut invalid = bytes.clone();
        put32(&mut invalid, 20, count);
        assert!(matches!(
            GraphicsBundle::parse(&invalid),
            Err(BundleError::Length)
        ));
    }
}

#[test]
fn rejects_corruption_kernel_addresses_duplicates_and_bad_permissions() {
    let bytes = synthetic();
    let mut invalid = bytes.clone();
    invalid[HEADER_BYTES + 32] ^= 1;
    assert!(matches!(
        GraphicsBundle::parse(&invalid),
        Err(BundleError::Checksum)
    ));
    for address in [
        0,
        0x10_0000_0001,
        0x10_0040_0000,
        0x70_0000_0000,
        0xffff_fc20_0040_0000,
        u64::MAX,
    ] {
        let mut invalid = bytes.clone();
        put64(&mut invalid, HEADER_BYTES, address);
        checksum(&mut invalid);
        assert!(
            matches!(GraphicsBundle::parse(&invalid), Err(BundleError::Address)),
            "{address:x}"
        );
    }
    let mut duplicate = bytes.clone();
    put64(&mut duplicate, HEADER_BYTES + RECORD_BYTES, 0x10_0000_0000);
    checksum(&mut duplicate);
    assert!(matches!(
        GraphicsBundle::parse(&duplicate),
        Err(BundleError::Address)
    ));
    for flags in [0, 2, 4, u32::MAX] {
        let mut invalid = bytes.clone();
        put32(&mut invalid, HEADER_BYTES + 8, flags);
        checksum(&mut invalid);
        assert!(matches!(
            GraphicsBundle::parse(&invalid),
            Err(BundleError::Permissions)
        ));
    }
}

#[test]
fn output_must_be_complete_writable_and_untouched() {
    let bytes = synthetic();
    let mut dirty = bytes.clone();
    dirty[HEADER_BYTES + RECORD_BYTES + 16] = 0;
    checksum(&mut dirty);
    assert!(matches!(
        GraphicsBundle::parse(&dirty),
        Err(BundleError::Output)
    ));
    let mut readonly = bytes.clone();
    put32(&mut readonly, HEADER_BYTES + RECORD_BYTES + 8, READ);
    checksum(&mut readonly);
    assert!(matches!(
        GraphicsBundle::parse(&readonly),
        Err(BundleError::Output)
    ));
    let mut missing = bytes.clone();
    put64(
        &mut missing,
        HEADER_BYTES + PIXEL_PAGES * RECORD_BYTES,
        PIXEL_ADDRESS + PIXEL_BYTES as u64,
    );
    checksum(&mut missing);
    assert!(matches!(
        GraphicsBundle::parse(&missing),
        Err(BundleError::Output)
    ));
}

#[test]
fn untouched_blank_or_wrong_pixels_never_pass() {
    for color in [
        [SENTINEL; 4],
        [16, 16, 16, 255],
        [64, 128, 191, 255],
        [0; 4],
    ] {
        let mut result = TriangleResult::default();
        for pixel in 0..WIDTH * HEIGHT {
            result.add(pixel, color);
        }
        assert!(!result.verified());
        assert_eq!(result.checked, 63234);
        assert!(result.wrong > 0);
        if color == [SENTINEL; 4] {
            assert_eq!(result.untouched, WIDTH * HEIGHT);
        }
    }
}
