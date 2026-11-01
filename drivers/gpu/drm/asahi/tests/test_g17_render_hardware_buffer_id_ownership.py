#!/usr/bin/env python3
"""A retained render storage owns its TA HardwareBufferID for its lifetime.

`prepare_retained_scene` refuses to restage a retained parameter-management
graph under any HardwareBufferID other than the one it was encoded against
(ENOTSUPP). The manager nevertheless allocated the ID per submission from one
shared LIFO whose top is ID 0, so the number a restage received was the number
the graph expected only while a single client rendered. Measured 2026-09-07:
gnome-shell on slot 0 (graph built with ID 0), vkcube on slot 1 built its graph
with the same ID 0 between two gnome frames, vkcube's render timed out and its
ID stayed pinned, gnome-shell's next frame received ID 1 and was refused --
"retained-scene preparation failed (ENOTSUPP)" -- and the compositor lost its
device. Two healthy clients whose renders merely overlap hit the same refusal.

This compiles the real allocator and replays that sequence against it, then
pins the manager to using the storage's own ID for a retained restage and to
returning it only when the storage is dropped.
"""
from pathlib import Path
import re
import subprocess
import tempfile

src = Path(__file__).resolve().parent.parent
manager = (src / "g17_manager.rs").read_text()
resources = (src / "g17_resources.rs").read_text()
runtime = (src / "g17_live_boot.rs").read_text()
drm = (src / "g17_drm.rs").read_text()


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


# --- the manager's use of the allocator ------------------------------------
# v92d: per-pair storage. The reservation consults the retained storage's own
# ID before any fresh allocation.
begin = block(manager, r"^    fn reserve_translated_render\(")
assert "retained_hardware_buffer_id" in begin, "a retained storage's own ID is not consulted"
assert ".acquire_reserved_with_token(" in begin and ".allocate_with_token()" in begin
assert begin.index("self.recycle_desynced_render_storage(pair_slot)?") < begin.index(".acquire_reserved_with_token(")
assert begin.index(".acquire_reserved_with_token(") < begin.index(".allocate_with_token()"), \
    "the retained path must be tried before a fresh allocation"

# The cold no-storage branch is an out-of-line helper (kernel stack depth);
# splice its body back at the call site so ordering invariants still read in order.
inner = block(manager, r"^    fn begin_translated_render_with_hardware_buffer\(")
inner = inner.replace("self.install_cold_render_storage(", block(manager, r"^    fn install_cold_render_storage\(") + "(", 1)
# PB_OWNER_REF (g17p_host_path bit18) reserves the ID with its owner reference.
assert inner.index('"new-user-render-storage"') < inner.index(".reserve_with_owner_ref(id,"), \
    "a fresh storage's ID is not reserved for the storage's lifetime"

# A desynced graph keeps its ID while it waits in its queue's teardown; the ID
# returns to the free list only when that teardown settles.
desync = block(manager, r"^    fn recycle_desynced_render_storage\(")
assert "storage.take()" in desync and "logical_queue_teardowns" in desync
assert "unreserve" not in desync
settle = block(manager, r"^    pub\(crate\) fn settle_logical_queue_teardowns\(")
assert "render_hardware_buffer_ids.unreserve(id)" in settle
stop = block(manager, r"^    pub\(crate\) fn release_runtime_objects_after_processor_stop\(")
assert "unreserve_render_storage_hardware_buffer_id" in stop
assert stop.index("unreserve_render_storage_hardware_buffer_id") < stop.index(
    "self.render_slots.get_mut(pair_slot).storage = None"
)
# j700's cross-VM retired-handoff graph (handoff_retained_render_vm) is not part
# of the per-pair design; test_g17_per_pair_invariants.py covers owner change.

# The identity gate stays: the fix satisfies it, it does not bypass it.
scene = block(resources, r"^    fn prepare_retained_scene\(")
assert "self.hardware_buffer_id != hardware_buffer_id" in scene
assert "return Err(ENOTSUPP)" in scene
getter = block(resources, r"^    pub\(crate\) fn retained_hardware_buffer_id\(")
assert "pm.hardware_buffer_id" in getter

# --- a firmware-retired timeout keeps the slot -------------------------------
retire = block(manager, r"^    pub\(crate\) fn retire_abandoned_render_if_firmware_retired\(")
for needle in ["completion.retired", "state.job_list_empty", "execution.observed()",
               ".release(hardware_buffer_id)",
               "tracker = ",
               ".retire_command_generation(ordinal)?"]:
    assert needle in retire, needle
assert retire.index("return Ok(false)") < retire.index("complete_g17p_cl_kicks("), \
    "bookkeeping must not run for a group the firmware still holds"
abandon = block(runtime, r"^    pub\(crate\) fn abandon_deferred_render\(")
assert "-> Result<bool>" in abandon
assert "retire_failed_deferred_render(render_slot, true)" in abandon
abandon = block(runtime, r"^    fn retire_failed_deferred_render\(")
assert "retire_abandoned_render_if_firmware_retired(" in abandon
assert "retire_render_usc_freelist_at_completion(" in abandon
assert abandon.index("retire_abandoned_render_if_firmware_retired(") \
    < abandon.index("abandon_render_submission_after_timeout()")
# v92d: watchdog and recovery settle share the final-observation sink.
timeout = block(runtime, r"^    pub\(crate\) fn timeout_deferred_render\(")
assert "fail_render_after_final_observation(" in timeout
timeout = block(runtime, r"^    fn fail_render_after_final_observation\(")
assert ".retire_failed_deferred_render(render_slot, submission_id.is_some())" in timeout
assert timeout.index("if retired") < timeout.index("signal.complete(Err(EIO))")
assert timeout.index("signal.complete(Err(EIO))") < timeout.index("self.quarantine_failed_render_completion(signal, error)")
# Stability patch 0006: the unretired group and its queued siblings are
# retained through one helper.
assert "quarantined_render_completions" in block(runtime, r"^    fn quarantine_failed_render_completion\("), \
    "only an unretired group may retain and quarantine its physical slot"

# --- the allocator itself ----------------------------------------------------
derive = "#[derive(Debug, Clone, Copy, PartialEq, Eq)]\n"
body = "\n".join([
    "#![allow(dead_code, non_upper_case_globals)]",
    constant(manager, "G17P_TA_HARDWARE_BUFFER_ID_COUNT"),
    derive + block(manager, r"^enum G17PHardwareBufferIdError\s*"),
    derive + block(manager, r"^struct G17PHardwareBufferIdAllocator\s*"),
    block(manager, r"^impl G17PHardwareBufferIdAllocator\s*"),
    r"""
use G17PHardwareBufferIdError::*;

/// One retained storage: takes its ID once, then re-acquires it per frame.
struct Storage { id: u32 }
impl Storage {
    fn build(ids: &mut G17PHardwareBufferIdAllocator) -> Self {
        let (id, _) = ids.allocate_with_token().unwrap();
        ids.reserve(id).unwrap();
        Self { id }
    }
    fn frame(&self, ids: &mut G17PHardwareBufferIdAllocator) -> u32 {
        let (id, _) = ids.acquire_reserved_with_token(self.id).unwrap();
        ids.release(id).unwrap();
        id
    }
    fn drop_storage(self, ids: &mut G17PHardwareBufferIdAllocator) { ids.unreserve(self.id); }
}

#[test]
fn the_measured_sequence_no_longer_changes_the_compositors_id() {
    let mut ids = G17PHardwareBufferIdAllocator::new();
    let gnome = Storage::build(&mut ids);            // slot 0, first render of the boot
    assert_eq!(gnome.id, 0);
    ids.release(0).unwrap();
    for _ in 0..1457 { assert_eq!(gnome.frame(&mut ids), 0); }
    let vkcube = Storage::build(&mut ids);           // slot 1, between two gnome frames
    assert_ne!(vkcube.id, 0, "a second storage must never be built on the compositor's ID");
    assert_eq!(vkcube.id, 1);
    // vkcube's render times out: its ID is never released while the slot lives.
    for _ in 0..300 { assert_eq!(gnome.frame(&mut ids), 0, "gnome must keep restaging under ID 0"); }
    // A third client while vkcube still pins ID 1.
    let third = Storage::build(&mut ids);
    assert_eq!(third.id, 2);
    ids.release(2).unwrap();
    assert_eq!(gnome.frame(&mut ids), 0);
}

#[test]
fn per_submission_allocation_reproduced_the_wedge() {
    // Documentation of the defect, using only the old operations.
    let mut ids = G17PHardwareBufferIdAllocator::new();
    let (a, _) = ids.allocate_with_token().unwrap();  // gnome's graph: 0
    ids.release(a).unwrap();
    let (b, _) = ids.allocate_with_token().unwrap();  // vkcube's graph, between frames
    assert_eq!(b, 0);                                 // built on the SAME id...
    // ...and pinned by the timeout (never released).
    let (a2, _) = ids.allocate_with_token().unwrap(); // gnome's next frame
    assert_eq!(a2, 1);
    assert_ne!(a2, a, "the restage arrives with a different ID than the graph expects -> ENOTSUPP");
}

#[test]
fn a_reserved_id_returns_to_the_free_list_only_when_its_storage_drops() {
    let mut ids = G17PHardwareBufferIdAllocator::new();
    let s = Storage::build(&mut ids);
    ids.release(s.id).unwrap();
    assert_eq!(ids.allocate(), Ok(1), "the free but reserved ID 0 must be skipped");
    ids.release(1).unwrap();
    s.drop_storage(&mut ids);
    assert_eq!(ids.allocate(), Ok(0), "an unreserved free ID is the LIFO top again");
}

#[test]
fn acquire_reserved_refcounts_an_already_held_retained_id() {
    let mut ids = G17PHardwareBufferIdAllocator::new();
    assert_eq!(ids.acquire_reserved(5), Err(NotReserved));
    let (id, _) = ids.allocate_with_token().unwrap();
    ids.reserve(id).unwrap();
    assert_eq!(ids.acquire_reserved(id), Ok(id), "a second ordered render shares the retained owner");
    ids.release(id).unwrap();
    ids.release(id).unwrap();
    assert_eq!(ids.acquire_reserved(id), Ok(id));
    assert_eq!(ids.reserve(G17P_TA_HARDWARE_BUFFER_ID_COUNT as u32), Err(InvalidId));
}

#[test]
fn reservations_neither_duplicate_nor_lose_ids() {
    let mut ids = G17PHardwareBufferIdAllocator::new();
    ids.reserve(0).unwrap();                 // reserved while free and on the stack
    let mut seen = std::collections::BTreeSet::new();
    while let Ok(id) = ids.allocate() { assert_ne!(id, 0); assert!(seen.insert(id)); }
    assert_eq!(seen.len(), G17P_TA_HARDWARE_BUFFER_ID_COUNT - 1);
    ids.unreserve(0);
    ids.unreserve(0);                        // idempotent
    assert_eq!(ids.allocate(), Ok(0));
    assert_eq!(ids.allocate(), Err(Exhausted));
    for id in 0..G17P_TA_HARDWARE_BUFFER_ID_COUNT as u32 { ids.release(id).unwrap(); }
    let mut again = std::collections::BTreeSet::new();
    while let Ok(id) = ids.allocate() { assert!(again.insert(id)); }
    assert_eq!(again.len(), G17P_TA_HARDWARE_BUFFER_ID_COUNT);
}

#[test]
fn overlapping_retained_reacquires_share_the_owner_token() {
    let mut ids = G17PHardwareBufferIdAllocator::new();
    assert_eq!(ids.allocate_with_token(), Ok((0, 0)));
    ids.reserve(0).unwrap();
    assert_eq!(ids.acquire_reserved_with_token(0), Ok((0, 0)));
    assert_eq!(ids.acquire_reserved_with_token(0), Ok((0, 0)));
    ids.release(0).unwrap();
    ids.release(0).unwrap();
    ids.release(0).unwrap();
    assert_eq!(ids.acquire_reserved_with_token(0), Ok((0, 1)));
    assert_eq!(ids.next_parameter_buffer_token, 2);
}
""",
])
with tempfile.TemporaryDirectory(prefix="g17-hwbuf-id-") as tmp:
    binary = Path(tmp) / "tests"
    subprocess.run(["rustc", "-Awarnings", "--edition=2021", "--test", "-", "-o", str(binary)],
                   input=body, text=True, check=True)
    subprocess.run([str(binary), "--test-threads=1"], check=True)
print("PASS: retained render storages own their TA HardwareBufferID; a firmware-retired timeout keeps its slot")
