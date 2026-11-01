#!/usr/bin/env python3
"""Execute the production G17P TVB growth ring plan and guard its publication."""
from pathlib import Path
import subprocess
import tempfile


SOURCE = (Path(__file__).resolve().parents[1] / "g17_resources.rs").read_text()


def item(marker: str) -> str:
    start = SOURCE.index(marker)
    brace = SOURCE.index("{", start)
    depth = 1
    end = brace + 1
    while depth:
        depth += (SOURCE[end] == "{") - (SOURCE[end] == "}")
        end += 1
    return SOURCE[start:end]


plan = "\n".join(
    [
        item("struct G17PPmControlCursors {"),
        item("fn g17p_pm_control_cursors("),
        item("struct G17PPmGrowthPlan {"),
        item("impl G17PPmGrowthPlan {"),
        item("fn g17p_pm_growth_plan("),
    ]
)
growth = item("    fn grow_render_tvb_owned(")

# Production must consume the firmware cursor at shared +8, build the active
# circular inventory, and publish it into PBState. The old block-zero rebuild
# and read-cursor reset are forbidden. Growth still deliberately resets only
# the PBDesc ring state, as native buildPageList -> loadPBDesc does.
assert (
    "g17p_pm_growth_plan(control.read,control.committed,control.total,added_blocks)"
    in growth
)
assert "ring.available_slot(ordinal)" in growth
assert "ring.added_slot(added)" in growth
assert "state+0x3c,ring.target_cursor as u32" in growth
assert "state+0x40,ring.read_cursor as u32" in growth
assert "g17_submission::G17PPbDescRingState::default()" in growth
assert "state+0x40,0" not in growth
assert "for block in 0..blocks" not in growth
assert (
    "put_u32(pm_raw,pm_layout.shared_control+G17P_PM_SHARED_READ_CURSOR" not in growth
)
assert growth.index("vm.append_render_pool_mappings") < growth.index(
    "put_u32(pm_raw,pm_layout.block_page_bases"
)

rust = r'''
#![allow(dead_code)]
const G17P_PM_MAX_BLOCKS: usize = 0xd1a;
const G17P_PM_SHARED_TOTAL_CURSOR: usize = 0x00;
const G17P_PM_SHARED_COMMITTED_CURSOR: usize = 0x04;
const G17P_PM_SHARED_READ_CURSOR: usize = 0x08;
''' + plan + r'''

fn reference(read: usize, producer: usize, added: usize) -> Option<(usize, Vec<usize>)> {
    if added == 0 || read >= G17P_PM_MAX_BLOCKS || producer >= G17P_PM_MAX_BLOCKS {
        return None;
    }
    let retained = (producer + G17P_PM_MAX_BLOCKS - read) % G17P_PM_MAX_BLOCKS;
    if retained + added >= G17P_PM_MAX_BLOCKS {
        return None;
    }
    let target = (producer + added) % G17P_PM_MAX_BLOCKS;
    let slots = (0..retained + added)
        .map(|ordinal| (read + ordinal) % G17P_PM_MAX_BLOCKS)
        .collect();
    Some((target, slots))
}

#[test]
fn shared_control_decodes_total_committed_and_firmware_read_at_plus_eight() {
    let mut raw = [0xa5; 32];
    raw[12..16].copy_from_slice(&7u32.to_le_bytes());
    raw[16..20].copy_from_slice(&8u32.to_le_bytes());
    raw[20..24].copy_from_slice(&9u32.to_le_bytes());
    let cursors = g17p_pm_control_cursors(&raw, 12).unwrap();
    assert_eq!(cursors.total, 7);
    assert_eq!(cursors.committed, 8);
    assert_eq!(cursors.read, 9);
    assert!(g17p_pm_control_cursors(&raw[..23], 12).is_none());
    assert!(g17p_pm_control_cursors(&raw, usize::MAX).is_none());
}

#[test]
fn cold_and_consumed_prefixes_start_at_the_native_read_cursor() {
    let cold = g17p_pm_growth_plan(0, 32, 32, 16).unwrap();
    assert_eq!(cold.retained_blocks, 32);
    assert_eq!(cold.target_cursor, 48);
    assert_eq!((0..cold.available_blocks())
        .map(|i| cold.available_slot(i).unwrap()).collect::<Vec<_>>(),
        (0..48).collect::<Vec<_>>());

    let consumed = g17p_pm_growth_plan(7, 32, 32, 4).unwrap();
    assert_eq!(consumed.retained_blocks, 25);
    assert_eq!(consumed.target_cursor, 36);
    assert_eq!(consumed.available_slot(0), Some(7));
    assert_eq!(consumed.available_slot(28), Some(35));
    assert_eq!(consumed.available_slot(29), None);
}

#[test]
fn page_list_and_new_block_slots_wrap_independently_of_physical_indices() {
    let plan = g17p_pm_growth_plan(0xd17, 0xd19, 0xd19, 3).unwrap();
    assert_eq!(plan.retained_blocks, 2);
    assert_eq!(plan.target_cursor, 2);
    assert_eq!((0..plan.available_blocks())
        .map(|i| plan.available_slot(i).unwrap()).collect::<Vec<_>>(),
        vec![0xd17, 0xd18, 0xd19, 0, 1]);
    assert_eq!((0..plan.added_blocks)
        .map(|i| plan.added_slot(i).unwrap()).collect::<Vec<_>>(),
        vec![0xd19, 0, 1]);

    let mut bases = (0..G17P_PM_MAX_BLOCKS)
        .map(|slot| 0x1000 + slot as u32 * 8).collect::<Vec<_>>();
    let new = [0xa000, 0xb000, 0xc000];
    for (index, first) in new.into_iter().enumerate() {
        bases[plan.added_slot(index).unwrap()] = first;
    }
    let pages = (0..plan.available_blocks()).flat_map(|ordinal| {
        let first = bases[plan.available_slot(ordinal).unwrap()];
        (0..4).map(move |page| first + page)
    }).collect::<Vec<_>>();
    assert_eq!(&pages[0..8], &[
        0x78b8, 0x78b9, 0x78ba, 0x78bb,
        0x78c0, 0x78c1, 0x78c2, 0x78c3,
    ]);
    assert_eq!(&pages[8..], &[
        0xa000, 0xa001, 0xa002, 0xa003,
        0xb000, 0xb001, 0xb002, 0xb003,
        0xc000, 0xc001, 0xc002, 0xc003,
    ]);
}

#[test]
fn one_empty_slot_and_staged_cursor_invariants_are_enforced() {
    let exact = g17p_pm_growth_plan(5, 3, 3, 1).unwrap();
    assert_eq!(exact.available_blocks(), G17P_PM_MAX_BLOCKS - 1);
    assert_eq!(exact.target_cursor, 4);
    assert!(g17p_pm_growth_plan(5, 3, 3, 2).is_none());
    assert!(g17p_pm_growth_plan(5, 2, 3, 1).is_none());
    assert!(g17p_pm_growth_plan(0, 0, 0, 0).is_none());
    assert!(g17p_pm_growth_plan(G17P_PM_MAX_BLOCKS as u32, 0, 0, 1).is_none());
    assert!(g17p_pm_growth_plan(0, G17P_PM_MAX_BLOCKS as u32, 0, 1).is_none());
    assert!(g17p_pm_growth_plan(0, 0, G17P_PM_MAX_BLOCKS as u32, 1).is_none());
}

#[test]
fn production_plan_matches_a_modular_reference_across_wrap_boundaries() {
    let cursors = [0, 1, 7, 31, 32, 0xd16, 0xd17, 0xd18, 0xd19];
    for read in cursors {
        for producer in cursors {
            for added in [1, 2, 3, 16, 64, 0xd19] {
                let expected = reference(read, producer, added);
                let actual = g17p_pm_growth_plan(
                    read as u32, producer as u32, producer as u32, added);
                assert_eq!(actual.is_some(), expected.is_some(),
                    "read={read:#x} producer={producer:#x} added={added:#x}");
                if let (Some(actual), Some((target, slots))) = (actual, expected) {
                    assert_eq!(actual.target_cursor, target);
                    assert_eq!((0..actual.available_blocks())
                        .map(|i| actual.available_slot(i).unwrap()).collect::<Vec<_>>(), slots);
                }
            }
        }
    }
}
'''

with tempfile.TemporaryDirectory() as directory:
    source = Path(directory) / "test.rs"
    binary = Path(directory) / "test"
    source.write_text(rust)
    subprocess.run(
        ["rustc", "--edition=2021", "--test", str(source), "-o", str(binary)],
        check=True,
    )
    subprocess.run([str(binary), "--test-threads=1"], check=True)

print("PASS G17P TVB growth: native circular page-list and cursor publication")
