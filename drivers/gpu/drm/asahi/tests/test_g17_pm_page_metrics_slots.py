#!/usr/bin/env python3
"""Every parameter-management graph leases its own page-metrics entry.

Native `AGXGart::requestPMPageMetricsBuffer` (AGXG17P 353.14) gives each
`AGXParameterManagement` one entry of the gart's page-metrics resource stack
and every Scene of that graph names `entry + scene * 4`. This driver used to
publish the ONE captured object (`T8140_PARAMETER_METRICS_LOW_VA`, 0x8000
bytes) for every graph on the device, so the two physical pairs of one
logical queue -- and two clients -- indexed the same 80 words with their own
Scene ordinals. Neo 2026-09-26 21:14 (`hw-warmpair-v2-2114`): pairs 14/15
and 16/17 of context 9 both bound `metrics=[0xfffffc21800a8000,0x1000080000]`.

This compiles the real entry arithmetic and allocator helpers from
g17_resources.rs and pins:

* entry 0 is byte-for-byte the historical address, so a lone client publishes
  exactly what it always did;
* entries are disjoint, inside the captured object, and every Scene metric a
  graph can encode stays inside its own entry;
* the allocator hands out the lowest free entry, refuses beyond the object,
  and reuses a released entry;
* the storage constructor leases an entry per graph and the leased entry
  travels with the storage, never with the manager's per-submit fields.
"""
from pathlib import Path
import re
import subprocess
import tempfile

src = Path(__file__).resolve().parent.parent
resources = (src / "g17_resources.rs").read_text()
manager = (src / "g17_manager.rs").read_text()


def braced(text, start):
    at = text.index("{", start)
    depth, end = 1, at + 1
    while depth:
        depth += (text[end] == "{") - (text[end] == "}")
        end += 1
    return text[start:end]


def block(source, pattern):
    match = re.search(pattern, source, re.M)
    assert match, pattern
    return braced(source, match.start())


def constant(name):
    match = re.search(r"^(?:pub\(crate\) )?const " + name + r": .*?;", resources,
                      re.M | re.S)
    assert match, name
    return match.group()


# --- source contracts ------------------------------------------------------
ctor = block(resources, r"^    pub\(crate\) fn new_user_render_storage\(")
assert "self.acquire_parameter_metrics_region()?" in ctor, \
    "a new graph must lease its own page-metrics entry"
assert "parameter_metrics_low_va,\n        ]" not in ctor, \
    "the bare device-global metrics addresses must not reach a graph"
assert "G17PUninstalledRenderGraph::build(" in ctor
assert ctor.index("acquire_parameter_metrics_region") < ctor.index("G17PUninstalledRenderGraph::build(")

acquire = block(resources, r"^    pub\(crate\) fn acquire_parameter_metrics_region\(")
assert "G17PPmPageMetricsLease::new(self.resources.parameter_metrics_slots.clone())" in acquire
assert "g17p_pm_metrics_slot_vas(base, usize::from(slot))" in acquire
assert ".ok_or(ENOSPC)?" in acquire, "exhaustion must not alias a live graph"
assert "unwrap_or(base)" not in acquire and "map_or(0" not in acquire
assert "g17p_pm_metrics_slot_clear(bytes, usize::from(slot))" in acquire

storage_struct = block(resources, r"^pub\(crate\) struct G17PUserRenderStorage\s")
assert "parameter_metrics_lease: Option<G17PPmPageMetricsLease>," in storage_struct, \
    "the lease must live and die with the storage"
storage_new = block(resources, r"^    fn new\(\n        dev: &AsahiDevice,\n        uat: &mmu::Uat,\n        parameter_metrics: G17PPmPageMetricsBinding,")
assert "parameter_metrics_lease: parameter_metrics.lease," in storage_new
assert "parameter_metrics.vas," in storage_new, "the PM graph is encoded against the leased entry"

slot_state = block(manager, r"^struct G17PRenderSlotState\s")
assert "metrics" not in slot_state, "the leased entry is graph state, not swapped slot state"

resources_struct = block(resources, r"^struct G17MappedResources\s")
assert "parameter_metrics_slots: Arc<kernel::sync::Mutex<u128>>," in resources_struct

read = block(resources, r"^    pub\(crate\) fn selected_parameter_metric\(")
assert "offset.checked_add(scene * 4)" in read, "diagnostic reads must use the graph's entry"
assert "storage.parameter_metrics_offset()" in manager

encode = block(resources, r"^fn encode_g17p_parameter_management\(")
assert "put_u64(raw, entry, metrics_high + (index * 4) as u64);" in encode
assert "put_u64(raw, entry + 0x08, metrics_low + (index * 4) as u64);" in encode

# --- compile the real helpers ----------------------------------------------
constants = [
    "G17P_PM_SCENE_COUNT", "G17P_PM_PAGE_METRICS_SIZE",
    "G17P_PM_METRICS_REGION_SIZE", "G17P_PM_METRICS_SLOT_STRIDE",
    "G17P_PM_METRICS_SLOT_COUNT", "G17P_PM_SELECTED_SCENE",
    "G17P_PM_SCENE_SCRATCH_SLOT_COUNT", "G17P_PM_SCENE_SCRATCH_STRIDE",
    "G17P_RENDER_USER_BASE",
]
body = "\n".join([
    "#![allow(dead_code)]",
    *(constant(name) for name in constants),
    "const T8140_PARAMETER_METRICS_LOW_VA: u64 = 0x0000_0010_0008_0000;",
    "const T8140_PARAMETER_METRICS_SIZE: usize = 0x8000;",
    "const RENDER_SLOT_COUNT: usize = 64;",
    block(resources, r"^fn g17p_pm_metrics_slot_vas\("),
    block(resources, r"^fn g17p_pm_metrics_slot_acquire\("),
    block(resources, r"^fn g17p_pm_metrics_slot_release\("),
    block(resources, r"^fn g17p_pm_metrics_slot_clear\("),
    block(resources, r"^fn g17p_render_scene_index\("),
    block(resources, r"^fn g17p_render_scene_registers\("),
    r'''
const BASE: [u64; 2] = [0xffff_fc21_800a_8000, T8140_PARAMETER_METRICS_LOW_VA];
const SCRATCH: u64 = 0x10_0017_8000;

#[test]
fn entry_zero_is_the_historical_address() {
    assert_eq!(g17p_pm_metrics_slot_vas(BASE, 0), Some(BASE));
    assert_eq!(G17P_PM_METRICS_REGION_SIZE, T8140_PARAMETER_METRICS_SIZE);
    assert_eq!(G17P_PM_METRICS_SLOT_STRIDE, G17P_PM_SCENE_COUNT * 4);
}

#[test]
fn entries_are_disjoint_and_inside_the_captured_object() {
    assert!(G17P_PM_METRICS_SLOT_COUNT <= 128);
    assert!(G17P_PM_METRICS_SLOT_COUNT >= RENDER_SLOT_COUNT);
    let mut regions = Vec::new();
    for slot in 0..G17P_PM_METRICS_SLOT_COUNT {
        let [high, low] = g17p_pm_metrics_slot_vas(BASE, slot).unwrap();
        let end = low + G17P_PM_PAGE_METRICS_SIZE as u64;
        assert!(end <= BASE[1] + T8140_PARAMETER_METRICS_SIZE as u64, "slot {slot}");
        assert_eq!(high - BASE[0], low - BASE[1]);
        for (other_low, other_end) in &regions {
            assert!(end <= *other_low || low >= *other_end, "slot {slot} overlaps");
        }
        regions.push((low, end));
        // Every Scene metric this graph can encode names its own entry.
        for ordinal in 0..(2 * G17P_PM_SCENE_COUNT as u64) {
            let scene = g17p_render_scene_index(0, ordinal).unwrap();
            assert!(g17p_render_scene_registers(scene, SCRATCH, low).is_some());
            let metric = low + (scene * 4) as u64;
            assert!(metric >= low && metric + 4 <= end, "slot {slot} scene {scene}");
        }
    }
    assert_eq!(g17p_pm_metrics_slot_vas(BASE, G17P_PM_METRICS_SLOT_COUNT), None);
}

#[test]
fn allocator_hands_out_lowest_free_entry_and_reuses_released_ones() {
    let mut used = 0u128;
    let mut seen = Vec::new();
    for expected in 0..G17P_PM_METRICS_SLOT_COUNT {
        let slot = g17p_pm_metrics_slot_acquire(&mut used).unwrap();
        assert_eq!(usize::from(slot), expected);
        assert!(!seen.contains(&slot));
        seen.push(slot);
    }
    assert_eq!(g17p_pm_metrics_slot_acquire(&mut used), None, "beyond the object");
    g17p_pm_metrics_slot_release(&mut used, 5);
    assert_eq!(g17p_pm_metrics_slot_acquire(&mut used), Some(5));
    assert_eq!(g17p_pm_metrics_slot_acquire(&mut used), None);
    // A second pair of the same client, then a second client: three graphs,
    // three entries, and the first graph keeps entry 0.
    let mut used = 0u128;
    let first_pair = g17p_pm_metrics_slot_acquire(&mut used).unwrap();
    let second_pair = g17p_pm_metrics_slot_acquire(&mut used).unwrap();
    let other_client = g17p_pm_metrics_slot_acquire(&mut used).unwrap();
    assert_eq!((first_pair, second_pair, other_client), (0, 1, 2));
    assert_ne!(g17p_pm_metrics_slot_vas(BASE, 0), g17p_pm_metrics_slot_vas(BASE, 1));
    // Releasing out of range is a no-op, never a corrupted bitmap.
    let before = used;
    g17p_pm_metrics_slot_release(&mut used, 127);
    assert_eq!(used, before);
}

#[test]
fn reused_entry_starts_empty_without_touching_another_graph() {
    let mut bytes = vec![0xa5; G17P_PM_METRICS_REGION_SIZE];
    let mut used = 0u128;
    let first = g17p_pm_metrics_slot_acquire(&mut used).unwrap();
    let second = g17p_pm_metrics_slot_acquire(&mut used).unwrap();
    g17p_pm_metrics_slot_release(&mut used, first);
    let reused = g17p_pm_metrics_slot_acquire(&mut used).unwrap();
    assert_eq!(reused, first);
    g17p_pm_metrics_slot_clear(&mut bytes, reused as usize).unwrap();
    assert!(bytes[..G17P_PM_METRICS_SLOT_STRIDE].iter().all(|b| *b == 0));
    assert!(bytes[second as usize * G17P_PM_METRICS_SLOT_STRIDE..].iter().all(|b| *b == 0xa5));
    let before = bytes.clone();
    assert_eq!(g17p_pm_metrics_slot_clear(&mut bytes, G17P_PM_METRICS_SLOT_COUNT), None);
    assert_eq!(bytes, before);
    assert_eq!(g17p_pm_metrics_slot_clear(&mut bytes[..8], 0), None);
    assert_eq!(bytes, before);
}
''',
])

with tempfile.TemporaryDirectory(prefix="g17-pm-metrics-") as tmp:
    binary = Path(tmp) / "t"
    subprocess.run(["rustc", "-Awarnings", "--edition=2021", "--test", "-", "-o", str(binary)],
                   input=body, text=True, check=True)
    subprocess.run([str(binary), "--test-threads=1"], check=True)
print("PASS: every parameter-management graph leases its own page-metrics entry; entry 0 is the historical address")
