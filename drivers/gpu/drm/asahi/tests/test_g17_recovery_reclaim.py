#!/usr/bin/env python3
"""v93r g17p_recovery_path: compile the production reclaim, drain predicate and
control-ring shadow against deterministic mocks and run their unit tests."""

from pathlib import Path
import os
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
live = Path(os.environ.get("NEO_RECLAIM_LIVE", root / "g17_live_boot.rs")).read_text()
manager = (root / "g17_manager.rs").read_text()
compute = (root / "g17_compute.rs").read_text()
recovery = (root / "g17_recovery_path.rs").read_text()


def item(source, prefix):
    start = source.index(prefix)
    opening = source.index("{", start)
    depth = 1
    end = opening + 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


def between(source, first, last):
    return source[source.index(first):source.index(last)]


# The reclaim/drain sections of g17_recovery_path.rs, verbatim.
rp_core = between(recovery, "pub(crate) const RECLAIM_DRAINED", "// ---- control-ring shadow")
rp_shadow = between(recovery, "const CONTROL_SLOTS", "// ---- parameter")
rp_parse = item(recovery, "fn parse_mask(")

common = r'''
#![allow(dead_code, unused_imports, unused_variables, unused_mut, unused_macros)]
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
type Error = i32;
type Result<T = ()> = std::result::Result<T, Error>;
const ENODEV: Error = 19;
const EAGAIN: Error = 11;
const EIO: Error = 5;
macro_rules! dev_warn { ($($t:tt)*) => {{}} }
mod g17_timing_buffer {
    use std::sync::atomic::{AtomicU64, Ordering};
    pub static NOW: AtomicU64 = AtomicU64::new(1);
    pub fn arch_counter() -> u64 { NOW.load(Ordering::SeqCst) }
}
mod g17_initdata { pub const CONTROL_RING_ENTRIES: usize = 0x100; }
mod g17_recovery_path {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    pub(crate) const ALL_BITS: u32 = 7; // v96 adds bit2 GROW_REKICK
    pub(crate) static MASK: AtomicU32 = AtomicU32::new(0);
    @CORE@
    @SHADOW@
    @PARSE@
}
'''.replace("@CORE@", rp_core.replace("const DEFAULT_MASK: u32 = RECLAIM_DRAINED;\nstatic MASK: AtomicU32 = AtomicU32::new(DEFAULT_MASK);", "")
            .replace("const ALL_BITS: u32 = RECLAIM_DRAINED | CONTROL_TRACE | GROW_REKICK | STALL_WATCH | STALL_REDOORBELL\n    | STALL_FRAGMENT;", "")).replace("@SHADOW@", rp_shadow).replace("@PARSE@", "pub(crate) " + rp_parse)

# ---------------------------------------------------------------------------
# 1. Runtime reclaim over a mock manager.
runtime_program = common + r'''
mod kernel { pub mod time {
    pub struct Monotonic;
    pub struct Instant<T>(std::marker::PhantomData<T>);
    impl<T> Instant<T> { pub fn now() -> Self { Instant(std::marker::PhantomData) } }
} }
mod g17_manager {
    pub enum G17PRenderPollOutcome { Complete(()), Pending, Retry }
}
mod g17_drm {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    pub struct G17PRenderCompletion { pub slot: Option<u8>, pub released: AtomicU32 }
    impl G17PRenderCompletion {
        pub fn render_slot(&self) -> Option<u8> { self.slot }
        pub fn release_after_firmware_drain(&self) { self.released.fetch_add(1, Ordering::SeqCst); }
    }
    pub enum G17PDeferredCompletion { ReleaseAfterDrain { completion: Arc<G17PRenderCompletion> } }
}
use g17_recovery_path::ReclaimBlock;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Group { Executed, Skipped, Stuck }
#[derive(Default)]
struct Slot { drained: Option<Option<ReclaimBlock>>, groups: std::collections::VecDeque<Group>,
    flist_refs: u32, retired: Vec<(Group, &'static str)> }
struct Manager { slots: std::collections::BTreeMap<u8, Slot>, drain_error: bool }
impl Manager {
    fn ensure_render_slot(&mut self, slot: u8) -> Result { self.slots.get(&slot).map(|_| ()).ok_or(ENODEV) }
    fn render_slot_firmware_drained(&mut self, slot: u8) -> Result<Option<ReclaimBlock>> {
        if self.drain_error { return Err(EIO); }
        Ok(self.slots[&slot].drained.unwrap_or(None))
    }
    fn render_slot_tracked(&self, slot: u8) -> Result<bool> { Ok(!self.slots[&slot].groups.is_empty()) }
    fn poll_translated_render_completion(&mut self, slot: u8) -> Result<g17_manager::G17PRenderPollOutcome> {
        let s = self.slots.get_mut(&slot).unwrap();
        match s.groups.front().copied().ok_or(ENODEV)? {
            Group::Executed => { s.groups.pop_front(); s.retired.push((Group::Executed, "paired")); s.flist_refs -= 1;
                Ok(g17_manager::G17PRenderPollOutcome::Complete(())) }
            _ => Ok(g17_manager::G17PRenderPollOutcome::Pending),
        }
    }
    fn retire_abandoned_render_if_firmware_retired(&mut self, slot: u8, accept_executed: bool) -> Result<bool> {
        assert!(accept_executed, "reclaim must accept the recovery-skipped, TA-executed group");
        let s = self.slots.get_mut(&slot).unwrap();
        match s.groups.front().copied().ok_or(ENODEV)? {
            Group::Skipped => { s.groups.pop_front(); s.retired.push((Group::Skipped, "abandoned")); s.flist_refs -= 1; Ok(true) }
            _ => Ok(false),
        }
    }
    fn retire_render_usc_freelist_at_completion(&mut self, slot: u8) -> Result<Option<u8>> {
        Ok((self.slots[&slot].flist_refs == 0).then_some(slot))
    }
}
struct Runtime {
    quarantined_render_completions: Vec<Arc<g17_drm::G17PRenderCompletion>>,
    reclaim_pending_since: Option<kernel::time::Instant<kernel::time::Monotonic>>,
    manager: Option<Manager>,
    pdev: Option<()>,
    crashed: bool,
    recovery: bool,
    deferred: Vec<g17_drm::G17PDeferredCompletion>,
    serviced: u32,
    releases: Vec<u8>,
    release_result: Result,
    service_effect: u8,
    release_invalidates_drain: bool,
}
impl Runtime {
    fn is_crashed(&self) -> bool { self.crashed }
    fn primary_recovery_pending(&self) -> bool { self.recovery }
    fn service_firmware_events_inline(&mut self) -> Result<u32> {
        self.serviced += 1;
        match self.service_effect {
            1 => return Err(EIO),
            2 => self.recovery = true,
            3 => self.crashed = true,
            4 => for s in self.manager.as_mut().unwrap().slots.values_mut() {
                s.drained = Some(Some(ReclaimBlock::Stamps));
            },
            _ => {},
        }
        Ok(0)
    }
    fn service_render_usc_freelist_release(&mut self, release: u8) -> Result {
        self.releases.push(release);
        if self.release_invalidates_drain {
            // Firmware can enter recovery while the host publishes a control
            // message; model its drain/handshake read returning blocked.
            for s in self.manager.as_mut().unwrap().slots.values_mut() {
                s.drained = Some(Some(ReclaimBlock::Recovery));
            }
        }
        self.release_result
    }
    fn defer_completion(&mut self, completion: g17_drm::G17PDeferredCompletion) { self.deferred.push(completion); }
    fn run_deferred(&mut self) {
        for d in self.deferred.drain(..) {
            match d { g17_drm::G17PDeferredCompletion::ReleaseAfterDrain { completion } => completion.release_after_firmware_drain() }
        }
    }
    @RECLAIM@
    @RECLAIM_SLOT@
}
fn completion(slot: Option<u8>) -> Arc<g17_drm::G17PRenderCompletion> {
    Arc::new(g17_drm::G17PRenderCompletion { slot, released: AtomicU32::new(0) })
}
static MASK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn fixture(mask: u32, groups: &[(u8, &[Group], Option<ReclaimBlock>)]) -> (Runtime, std::sync::MutexGuard<'static, ()>) {
    let guard = MASK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    g17_recovery_path::MASK.store(mask, Ordering::SeqCst);
    let mut slots = std::collections::BTreeMap::new();
    for (slot, g, drained) in groups {
        slots.insert(*slot, Slot { drained: Some(*drained), groups: g.iter().copied().collect(),
            flist_refs: g.len() as u32, retired: vec![] });
    }
    (Runtime { quarantined_render_completions: vec![], reclaim_pending_since: None,
        manager: Some(Manager { slots, drain_error: false }), pdev: None, crashed: false, recovery: false,
        deferred: vec![], serviced: 0, releases: vec![], release_result: Ok(()),
        service_effect: 0, release_invalidates_drain: false }, guard)
}
fn released(c: &Arc<g17_drm::G17PRenderCompletion>) -> u32 { c.released.load(Ordering::SeqCst) }
const HANG: &[Group] = &[Group::Skipped, Group::Executed, Group::Executed, Group::Executed];

#[test]
fn bit_clear_is_exactly_v93f() {
    let (mut rt, _g) = fixture(0, &[(7, HANG, None)]);
    let c = completion(Some(7));
    rt.quarantined_render_completions.push(c.clone());
    rt.reclaim_drained_quarantined_render_slots();
    rt.run_deferred();
    assert_eq!(released(&c), 0);
    assert_eq!(rt.quarantined_render_completions.len(), 1);
    assert_eq!(rt.manager.as_ref().unwrap().slots[&7].groups.len(), 4);
    assert!(rt.reclaim_pending_since.is_none());
}
#[test]
fn open_recovery_blocks_everything() {
    let (mut rt, _g) = fixture(1, &[(7, HANG, None)]);
    let c = completion(Some(7));
    rt.quarantined_render_completions.push(c.clone());
    rt.recovery = true;
    rt.reclaim_drained_quarantined_render_slots();
    rt.run_deferred();
    assert_eq!(released(&c), 0);
    assert_eq!(rt.manager.as_ref().unwrap().slots[&7].groups.len(), 4);
    assert!(rt.reclaim_pending_since.is_some(), "pending work keeps the worker polling");
}
#[test]
fn undrained_pair_is_left_untouched() {
    for reason in [ReclaimBlock::Stamps, ReclaimBlock::Flist, ReclaimBlock::Recovery, ReclaimBlock::Preparing] {
        let (mut rt, _g) = fixture(1, &[(7, HANG, Some(reason))]);
        let c = completion(Some(7));
        rt.quarantined_render_completions.push(c.clone());
        rt.reclaim_drained_quarantined_render_slots();
        rt.run_deferred();
        assert_eq!(released(&c), 0);
        assert_eq!(rt.serviced, 0, "no event service or retirement before drain proof");
        assert_eq!(rt.manager.as_ref().unwrap().slots[&7].retired.len(), 0);
    }
}
#[test]
fn drained_pair_retires_in_order_then_releases_only_its_own_completions() {
    let (mut rt, _g) = fixture(1, &[(7, HANG, None), (8, &[Group::Executed], Some(ReclaimBlock::Stamps))]);
    let blamed = completion(Some(7));
    let queued: Vec<_> = (0..3).map(|_| completion(Some(7))).collect();
    let other = completion(Some(8));
    let unleased = completion(None);
    rt.quarantined_render_completions.push(blamed.clone());
    for q in &queued { rt.quarantined_render_completions.push(q.clone()); }
    rt.quarantined_render_completions.push(other.clone());
    rt.quarantined_render_completions.push(unleased.clone());
    rt.reclaim_drained_quarantined_render_slots();
    // Guards are released only through the deferred list (after the mutex).
    assert_eq!(released(&blamed), 0);
    rt.run_deferred();
    let m = rt.manager.as_ref().unwrap();
    assert_eq!(m.slots[&7].retired, vec![(Group::Skipped, "abandoned"), (Group::Executed, "paired"),
        (Group::Executed, "paired"), (Group::Executed, "paired")]);
    assert_eq!(rt.releases, vec![7], "FList release exactly once, after the last reference");
    assert_eq!(rt.serviced, 1, "event service once before FList retirement");
    assert_eq!(released(&blamed), 1);
    for q in &queued { assert_eq!(released(q), 1); }
    assert_eq!(released(&other), 0);
    assert_eq!(released(&unleased), 0);
    assert_eq!(rt.quarantined_render_completions.len(), 2);
    assert_eq!(m.slots[&8].groups.len(), 1);
    assert!(rt.reclaim_pending_since.is_some());
}
#[test]
fn a_group_not_yet_retired_stops_the_pair_and_keeps_its_guards() {
    let (mut rt, _g) = fixture(1, &[(7, &[Group::Skipped, Group::Stuck, Group::Executed], None)]);
    let c = completion(Some(7));
    rt.quarantined_render_completions.push(c.clone());
    rt.reclaim_drained_quarantined_render_slots();
    rt.run_deferred();
    assert_eq!(released(&c), 0);
    let m = rt.manager.as_ref().unwrap();
    assert_eq!(m.slots[&7].retired.len(), 1);
    assert_eq!(m.slots[&7].groups.len(), 2);
    assert!(rt.releases.is_empty());
    // A later turn, once firmware finished the group, completes the reclaim.
    rt.manager.as_mut().unwrap().slots.get_mut(&7).unwrap().groups[0] = Group::Executed;
    rt.reclaim_drained_quarantined_render_slots();
    rt.run_deferred();
    assert_eq!(released(&c), 1);
    assert!(rt.quarantined_render_completions.is_empty());
    assert!(rt.reclaim_pending_since.is_none(), "nothing left to poll for");
}
#[test]
fn flist_release_backpressure_is_tolerated_but_errors_keep_the_quarantine() {
    let (mut rt, _g) = fixture(1, &[(7, &[Group::Skipped], None)]);
    let c = completion(Some(7));
    rt.quarantined_render_completions.push(c.clone());
    rt.release_result = Err(EAGAIN);
    rt.reclaim_drained_quarantined_render_slots();
    rt.run_deferred();
    assert_eq!(released(&c), 1);
    let (mut rt, _g2) = { drop(_g); fixture(1, &[(7, &[Group::Skipped], None)]) };
    let c = completion(Some(7));
    rt.quarantined_render_completions.push(c.clone());
    rt.release_result = Err(EIO);
    rt.reclaim_drained_quarantined_render_slots();
    rt.run_deferred();
    assert_eq!(released(&c), 0);
    assert_eq!(rt.quarantined_render_completions.len(), 1);
}
#[test]
fn failed_event_service_cannot_reclaim_any_pair() {
    let (mut rt, _g) = fixture(1, &[(7, HANG, None), (8, HANG, None)]);
    let a = completion(Some(7));
    let b = completion(Some(8));
    rt.quarantined_render_completions.extend([a.clone(), b.clone()]);
    rt.service_effect = 1;
    rt.reclaim_drained_quarantined_render_slots();
    rt.run_deferred();
    assert_eq!(released(&a), 0);
    assert_eq!(released(&b), 0);
    let m = rt.manager.as_ref().unwrap();
    assert_eq!(m.slots[&7].groups.len(), HANG.len());
    assert_eq!(m.slots[&8].groups.len(), HANG.len());
}
#[test]
fn rechecks_recovery_opened_by_event_service() {
    let (mut rt, _g) = fixture(1, &[(7, HANG, None)]);
    let c = completion(Some(7));
    rt.quarantined_render_completions.push(c.clone());
    rt.service_effect = 2;
    rt.reclaim_drained_quarantined_render_slots();
    rt.run_deferred();
    assert_eq!(released(&c), 0);
    assert_eq!(rt.manager.as_ref().unwrap().slots[&7].groups.len(), HANG.len());
}
#[test]
fn rechecks_session_failure_during_event_service() {
    let (mut rt, _g) = fixture(1, &[(7, HANG, None)]);
    let c = completion(Some(7));
    rt.quarantined_render_completions.push(c.clone());
    rt.service_effect = 3;
    rt.reclaim_drained_quarantined_render_slots();
    rt.run_deferred();
    assert_eq!(released(&c), 0);
    assert_eq!(rt.manager.as_ref().unwrap().slots[&7].groups.len(), HANG.len());
}
#[test]
fn rechecks_drain_proof_after_event_service() {
    let (mut rt, _g) = fixture(1, &[(7, HANG, None)]);
    let c = completion(Some(7));
    rt.quarantined_render_completions.push(c.clone());
    rt.service_effect = 4;
    rt.reclaim_drained_quarantined_render_slots();
    rt.run_deferred();
    assert_eq!(released(&c), 0);
    assert_eq!(rt.manager.as_ref().unwrap().slots[&7].groups.len(), HANG.len());
}
#[test]
fn flist_service_cannot_invalidate_the_final_reclaim_proof() {
    let (mut rt, _g) = fixture(1, &[(7, HANG, None)]);
    let c = completion(Some(7));
    rt.quarantined_render_completions.push(c.clone());
    rt.release_invalidates_drain = true;
    rt.reclaim_drained_quarantined_render_slots();
    rt.run_deferred();
    assert_eq!(released(&c), 0);
    // The retired trackers stay retired; only releasing their guards waits.
    assert!(rt.manager.as_ref().unwrap().slots[&7].groups.is_empty());
    rt.manager.as_mut().unwrap().slots.get_mut(&7).unwrap().drained = Some(None);
    rt.release_invalidates_drain = false;
    rt.reclaim_drained_quarantined_render_slots();
    rt.run_deferred();
    assert_eq!(released(&c), 1);
}
#[test]
fn predicate_errors_keep_the_quarantine() {
    let (mut rt, _g) = fixture(1, &[(7, HANG, None)]);
    let c = completion(Some(7));
    rt.quarantined_render_completions.push(c.clone());
    rt.manager.as_mut().unwrap().drain_error = true;
    rt.reclaim_drained_quarantined_render_slots();
    rt.run_deferred();
    assert_eq!(released(&c), 0);
    assert_eq!(rt.manager.as_ref().unwrap().slots[&7].groups.len(), 4);
}
'''.replace("@RECLAIM@", item(live, "fn reclaim_drained_quarantined_render_slots(")).replace(
    "@RECLAIM_SLOT@", item(live, "fn reclaim_drained_render_slot("))

# ---------------------------------------------------------------------------
# 2. Manager drain predicate over mock pair state, with the real stamp helpers.
drain_program = common + r'''
mod g17_compute {
    @STAMP_VALUE@
    @STAMP_COVERS@
}
use g17_recovery_path::ReclaimBlock;
#[derive(Clone, Copy)] struct Pair { tiling: u16, fragment: u16 }
struct Storage { counters: [u64; 2] }
impl Storage {
    fn queue_pair(&self) -> Pair { Pair { tiling: 18, fragment: 19 } }
    fn render_flist_completion_counters(&self) -> Result<[u64; 2]> { Ok(self.counters) }
}
struct SlotState { preparing: bool, submission_ordinal: u64, storage: Option<Box<Storage>> }
struct Slots { slot: SlotState }
impl Slots { fn allocated(&self, _: u8) -> Result<&SlotState> { Ok(&self.slot) } }
struct Resources { stamps: [u32; 2] }
impl Resources { fn render_completion_stamps(&mut self, _: Pair) -> Result<[u32; 2]> { Ok(self.stamps) } }
struct Handshake { state: u32 }
struct Manager { render_slots: Slots, resources: Resources, handshake: u32 }
impl Manager {
    fn firmware_recovery_handshake_snapshot(&mut self) -> Result<Handshake> { Ok(Handshake { state: self.handshake }) }
    @DRAINED@
}
fn manager(ordinal: u64, stamps: [u32; 2], counters: [u64; 2]) -> Manager {
    Manager { render_slots: Slots { slot: SlotState { preparing: false, submission_ordinal: ordinal,
        storage: Some(Box::new(Storage { counters })) } }, resources: Resources { stamps }, handshake: 0 }
}
fn stamp(ordinal_plus_one: u64) -> u32 { g17_compute::completion_stamp_value(ordinal_plus_one) }
#[test]
fn drained_needs_both_engines_past_the_last_publication_and_equal_flist_counters() {
    // 21:03 shape: last published ordinal 288341 -> kick timestamp 288342.
    let last = 288342;
    assert_eq!(manager(last, [stamp(last), stamp(last)], [0x9aa, 0x9aa]).render_slot_firmware_drained(7), Ok(None));
    assert_eq!(manager(last, [stamp(last), stamp(last - 1)], [0x9aa, 0x9aa]).render_slot_firmware_drained(7),
        Ok(Some(ReclaimBlock::Stamps)), "the 3D of the last publication is not retired");
    assert_eq!(manager(last, [stamp(last - 1), stamp(last)], [0x9aa, 0x9aa]).render_slot_firmware_drained(7),
        Ok(Some(ReclaimBlock::Stamps)));
    assert_eq!(manager(last, [stamp(last), stamp(last)], [0x9aa, 0x995]).render_slot_firmware_drained(7),
        Ok(Some(ReclaimBlock::Flist)), "fault-time FList counters");
    let mut m = manager(last, [stamp(last), stamp(last)], [0x9aa, 0x9aa]);
    m.handshake = 1;
    assert_eq!(m.render_slot_firmware_drained(7), Ok(Some(ReclaimBlock::Recovery)));
}
#[test]
fn later_stamps_cover_and_wrap_is_modular() {
    let last = 0x00ff_ffff;
    assert_eq!(manager(last, [stamp(last + 3), stamp(last + 1)], [4, 4]).render_slot_firmware_drained(7), Ok(None));
}
#[test]
fn preparing_and_never_published_pairs() {
    let mut m = manager(5, [0, 0], [1, 0]);
    m.render_slots.slot.preparing = true;
    assert_eq!(m.render_slot_firmware_drained(7), Ok(Some(ReclaimBlock::Preparing)));
    assert_eq!(manager(0, [0, 0], [1, 0]).render_slot_firmware_drained(7), Ok(None));
}
'''.replace("@STAMP_VALUE@", item(compute, "pub(crate) const fn completion_stamp_value(")).replace(
    "@STAMP_COVERS@", item(compute, "pub(crate) const fn completion_stamp_covers(")).replace(
    "@DRAINED@", item(manager, "pub(crate) fn render_slot_firmware_drained("))

# ---------------------------------------------------------------------------
# 3. Control-ring shadow and parameter parsing.
control_program = common + r'''
use g17_recovery_path as rp;
static MASK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn at(t: u64) { g17_timing_buffer::NOW.store(t, Ordering::SeqCst); }
#[test]
fn off_records_nothing() {
    let _g = MASK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    rp::MASK.store(0, Ordering::SeqCst);
    at(10); rp::note_control_publish(3, 0x0a, 1); rp::note_control_announce();
    assert_eq!(rp::describe_pending(3), None);
}
#[test]
fn names_the_pending_record_and_whether_it_was_announced() {
    let _g = MASK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    rp::MASK.store(rp::CONTROL_TRACE, Ordering::SeqCst);
    at(100); rp::note_control_publish(0x101, 0x2e, 7);      // wraps to slot 1
    at(150); rp::note_control_announce();
    at(1150);
    assert_eq!(rp::describe_pending(1), Some((0x2e, 7, 1050, true)));
    at(2000); rp::note_control_publish(2, 0x14, 0);         // published, never announced
    at(3000);
    assert_eq!(rp::describe_pending(2), Some((0x14, 0, 1000, false)));
    rp::MASK.store(0, Ordering::SeqCst);
}
#[test]
fn parameter_accepts_only_known_bits() {
    assert_eq!(rp::parse_mask("0x3"), Some(3));
    assert_eq!(rp::parse_mask("1"), Some(1));
    assert_eq!(rp::parse_mask("4"), Some(4));
    assert_eq!(rp::parse_mask("0x7"), Some(7));
    assert_eq!(rp::parse_mask("8"), None);
    assert_eq!(rp::parse_mask("zz"), None);
}
'''

tmp = os.environ.get("TMPDIR")
with tempfile.TemporaryDirectory(prefix="g17p-reclaim-", dir=tmp) as directory:
    path = Path(directory)
    for name, program in [("runtime", runtime_program), ("drain", drain_program), ("control", control_program)]:
        source = path / f"{name}.rs"
        source.write_text(program)
        subprocess.run(["rustc", "--edition=2021", "--test", str(source), "-o", str(path / name)], check=True)
        subprocess.run([str(path / name)], check=True)
print("PASS v93r recovery reclaim: drain proof gates retirement; guards released after the mutex; control shadow")
