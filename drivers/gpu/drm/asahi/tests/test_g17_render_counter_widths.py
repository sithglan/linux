#!/usr/bin/env python3
"""Exercise real retained encoders and unaligned native counter boundaries.

Only module-parameter reads are mocked. --native-blobs optionally verifies the
SHA-identified valid 092506 retained TA records; it does not label them executing.
"""
from pathlib import Path
from g17_config_test_support import with_g17_config
import argparse
import hashlib
import re
import struct
import subprocess
import tempfile

src = Path(__file__).resolve().parent.parent
sub = (src / "g17_submission.rs").read_text()
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--native-blobs", type=Path)
args = parser.parse_args()


def block(text, name):
    m = re.search(r"^(?:pub\(crate\) )?(?:(?:const )?fn|enum|struct) " + name + r"\b", text, re.M)
    assert m, name
    end = text.index("{", m.end()) + 1
    depth = 1
    while depth:
        depth += (text[end] == "{") - (text[end] == "}")
        end += 1
    return text[m.start():end]


def constant(name):
    return re.search(r"^pub\(crate\) const " + name + r": .*?;", sub, re.M | re.S).group()


body = "\n".join([
    "#![allow(dead_code, non_upper_case_globals)]",
    r'''
mod module_parameters {
    pub struct Param(u32);
    impl Param { pub fn value(&self) -> &u32 { &self.0 } }
    pub static g17p_ta_oracle: Param = Param(0);
    pub static g17p_frag_oracle: Param = Param(0);
    pub static g17p_render_ta_qid_field: Param = Param(1);
    pub static g17p_render_3d_dm: Param = Param(0xff);
    pub static g17p_render_ta_dm: Param = Param(0xff);
}
''',
    '#[path = "' + str(src / "g17_render.rs") + '"] mod g17_render;',
    "mod g17_submission {",
    *[constant(n) for n in [
        "G17P_COLD_OPENING_OPTIONAL_SIZE", "G17P_COLD_OPENING_EVENT_SIZE",
        "KICK_TS_BITS", "KICK_TS_MASK",
        "G17P_NATIVE_RENDER_DOORBELL_PRIORITY", "G17P_NATIVE_RENDER_QOS_HARDWARE_BUFFER_ID",
        "G17P_NATIVE_RENDER_POLICY"]],
    *["#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n" + block(sub, n) for n in [
        "SubmissionError", "G17PColdOpeningStage", "G17PColdOpeningOptionalAddresses"]],
    *[block(sub, n) for n in [
        "put_u16_le", "put_u32_le", "put_u64_le", "apply_g17p_cold_opening_optional",
        "apply_g17p_retained_optional", "apply_g17p_retained_optional_with_flist_generation",
        "apply_g17p_retained_optional_with_flist_generation_and_context_update",
        "g17p_render_flist_generation",
        "apply_g17p_cold_opening_event", "apply_g17p_retained_event_with_stamp"]],
    "}",
    r'''
use g17_render::*;
use g17_submission::*;
fn u32_at(b: &[u8], at: usize) -> u32 { u32::from_le_bytes(b[at..at+4].try_into().unwrap()) }
fn u64_at(b: &[u8], at: usize) -> u64 { u64::from_le_bytes(b[at..at+8].try_into().unwrap()) }
fn refresh(kind: RenderDescriptorKind, ordinal: u64, raw: &mut [u8]) -> Result<(), RenderBuildError> {
    apply_g17p_retained_descriptor_fields(kind, 0x7000098000, 0xfffffc20c0828000,
        if kind == RenderDescriptorKind::Tiling { 0 } else { 1 }, 1,
        ordinal, ordinal == 0, true, raw)
}
fn addresses() -> G17PColdOpeningOptionalAddresses {
    G17PColdOpeningOptionalAddresses { context_scratch: 0x7000100000,
        firmware_scratch: 0x7000104000, usc_priv_mem_flist: 0xfffffc20001f8000,
        usc_freelist_hardware_buffer_id: 0x11223344, channel_control: 0x700010c000,
        tiling_shared_object: 0xfffffc20c0060000, parameter_buffer_token: 0x123456789abc,
        owner_pid: 0x5678, ta_hardware_buffer_id: 7 }
}

#[test]
fn long_width_native_255_256_257_and_high_ordinal_fields() {
    for ordinal in [0, 1, 2, 253, 254, 255, 256, 999, 65535, 65536,
                    0x00fffffe, 0x00ffffff, 0x01000000, 0xffffffff,
                    0x100000000, 0xffffffffff, 0x10000000000] {
        let n = (ordinal + 1) & 0xffffffffff;
        let mut ta = [0xa5; TA_DESCRIPTOR_SIZE];
        let mut fragment = [0xa5; FRAGMENT_DESCRIPTOR_SIZE];
        refresh(RenderDescriptorKind::Tiling, ordinal, &mut ta).unwrap();
        refresh(RenderDescriptorKind::Fragment, ordinal, &mut fragment).unwrap();
        assert_eq!(u32_at(&ta, 0x7a0), (n as u32).wrapping_shl(8));
        assert_eq!(u64_at(&ta, 0x7a8), n as u64);
        assert_eq!(&ta[0x7b0..0x7b2], &[n as u8; 2]);
        assert_eq!(u32_at(&ta, 0x8b6), (n as u32).wrapping_shl(8));
        assert_eq!(u64_at(&ta, 0x8c6), ((n as u8 as u64) << 40) | n as u64);
        assert_eq!(u32_at(&ta, 0x8d6), ordinal as u32);
        assert_eq!(u32_at(&fragment, 0x2150), (n as u32).wrapping_shl(8));
        assert_eq!(u32_at(&fragment, 0x215c), u32::from(ordinal == 0));
        assert_eq!(u32_at(&fragment, 0x2170), ordinal as u32);
        // These are adjacent independently owned pointer/lifecycle bytes.
        for span in [0x7b2..0x7b4, 0x8ae..0x8b6, 0x8ce..0x8d6] {
            assert!(ta[span].iter().all(|b| *b == 0xa5));
        }
        assert_eq!(u32_at(&ta, 0x8c2), 1);
        // Fragment scratch is staged separately from its real producer.
        assert_eq!(&fragment[0x2160..0x2168], &[0xa5; 8]);
    }
}

#[test]
fn long_width_first_255_payloads_keep_the_original_encoded_values() {
    for ordinal in 0..255u32 {
        let mut ta = [0u8; TA_DESCRIPTOR_SIZE];
        // The old splice fabricated these pointer bytes; real owner supplies
        // them. Seed that native high pointer word to compare cold bytes.
        ta[0x8b4..0x8b6].fill(0xff);
        refresh(RenderDescriptorKind::Tiling, u64::from(ordinal), &mut ta).unwrap();
        let n = ordinal + 1;
        assert_eq!(u32_at(&ta, 0x7a8), n);
        assert_eq!(u32_at(&ta, 0x7b0), n | (n << 8));
        assert_eq!(u32_at(&ta, 0x8b4), (n << 24) | 0xffff);
        assert_eq!(u64_at(&ta, 0x8c6), ((n as u64) << 40) | n as u64);
        assert_eq!(u32_at(&ta, 0x8d4), ordinal << 16);
    }
}

#[test]
fn long_width_actual_linked_timestamp_and_payload_are_independent() {
    for (tiling_timestamp, fragment_timestamp, payload) in [
        (0, 0, 0), (1, 7, 255), (256, 0x9876543210, 0),
        (0x123456789a, 0x9988776655, 2), (0xffffffffff, 1, 255),
    ] {
        let mut ta = [0xa5; TA_DESCRIPTOR_SIZE];
        let before = ta;
        apply_g17p_ta_linked_completion_scratch(tiling_timestamp, fragment_timestamp, payload, &mut ta).unwrap();
        assert_eq!(u64_at(&ta, 0x7a8), fragment_timestamp);
        assert_eq!(&ta[0x7b0..0x7b2], &[payload; 2]);
        assert_eq!(u64_at(&ta, 0x8c6), tiling_timestamp | ((payload as u64) << 40));
        for at in 0..ta.len() {
            if !(0x7a8..0x7b2).contains(&at) && !(0x8c6..0x8ce).contains(&at) {
                assert_eq!(ta[at], before[at], "unexpected write at {at:#x}");
            }
        }
    }
}

#[test]
fn long_width_invalid_input_is_transactional() {
    for ordinal in [u64::MAX] {
        for kind in [RenderDescriptorKind::Tiling, RenderDescriptorKind::Fragment] {
            let mut raw = [0xa5; FRAGMENT_DESCRIPTOR_SIZE];
            let before = raw;
            assert_eq!(refresh(kind, ordinal, &mut raw), Err(RenderBuildError::ArithmeticOverflow));
            assert_eq!(raw, before);
        }
    }
    for len in [0, TA_DESCRIPTOR_SIZE - 1] {
        let mut raw = vec![0xa5; len];
        let before = raw.clone();
        assert_eq!(apply_g17p_ta_linked_completion_scratch(1, 2, 3, &mut raw), Err(RenderBuildError::BufferTooSmall));
        assert_eq!(raw, before);
    }
    let mut raw = [0xa5; TA_DESCRIPTOR_SIZE];
    let before = raw;
    assert_eq!(apply_g17p_ta_linked_completion_scratch(1 << 40, 1, 0, &mut raw), Err(RenderBuildError::ArithmeticOverflow));
    assert_eq!(raw, before);
    assert_eq!(apply_g17p_ta_linked_completion_scratch(1, 1 << 40, 0, &mut raw), Err(RenderBuildError::ArithmeticOverflow));
    assert_eq!(raw, before);
}

#[test]
fn long_width_flist_generation_retains_all_counter_bits_and_neighbors() {
    for stage in [G17PColdOpeningStage::Tiling, G17PColdOpeningStage::Fragment] {
        for ordinal in [0, 1, 254, 255, 256, 999, 65535, 65536, 0x00fffffe,
                        0x00ffffff, 0x01000000, 0xffffffff, 0x100000000] {
            let mut raw = [0xa5; G17P_COLD_OPENING_OPTIONAL_SIZE];
            apply_g17p_retained_optional(stage, addresses(), ordinal, false, 0, 1, 1, 1, 1, &mut raw).unwrap();
            assert_eq!(u64_at(&raw, 0x3e), ordinal as u64);
            assert_eq!(u64_at(&raw, 0x36), addresses().usc_priv_mem_flist);
            assert_eq!(u32_at(&raw, 0x46), addresses().usc_freelist_hardware_buffer_id);
            assert_eq!(raw[0x2f], ordinal as u8);
            if stage == G17PColdOpeningStage::Tiling {
                assert_eq!(u64_at(&raw, 0x76), addresses().parameter_buffer_token);
                assert_eq!(u32_at(&raw, 0x7e), addresses().ta_hardware_buffer_id);
            }
        }
    }
    assert_eq!(g17p_render_flist_generation(u64::MAX), u64::MAX);
}

#[test]
fn long_width_event_stamp_retains_native_forty_bit_boundary() {
    for stage in [G17PColdOpeningStage::Tiling, G17PColdOpeningStage::Fragment] {
        for stamp in [0, 1, 255, 256, 257, 65536, 0x00ffffff,
                      0x01000000, 0x100000000, 0xffffffffff] {
            let mut raw = [0xa5; G17P_COLD_OPENING_EVENT_SIZE];
            apply_g17p_retained_event_with_stamp(stage, stamp, 1, &mut raw).unwrap();
            assert_eq!(u64_at(&raw, 9), stamp);
            assert_eq!(raw[8], G17P_NATIVE_RENDER_DOORBELL_PRIORITY);
        }
        for stamp in [1u64 << 40, u64::MAX] {
            let mut raw = [0xa5; G17P_COLD_OPENING_EVENT_SIZE];
            let before = raw;
            assert!(apply_g17p_retained_event_with_stamp(stage, stamp, 1, &mut raw).is_err());
            assert_eq!(raw, before);
        }
    }
}
''',
])
with tempfile.TemporaryDirectory(prefix="g17-render-counter-widths-") as temp:
    binary = Path(temp) / "tests"
    subprocess.run(["rustc", "--edition=2021", "--test", "-", "-o", str(binary)],
                   input=with_g17_config(body), text=True, check=True)
    subprocess.run([str(binary), "long_width_"], check=True)

if args.native_blobs:
    for n, sha in [
        (255, "a8a3e4021a32d798cd19548ed222a759a0c124422abc23aa462d3a3ad83cfbc8"),
        (256, "a1b95c58053dd9793e765d4c693e999b78ca239c69a50c95c9bd34819ea4e626"),
        (257, "24fc5dbafeb7e9496cf413bb939f7033edb79108e765a037f0f8cc69876e0b26"),
    ]:
        raw = (args.native_blobs / (sha + ".bin")).read_bytes()
        assert len(raw) == 0x9c0 and hashlib.sha256(raw).hexdigest() == sha
        assert struct.unpack_from("<Q", raw, 0x7a8)[0] == n
        assert raw[0x7b0:0x7b2] == bytes([n & 255] * 2)
        assert struct.unpack_from("<I", raw, 0x8b6)[0] == n << 8
        assert struct.unpack_from("<Q", raw, 0x8c6)[0] == ((n & 255) << 40) | n
        # This retained native owner has an independently advanced logical
        # ordinal. Do not turn its physical timestamp into a universal ordinal.
        assert struct.unpack_from("<I", raw, 0x8d6)[0] == n + 4
    print("Valid native retained TA 255/256/257 width witnesses PASS")
print("Actual retained descriptor/tag-15/event counter widths and bounded errors PASS")
