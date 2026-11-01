#!/usr/bin/env python3
"""Render publication fields follow their native ownership domains.

Two bounded render slots own distinct outer AGXChannel pairs but share the USC
FList. The two fields therefore have different ownership domains:

* the outer ring entry's channel-started byte (+0x17, native `!chan[0x3c]`)
  follows the selected pair;
* tag-15 +0x3e, the FList generation, written as the slot ordinal.

This compiles the real tag-15 optional encoder and pins the manager's sources
for both fields.
"""
from pathlib import Path
from g17_config_test_support import with_g17_config
import re
import subprocess
import tempfile

src = Path(__file__).resolve().parent.parent
sub = (src / "g17_submission.rs").read_text()
manager = (src / "g17_manager.rs").read_text()
resources = (src / "g17_resources.rs").read_text()


def block(text, name):
    m = re.search(r"^[ \t]*(?:pub\(crate\) )?(?:const )?(?:fn|struct|enum) " + name + r"\b", text, re.M)
    assert m, name
    start = text.index("{", m.end())
    end, depth = start + 1, 1
    while depth:
        depth += (text[end] == "{") - (text[end] == "}")
        end += 1
    return text[m.start():end]


def constant(name):
    return re.search(r"^pub\(crate\) const " + name + r": .*?;", sub, re.M | re.S).group()


# --- channel-started byte -----------------------------------------------------
inner = block(manager, "begin_translated_render_with_hardware_buffer")
assert "let first_outer_submit =" in inner
assert "g17p_render_pair_first_outer_submit(" in inner
assert "queue_pair.slot()," in inner
publish = inner[inner.index(".publish_partial_opening_pair("):inner.index("move || {", inner.index(".publish_partial_opening_pair("))]
assert "first_outer_submit," in publish
assert inner.index("self.render_slots.get_mut(pair_slot).hardware_buffer_published = true;") \
    < inner.index("self.render_outer_channels_started[usize::from(queue_pair.slot())] = true;")
stop = block(manager, "release_runtime_objects_after_processor_stop")
assert "self.render_outer_channels_started = [false; 64];" in stop
outer = block(sub, "prepare_g17p_partial_opening_work_pair")
assert "let render_channel_started = !first_submit;" in outer

# --- FList generation ---------------------------------------------------------
assert inner.index("set_parameter_buffer_lease_token(") < inner.index("set_flist_generation(") \
    < inner.index("stage_render_groups_with_sksm("), "the generation must be set before staging"
assert "lifecycle.sequence" in inner[inner.index("set_flist_generation("):][:600]
# v92d: both fragment stages build their tag-15 config update through
# prepare_render_items -> final_config_update, which carries the FList
# generation and the GART context-update flag.
final = block(resources, "final_config_update")
assert "self.optional_addresses(tiling), ordinal, self.flist_generation," in final
assert "self.context_update_pending" in final
for name in ["stage_fragment", "stage_fragment_prefix"]:
    assert "self.stage_fragment_config_group(" in block(resources, name), name
assert "self.prepare_render_items(ordinal)?" in block(resources, "stage_fragment_config_group")
assert "final_config_update(" in block(resources, "prepare_render_items")
late = block(resources, "prepare_late_tiling_publication_with_stamp")
assert "self.flist_generation," in late
restage = block(resources, "restage_command")
assert "self.flist_generation = g17_submission::g17p_render_flist_generation(submission_ordinal);" in restage
assert "apply_g17p_retained_optional(" not in resources.replace("apply_g17p_retained_optional_with_flist_generation_and_context_update(", ""), \
    "a tag-15 optional is still encoded without the device-global generation"

# --- GART context update -----------------------------------------------------
assert "context_update_pending: true," in resources
assert "self.context_update_pending" in block(resources, "prepare_late_tiling_publication_with_stamp")
consistency = block(manager, "validate_render_queue_consistency")
assert "header.context_update != u16::from(context_update)" in consistency
completion = block(manager, "commit_render_observation")
assert "complete_render_context_update_retirement(pair_slot, true)?" in completion
context_retirement = block(manager, "complete_render_context_update_retirement")
assert "complete_context_update()" in context_retirement

body = "\n".join([
    "#![allow(dead_code, non_upper_case_globals)]",
    block(manager, "g17p_render_pair_first_outer_submit"),
    "use g17_submission::*;",
    block(sub, "put_u32_le"),
    block(sub, "put_u64_le"),
    r'''
const G17P_PARTIAL_OPENING_OUTER_SLOT_SIZE: usize = 0x18;
const G17P_PARTIAL_OPENING_OUTER_SLOT_COUNT: u32 = 0x100;
const G17P_PARTIAL_OPENING_TILING_CHANNEL_TABLE_INDEX: u8 = 0;
const G17P_PARTIAL_OPENING_FRAGMENT_CHANNEL_TABLE_INDEX: u8 = 1;
const G17P_PARTIAL_OPENING_PAIRED_DOORBELL: u64 = 0x0083_0000_0000_0000;
''',
    "#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n" + block(sub, "G17PPartialOpeningOuterChannelState"),
    "#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n" + block(sub, "G17PPartialOpeningOuterSlotPlan"),
    "#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n" + block(sub, "PreparedG17PPartialOpeningWorkPair"),
    block(sub, "prepare_g17p_partial_opening_outer_slot"),
    block(sub, "prepare_g17p_partial_opening_work_pair"),
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
    "mod g17_submission {",
    *[constant(n) for n in [
        "G17P_COLD_OPENING_OPTIONAL_SIZE", "G17P_NATIVE_RENDER_DOORBELL_PRIORITY",
        "G17P_NATIVE_RENDER_QOS_HARDWARE_BUFFER_ID", "G17P_NATIVE_RENDER_POLICY"]],
    *["#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n" + block(sub, n) for n in [
        "SubmissionError", "G17PColdOpeningStage", "G17PColdOpeningOptionalAddresses"]],
    *[block(sub, n) for n in [
        "put_u16_le", "put_u32_le", "put_u64_le", "apply_g17p_cold_opening_optional",
        "apply_g17p_retained_optional", "apply_g17p_retained_optional_with_flist_generation",
        "apply_g17p_retained_optional_with_flist_generation_and_context_update",
        "g17p_render_flist_generation"]],
    "}",
    r'''
fn u64_at(b: &[u8], at: usize) -> u64 { u64::from_le_bytes(b[at..at+8].try_into().unwrap()) }
fn u32_at(b: &[u8], at: usize) -> u32 { u32::from_le_bytes(b[at..at+4].try_into().unwrap()) }
fn addresses() -> G17PColdOpeningOptionalAddresses {
    G17PColdOpeningOptionalAddresses { context_scratch: 0x7000100000,
        firmware_scratch: 0x7000104000, usc_priv_mem_flist: 0xfffffc20001f8000,
        usc_freelist_hardware_buffer_id: 0x11223344, channel_control: 0x700010c000,
        tiling_shared_object: 0xfffffc20c0060000, parameter_buffer_token: 0x123456789abc,
        owner_pid: 0x5678, ta_hardware_buffer_id: 7 }
}

#[test]
fn outer_channel_started_is_independent_for_both_native_pairs() {
    let mut started = [false, false];
    assert_eq!(g17p_render_pair_first_outer_submit(&started, 0), Some(true));
    started[0] = true;
    assert_eq!(g17p_render_pair_first_outer_submit(&started, 0), Some(false));
    assert_eq!(g17p_render_pair_first_outer_submit(&started, 1), Some(true));
    started[1] = true;
    assert_eq!(g17p_render_pair_first_outer_submit(&started, 1), Some(false));
    assert_eq!(g17p_render_pair_first_outer_submit(&started, 2), None);
}

#[test]
fn secondary_pair_outer_slots_match_native_first_and_repeat_bytes() {
    let fresh = prepare_g17p_partial_opening_work_pair(
        G17PPartialOpeningOuterChannelState {
            consumers: [0, 0], producer: 0,
            queue_gpu_va: 0xffff_fc20_c000_0240, queue_write_index: 3,
        },
        G17PPartialOpeningOuterChannelState {
            consumers: [0, 0], producer: 0,
            queue_gpu_va: 0xffff_fc20_c000_0180, queue_write_index: 3,
        },
        3, 2, true,
    ).unwrap();
    assert_eq!(fresh.tiling.slot, [
        0, 0, 0, 0, 0, 0, 0, 0, 0x80, 0x01, 0x00, 0xc0,
        0x20, 0xfc, 0xff, 0xff, 0, 0, 0, 0, 3, 0, 2, 1,
    ]);
    assert_eq!(fresh.fragment.slot, [
        0, 0, 0, 0, 0, 0, 0, 0, 0x40, 0x02, 0x00, 0xc0,
        0x20, 0xfc, 0xff, 0xff, 1, 0, 0, 0, 3, 0, 3, 1,
    ]);

    let repeat = prepare_g17p_partial_opening_work_pair(
        G17PPartialOpeningOuterChannelState {
            consumers: [0, 0], producer: 1,
            queue_gpu_va: 0xffff_fc20_c000_0240, queue_write_index: 6,
        },
        G17PPartialOpeningOuterChannelState {
            consumers: [0, 0], producer: 1,
            queue_gpu_va: 0xffff_fc20_c000_0180, queue_write_index: 6,
        },
        3, 2, false,
    ).unwrap();
    assert_eq!(&repeat.tiling.slot[0x14..], &[6, 0, 2, 0]);
    assert_eq!(&repeat.fragment.slot[0x14..], &[6, 0, 3, 0]);
}

#[test]
fn plus_3e_follows_the_device_generation_and_nothing_else_moves() {
    for stage in [G17PColdOpeningStage::Tiling, G17PColdOpeningStage::Fragment] {
        for ordinal in [0u64, 5, 1458] {
            for generation in [0u64, 1, 1459, 0x1_0000_0000] {
                let mut raw = [0xa5; G17P_COLD_OPENING_OPTIONAL_SIZE];
                apply_g17p_retained_optional_with_flist_generation(
                    stage, addresses(), ordinal, generation, false, 0, 1, 1, 1, 1, &mut raw).unwrap();
                assert_eq!(u64_at(&raw, 0x3e), generation, "ordinal {ordinal}");
                // Per-QID change-detected generation still follows the slot.
                assert_eq!(raw[0x2f], ordinal as u8);
                // +0x46 (hardware-buffer ID) is untouched by the qword at +0x3e.
                assert_eq!(u32_at(&raw, 0x46), addresses().usc_freelist_hardware_buffer_id);
                let mut per_slot = [0xa5; G17P_COLD_OPENING_OPTIONAL_SIZE];
                apply_g17p_retained_optional(stage, addresses(), ordinal, false, 0, 1, 1, 1, 1, &mut per_slot).unwrap();
                if generation == ordinal as u64 {
                    assert_eq!(raw, per_slot, "a single client is byte-identical to the previous encoder");
                } else {
                    let mut expected = per_slot;
                    expected[0x3e..0x46].copy_from_slice(&generation.to_le_bytes());
                    assert_eq!(raw, expected, "only the FList generation may differ");
                }
            }
        }
    }
}

#[test]
fn the_second_slots_first_render_names_the_live_pool_generation() {
    // Slot 1, ordinal 0, on a device whose FList sequence is at 1459.
    let mut raw = [0u8; G17P_COLD_OPENING_OPTIONAL_SIZE];
    apply_g17p_retained_optional_with_flist_generation(
        G17PColdOpeningStage::Tiling, addresses(), 0, 1459, false, 2, 3, 2, 1, 6, &mut raw).unwrap();
    assert_eq!(u64_at(&raw, 0x3e), 1459);
    let mut old = [0u8; G17P_COLD_OPENING_OPTIONAL_SIZE];
    apply_g17p_retained_optional(
        G17PColdOpeningStage::Tiling, addresses(), 0, false, 2, 3, 2, 1, 6, &mut old).unwrap();
    assert_eq!(u64_at(&old, 0x3e), 0, "the per-slot encoder regressed the pool to generation 0");
}

#[test]
fn a_fresh_nonzero_ordinal_updates_context_then_retained_work_does_not() {
    for stage in [G17PColdOpeningStage::Tiling, G17PColdOpeningStage::Fragment] {
        let mut fresh = [0u8; G17P_COLD_OPENING_OPTIONAL_SIZE];
        apply_g17p_retained_optional_with_flist_generation_and_context_update(
            stage, addresses(), 195, 195, false, 2, 3, 2, 0, 6, true,
            &mut fresh).unwrap();
        assert_eq!(&fresh[0x1a..0x1c], &0u16.to_le_bytes());
        assert_eq!(&fresh[0x52..0x54], &0u16.to_le_bytes());
        assert_eq!(&fresh[0x62..0x64], &1u16.to_le_bytes());

        let mut retained = [0u8; G17P_COLD_OPENING_OPTIONAL_SIZE];
        apply_g17p_retained_optional_with_flist_generation_and_context_update(
            stage, addresses(), 196, 196, false, 2, 3, 2, 0, 6, false,
            &mut retained).unwrap();
        assert_eq!(&retained[0x62..0x64], &0u16.to_le_bytes());
    }
}
''',
])
with tempfile.TemporaryDirectory(prefix="g17-slot-globals-") as tmp:
    binary = Path(tmp) / "tests"
    subprocess.run(["rustc", "-Awarnings", "--edition=2021", "--test", "-", "-o", str(binary)],
                   input=with_g17_config(body), text=True, check=True)
    subprocess.run([str(binary), "--test-threads=1"], check=True)
print("PASS: channel-started byte and FList generation follow the device, not the slot")
