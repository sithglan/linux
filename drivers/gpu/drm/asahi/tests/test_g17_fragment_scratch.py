#!/usr/bin/env python3
"""Compile actual fragment scratch codec/staging code with a byte-map adapter."""
from pathlib import Path
import re
import json
import subprocess
import tempfile

source_dir = Path(__file__).resolve().parent.parent
submission = (source_dir / "g17_submission.rs").read_text()
resources = (source_dir / "g17_resources.rs").read_text()
manager = (source_dir / "g17_manager.rs").read_text()
render = (source_dir / "g17_render.rs").read_text()


def block(text, pattern):
    match = re.search(pattern, text, re.M)
    assert match, pattern
    end = text.index("{", match.start()) + 1
    depth = 1
    while depth:
        depth += (text[end] == "{") - (text[end] == "}")
        end += 1
    return text[match.start():end]


constants = "\n".join(
    re.search(r"^pub\(crate\) const " + name + r": .*?;", submission, re.M).group()
    for name in ["KICK_TS_BITS", "KICK_TS_MASK", "KICK_QID_SHIFT"]
)
body = "\n".join([
    "#![allow(dead_code)]",
    '#[path=' + json.dumps(str(source_dir / 'g17_work_state.rs')) + '] mod g17_work_state;',
    "type Result<T = ()> = core::result::Result<T, i32>;",
    "const EINVAL: i32 = 22; const EIO: i32 = 5; const ERANGE:i32=34;",
    "mod mmu {pub const UAT_PGSZ:usize=0x4000;}",
    "mod g17_uapi {",
    re.search(r"^pub\(crate\) const UAPI_MAX_ATTACHMENTS: .*?;", (source_dir / "g17_uapi.rs").read_text(), re.M).group(),
    "}",
    "mod g17_submission {",
    constants,
    "#[derive(Debug, Copy, Clone, PartialEq, Eq)]",
    block(submission, r"^pub\(crate\) enum SubmissionError\s+"),
    block(submission, r"^pub\(crate\) const fn g17p_serialized_render_scratch_payload\("),
    block(submission, r"^pub\(crate\) const fn encode_g17p_3d_completion_scratch\("),
    block(submission, r"^fn put_u32_le\("),
    block(submission, r"^fn put_u64_le\("),
    block(submission, r"^pub\(crate\) fn apply_g17p_idle_scratch_seed\("),
    "#[test]",
    block(submission, r"^    fn g17p_3d_completion_scratch_matches_native_full_command\("),
    "#[test]",
    block(submission, r"^    fn g17p_serialized_render_scratch_payload_advances_and_wraps_independently\("),
    "}",
    "mod g17_render {",
    re.search(r"^pub\(crate\) const G17P_MCACHE_MAPPING_SIZE: .*?;", render, re.M).group(),
    re.search(r"^pub\(crate\) const TA_DESCRIPTOR_SIZE: .*?;", render, re.M).group(),
    re.search(r"^pub\(crate\) const FRAGMENT_DESCRIPTOR_SIZE: .*?;", render, re.M).group(),
    "#[derive(Debug, Copy, Clone, PartialEq, Eq)]",
    block(render, r"^pub\(crate\) enum RenderBuildError\s+"),
    block(render, r"^fn put_u8\("),
    block(render, r"^fn put_u64\("),
    block(render, r"^pub\(crate\) fn apply_g17p_ta_linked_completion_scratch\("),
    "}",
    block(resources, r"^fn put_u64\("),
    "\n".join(re.search(r"^const " + name + r": [^;]+;", resources, re.M).group()
        for name in ["G17P_RENDER_DESCRIPTOR_SLOT_COUNT", "G17P_RENDER_TA_DESCRIPTOR_STRIDE",
                     "G17P_RENDER_3D_DESCRIPTOR_STRIDE", "G17P_RENDER_TA_DESCRIPTOR_ARRAY_SIZE",
                     "G17P_RENDER_3D_DESCRIPTOR_OFFSET"]),
    block(resources, r"^fn g17p_render_descriptor_slot\("),
    block(resources, r"^fn g17p_render_ta_descriptor_offset\("),
    block(resources, r"^fn g17p_render_3d_descriptor_offset\("),
    # The production method is verbatim; the test harness supplies only the
    # descriptor field it accesses, backed by a fallible mapped-byte adapter.
    "struct G17PUserRenderStorage { descriptors: MappedObject, descriptor_ordinal:u64, scratch_handoff:Option<[Option<u64>;2]> }",
    "impl G17PUserRenderStorage {",
    block(resources, r"^    pub\(crate\) fn stage_fragment_completion_scratch\("),
    block(resources, r"^    pub\(crate\) fn stage_render_scratch_handoff\("),
    block(resources, r"^    pub\(crate\) fn scratch_handoff_matches\("),
    "}",
    r'''
#[derive(Clone,Copy)]
struct G17PRenderConfigUpdateHeader { scratch_seed_valid:u32, scratch_seed:u64 }
mod module_parameters {
    std::thread_local! { pub static PROBE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) }; }
    pub struct Param;
    #[allow(non_upper_case_globals)] pub static g17p_ta_previous_fragment_probe: Param = Param;
    impl Param { pub fn value(&self) -> &'static u32 { if PROBE.with(|p| p.get()) == 0 { &0 } else { &1 } } }
}
fn put_u32(raw: &mut [u8], offset: usize, value: u32) { raw[offset..offset+4].copy_from_slice(&value.to_le_bytes()); }
struct MappedObject { bytes: Vec<u8>, accesses: usize, fail: bool }
impl MappedObject {
    fn with_bytes_mut<T>(&mut self, f: impl FnOnce(&mut [u8]) -> Result<T>) -> Result<T> {
        self.accesses += 1;
        if self.fail { return Err(EIO); }
        f(&mut self.bytes)
    }
}
fn storage(size: usize) -> G17PUserRenderStorage {
    G17PUserRenderStorage { descriptor_ordinal:0, scratch_handoff:None, descriptors: MappedObject { bytes: vec![0xa5; size], accesses: 0, fail: false } }
}
#[test]
fn coherent_idle_seed_keeps_channels_independent_and_preserves_neighbors() {
    let mut s=storage(G17P_RENDER_3D_DESCRIPTOR_OFFSET + G17P_RENDER_3D_DESCRIPTOR_STRIDE);
    for seeds in [[Some(0),Some(0)], [None,None],
                  [Some(0x00ff_123456789a),None], [None,Some(0x0000_ffffffffff)]] {
        s.stage_render_scratch_handoff(seeds).unwrap();
        assert_eq!(s.scratch_handoff,Some(seeds));
        assert_eq!(&s.descriptors.bytes[0x7a4..0x7a8], &u32::from(seeds[1].is_none()).to_le_bytes());
        for (index,seed) in seeds.into_iter().enumerate() {
            let header=G17PRenderConfigUpdateHeader { scratch_seed_valid:u32::from(seed.is_some()), scratch_seed:seed.unwrap_or(0) };
            assert!(s.scratch_handoff_matches(index==0,header));
            assert!(!s.scratch_handoff_matches(index==0,G17PRenderConfigUpdateHeader { scratch_seed_valid:header.scratch_seed_valid^1,..header }));
            assert!(!s.scratch_handoff_matches(index==0,G17PRenderConfigUpdateHeader { scratch_seed:header.scratch_seed^1,..header }));
            let mut raw=[0xa5;0x90];
            g17_submission::apply_g17p_idle_scratch_seed(&mut raw,seed).unwrap();
            assert_eq!(&raw[..0x26], &[0xa5;0x26]);
            assert_eq!(&raw[0x26..0x2a], &u32::from(seed.is_some()).to_le_bytes());
            assert_eq!(&raw[0x2a..0x32], &seed.unwrap_or(0).to_le_bytes());
            assert!(raw[0x32..].iter().all(|b| *b==0xa5));
        }
    }
    let mut short=[0xa5;0x31];
    assert!(g17_submission::apply_g17p_idle_scratch_seed(&mut short,Some(7)).is_err());
    assert_eq!(short,[0xa5;0x31]);
}
#[test]
fn opt_in_handoff_probe_preserves_cold_descriptor_and_sets_warm_word() {
    module_parameters::PROBE.with(|p| p.set(1));
    let mut s = storage(G17P_RENDER_3D_DESCRIPTOR_OFFSET + G17P_RENDER_3D_DESCRIPTOR_STRIDE * G17P_RENDER_DESCRIPTOR_SLOT_COUNT);
    s.stage_fragment_completion_scratch(1, 1, 1).unwrap();
    assert_eq!(&s.descriptors.bytes[0x7a4..0x7a8], &[0xa5;4]);
    s.descriptor_ordinal=1;
    s.stage_fragment_completion_scratch(2, 2, 2).unwrap();
    let ta=g17p_render_ta_descriptor_offset(1);
    assert_eq!(&s.descriptors.bytes[ta+0x7a4..ta+0x7a8], &1u32.to_le_bytes());
    module_parameters::PROBE.with(|p| p.set(0));
}
#[test]
fn replacement_channel_restarts_all_payload_fields_without_rebasing_physical_stamps() {
    for origin in [6u64, 11, 17689] {
        let mut channels = g17_work_state::RenderTsqSlots::new();
        let mut s = storage(G17P_RENDER_3D_DESCRIPTOR_OFFSET + G17P_RENDER_3D_DESCRIPTOR_STRIDE * G17P_RENDER_DESCRIPTOR_SLOT_COUNT);
        for local in [0u64, 1, 255, 256] {
            let ordinal = origin + local;
            s.descriptor_ordinal = ordinal;
            let payload = channels.channel_payload(3, ordinal).unwrap();
            let value = s.stage_fragment_completion_scratch(ordinal+1, ordinal+1, payload).unwrap();
            let seed = g17_submission::encode_g17p_3d_completion_scratch(ordinal, payload.wrapping_sub(1)).unwrap();
            s.stage_render_scratch_handoff([Some(seed);2]).unwrap();
            assert_eq!(value & g17_submission::KICK_TS_MASK, ordinal+1);
            assert_eq!(value >> 40, ((local+1) as u8) as u64);
            assert_eq!(seed >> 40, (local as u8) as u64);
            let ta = g17p_render_ta_descriptor_offset(ordinal);
            assert_eq!(&s.descriptors.bytes[ta+0x7b0..ta+0x7b2], &[payload;2]);
            assert_eq!(&s.descriptors.bytes[ta+0x8c6..ta+0x8ce], &value.to_le_bytes());
            assert_eq!(s.scratch_handoff, Some([Some(seed);2]));
        }
    }
}
#[test]
fn actual_staging_changes_only_owned_ta_fields_and_fragment_scratch() {
    let mut s = storage(G17P_RENDER_3D_DESCRIPTOR_OFFSET + G17P_RENDER_3D_DESCRIPTOR_STRIDE * G17P_RENDER_DESCRIPTOR_SLOT_COUNT);
    for ordinal in [0, 1, 2, 254, 255, 256, 999] {
        s.descriptor_ordinal = ordinal;
        let before = s.descriptors.bytes.clone();
        let stamp = ordinal as u64 + 1;
        let payload = g17_submission::g17p_serialized_render_scratch_payload(ordinal);
        let value = s.stage_fragment_completion_scratch(stamp, stamp, payload).unwrap();
        assert_eq!(value & g17_submission::KICK_TS_MASK, stamp);
        assert_eq!(value >> 40, payload as u64);
        let ta = g17p_render_ta_descriptor_offset(ordinal);
        let at = g17p_render_3d_descriptor_offset(ordinal) + 0x2160;
        let payload_at = g17p_render_3d_descriptor_offset(ordinal) + 0x90;
        assert_eq!(s.descriptors.bytes[payload_at], payload);
        assert_eq!(&s.descriptors.bytes[at..at + 8], &value.to_le_bytes());
        assert_eq!(&s.descriptors.bytes[ta+0x7a8..ta+0x7b0], &stamp.to_le_bytes());
        assert_eq!(&s.descriptors.bytes[ta+0x7b0..ta+0x7b2], &[payload; 2]);
        assert_eq!(&s.descriptors.bytes[ta+0x8c6..ta+0x8ce], &value.to_le_bytes());
        for byte in 0..before.len() {
            if !(ta+0x7a8..ta+0x7b2).contains(&byte) && !(ta+0x8c6..ta+0x8ce).contains(&byte)
                && !(at..at + 8).contains(&byte) && byte != payload_at {
                assert_eq!(s.descriptors.bytes[byte], before[byte], "unexpected write at {byte:#x}");
            }
        }
    }
}
#[test]
fn actual_staging_preserves_distinct_physical_timestamps_and_payload() {
    let mut s = storage(G17P_RENDER_3D_DESCRIPTOR_OFFSET + g17_render::FRAGMENT_DESCRIPTOR_SIZE);
    let ta = 0x12_3456_789au64;
    let fragment = 0x98_7654_3210u64;
    let payload = 7;
    let value = s.stage_fragment_completion_scratch(ta, fragment, payload).unwrap();
    assert_eq!(value, fragment | ((payload as u64) << 40));
    assert_eq!(&s.descriptors.bytes[0x7a8..0x7b0], &fragment.to_le_bytes());
    assert_eq!(&s.descriptors.bytes[0x7b0..0x7b2], &[payload; 2]);
    assert_eq!(&s.descriptors.bytes[0x8c6..0x8ce], &(ta | ((payload as u64) << 40)).to_le_bytes());
    let at = G17P_RENDER_3D_DESCRIPTOR_OFFSET + 0x2160;
    assert_eq!(&s.descriptors.bytes[at..at+8], &value.to_le_bytes());
    assert_eq!(s.descriptors.bytes[G17P_RENDER_3D_DESCRIPTOR_OFFSET + 0x90], payload);
}
#[test]
fn staging_rejects_bad_timestamp_or_backing_without_partial_writes() {
    let mut s = storage(G17P_RENDER_3D_DESCRIPTOR_OFFSET + g17_render::FRAGMENT_DESCRIPTOR_SIZE);
    let before = s.descriptors.bytes.clone();
    assert_eq!(s.stage_fragment_completion_scratch(1, 1 << 40, 1), Err(EINVAL));
    assert_eq!(s.descriptors.accesses, 0);
    assert_eq!(s.descriptors.bytes, before);
    // Fragment encoding succeeds but the real TA helper rejects its input:
    // neither descriptor may be partially updated inside the mapped closure.
    assert_eq!(s.stage_fragment_completion_scratch(1 << 40, 1, 1), Err(EINVAL));
    assert_eq!(s.descriptors.accesses, 1);
    assert_eq!(s.descriptors.bytes, before);
    s.descriptors.fail = true;
    assert_eq!(s.stage_fragment_completion_scratch(1, 2, 3), Err(EIO));
    assert_eq!(s.descriptors.bytes, before);
    for size in [0, g17_render::TA_DESCRIPTOR_SIZE-1, G17P_RENDER_3D_DESCRIPTOR_OFFSET + g17_render::FRAGMENT_DESCRIPTOR_SIZE - 1] {
        let mut short = storage(size);
        let before = short.descriptors.bytes.clone();
        assert_eq!(short.stage_fragment_completion_scratch(1, 2, 3), Err(ERANGE));
        assert!(short.descriptors.bytes == before, "short backing changed descriptor bytes");
    }
}
''',
])
with tempfile.TemporaryDirectory(prefix="g17-fragment-scratch-") as temp:
    binary = Path(temp) / "tests"
    subprocess.run(["rustc", "--edition=2021", "--test", "-", "-o", str(binary)],
                   input=body, text=True, check=True)
    subprocess.run([str(binary)], check=True)

# Runtime must use owner-local payload in coherent mode and feed the same
# byte to TA/FRG plus the prepublication idle seed.
assert "G17P_3D_COMPLETION_SCRATCH_PAYLOAD" not in manager + resources + submission
assert ".logical_channel_payload(ordinal)?" in manager
assert "channel_payload.wrapping_sub(1)" in manager
assert re.search(r"\.stage_fragment_completion_scratch\(\s*tiling_command_timestamp,\s*fragment_command_timestamp,\s*channel_payload,?\s*\)", manager)
assert re.search(r"let tiling_command_timestamp = self\.render_slots\.get\(pair_slot\)\.sksm\.as_ref\(\)\.ok_or\(ENODEV\)\?\s*\.tiling\.producer\.current_timestamp;", manager)
assert manager.index("let channel_payload =") < manager.index(".stage_fragment_completion_scratch(")
print("actual fragment scratch codec, byte staging and runtime caller wiring PASS")
