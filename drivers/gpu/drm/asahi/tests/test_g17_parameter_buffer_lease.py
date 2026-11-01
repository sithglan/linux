#!/usr/bin/env python3
"""Compile the actual PB ID/token allocator and both tag-15 encoders.

No hardware or Rust kernel mocks: only module parameter reads are fixed at
their native DM defaults. Source-order checks cover the production handoff.
"""
from pathlib import Path
from g17_config_test_support import with_g17_config
import re
import subprocess
import tempfile

src = Path(__file__).resolve().parent.parent
manager = (src / "g17_manager.rs").read_text()
submission = (src / "g17_submission.rs").read_text()


def block(source, pattern):
    match = re.search(pattern, source, re.M)
    assert match, pattern
    start = source.index("{", match.end())
    end, depth = start + 1, 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[match.start():end]


def constant(source, name):
    return re.search(r"(?:pub\(crate\) )?const " + name + r": .*?;", source, re.S).group()


derive = "#[derive(Debug, Clone, Copy, PartialEq, Eq)]\n"
body = "\n".join([
    "#![allow(dead_code, non_upper_case_globals)]",
    constant(manager, "G17P_TA_HARDWARE_BUFFER_ID_COUNT"),
    derive + block(manager, r"^enum G17PHardwareBufferIdError\s*"),
    derive + block(manager, r"^struct G17PHardwareBufferIdAllocator\s*"),
    block(manager, r"^impl G17PHardwareBufferIdAllocator\s*"),
    derive + block(submission, r"^pub\(crate\) enum G17PColdOpeningStage\s*"),
    derive + block(submission, r"^pub\(crate\) struct G17PColdOpeningOptionalAddresses\s*"),
    *[constant(submission, name) for name in [
        "G17P_COLD_OPENING_OPTIONAL_SIZE", "G17P_NATIVE_RENDER_DOORBELL_PRIORITY",
        "G17P_NATIVE_RENDER_QOS_HARDWARE_BUFFER_ID", "G17P_NATIVE_RENDER_POLICY"]],
    *[block(submission, r"^fn " + name + r"\(") for name in [
        "put_u16_le", "put_u32_le", "put_u64_le"]],
    *[block(submission, r"^pub\(crate\) (?:const )?fn " + name + r"\(") for name in [
        "apply_g17p_cold_opening_optional", "apply_g17p_retained_optional",
        "apply_g17p_retained_optional_with_flist_generation",
        "apply_g17p_retained_optional_with_flist_generation_and_context_update",
        "g17p_render_flist_generation"]],
    r"""
#[derive(Debug)] enum SubmissionError {
    PartialOpeningGraphObjectSize, PartialOpeningQueueHeadOutOfRange,
}
mod module_parameters {
    pub struct Param;
    impl Param { pub fn value(&self) -> &u8 { &0xff } }
    pub static g17p_render_3d_dm: Param = Param;
    pub static g17p_render_ta_dm: Param = Param;
}

#[test]
fn cold_and_reacquired_ids_get_distinct_lease_tokens() {
    let mut ids = G17PHardwareBufferIdAllocator::new();
    for token in 0..3 {
        assert_eq!(ids.allocate_with_token(), Ok((0, token)));
        ids.release(0).unwrap();
    }
    // Independent owners share the PB namespace, not a per-queue ordinal.
    assert_eq!(ids.allocate_with_token(), Ok((0, 3)));
    assert_eq!(ids.allocate_with_token(), Ok((1, 4)));
    ids.release(0).unwrap();
    assert_eq!(ids.allocate_with_token(), Ok((0, 5)));
}

#[test]
fn failed_id_allocation_does_not_consume_a_token() {
    let mut ids = G17PHardwareBufferIdAllocator::new();
    for id in 0..G17P_TA_HARDWARE_BUFFER_ID_COUNT as u32 {
        assert_eq!(ids.allocate_with_token(), Ok((id, id as u64)));
    }
    let next = ids.next_parameter_buffer_token;
    assert_eq!(ids.allocate_with_token(), Err(G17PHardwareBufferIdError::Exhausted));
    assert_eq!(ids.next_parameter_buffer_token, next);
    ids.release(42).unwrap();
    assert_eq!(ids.release(42), Err(G17PHardwareBufferIdError::DoubleFree));
    assert_eq!(ids.next_parameter_buffer_token, next);
    assert_eq!(ids.allocate_with_token(), Ok((42, next)));
    // A successfully acquired then abandoned prepublication lease is still
    // a real allocation: its token is not rolled back/reused.
    ids.release(42).unwrap();
    assert_eq!(ids.allocate_with_token(), Ok((42, next + 1)));
}

#[test]
fn token_wrap_skips_native_minus_one_sentinel() {
    let mut ids = G17PHardwareBufferIdAllocator::new();
    ids.next_parameter_buffer_token = u64::MAX - 1;
    assert_eq!(ids.allocate_with_token(), Ok((0, u64::MAX - 1)));
    ids.release(0).unwrap();
    assert_eq!(ids.allocate_with_token(), Ok((0, 0)));
    ids.release(0).unwrap();
    assert_eq!(ids.allocate_with_token(), Ok((0, 1)));
}

fn addresses(token: u64) -> G17PColdOpeningOptionalAddresses {
    G17PColdOpeningOptionalAddresses {
        context_scratch: 0x7002000000, firmware_scratch: 0x7002004000,
        usc_priv_mem_flist: 0x7002008000, usc_freelist_hardware_buffer_id: 0,
        channel_control: 0x700200c000, tiling_shared_object: 0xfffffc20c0060000,
        parameter_buffer_token: token, owner_pid: 0x625, ta_hardware_buffer_id: 7,
    }
}

#[test]
fn cold_wire_zero_is_preserved_and_only_ta_token_bytes_change() {
    let mut zero = [0xa5; G17P_COLD_OPENING_OPTIONAL_SIZE];
    apply_g17p_cold_opening_optional(
        G17PColdOpeningStage::Tiling, addresses(0), 0, 1, 1, 1, 1, &mut zero).unwrap();
    assert_eq!(&zero[0x76..0x7e], &[0; 8]);
    assert_eq!(&zero[0x6e..0x76], &0xfffffc20c0060000u64.to_le_bytes());
    assert_eq!(&zero[0x7e..0x82], &7u32.to_le_bytes());
    let token = 0x0123456789abcdef;
    let mut changed = [0xa5; G17P_COLD_OPENING_OPTIONAL_SIZE];
    apply_g17p_cold_opening_optional(
        G17PColdOpeningStage::Tiling, addresses(token), 0, 1, 1, 1, 1, &mut changed).unwrap();
    let mut expected = zero;
    expected[0x76..0x7e].copy_from_slice(&token.to_le_bytes());
    assert_eq!(changed, expected);
    apply_g17p_cold_opening_optional(
        G17PColdOpeningStage::Fragment, addresses(0), 1, 0, 1, 1, 1, &mut zero).unwrap();
    apply_g17p_cold_opening_optional(
        G17PColdOpeningStage::Fragment, addresses(token), 1, 0, 1, 1, 1, &mut changed).unwrap();
    assert_eq!(changed, zero);
    assert_eq!(&changed[0x76..0x86], &[0xff; 16]);
}

#[test]
fn retained_encoder_keeps_captured_token_independent_of_generations() {
    let token = 0xdeadbeef12345678u64;
    for ordinal in [0, 1, 2, 254] {
        let mut raw = [0xa5; G17P_COLD_OPENING_OPTIONAL_SIZE];
        apply_g17p_retained_optional(
            G17PColdOpeningStage::Tiling, addresses(token), ordinal,
            false, 0, 1, 1, 1, 1, &mut raw).unwrap();
        assert_eq!(&raw[0x76..0x7e], &token.to_le_bytes());
        assert_eq!(&raw[0x7e..0x82], &7u32.to_le_bytes());
        assert_eq!(&raw[0x3e..0x40], &(ordinal as u16).to_le_bytes());
    }
}
""",
])
with tempfile.TemporaryDirectory(prefix="g17-pb-lease-") as tmp:
    binary = Path(tmp) / "tests"
    subprocess.run(["rustc", "--edition=2021", "--test", "-", "-o", str(binary)],
                   input=with_g17_config(body), text=True, check=True)
    subprocess.run([str(binary)], check=True)

outer = block(manager, r"^    pub\(crate\) fn begin_translated_render\(")
reservation = block(manager, r"^    fn reserve_translated_render\(")
assert ".allocate_with_token()" in reservation
assert outer.index("self.reserve_translated_render(") < outer.index("self.begin_translated_render_with_hardware_buffer(")
# The cold no-storage branch is an out-of-line helper (kernel stack depth);
# splice its body back at the call site so ordering invariants still read in order.
inner = block(manager, r"^    fn begin_translated_render_with_hardware_buffer\(")
inner = inner.replace("self.install_cold_render_storage(", block(manager, r"^    fn install_cold_render_storage\(") + "(", 1)
assert inner.index(".restage_command(") < inner.index(".set_parameter_buffer_lease_token(")
assert inner.index(".new_user_render_storage(") < inner.index(".set_parameter_buffer_lease_token(")
assert inner.index(".set_parameter_buffer_lease_token(") < inner.index(".stage_render_groups_with_sksm(")
assert inner.index(".set_parameter_buffer_lease_token(") < inner.index(".stage_fragment(")
assert ".stage_render_parameter_buffer_page_list(" not in inner
assert ".stage_render_parameter_buffer_descriptor(" not in inner
assert ".selected_scene_index()" in manager
# The diagnostic read names the graph's own leased page-metrics entry, never
# the device-global entry 0 another graph may be using.
assert ".selected_parameter_metric(offset, scene)" in manager
assert "storage.parameter_metrics_offset()" in manager
assert ".selected_parameter_metric(scene)" not in manager
print("Production PB token handoff, retained backing preservation, and active Scene metric checks passed")
