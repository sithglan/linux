#!/usr/bin/env python3
"""Exercise actual retained-PM helpers, mocking only the mapped byte backing.

Run with python3; rustc is the only dependency. This compiles the production
layout, Scene selection/encoding/preparation helpers, and PM preparation methods
from g17_resources.rs, rather than a second implementation of their policy.
"""
from pathlib import Path
import re
import subprocess
import tempfile
import os


source_dir = Path(__file__).resolve().parent.parent
source = Path(os.environ.get("G17_SCENE_RESOURCE_SOURCE", str(source_dir / "g17_resources.rs"))).read_text()


def braced(text, start):
    at = text.index("{", start)
    depth = 1
    end = at + 1
    while depth:
        depth += (text[end] == "{") - (text[end] == "}")
        end += 1
    return text[start:end]


def block(pattern):
    match = re.search(pattern, source, re.M)
    assert match, pattern
    return braced(source, match.start())


def constant(name):
    match = re.search(r"^(?:pub\(crate\) )?const " + name + r": .*?;", source,
                      re.M | re.S)
    assert match, name
    return match.group()


constants = [
    "G17P_RENDER_USER_BASE", "G17P_PM_PAGE_SIZE", "G17P_PM_PAGES_PER_BLOCK",
    "G17P_PM_MAX_BLOCKS", "G17P_PM_MAX_OWNED_BLOCKS", "G17P_PM_BLOCK_COUNT", "G17P_PM_HWPB_STATE_SIZE", "G17P_PM_PAGE_LIST_SIZE",
    "G17P_PM_BLOCK_PAGE_BASE_TABLE_SIZE", "G17P_PM_SHARED_CONTROL_SIZE",
    "G17P_PM_COUNTER_SIZE", "G17P_PM_SCENE_COUNT", "G17P_PM_SCENE_ENTRY_SIZE",
    "G17P_PM_SCENE_TRAILER_SIZE", "G17P_PM_SCENE_ALLOCATION_SIZE",
    "G17P_PM_SELECTED_SCENE", "G17P_PM_INITIAL_HWPB_MANAGER_GENERATION",
    "G17P_PM_PAGE_METRICS_SIZE", "G17P_PM_SCENE_SCRATCH_SLOT_COUNT",
    "G17P_PM_SCENE_SCRATCH_STRIDE",
]
methods = [
    "prepare_retained_scene", "scene_registers", "selected_scene_va",
    "hwpb_state_va", "page_list_low_va", "pb_descriptor",
]
body = "\n".join([
    "#![allow(dead_code)]",
    "type Result<T = ()> = core::result::Result<T, i32>;",
    "type KBox<T> = Box<T>; const ENOSPC: i32 = 28;",
    "const EINVAL: i32 = 22; const ERANGE: i32 = 34;",
    "const ENOTSUPP: i32 = 95; const EIO: i32 = 5;",
    *(constant(name) for name in constants),
    "#[derive(Default, Debug, Clone, Copy)]",
    block(r"^struct G17PRenderStateLayout\s*"),
    "#[derive(Debug, Clone, Copy)]",
    block(r"^struct G17PParameterManagementLayout\s*"),
    block(r"^fn g17p_pm_page_align\("),
    block(r"^fn g17p_parameter_management_layout\("),
    block(r"^fn g17p_render_scene_index\("),
    block(r"^fn g17p_render_scene_registers\("),
    block(r"^fn prepare_g17p_retained_scene\("),
    block(r"^fn put_u32\("),
    block(r"^fn put_u64\("),
    block(r"^struct G17PSceneOwnership\s*"),
    block(r"^impl G17PSceneOwnership\s*"),
    block(r"^struct G17PParameterManagement\s+"),
    "impl G17PParameterManagement {",
    *(block(r"^    fn " + name + r"\(") for name in methods),
    "}",
    r'''
// Only the kernel mapping interface is mocked. Production helpers and methods
// above operate directly on this byte vector and use their real error paths.
mod mmu { pub struct KernelMapping; }
struct MappedObject {
    bytes: Vec<u8>,
    address: u64,
    access_count: usize,
    fail_access: bool,
}
impl MappedObject {
    fn iova(&self) -> u64 { self.address }
    fn with_bytes_mut<T>(&mut self, f: impl FnOnce(&mut [u8]) -> Result<T>) -> Result<T> {
        self.access_count += 1;
        if self.fail_access { return Err(EIO); }
        f(&mut self.bytes)
    }
}

const SCRATCH: u64 = 0x10_0017_8000;
const METRICS: u64 = 0x10_0008_0000;
fn patterned(size: usize) -> Vec<u8> {
    (0..size).map(|i| ((i * 37 + 13) % 251 + 1) as u8).collect()
}
fn render_layout(blocks: usize) -> G17PRenderStateLayout {
    G17PRenderStateLayout { tvb_blocks: blocks, ..Default::default() }
}
fn new_pm_at(first_submission_ordinal: u64) -> G17PParameterManagement {
    let layout = g17p_parameter_management_layout().unwrap();
    let mut scene_ownership = Box::new(G17PSceneOwnership::new());
    assert!(scene_ownership.commit(first_submission_ordinal, G17P_PM_SELECTED_SCENE));
    G17PParameterManagement {
        _page_list_low: mmu::KernelMapping,
        backing: MappedObject {
            bytes: patterned(layout.total_size), address: 0xffff_fc20_c006_0000,
            access_count: 0, fail_access: false,
        },
        layout, page_count: 128, page_list_vas: [0xffff_fc20_0010_0000, 0x70_0247_8000],
        metrics_vas: [0xffff_fc20_001f_0000, METRICS], hardware_buffer_id: 0,
        scene_ownership,
        selected_scene: G17P_PM_SELECTED_SCENE,
        manager_generation: G17P_PM_INITIAL_HWPB_MANAGER_GENERATION,
    }
}
fn new_pm() -> G17PParameterManagement { new_pm_at(0) }
fn assert_only_scene_write(before: &[u8], after: &[u8], at: usize, generation: u64) {
    assert_eq!(before.len(), after.len());
    let mut expected = before.to_vec();
    // loadPBDesc also restores the allocated HardwareBufferID on every use.
    put_u32(&mut expected, g17p_parameter_management_layout().unwrap().hwpb_state + 0xc, 0);
    assert_eq!(&after[..at + 0x48], &expected[..at + 0x48]);
    assert_eq!(&after[at + 0x48..at + 0x4c], &[0; 4]);
    assert_eq!(&after[at + 0x4c..at + 0x54], &generation.to_le_bytes());
    assert_eq!(&after[at + 0x54..], &before[at + 0x54..]);
}

#[test]
fn live_block_table_follows_the_complete_scene_allocation() {
    let layout = g17p_parameter_management_layout().unwrap();
    assert_eq!(layout.page_list, 0x8000);
    assert_eq!(layout.shared_control, 0x20000);
    assert_eq!(layout.page_metrics, 0x30000);
    assert_eq!(layout.scene_states, 0x38000);
    assert_eq!(layout.scene_states + G17P_PM_SCENE_ALLOCATION_SIZE, 0x3a840);
    assert_eq!(layout.block_page_bases, 0x40000);
    assert_eq!(layout.total_size, 0x48000);
}

#[test]
fn all_scene_and_scratch_positions_match_native_ring_selection() {
    assert_eq!((G17P_PM_SCENE_COUNT, G17P_PM_SCENE_SCRATCH_SLOT_COUNT), (80, 36));
    for ordinal in (0..=255u64).chain([1 << 24, 1 << 32, 1 << 40, u64::MAX]) {
        let scene = g17p_render_scene_index(0, ordinal).unwrap();
        assert_eq!(scene, ((u128::from(ordinal) + 1) % 80) as usize);
        assert_eq!(g17p_render_scene_registers(scene, SCRATCH, METRICS),
                   Some((0x178000 + (scene % 36) as u64 * 0x20,
                         0x80001 + scene as u64 * 4)));
    }
    assert_eq!(g17p_render_scene_index(0, u32::MAX as u64), Some(16));
    for (ordinal, cycle, metric) in [(0, 0x178020, 0x80005),
                                    (1, 0x178040, 0x80009),
                                    (2, 0x178060, 0x8000d)] {
        assert_eq!(g17p_render_scene_registers(g17p_render_scene_index(0, ordinal).unwrap(), SCRATCH, METRICS),
                   Some((cycle, metric)));
    }
    assert_eq!(g17p_render_scene_registers(36, SCRATCH, METRICS).unwrap().0, 0x178000);
    assert_eq!(g17p_render_scene_index(0, 79), Some(0));
    assert_eq!(g17p_render_scene_index(0, 80), Some(1));
}

#[test]
fn replacement_managers_keep_cold_ownership_and_uncommitted_plans_do_not_advance() {
    for first in [195, 2022, 1 << 24, 1 << 32, 1 << 40] {
        let mut pm = new_pm_at(first);
        pm.prepare_retained_scene(first, 0, pm.page_list_vas, &render_layout(32)).unwrap();
        assert_eq!(pm.selected_scene, 1);
        pm.prepare_retained_scene(first + 1, 0, pm.page_list_vas, &render_layout(32)).unwrap();
        assert_eq!(pm.selected_scene, 2);
        pm.prepare_retained_scene(first + 79, 0, pm.page_list_vas, &render_layout(32)).unwrap();
        assert_eq!(pm.selected_scene, 2);
        assert_eq!(pm.scene_ownership.select(first), Some(1));
        assert!(pm.scene_ownership.commit(first + 79, 2));
        assert_eq!(pm.scene_ownership.select(first + 80), Some(3));
    }
}

#[test]
fn register_encoding_rejects_invalid_ranges_and_preserves_native_domain_bit() {
    for scene in [80, usize::MAX] {
        assert_eq!(g17p_render_scene_registers(scene, SCRATCH, METRICS), None);
    }
    for scratch in [G17P_RENDER_USER_BASE - 1, SCRATCH + 1,
                    G17P_RENDER_USER_BASE + (1u64 << 32), u64::MAX] {
        assert_eq!(g17p_render_scene_registers(1, scratch, METRICS), None);
    }
    for metrics in [METRICS + 1, METRICS + 2, METRICS + 3, u64::MAX - 3] {
        assert_eq!(g17p_render_scene_registers(1, SCRATCH, metrics), None);
    }
    // Exact alternate domain encoding in the kext: P bit42 becomes encoded
    // bit39. Keep this independent known vector, not a copied encoder model.
    assert_eq!(g17p_render_scene_registers(0, SCRATCH, 0x400_0000_0000),
               Some((0x178000, 0x80_0000_0001)));
}

#[test]
fn every_scene_updates_its_host_fields_and_restores_hardware_buffer_id() {
    let layout = g17p_parameter_management_layout().unwrap();
    let before = patterned(layout.total_size);
    for scene in 0..80 {
        let mut raw = before.clone();
        let generation = 0x1234_5678_abcdef01;
        prepare_g17p_retained_scene(&mut raw, &layout, scene, generation, 0).unwrap();
        assert_only_scene_write(&before, &raw,
                                layout.scene_states + scene * 0x80, generation);
    }
}

#[test]
fn invalid_scene_spans_fail_without_partial_writes() {
    let layout = g17p_parameter_management_layout().unwrap();
    for scene in [80, usize::MAX] {
        let mut raw = patterned(layout.total_size);
        let before = raw.clone();
        assert_eq!(prepare_g17p_retained_scene(&mut raw, &layout, scene, 1, 0), Err(EINVAL));
        assert_eq!(raw, before);
    }
    for size in [0, layout.scene_states, layout.scene_states + 0x100 - 1] {
        let mut raw = patterned(size);
        let before = raw.clone();
        assert_eq!(prepare_g17p_retained_scene(&mut raw, &layout, 1, 1, 0), Err(ERANGE));
        assert_eq!(raw, before);
    }
    for at in [usize::MAX, usize::MAX - 0x80] {
        let mut bad = layout;
        bad.scene_states = at;
        let mut raw = patterned(layout.total_size);
        let before = raw.clone();
        assert_eq!(prepare_g17p_retained_scene(&mut raw, &bad, 1, 1, 0), Err(ERANGE));
        assert_eq!(raw, before);
    }
}

#[test]
fn retained_method_keeps_pool_identity_and_runtime_contents_over_three_scene_wraps() {
    let mut pm = new_pm();
    let initial_pb = pm.pb_descriptor().unwrap();
    let initial_hwpb = pm.hwpb_state_va();
    for ordinal in 1..=255 {
        let before = pm.backing.bytes.clone();
        pm.prepare_retained_scene(ordinal, 0, pm.page_list_vas, &render_layout(32)).unwrap();
        let scene = g17p_render_scene_index(0, ordinal).unwrap();
        assert_eq!(pm.selected_scene, scene);
        assert!(pm.scene_ownership.commit(ordinal, scene));
        assert!(pm.scene_ownership.release(ordinal - 1));
        assert_eq!(pm.selected_scene_va(), initial_hwpb + pm.layout.scene_states as u64 + scene as u64 * 0x80);
        assert_eq!(pm.pb_descriptor().unwrap(), initial_pb);
        assert_eq!(pm.hwpb_state_va(), initial_hwpb);
        assert_eq!(pm.manager_generation, 1);
        assert_eq!(pm.scene_registers(SCRATCH).unwrap(),
                   (0x178000 + (scene % 36) as u64 * 32, 0x80001 + scene as u64 * 4));
        assert_only_scene_write(&before, &pm.backing.bytes,
                                pm.layout.scene_states + scene * 0x80, 1);
    }
}

#[test]
fn retained_pool_restrictions_are_checked_before_mapping_or_state_changes() {
    for case in 0..6 {
        let mut pm = new_pm();
        let before = pm.backing.bytes.clone();
        let mut id = 0;
        let mut pages = pm.page_list_vas;
        let mut blocks = 32;
        match case {
            0 => id = 1,
            1 => pages[0] += 0x4000,
            2 => pages[1] += 0x4000,
            3 => blocks = 31,
            4 => blocks = G17P_PM_MAX_OWNED_BLOCKS + 1,
            5 => blocks = usize::MAX,
            _ => unreachable!(),
        }
        assert_eq!(pm.prepare_retained_scene(1, id, pages, &render_layout(blocks)), Err(ENOTSUPP));
        assert_eq!(pm.backing.access_count, 0);
        assert_eq!(pm.selected_scene, 1);
        assert_eq!(pm.backing.bytes, before);
    }
}

#[test]
fn grown_pool_capacity_is_reused_by_smaller_scenes() {
    let mut pm = new_pm();
    pm.page_count = 512;
    for (ordinal, blocks) in [32, 64, 128, 32].into_iter().enumerate() {
        assert_eq!(pm.prepare_retained_scene(ordinal as u64 + 1, 0,
            pm.page_list_vas, &render_layout(blocks)), Ok(()));
        assert_eq!(pm.page_count, 512);
        assert_eq!(pm.pb_descriptor().unwrap().2, 512);
    }
    assert_eq!(pm.prepare_retained_scene(5, 0, pm.page_list_vas,
        &render_layout(129)), Ok(()));
    assert_eq!(pm.page_count, 512);
}

#[test]
fn larger_target_keeps_busy_pm_inventory_until_firmware_demand_growth() {
    let mut pm = new_pm();
    // Chrome/compositor reproduction: existing 32-block pool, new 40-block
    // estimate, prior scenes still owned. Do not drain them or rewrite PM.
    for (index, blocks) in [32, 40, 64, 256, 32].into_iter().enumerate() {
        let ordinal = index as u64 + 1;
        let before = pm.backing.bytes.clone();
        let descriptor = pm.pb_descriptor().unwrap();
        pm.prepare_retained_scene(ordinal, 0, pm.page_list_vas,
            &render_layout(blocks)).unwrap();
        assert_only_scene_write(&before, &pm.backing.bytes,
            pm.layout.scene_states + pm.selected_scene * G17P_PM_SCENE_ENTRY_SIZE,
            pm.manager_generation);
        assert_eq!(pm.pb_descriptor().unwrap(), descriptor);
        assert_eq!(pm.page_count, 128);
        assert!(pm.scene_ownership.commit(ordinal, pm.selected_scene));
    }
}

#[test]
fn corrupt_actual_inventory_is_rejected_before_scene_write() {
    for pages in [0, 127, 129, ((G17P_PM_MAX_OWNED_BLOCKS + 1) * G17P_PM_PAGES_PER_BLOCK) as u32] {
        let mut pm = new_pm();
        pm.page_count = pages;
        let before = pm.backing.bytes.clone();
        assert_eq!(pm.prepare_retained_scene(1, 0, pm.page_list_vas,
            &render_layout(40)), Err(ENOTSUPP));
        assert_eq!(pm.backing.access_count, 0);
        assert_eq!(pm.backing.bytes, before);
    }
}

#[test]
fn mapped_access_and_layout_failure_do_not_commit_new_scene() {
    let mut pm = new_pm();
    let before = pm.backing.bytes.clone();
    pm.backing.fail_access = true;
    assert_eq!(pm.prepare_retained_scene(1, 0, pm.page_list_vas, &render_layout(32)), Err(EIO));
    assert_eq!(pm.selected_scene, 1);
    assert_eq!(pm.backing.bytes, before);
    pm.backing.fail_access = false;
    pm.layout.scene_states = usize::MAX;
    assert_eq!(pm.prepare_retained_scene(1, 0, pm.page_list_vas, &render_layout(32)), Err(ERANGE));
    assert_eq!(pm.selected_scene, 1);
    assert_eq!(pm.backing.bytes, before);
}
''',
])

with tempfile.TemporaryDirectory(prefix="g17-retained-scene-") as temp:
    binary = Path(temp) / "tests"
    subprocess.run(
        ["rustc", "--edition=2021", "--test", "-", "-o", str(binary)],
        input=body, text=True, check=True,
    )
    subprocess.run([str(binary)], check=True)

# Tie the exercised methods to the actual warm path. Command-local buffers may
# be refreshed, but the persistent PM/TVB/page-list closure must not be rebuilt.
warm = block(r"^    pub\(crate\) fn restage_command\(")
for forbidden in ["G17PParameterManagement::new", "next_parameter_management",
                  "tvb.with_bytes_mut", "copy_page_list_to", "stage_render_parameter_buffer",
                  "self.parameter_management =", "&mut self.parameter_management,"]:
    assert forbidden not in warm, forbidden
assert ".prepare_retained_scene(" in warm
assert "pm.scene_registers(" in warm
assert "translated.parameters.record_index = record_index;" in warm
assert "translated.parameters.cycle = cycle;" in warm
assert "pm.selected_scene_va()" in warm
manager = (source_dir / "g17_manager.rs").read_text()
# The normal path now restages its exclusively borrowed graph outside Runtime.
retained = braced(manager, manager.index("impl G17PRenderRestage {"))
assert ".restage_command(" in retained
for forbidden in ["stage_render_parameter_buffer", "G17PParameterManagement::new",
                  "new_user_render_storage", "self.storage.parameter_management ="]:
    assert forbidden not in retained, forbidden
# Preserve the same invariant for the inline legacy fallback.
start = manager.index("fn begin_translated_render_with_hardware_buffer(")
call = manager.index(".restage_command(", start)
legacy = braced(manager, manager.rfind("if retained {", start, call))
assert "stage_render_parameter_buffer" not in legacy
print("actual retained Scene helpers/methods and warm no-reinitialization callsites PASS")
