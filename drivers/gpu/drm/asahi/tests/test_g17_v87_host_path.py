#!/usr/bin/env python3
"""v87 host-path changes, tested on the actual source text.

Every function under test is extracted verbatim from the driver source and
compiled with host mocks (rustc --test), in the style of
test_g17_vm_status.py. Covered:

  * EARLY_SHALLOW gate (g17_host_path) together with the real
    `prune_render_frontier` (g17_drm): depth 1/2 admit, depth 3+ decline,
    bit0 alone keeps v84 behaviour, dependencies always decline.
  * RING_CACHE decision/revalidation cadence and key invalidation.
  * VM-status mirror (g17_status): attach/record ordering, sticky first
    error, no regression to 0 under concurrent attach/record, request decode.
  * OBS_SKIP (g17_live_boot): `render_observation_possible() == false`
    implies the real `next_render_observation(0)` returns None, over an
    exhaustive small state space.
  * SETTLE_ONCE (g17_live_boot): one gate test per hold when enabled, every
    call when disabled, failures never mark the hold settled.
  * STATE_WORD (g17_resources): `firmware_recovery_state` returns the same
    state and the same address validation as the three-word snapshot, with
    one live read instead of three.
  * WORKER_TAIL (driver.rs): the merged decision equals the original
    back-pressure / recovery / poll / requeue chain for every input.
"""
from pathlib import Path
import re, subprocess, tempfile
from g17_recovery_isolation_test_support import recovery_isolation_mod

p = Path(__file__).resolve().parents[1]


def item(text, needle):
    start = text.index(needle)
    op = text.index('{', start)
    depth = 1
    i = op + 1
    while depth:
        depth += (text[i] == '{') - (text[i] == '}')
        i += 1
    return text[start:i]


def line(text, needle):
    start = text.index(needle)
    return text[start:text.index('\n', start)]


hp = (p / 'g17_host_path.rs').read_text()
host_path = '\n'.join([
    'use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};',
    *(line(hp, f'pub(crate) const {n}: u32') for n in [
        'EARLY_PUBLISH', 'FEED_PERF', 'EARLY_SHALLOW', 'STATE_WORD', 'SETTLE_ONCE',
        'RING_CACHE', 'WORKER_TAIL', 'OBS_SKIP', 'VM_STATUS_MIRROR']),
    'pub(crate) static MASK: AtomicU32 = AtomicU32::new(0);',
    'pub(crate) ' + line(hp, 'static EARLY_DEPTH: AtomicU32'),
    line(hp, 'pub(crate) const RING_REVALIDATE: u32'),
    *(line(hp, f'pub(crate) static {n}: AtomicU64') for n in [
        'EARLY_TAKEN', 'SETTLE_SKIPPED', 'RING_CACHE_HITS', 'TAIL_MERGED', 'OBS_SKIPPED', 'MIRRORS']),
    item(hp, 'pub(crate) fn enabled(bit: u32)'),
    item(hp, 'pub(crate) fn count(counter: &AtomicU64)'),
    item(hp, 'pub(crate) const fn early_shallow_admits('),
    item(hp, 'pub(crate) fn early_publish_allowed('),
    item(hp, 'pub(crate) fn ring_cache_hit('),
    item(hp, 'pub(crate) fn ring_checked('),
])

drm = (p / 'g17_drm.rs').read_text()
prune = item(drm, '    fn prune_render_frontier(&mut self)')

status = (p / 'g17_status.rs').read_text().replace('//!', '//')

lb = (p / 'g17_live_boot.rs').read_text()
possible = item(lb, '    pub(crate) fn render_observation_possible(&self)')
next_obs = item(lb, '    pub(crate) fn next_render_observation(&mut self, start: u8)')
settle_once = item(lb, '    fn settle_open_recovery_once(&mut self, settled: &mut bool)')

comp = (p / 'g17_completion.rs').read_text()
type1 = item(comp, 'pub(crate) const fn g17p_render_type1_complete(')

res = (p / 'g17_resources.rs').read_text()
state_word = item(res, '    pub(crate) fn firmware_recovery_state(&mut self)')
snapshot = item(res, '    pub(crate) fn firmware_recovery_handshake_snapshot(\n')
snap_struct = item(res, 'pub(crate) struct G17PFirmwareRecoveryHandshakeSnapshot {')
snap_impl = item(res, 'impl G17PFirmwareRecoveryHandshakeSnapshot {')
consts = '\n'.join(line(res, f'const {n}: u64') for n in [
    'PRIMARY_STATUS_A_POWER_CALLBACK_COUNT_OFFSET',
    'PRIMARY_STATUS_B_FIRMWARE_RECOVERY_EPOCH_OFFSET',
    'PRIMARY_STATUS_B_FIRMWARE_RECOVERY_STATE_OFFSET'])
grid = line((p / 'g17_initdata.rs').read_text(), 'pub(crate) const PRIMARY_STATUS_B_STATE_GRID_OFFSET')

drv = (p / 'driver.rs').read_text()
tail_enum = item(drv, 'pub(crate) enum G17PWorkerTailNext {')
tail_next = item(drv, 'pub(crate) const fn g17p_worker_tail_next(')

mock = r'''
#![allow(dead_code, unused_imports, unused_variables, unused_mut, non_upper_case_globals)]
extern crate self as kernel;
pub type Result<T = ()> = core::result::Result<T, i32>;
pub const EINVAL: i32 = -22; pub const EBUSY: i32 = -16; pub const ENODEV: i32 = -19;
pub mod prelude { pub use crate::{Errno, Result, EINVAL, EBUSY, ENODEV}; }
pub mod error { pub mod code { pub const EIO: i32 = -5; pub const ETIMEDOUT: i32 = -110; pub const ENODEV: i32 = -19; } }
pub trait Errno { fn to_errno(&self) -> i32; }
impl Errno for i32 { fn to_errno(&self) -> i32 { *self } }
macro_rules! pr_err { ($($t:tt)*) => {} }
macro_rules! dev_err { ($($t:tt)*) => {} }
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub mod g17_host_path { @@HOST_PATH@@ }

pub mod g17_timing_buffer { pub fn record_render_gate(_: u8, _: u64, _: u32, _: u32, _: [u32; 2], _: [u64; 4]) {} }
pub mod g17_completion { @@TYPE1@@ }
pub mod g17_initdata { @@GRID@@ }
pub mod g17_resources {
    pub struct G17PRenderQueuePair;
    impl G17PRenderQueuePair { pub const SLOT_COUNT: u8 = 4; }
}
pub mod g17_manager { #[derive(Debug, PartialEq)] pub struct G17PRenderObservation(pub u8); }
pub mod g17_status { @@STATUS@@ }
@@RECOVERY_ISOLATION@@

// ---- prune_render_frontier + EARLY_SHALLOW -------------------------------
pub mod bindings {
    pub unsafe fn dma_fence_get_status(fence: *const super::MockFence) -> i32 { unsafe { (*fence).0 } }
}
pub struct MockFence(pub i32);
impl MockFence { fn raw(&self) -> *const MockFence { self } }
pub struct CommandFences { completed: MockFence }
pub struct Queue { render_frontier: Vec<(u64, CommandFences)> }
impl Queue { @@PRUNE@@ }

// ---- live runtime mock for OBS_SKIP and settle-once ---------------------
pub struct FenceState { pending: u64 }
impl FenceState { fn pending(&self, slot: u8) -> bool { self.pending & (1 << slot) != 0 } }
pub struct Manager { expected: [[u64; 2]; 4], takeable: [bool; 4], mask_error: Option<u8> }
impl Manager {
    fn render_completion_masks_for_slot(&self, slot: u8) -> Result<[u64; 2]> {
        if self.mask_error == Some(slot) { return Err(EINVAL); }
        Ok(self.expected[slot as usize])
    }
    fn take_render_observation(&mut self, slot: u8) -> Result<Option<g17_manager::G17PRenderObservation>> {
        Ok(self.takeable[slot as usize].then(|| g17_manager::G17PRenderObservation(slot)))
    }
}
pub struct Runtime {
    manager: Option<Manager>, render_fence_state: FenceState,
    render_event_masks: [[u64; 2]; 4], crashed: bool, recovery: bool,
    settles: u32, settle_result: Result,
}
impl Runtime {
    fn is_crashed(&self) -> bool { self.crashed }
    fn primary_recovery_pending(&self) -> bool { self.recovery }
    fn settle_open_recovery_before_publication(&mut self) -> Result { self.settles += 1; self.settle_result }
    @@POSSIBLE@@
    @@NEXT_OBS@@
    @@SETTLE_ONCE@@
}

// ---- STATE_WORD ----------------------------------------------------------
@@CONSTS@@
@@SNAP_STRUCT@@
@@SNAP_IMPL@@
pub struct Mem { base: u64, words: std::collections::BTreeMap<u64, u64>, reads: u32 }
impl Mem {
    fn iova(&self) -> u64 { self.base }
    fn read_u64(&mut self, a: u64) -> Result<u64> { self.reads += 1; self.words.get(&a).copied().ok_or(EINVAL) }
    fn read_live_u32(&mut self, a: u64) -> Result<u32> { self.reads += 1; self.words.get(&a).map(|v| *v as u32).ok_or(EINVAL) }
}
pub struct Instance { status_b: u64, status_a: u64 }
pub struct Primary { instance: Instance, state_grid: u64 }
pub struct Handoff { primary: Primary }
pub struct Mapped { primary_state: Mem }
pub struct Owner { handoff: Handoff, resources: Mapped }
impl Owner {
    @@STATE_WORD_FN@@
    @@SNAPSHOT_FN@@
}

// ---- WORKER_TAIL ---------------------------------------------------------
@@TAIL_ENUM@@
@@TAIL_NEXT@@
'''
for key, value in {
    'HOST_PATH': host_path, 'TYPE1': type1, 'GRID': grid, 'STATUS': status,
    'RECOVERY_ISOLATION': recovery_isolation_mod(),
    'PRUNE': prune, 'POSSIBLE': possible, 'NEXT_OBS': next_obs, 'SETTLE_ONCE': settle_once,
    'CONSTS': consts, 'SNAP_STRUCT': snap_struct, 'SNAP_IMPL': snap_impl,
    'STATE_WORD_FN': state_word, 'SNAPSHOT_FN': snapshot,
    'TAIL_ENUM': '#[derive(Copy, Clone, PartialEq, Eq, Debug)]\n' + tail_enum, 'TAIL_NEXT': tail_next,
}.items():
    mock = mock.replace('@@' + key + '@@', value, 1)

tests = r'''
#[cfg(test)]
mod tests {
    use super::*;
    use g17_host_path as hp;
    use std::sync::Arc;

    fn set_mask(mask: u32) { hp::MASK.store(mask, Ordering::Relaxed); }
    fn queue(statuses: &[i32]) -> Queue {
        Queue { render_frontier: statuses.iter().enumerate()
            .map(|(i, s)| (i as u64, CommandFences { completed: MockFence(*s) })).collect() }
    }

    #[test]
    fn early_shallow_depth_gate_uses_pruned_frontier() {
        let _g = LOCK.lock().unwrap();
        set_mask(hp::EARLY_SHALLOW);
        hp::EARLY_DEPTH.store(2, Ordering::Relaxed);
        // d1: previous render signalled (1) or failed (<0) is pruned -> admit.
        for statuses in [&[][..], &[1][..], &[-5][..], &[1, 1, 1][..]] {
            let mut q = queue(statuses); q.prune_render_frontier();
            assert_eq!(q.render_frontier.len(), 0);
            assert!(hp::early_publish_allowed(q.render_frontier.len(), false));
        }
        // d2: one other slot in flight -> admit.
        let mut q = queue(&[1, 0]); q.prune_render_frontier();
        assert!(hp::early_publish_allowed(q.render_frontier.len(), false));
        // d3 and deeper: the v84 scheduler path.
        for n in 2..40 {
            let mut q = queue(&vec![0; n]); q.prune_render_frontier();
            assert!(!hp::early_publish_allowed(q.render_frontier.len(), false), "depth {}", n + 1);
        }
        // Dependencies always decline.
        assert!(!hp::early_publish_allowed(0, true));
        // Depth knob.
        hp::EARLY_DEPTH.store(1, Ordering::Relaxed);
        assert!(hp::early_publish_allowed(0, false));
        assert!(!hp::early_publish_allowed(1, false));
        hp::EARLY_DEPTH.store(2, Ordering::Relaxed);
    }

    #[test]
    fn early_publish_mask_semantics() {
        let _g = LOCK.lock().unwrap();
        hp::EARLY_DEPTH.store(2, Ordering::Relaxed);
        set_mask(0);                 // v84-final default: never early
        for n in 0..40 { assert!(!hp::early_publish_allowed(n, false)); }
        set_mask(hp::EARLY_PUBLISH); // v84 bit0: early at any depth
        for n in 0..40 { assert!(hp::early_publish_allowed(n, false)); assert!(!hp::early_publish_allowed(n, true)); }
        set_mask(hp::EARLY_PUBLISH | hp::EARLY_SHALLOW);
        assert!(hp::early_publish_allowed(30, false));
        set_mask(0);
    }

    #[test]
    fn ring_cache_revalidates_and_invalidates() {
        let mut e = (0u64, 0u64, 0u32);
        let k = (0x1000, 0x2000);
        assert!(!hp::ring_cache_hit(&mut e, k, true));           // cold: full check
        hp::ring_checked(&mut e, k);
        for _ in 0..hp::RING_REVALIDATE { assert!(hp::ring_cache_hit(&mut e, k, true)); }
        assert!(!hp::ring_cache_hit(&mut e, k, true));           // budget spent: full check
        hp::ring_checked(&mut e, k);
        assert!(!hp::ring_cache_hit(&mut e, (0x1000, 0x2008), true)); // derivation changed
        assert!(!hp::ring_cache_hit(&mut e, (0x1008, 0x2000), true));
        assert!(!hp::ring_cache_hit(&mut e, k, false));          // knob off: always check
        assert!(hp::ring_cache_hit(&mut e, k, true));
    }

    #[test]
    fn mirror_attach_before_and_after_error() {
        let mut word = Box::new(123i32);
        let s = g17_status::VmStatus::new();
        s.attach_mirror((), &mut *word).unwrap();
        assert_eq!(*word, 0);                         // zeroed at attach
        assert_eq!(s.attach_mirror((), &mut *word), Err(EBUSY));
        s.record(-110); assert_eq!(*word, -110);
        s.record(-5);   assert_eq!(*word, -110);      // first error sticks
        let mut late = Box::new(77i32);
        let t = g17_status::VmStatus::new();
        t.record(-5);
        t.attach_mirror((), &mut *late).unwrap();
        assert_eq!(*late, -5);                        // error before attach
        let mut unaligned = [0u8; 8];
        let u = g17_status::VmStatus::new();
        assert_eq!(u.attach_mirror((), unaligned.as_mut_ptr().wrapping_add(1).cast()), Err(EINVAL));
        assert_eq!(u.attach_mirror((), std::ptr::null_mut()), Err(EINVAL));
        s.record_if_device_loss(-16);                 // admission error: no change
        assert_eq!(*word, -110);
    }

    #[test]
    fn mirror_concurrent_attach_and_record_never_regresses() {
        for round in 0..2000 {
            let s = Arc::new(g17_status::VmStatus::new());
            let word: &'static std::sync::atomic::AtomicI32 = Box::leak(Box::new(std::sync::atomic::AtomicI32::new(99)));
            let ptr = word as *const _ as *mut i32 as usize;
            let a = { let s = s.clone(); std::thread::spawn(move || s.attach_mirror((), ptr as *mut i32).unwrap()) };
            let r = { let s = s.clone(); std::thread::spawn(move || s.record(-(1 + (round % 7)))) };
            a.join().unwrap(); r.join().unwrap();
            assert_eq!(word.load(Ordering::SeqCst), s.get(), "round {}", round);
            assert_ne!(word.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn mirror_request_decode() {
        let req = |w: [u32; 4]| { let mut b = [0u8; 16]; for i in 0..4 { b[i*4..i*4+4].copy_from_slice(&w[i].to_ne_bytes()); } b };
        assert_eq!(g17_status::decode_mirror_request(&req([7, 3, 8, 0])), Some((7, 3, 8)));
        assert_eq!(g17_status::decode_mirror_request(&req([7, 3, 6, 0])), None); // unaligned
        assert_eq!(g17_status::decode_mirror_request(&req([7, 3, 8, 1])), None); // flags
        assert_eq!(g17_status::decode_mirror_request(&req([7, 0, 8, 0])), None); // no handle
    }

    fn runtime(pending: u64, masks: [[u64; 2]; 4], expected: [[u64; 2]; 4], takeable: [bool; 4],
               crashed: bool, recovery: bool) -> Runtime {
        Runtime { manager: Some(Manager { expected, takeable, mask_error: None }),
            render_fence_state: FenceState { pending }, render_event_masks: masks,
            crashed, recovery, settles: 0, settle_result: Ok(()) }
    }

    #[test]
    fn obs_skip_never_hides_an_observation() {
        let exp = [[0b01, 0b10], [0b100, 0], [0, 0b1000], [0b11, 0b11]];
        let choices = [[0u64, 0u64], [0b01, 0], [0b01, 0b10], [0b111, 0], [0, 0b1000], [0b11, 0b11]];
        let mut checked = 0;
        for pending in 0..16u64 {
            for m in 0..choices.len().pow(4) {
                let mut masks = [[0; 2]; 4];
                let mut x = m;
                for s in 0..4 { masks[s] = choices[x % choices.len()]; x /= choices.len(); }
                for take in 0..16u32 {
                    let takeable = [take & 1 != 0, take & 2 != 0, take & 4 != 0, take & 8 != 0];
                    for flags in 0..4 {
                        let mut rt = runtime(pending, masks, exp, takeable, flags & 1 != 0, flags & 2 != 0);
                        let possible = rt.render_observation_possible();
                        let next = rt.next_render_observation(0).unwrap();
                        if !possible { assert_eq!(next, None); }
                        // Liveness: a takeable complete slot is never skipped.
                        if next.is_some() { assert!(possible); }
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 1_000_000);
        // A mask lookup error keeps the loop (which reports it).
        let mut rt = runtime(1, [[0; 2]; 4], exp, [true; 4], false, false);
        rt.manager.as_mut().unwrap().mask_error = Some(0);
        assert!(rt.render_observation_possible());
        assert!(rt.next_render_observation(0).is_err());
    }

    #[test]
    fn settle_once_per_hold() {
        let _g = LOCK.lock().unwrap();
        let mut rt = runtime(0, [[0; 2]; 4], [[0; 2]; 4], [false; 4], false, false);
        set_mask(hp::SETTLE_ONCE);
        let mut settled = false;
        for _ in 0..3 { rt.settle_open_recovery_once(&mut settled).unwrap(); }
        assert_eq!(rt.settles, 1);
        settled = false;                      // caller relocked Runtime
        rt.settle_open_recovery_once(&mut settled).unwrap();
        assert_eq!(rt.settles, 2);
        set_mask(0);                          // knob off: every helper tests
        let mut settled = false;
        for _ in 0..3 { rt.settle_open_recovery_once(&mut settled).unwrap(); }
        assert_eq!(rt.settles, 5);
        set_mask(hp::SETTLE_ONCE);            // failure never marks the hold
        rt.settle_result = Err(-11);
        let mut settled = false;
        assert!(rt.settle_open_recovery_once(&mut settled).is_err());
        assert!(!settled);
        rt.settle_result = Ok(());
        rt.settle_open_recovery_once(&mut settled).unwrap();
        assert_eq!(rt.settles, 7);
        set_mask(0);
    }

    fn owner(status_b: u64, grid: u64, state: u32) -> Owner {
        let mut words = std::collections::BTreeMap::new();
        words.insert(status_b + PRIMARY_STATUS_B_FIRMWARE_RECOVERY_EPOCH_OFFSET, 0x55);
        words.insert(status_b + PRIMARY_STATUS_B_FIRMWARE_RECOVERY_STATE_OFFSET, u64::from(state));
        words.insert(0x9000 + PRIMARY_STATUS_A_POWER_CALLBACK_COUNT_OFFSET, 3);
        Owner { handoff: Handoff { primary: Primary { instance: Instance { status_b, status_a: 0x9000 }, state_grid: grid } },
            resources: Mapped { primary_state: Mem { base: 0, words, reads: 0 } } }
    }

    #[test]
    fn state_word_matches_snapshot_with_one_read() {
        let grid = 0x10_0000u64;
        let status_b = grid + g17_initdata::PRIMARY_STATUS_B_STATE_GRID_OFFSET as u64;
        for state in [0u32, 1, 2, 3, 7, u32::MAX] {
            let mut a = owner(status_b, grid, state);
            let mut b = owner(status_b, grid, state);
            assert_eq!(a.firmware_recovery_state().unwrap(), b.firmware_recovery_handshake_snapshot().unwrap().state);
            assert_eq!(a.resources.primary_state.reads, 1);
            assert_eq!(b.resources.primary_state.reads, 3);
        }
        // Same address validation.
        let mut a = owner(status_b + 8, grid, 0);
        let mut b = owner(status_b + 8, grid, 0);
        assert_eq!(a.firmware_recovery_state(), Err(EINVAL));
        assert_eq!(b.firmware_recovery_handshake_snapshot().map(|s| s.state), Err(EINVAL));
        assert_eq!(a.resources.primary_state.reads, 0);
    }

    #[derive(Debug, PartialEq)]
    enum Act { SleepThenOld, Recovery, Poll, Requeue, Nothing }

    #[test]
    fn worker_tail_decision_matches_original_chain() {
        for bits in 0..16u32 {
            let (bp, rec, poll, pending) = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0);
            // Original WorkItem<1>::run order.
            let original = if bp { Act::SleepThenOld } else if rec { Act::Recovery }
                else if poll { Act::Poll } else if pending { Act::Requeue } else { Act::Nothing };
            let next = g17p_worker_tail_next(bp, rec, !rec && poll);
            let merged = match next {
                G17PWorkerTailNext::Backpressured => Act::SleepThenOld,
                G17PWorkerTailNext::Recovery => Act::Recovery,
                G17PWorkerTailNext::TeardownPoll => Act::Poll,
                G17PWorkerTailNext::Done => if pending { Act::Requeue } else { Act::Nothing },
            };
            assert_eq!(original, merged, "bits {:04b}", bits);
        }
    }

    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
'''

with tempfile.TemporaryDirectory() as d:
    f = Path(d) / 'v87.rs'
    f.write_text(mock + tests)
    subprocess.run(['rustc', '--edition=2021', '-O', '--test', str(f), '-o', str(Path(d) / 'test')], check=True)
    subprocess.run([str(Path(d) / 'test'), '--test-threads=4'], check=True)
