#!/usr/bin/env python3
"""Exercise the actual retained EventControl counter and publication methods.

The completion simulation changes only firmware-owned fields explicitly;
it is not hardware qualification of unlink or parameter-page reclamation.
"""
from pathlib import Path
import re
import subprocess
import tempfile
from g17_native_channel_test_support import native_channel_helpers

root = Path(__file__).resolve().parent.parent
source = (root / "g17_resources.rs").read_text()
submission = (root / "g17_submission.rs").read_text()
manager = (root / "g17_manager.rs").read_text()


def block(pattern, text=source):
    match = re.search(pattern, text, re.M)
    assert match, pattern
    end = text.index("{", match.end()) + 1
    depth = 1
    while depth:
        depth += (text[end] == "{") - (text[end] == "}")
        end += 1
    return text[match.start():end]


constants = "\n".join(
    re.search(r"(?:pub\(crate\) )?const " + name + r": (?:usize|u32|u64) = .*?;", source).group()
    for name in ["G17P_RENDER_QUEUE_METADATA_BASE", "G17P_RENDER_POOL_A_SLOTS", "G17P_RENDER_POOL_A",
                 "G17P_RENDER_SHARED_CONTROL_INNER", "G17P_RETAINED_RENDER_ORDINALS",
                 "G17P_USER_TILING_RING", "G17P_USER_TILING_POINTERS",
                 "G17P_USER_TILING_OPTIONAL", "G17P_USER_TILING_EVENT",
                 "G17P_USER_TILING_ENTRY_SIGNAL"]
)
sizes = "\n".join(
    re.search(r"pub\(crate\) const " + name + r": usize = .*?;", submission).group()
    for name in ["G17P_COMPUTE_OPTIONAL_EVENT_SIZE", "G17P_COLD_OPENING_OPTIONAL_SIZE",
                 "G17P_COLD_OPENING_EVENT_SIZE", "G17P_COLD_OPENING_RECORD_POOL_A_COUNT"]
)
body = "\n".join([
    "#![allow(dead_code)]",
"type Result<T=()> = core::result::Result<T,i32>;",
"type LiveResult<T=()> = core::result::Result<T,i32>;", "const EINVAL:i32=22;",
    block(r"^fn g17p_live_word_bounds\("),
    block(r"^fn g17p_live_read_u32\("),
    block(r"^fn g17p_live_read_u64\("),
    block(r"^fn g17p_live_write_u32\("),
    block(r"^fn g17p_live_write_u64\("),
 constants,
    "mod g17_submission {", sizes, native_channel_helpers(submission),
    block(r"^pub\(crate\) const fn g17p_render_pool_a_record_index\(", submission), "}",
    "#[derive(Debug, Copy, Clone, PartialEq, Eq)]",
    block(r"^struct G17PEventControlKick\s*"),
    block(r"^impl G17PEventControlKick\s*"),
    "#[derive(Debug, Copy, Clone, PartialEq, Eq)]",
    block(r"^pub\(crate\) struct G17PRenderEventControlState\s*"),
    block(r"^impl G17PRenderEventControlState\s*"),
    "#[derive(Debug, Copy, Clone, PartialEq, Eq)]",
    block(r"^struct G17PRenderItemPlan\s*"),
    block(r"^impl G17PRenderItemPlan\s*"),
    block(r"^struct G17PLateTilingBytes\s*"),
    block(r"^impl G17PLateTilingBytes\s*"),
    block(r"^fn put_u32\("), block(r"^fn put_u64\("),
    r'''
const SUPPORT_VA: u64 = 0xfffffc20c001c000;
fn read32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at+4].try_into().unwrap())
}
fn support() -> Vec<u8> {
    let mut bytes = vec![0xa5; 0x40000];
    for index in 0..35 {
        let slot = G17P_RENDER_POOL_A_SLOTS + index * 4;
        let record = G17P_RENDER_POOL_A + index * 0x100;
        put_u32(&mut bytes, slot, 0);
        put_u64(&mut bytes, record, SUPPORT_VA + slot as u64);
        put_u32(&mut bytes, record + 0xc, 0);
        put_u32(&mut bytes, record + 0x94, 0);
    }
    bytes
}
fn late(kick: G17PEventControlKick, ordinal: u64) -> G17PLateTilingBytes {
    G17PLateTilingBytes {
        item_group: [0; 32], items: G17PRenderItemPlan::new(ordinal, false).unwrap(),
        entry_signal: [0; g17_submission::G17P_COMPUTE_OPTIONAL_EVENT_SIZE],
        optional: [0; g17_submission::G17P_COLD_OPENING_OPTIONAL_SIZE],
        event: [0; g17_submission::G17P_COLD_OPENING_EVENT_SIZE],
        optional_offset: 0x1000,
        event_offset: 0x2000,
        pool_a_slot_offset: kick.slot_offset, pool_a_slot_value: kick.next,
        shared_inner_value: (ordinal as u32).wrapping_add(1).wrapping_mul(2),
    }
}
#[test]
fn native_cold_and_reuse_counts_follow_each_of_35_owners() {
    for native_cold in [false, true] {
        let mut bytes = support();
        let mut completed = [0u32; 35];
        for ordinal in 0..105 {
            let index = g17_submission::g17p_render_pool_a_record_index(ordinal, native_cold);
            let record = G17P_RENDER_POOL_A + index * 0x100;
            let original = bytes.clone();
            let first = G17PEventControlKick::prepare_reuse(&bytes, SUPPORT_VA, index).unwrap();
            assert_eq!(bytes, original, "preparation must not publish");
            assert_eq!(first.next, completed[index] + 1);
            if native_cold && ordinal == 18 { assert_eq!((index, first.next), (1, 3)); }
            first.apply(&mut bytes);
            for offset in 0..bytes.len() {
                if !(first.slot_offset..first.slot_offset + 4).contains(&offset) {
                    assert_eq!(bytes[offset], original[offset]);
                }
            }
            let second = G17PEventControlKick::prepare(&bytes, index).unwrap();
            assert_eq!(second.next, completed[index] + 2);
            let publication = late(second, ordinal);
            // Firmware may finish the first kick after the value was captured.
            put_u32(&mut bytes, record + 0xc, first.next);
            put_u32(&mut bytes, record + 0x94, 1);
            publication.apply_native_pool_transition(&mut bytes);
            assert_eq!(read32(&bytes, first.slot_offset), second.next);
            assert_eq!(read32(&bytes, record + 0xc), first.next);
            assert_eq!(read32(&bytes, record + 0x94), 1, "host must not unlink");
            assert!(G17PEventControlKick::prepare_reuse(&bytes, SUPPORT_VA, index).is_none());
            // Model both actual firmware completion and its separate unlink.
            put_u32(&mut bytes, record + 0xc, second.next);
            assert!(G17PEventControlKick::prepare_reuse(&bytes, SUPPORT_VA, index).is_none());
            put_u32(&mut bytes, record + 0x94, 0);
            completed[index] = second.next;
            assert!(G17PEventControlKick::prepare_reuse(&bytes, SUPPORT_VA, index).is_some());
        }
        assert!(completed.iter().all(|count| *count >= 4));
    }
}
#[test]
fn captured_late_value_is_identical_for_both_publication_paths() {
    let mut support = support();
    let slot = G17P_RENDER_POOL_A_SLOTS + 4;
    for previous in [1, 3, 101, u32::MAX] {
        put_u32(&mut support, slot, previous);
        let kick = G17PEventControlKick::prepare(&support, 1).unwrap();
        let publication = late(kick, 0);
        let original = support.clone();
        let mut atomic = support.clone();
        publication.apply(&mut vec![0; G17P_RENDER_QUEUE_METADATA_BASE + 0xc000], &mut atomic);
        publication.apply_native_pool_transition(&mut support);
        assert_eq!(support, atomic);
        assert_eq!(read32(&support, slot), previous.wrapping_add(1));
        for offset in 0..support.len() {
            if !(slot..slot+4).contains(&offset)
                && !(G17P_RENDER_SHARED_CONTROL_INNER..G17P_RENDER_SHARED_CONTROL_INNER+4).contains(&offset) {
                assert_eq!(support[offset], original[offset]);
            }
        }
        publication.apply_native_pool_transition(&mut support);
        assert_eq!(support, atomic, "reapplying captured bytes must not increment again");
    }
}
#[test]
fn wrapping_u32_and_drop_without_publication() {
    let mut bytes = support();
    let slot = G17P_RENDER_POOL_A_SLOTS;
    let record = G17P_RENDER_POOL_A;
    for previous in [u32::MAX-1, u32::MAX, 0] {
        put_u32(&mut bytes, slot, previous);
        put_u32(&mut bytes, record+0xc, previous);
        let original = bytes.clone();
        let discarded = G17PEventControlKick::prepare_reuse(&bytes, SUPPORT_VA, 0).unwrap();
        assert_eq!(discarded.next, previous.wrapping_add(1));
        assert_eq!(bytes, original);
        let retry = G17PEventControlKick::prepare_reuse(&bytes, SUPPORT_VA, 0).unwrap();
        assert_eq!(retry, discarded);
        retry.apply(&mut bytes);
        assert_eq!(read32(&bytes, slot), previous.wrapping_add(1));
    }
}
#[test]
fn invalid_bounds_pointer_counts_or_membership_do_not_mutate() {
    let mut bytes = support();
    for bad_index in [35, usize::MAX] {
        assert!(G17PEventControlKick::prepare(&bytes, bad_index).is_none());
        assert!(G17PEventControlKick::prepare_reuse(&bytes, SUPPORT_VA, bad_index).is_none());
    }
    for length in [0, G17P_RENDER_POOL_A_SLOTS+3, G17P_RENDER_POOL_A+0x97] {
        assert!(G17PEventControlKick::prepare_reuse(&bytes[..length], SUPPORT_VA, 0).is_none());
    }
    assert!(G17PEventControlKick::prepare_reuse(&bytes, u64::MAX, 0).is_none());
    for (offset, value) in [(G17P_RENDER_POOL_A, 0), (G17P_RENDER_POOL_A+0xc, 1),
                            (G17P_RENDER_POOL_A+0x94, 1)] {
        let saved = bytes.clone();
        put_u32(&mut bytes, offset, value);
        let invalid = bytes.clone();
        assert!(G17PEventControlKick::prepare_reuse(&bytes, SUPPORT_VA, 0).is_none());
        assert_eq!(bytes, invalid);
        bytes = saved;
    }
}
#[test]
fn fence_requires_its_exact_completed_and_unlinked_event_control() {
    // `slot_word` is the submitted counter as it reads in SHARED memory;
    // `submitted` is the driver's mirror of it. `unlinked()` deliberately does
    // not consult slot_word -- it is carried so a capture can show whether the
    // mirror drifted, which is a different bug from the firmware being behind.
    let base = G17PRenderEventControlState {
        record_index: 17, submitted: 8, slot_word: 8, completed: 7, membership: 1,
    };
    assert!(!G17PRenderEventControlState { slot_word: 0, ..base }.unlinked());
    assert!(!base.unlinked());
    assert!(!G17PRenderEventControlState { completed: 8, ..base }.unlinked());
    assert!(!G17PRenderEventControlState { membership: 0, ..base }.unlinked());
    assert!(G17PRenderEventControlState {
        completed: 8, membership: 0, ..base
    }.unlinked());
}
''',
])
with tempfile.TemporaryDirectory(prefix="g17-event-control-") as temp:
    binary = Path(temp) / "tests"
    subprocess.run(["rustc", "--edition=2021", "--test", "-", "-o", str(binary)],
                   input=body, text=True, check=True)
    subprocess.run([str(binary)], check=True)

restage = block(r"^    pub\(crate\) fn restage_command\(")
assert "G17PEventControlKick::prepare_reuse(" in restage
assert restage.index("G17PEventControlKick::prepare_reuse") < restage.index("g17_render::build_cold_opening_ta_descriptor(")
assert restage.count("pool_a_kick.apply(support);") == 1
prepare = block(r"^    pub\(crate\) fn prepare_late_tiling_publication_with_stamp\(")
assert prepare.count("G17PEventControlKick::prepare(raw, record_a_index)") == 1
assert "pool_a_kick.next," in prepare
assert "usize::from(self.pool_a_record_index)" in prepare
assert "self.pool_a_submitted = pool_a_kick.next" in prepare
assert "g17p_render_pool_a_record_index(" not in prepare
assert "quiesce_completed_job_list" not in source + manager
assert "g17p_render_can_reset_job_list" not in source + manager
poll = block(r"^    pub\(crate\) fn commit_render_observation\(", manager)
assert "job_list_empty: state.job_list_empty" in poll
assert "render_event_control_state" in poll
assert "event_control.unlinked()" in poll
assert "put_u" not in poll
print("actual counter increments, captured publication, retained owner and firmware-unlink checks PASS")
