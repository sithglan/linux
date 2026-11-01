// SPDX-License-Identifier: GPL-2.0-only OR MIT
//! Controlled M5 compute program from our compiled Metal reference.
//!
//! Native replay qualifies the first 1 KiB of the shader page; its 64-byte
//! body alone is insufficient. Native captures also agree on the 132-byte
//! Ioto pipeline and direct 1024-thread stream. These fixed VAs are a
//! diagnostic profile;
//! callers must back them with owned mappings in the submitted M5 context.
//! This module neither installs mappings nor publishes work.

#![cfg_attr(not(test), allow(dead_code))]

pub(crate) const PAGE_BYTES: usize = 0x4000;
pub(crate) const ELEMENTS: usize = 1024;
pub(crate) const SENTINEL: u32 = 0xdead_beef;
pub(crate) const SHADER_PAGE: u64 = 0x100_0000_0000;
pub(crate) const PIPELINE_PAGE: u64 = 0x100_0009_0000;
pub(crate) const STREAM_PAGE: u64 = 0x100_000b_0000;
pub(crate) const PREEMPT_PAGE: u64 = 0x100_000e_0000;
pub(crate) const BUFFER_PAGE: u64 = 0x100_0001_c000;
pub(crate) const ROBUSTNESS_PAGE: u64 = 0x10_0001_8000;
pub(crate) const INPUT_OFFSET: usize = 0xd00;
pub(crate) const OUTPUT_OFFSET: usize = 0x1d00;

#[path = "m5_triangle_program.rs"]
mod triangle;
pub(crate) const CONSTANTS_PAGE: u64 = triangle::CONSTANTS_PAGE;

pub(crate) fn expected_output(index: usize, triangle: bool) -> u32 {
    if !triangle { return index as u32 * 21 + 5; }
    let x = (index % 32) as i32;
    let y = (index / 32) as i32;
    if y >= 4 && y < 28 && (2*x-31).abs() <= y-4 {
        0xffff_0000 | ((y as u32 * 8) << 8) | (x as u32 * 8)
    } else {
        0xff18_1818
    }
}

// Native replay proves the 64-byte body alone produces no output. The
// preceding program setup is required: preserving this first 1 KiB and
// zeroing the remaining page verifies all 1024 replacement results.
// Full-page SHA256: 32f033d1c45c805abc61f7fa1a805847ac648ddc0377a7a6f0f1450ea07d15f8
const SHADER: [u32; 256] = [
    0x00000340, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
    0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006,
    0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006,
    0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006,
    0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006,
    0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006,
    0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006,
    0x0e33005e, 0x00000000, 0x00060000, 0x00060006, 0x0e31405e, 0x00000000, 0x00060000, 0x00060006,
    0x0e31005e, 0x00000000, 0x00060000, 0x00060006, 0x0e1b405e, 0x00000000, 0x00060000, 0x00060006,
    0x0e1b005e, 0x00000000, 0x00060000, 0x00060006, 0x0e19405e, 0x00000000, 0x00060000, 0x00060006,
    0x0e19005e, 0x00000000, 0x00060000, 0x00060006, 0x0e13405e, 0x00000000, 0x00060000, 0x00060006,
    0x0e13005e, 0x00000000, 0x00060000, 0x00060006, 0x0e11405e, 0x00000000, 0x00060000, 0x00060006,
    0x0f100007, 0x00270000, 0x00200004, 0x00060aa5, 0x0f100007, 0x00270000, 0x00200004, 0x00060aa5,
    0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006,
    0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006,
    0x0e33005e, 0x00000000, 0x00060000, 0x00060006, 0x0e31405e, 0x00000000, 0x00060000, 0x00060006,
    0x0e31005e, 0x00000000, 0x00060000, 0x00060006, 0x0e1b405e, 0x00000000, 0x00060000, 0x00060006,
    0x0e1b005e, 0x00000000, 0x00060000, 0x00060006, 0x0e19405e, 0x00000000, 0x00060000, 0x00060006,
    0x0e19005e, 0x00000000, 0x00060000, 0x00060006, 0x0e13405e, 0x00000000, 0x00060000, 0x00060006,
    0x0e13005e, 0x00000000, 0x00060000, 0x00060006, 0x0e11405e, 0x00000000, 0x00060000, 0x00060006,
    0x0f100007, 0x00270000, 0x00200004, 0x00060aa5, 0x0f100007, 0x00270000, 0x00200004, 0x00060aa5,
    0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006,
    0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006,
    0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006,
    0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006,
    0x000000c0, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000, 0x00000000,
    0x02070023, 0x02a02042, 0x0000000e, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006,
    0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006, 0x00060006,
    0x0610a01c, 0x0203000f, 0xc0102218, 0x00800041, 0x002f0000, 0x00291a04, 0x802802a3, 0x01270024,
    0x00253a04, 0x802002a3, 0x040f0021, 0x06010203, 0x000e4010, 0x00000000, 0x00000000, 0x00000000,
];
const PIPELINE: [u32; 33] = [
    0x0002a02c, 0x00700812, 0x0002803c, 0x00000004,
    0x0043042f, 0x40000ce8, 0x0c000007, 0x00000000,
    0x0e4003c7, 0x00000000, 0x00070100, 0x00000d00,
    0x00000000, 0x0002801c, 0x00000000, 0x00018134,
    0x02a00020, 0x0002800c, 0x00000004, 0x1a040427,
    0x02a30021, 0x00008088, 0x0002801c, 0x00000004,
    0x00040427, 0x12a40030, 0x4409240b, 0x0409261b,
    0x0409282b, 0x04092a3b, 0x02070023, 0x02a02042,
    0x0000000e,
];
const STREAM: [u32; 12] = [
    0x00080000, 0x01000000, 0x00002400, 0x40000001,
    0x00000400, 1, 1, 0x40, 1, 1, 0x60000160, 0x40000000,
];

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum PageKind { Shader, Pipeline, Stream, Preempt, Buffers, Robustness, Constants }

/// Construct one complete owned page; reject the wrong extent before writing.
pub(crate) fn initialize_page(kind: PageKind, page: &mut [u8]) -> Result<(), ()> {
    initialize_page_for(kind, page, false)
}

pub(crate) fn initialize_page_for(kind: PageKind, page: &mut [u8], triangle: bool) -> Result<(), ()> {
    if page.len() != PAGE_BYTES { return Err(()); }
    page.fill(0);
    let (offset, words): (usize, &[u32]) = match kind {
        PageKind::Shader => (0, if triangle { &triangle::SHADER } else { &SHADER }),
        PageKind::Pipeline => (0, if triangle { &triangle::PIPELINE } else { &PIPELINE }),
        PageKind::Constants => {
            if !triangle { return Err(()); }
            (0, &triangle::CONSTANTS)
        }
        PageKind::Stream => (0, &STREAM),
        PageKind::Preempt => {
            // Ioto's first LDIMM pair selects PREEMPT_PAGE + 0x14a0.
            // The captured page is otherwise zero before execution.
            page[0x14a0..0x14a8].copy_from_slice(&(BUFFER_PAGE + INPUT_OFFSET as u64).to_le_bytes());
            page[0x14a8..0x14b0].copy_from_slice(&(BUFFER_PAGE + OUTPUT_OFFSET as u64).to_le_bytes());
            return Ok(());
        }
        PageKind::Buffers => {
            for index in 0..ELEMENTS {
                let input = INPUT_OFFSET + index * 4;
                let output = OUTPUT_OFFSET + index * 4;
                page[input..input + 4].copy_from_slice(&(index as u32 * 7).to_le_bytes());
                page[output..output + 4].copy_from_slice(&SENTINEL.to_le_bytes());
            }
            return Ok(());
        }
        PageKind::Robustness => return Ok(()),
    };
    for (index, word) in words.iter().enumerate() {
        let start = offset + index * 4;
        page[start..start + 4].copy_from_slice(&word.to_le_bytes());
    }
    Ok(())
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct ComputeResult {
    pub(crate) inputs_changed: usize,
    pub(crate) outputs_wrong: usize,
    pub(crate) outputs_untouched: usize,
}

/// Evaluate a CPU-visible snapshot after the caller's cache synchronization.
pub(crate) fn check_result(page: &[u8]) -> Result<ComputeResult, ()> {
    if page.len() != PAGE_BYTES { return Err(()); }
    let mut result = ComputeResult { inputs_changed: 0, outputs_wrong: 0, outputs_untouched: 0 };
    for index in 0..ELEMENTS {
        let input = INPUT_OFFSET + index * 4;
        let output = OUTPUT_OFFSET + index * 4;
        let input = u32::from_le_bytes(page[input..input + 4].try_into().map_err(|_| ())?);
        let output = u32::from_le_bytes(page[output..output + 4].try_into().map_err(|_| ())?);
        result.inputs_changed += usize::from(input != index as u32 * 7);
        result.outputs_wrong += usize::from(output != index as u32 * 21 + 5);
        result.outputs_untouched += usize::from(output == SENTINEL);
    }
    Ok(result)
}
