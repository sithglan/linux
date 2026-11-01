#!/usr/bin/env python3
"""v93: firmware-side same-queue input dependencies, AGG_FIRST, EARLY_DEPS,
and the default-off statistics pstate sampler.

Executed (production code extracted verbatim, mock kernel types):
  * g17_host_path: fw_dep_step / fw_dep_raise / early gate;
  * g17_drm G17PQueue: fw_dep_walk (timeline chain unwrap with kernel
    dma_fence_chain_walk reference semantics), plan_input_syncs,
    note_recent_submission / note_ioctl_command ring, render_retired_by_aggregate
    / outstanding_renders;
  * the converted prefix encoded by the production render predecessor
    resolution (token, retired, failed predecessor);
  * check_firmware_fence on a converted command's failed hardware fence;
  * the native statistics drain with the pstate sampler on/off.
Static (text) checks: completion orders, knob defaults, the unchanged None
path, fault-release wiring, early-publication gate order.
"""
from g17_recovery_isolation_test_support import recovery_isolation_mod
import os, re, subprocess, tempfile
from pathlib import Path

ROOT = Path(os.environ.get('DRIVER_ROOT', Path(__file__).resolve().parents[1]))
DRM = (ROOT / 'g17_drm.rs').read_text()
HP = (ROOT / 'g17_host_path.rs').read_text()
JOB = (ROOT / 'g17_job.rs').read_text()
LIVE = (ROOT / 'g17_live_boot.rs').read_text()
COMP = (ROOT / 'g17_async_compute.rs').read_text()
CODEC = (ROOT / 'g17_submission.rs').read_text()
RES = (ROOT / 'g17_resources.rs').read_text()
TB = (ROOT / 'g17_timing_buffer.rs').read_text()


def block(text, marker):
    start = text.index(marker)
    depth, i = 1, text.index('{', start) + 1
    while depth:
        depth += (text[i] == '{') - (text[i] == '}')
        i += 1
    return text[start:i]


def span(text, first, last_marker):
    """From `first` through the end of the block that starts at last_marker."""
    a = text.index(first)
    b = text.index(last_marker, a)
    return text[a:b] + block(text[b:], last_marker)


def run(name, program):
    with tempfile.TemporaryDirectory() as d:
        f = Path(d) / (name + '.rs')
        f.write_text(program)
        subprocess.run(['rustc', '--edition=2021', '--test', str(f), '-o', str(Path(d) / name)], check=True)
        subprocess.run([str(Path(d) / name), '--quiet', '--test-threads=1'], check=True)


# ---------------------------------------------------------------- program A
bits = '\n'.join(m.group(0).replace('pub(crate)', 'pub') for m in re.finditer(
    r'^pub\(crate\) const [A-Z_]+: u32 = 1 << \d+;', HP, re.M))
pure = span(HP, 'pub(crate) const FW_DEP_LINKS_MAX', 'pub(crate) const fn fw_dep_raise(')
pure = pure.replace('pub(crate)', 'pub')
gates = (block(HP, 'pub(crate) const fn early_shallow_admits(') + '\n'
         + block(HP, 'pub(crate) fn early_publish_allowed(')).replace('pub(crate)', 'pub')

queue_methods = ''.join(block(DRM, m) + '\n' for m in [
    '    fn note_recent_submission(', '    fn note_ioctl_command(', '    fn recent_identities(',
    '    fn render_retired_by_aggregate(', '    fn outstanding_renders(', '    fn fw_dep_walk(',
    '    fn plan_input_syncs(',
])

program_a = r'''
#![allow(dead_code, unused_imports, unused_variables, non_camel_case_types, unused_unsafe, non_upper_case_globals)]
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, Ordering};
type Result<T=()> = std::result::Result<T, i32>;
const GFP_KERNEL: () = ();
static FAIL_PUSH: AtomicBool = AtomicBool::new(false);
pub mod bindings {
    use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
    #[repr(C)] pub struct dma_fence_ops { pub kind: u32 }
    pub static dma_fence_chain_ops: dma_fence_ops = dma_fence_ops { kind: 1 };
    pub static dma_fence_array_ops: dma_fence_ops = dma_fence_ops { kind: 2 };
    pub static plain_ops: dma_fence_ops = dma_fence_ops { kind: 0 };
    #[repr(C)] pub struct dma_fence { pub ops: *const dma_fence_ops, pub refs: AtomicI64, pub status: AtomicI32 }
    #[repr(C)] pub struct dma_fence_chain { pub base: dma_fence, pub prev: *mut dma_fence, pub fence: *mut dma_fence }
    pub unsafe fn dma_fence_get(f: *mut dma_fence) { if !f.is_null() { (*f).refs.fetch_add(1, Ordering::SeqCst); } }
    pub unsafe fn dma_fence_put(f: *mut dma_fence) {
        if !f.is_null() { let r = (*f).refs.fetch_sub(1, Ordering::SeqCst); assert!(r > 0, "put below zero"); }
    }
    pub unsafe fn dma_fence_get_status(f: *mut dma_fence) -> i32 { (*f).status.load(Ordering::SeqCst) }
    /// Kernel contract: consumes the reference on `f`; returns the previous
    /// link referenced, or NULL at the end or for a non-chain fence.
    pub unsafe fn dma_fence_chain_walk(f: *mut dma_fence) -> *mut dma_fence {
        if (*f).ops != core::ptr::addr_of!(dma_fence_chain_ops) { dma_fence_put(f); return core::ptr::null_mut(); }
        let prev = (*f.cast::<dma_fence_chain>()).prev;
        dma_fence_get(prev);
        dma_fence_put(f);
        prev
    }
}
pub struct Fence(*mut bindings::dma_fence);
impl Fence {
    fn raw(&self) -> *mut bindings::dma_fence { self.0 }
    fn of(p: *mut bindings::dma_fence) -> Self { unsafe { bindings::dma_fence_get(p) }; Fence(p) }
}
impl Clone for Fence { fn clone(&self) -> Self { Fence::of(self.0) } }
impl Drop for Fence { fn drop(&mut self) { unsafe { bindings::dma_fence_put(self.0) } } }
pub struct KVec<T>(pub Vec<T>);
impl<T> KVec<T> {
    pub const fn new() -> Self { KVec(Vec::new()) }
    pub fn with_capacity(n: usize, _: ()) -> Result<Self> { Ok(KVec(Vec::with_capacity(n))) }
    pub fn push(&mut self, t: T, _: ()) -> Result { if FAIL_PUSH.load(Ordering::SeqCst) { return Err(-12); } self.0.push(t); Ok(()) }
    pub fn clear(&mut self) { self.0.clear() }
}
impl<T> std::ops::Deref for KVec<T> { type Target = [T]; fn deref(&self) -> &[T] { &self.0 } }
mod file { pub struct SyncItem { pub fence: Option<super::Fence> } }
mod g17_job { pub struct G17PCommandFences { pub published: super::Fence, pub completed: super::Fence } }
pub mod g17_host_path {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    pub static MASK: AtomicU32 = AtomicU32::new(0);
    pub static EARLY_DEPTH: AtomicU32 = AtomicU32::new(2);
    pub fn enabled(bit: u32) -> bool { MASK.load(Ordering::Relaxed) & bit != 0 }
    pub fn count(c: &AtomicU64) { c.fetch_add(1, Ordering::Relaxed); }
    pub static FW_DEP_CONVERTED: AtomicU64 = AtomicU64::new(0);
    pub static FW_DEP_HOST: AtomicU64 = AtomicU64::new(0);
    pub static FW_DEP_SATISFIED: AtomicU64 = AtomicU64::new(0);
''' + bits + '\n' + pure + '\n' + gates + r'''
}
''' + block(DRM, 'struct G17PRecentSubmission {') + '\n' + block(DRM, 'impl G17PRecentSubmission {') + '\n' \
    + block(DRM, 'struct G17PSyncPlan {') + r'''
struct G17PQueue {
    recent: [G17PRecentSubmission; g17_host_path::FW_DEP_RECENT],
    recent_next: usize,
    ioctl_commands: Option<KVec<Fence>>,
    render_frontier: KVec<(u64, g17_job::G17PCommandFences)>,
    render_sequence: u64,
    compute_sequence: u64,
}
impl G17PQueue {
''' + queue_methods + r'''
}
use bindings::*;
fn mk(ops: *const dma_fence_ops, status: i32) -> *mut dma_fence {
    Box::into_raw(Box::new(dma_fence { ops, refs: AtomicI64::new(1), status: AtomicI32::new(status) }))
}
fn plain(status: i32) -> *mut dma_fence { mk(core::ptr::addr_of!(plain_ops), status) }
fn array(status: i32) -> *mut dma_fence { mk(core::ptr::addr_of!(dma_fence_array_ops), status) }
fn chain(prev: *mut dma_fence, leaf: *mut dma_fence) -> *mut dma_fence {
    unsafe { dma_fence_get(prev); dma_fence_get(leaf); }
    Box::into_raw(Box::new(dma_fence_chain {
        base: dma_fence { ops: core::ptr::addr_of!(dma_fence_chain_ops), refs: AtomicI64::new(1), status: AtomicI32::new(0) },
        prev, fence: leaf })).cast()
}
fn refs(p: *mut dma_fence) -> i64 { unsafe { (*p).refs.load(Ordering::SeqCst) } }
fn set(p: *mut dma_fence, s: i32) { unsafe { (*p).status.store(s, Ordering::SeqCst) } }
fn queue() -> G17PQueue {
    G17PQueue { recent: [const { G17PRecentSubmission::EMPTY }; g17_host_path::FW_DEP_RECENT], recent_next: 0,
        ioctl_commands: Some(KVec::new()), render_frontier: KVec::new(), render_sequence: 0, compute_sequence: 0 }
}
/// One fully enqueued ioctl: `renders` then `computes` commands.
fn ioctl(q: &mut G17PQueue, renders: u64, computes: u64) -> (*mut dma_fence, Vec<*mut dma_fence>) {
    let (rb, cb) = (q.render_sequence, q.compute_sequence);
    if let Some(c) = q.ioctl_commands.as_mut() { c.clear() } else { q.ioctl_commands = Some(KVec::new()) }
    let mut hw = vec![];
    for _ in 0..renders + computes {
        let f = plain(0);
        let fence = Fence::of(f);
        q.note_ioctl_command(&fence, false);   // the ring slot takes its own reference
        drop(fence);
        hw.push(f);
    }
    q.render_sequence += renders; q.compute_sequence += computes;
    let agg = plain(0);
    q.note_recent_submission(Fence::of(agg), rb, cb);
    (agg, hw)
}
fn sync(p: *mut dma_fence) -> file::SyncItem { file::SyncItem { fence: Some(Fence::of(p)) } }
fn ptrs(v: &KVec<Fence>) -> Vec<*mut dma_fence> { v.iter().map(|f| f.raw()).collect() }
fn counts() -> [u64; 3] {
    [g17_host_path::FW_DEP_CONVERTED.load(Ordering::SeqCst), g17_host_path::FW_DEP_HOST.load(Ordering::SeqCst),
     g17_host_path::FW_DEP_SATISFIED.load(Ordering::SeqCst)]
}

#[test] fn step_and_raise_are_conservative() {
    use g17_host_path::*;
    let recent = [Some(FwDepRecent { identity: 0x100, render_last: 5, compute_last: 2 }), None,
                  Some(FwDepRecent { identity: 0x200, render_last: 9, compute_last: 0 })];
    let mut r = FwDepResolution::default();
    assert!(fw_dep_step(&mut r, FwDepLeaf::Signalled, &recent));
    assert!(fw_dep_step(&mut r, FwDepLeaf::Pending(0x200), &recent));
    assert!(fw_dep_step(&mut r, FwDepLeaf::Pending(0x100), &recent));
    assert_eq!((r.render_through, r.compute_through, r.used, r.leaves), (9, 2, 0b101, 3));
    for bad in [FwDepLeaf::Failed, FwDepLeaf::Unsupported, FwDepLeaf::Pending(0x300), FwDepLeaf::Pending(0)] {
        assert!(!fw_dep_step(&mut FwDepResolution::default(), bad, &recent), "{bad:?}");
    }
    assert_eq!(fw_dep_raise(None, 0), None);
    assert_eq!(fw_dep_raise(None, 5), Some(5));
    assert_eq!(fw_dep_raise(Some(7), 5), Some(7));
    assert_eq!(fw_dep_raise(Some(3), 5), Some(5));
    assert_eq!(fw_dep_raise(Some(0), 5), Some(5));
}

#[test] fn early_gate_bits_and_defaults() {
    use g17_host_path::*;
    MASK.store(EARLY_SHALLOW, Ordering::SeqCst);
    assert!(early_publish_allowed(1, false));
    assert!(!early_publish_allowed(2, false));
    assert!(!early_publish_allowed(0, true));
    assert_eq!((FW_DEPS, AGG_FIRST, EARLY_DEPS), (1 << 13, 1 << 14, 1 << 15));
    MASK.store(0, Ordering::SeqCst);
}

#[test] fn own_aggregate_and_timeline_chain_convert_with_every_command_fence() {
    let mut q = queue();
    let (a, a_hw) = ioctl(&mut q, 2, 1);           // renders 1..2, compute 1
    let (b, b_hw) = ioctl(&mut q, 1, 0);           // render 3
    let stub = plain(1);
    // Timeline: point1 = signalled stub, point2 = A, point3 = B.
    let p1 = chain(core::ptr::null_mut(), stub);
    let p2 = chain(p1, a);
    let p3 = chain(p2, b);
    let before: Vec<i64> = [a, b, stub, p1, p2, p3].iter().map(|p| refs(*p)).collect();
    let c0 = counts();
    let plan = q.plan_input_syncs(&[sync(p3), sync(a)]).unwrap();
    let c1 = counts();
    assert_eq!([c1[0] - c0[0], c1[1] - c0[1], c1[2] - c0[2]], [2, 0, 0]);
    assert!(plan.host.is_empty());
    assert_eq!((plan.render_through, plan.compute_through), (3, 1));
    let mut want = vec![a]; want.extend(&a_hw); want.push(b); want.extend(&b_hw);
    let mut got = ptrs(&plan.firmware); got.sort(); want.sort();
    assert_eq!(got, want, "aggregate plus every command hardware fence, deduplicated");
    drop(plan);
    let after: Vec<i64> = [a, b, stub, p1, p2, p3].iter().map(|p| refs(*p)).collect();
    assert_eq!(before, after, "walk and plan references are balanced");
}

#[test] fn anything_not_provably_ours_keeps_the_host_wait() {
    let mut q = queue();
    let (a, _) = ioctl(&mut q, 1, 0);
    let foreign = plain(0);
    let failed = plain(-5);
    let arr = array(0);
    let cases = [
        chain(chain(core::ptr::null_mut(), foreign), a),   // foreign pending link
        chain(chain(core::ptr::null_mut(), failed), a),    // errored link
        chain(core::ptr::null_mut(), arr),                 // array leaf
        arr, foreign, failed,
        chain(core::ptr::null_mut(), chain(core::ptr::null_mut(), a)), // nested chain leaf
    ];
    for (i, case) in cases.iter().enumerate() {
        let before = refs(*case);
        let c0 = counts();
        let plan = q.plan_input_syncs(&[sync(*case)]).unwrap();
        assert_eq!(ptrs(&plan.host), vec![*case], "case {i}");
        assert!(plan.firmware.is_empty() && plan.render_through == 0 && plan.compute_through == 0, "case {i}");
        assert_eq!(counts()[1] - c0[1], 1);
        drop(plan);
        assert_eq!(refs(*case), before, "case {i} refs");
    }
}

#[test] fn signalled_inputs_are_dropped_and_long_chains_fall_back() {
    let q = queue();
    let mut node = core::ptr::null_mut();
    for _ in 0..16 { node = chain(node, plain(1)); }
    let c0 = counts();
    let plan = q.plan_input_syncs(&[sync(node), sync(plain(1))]).unwrap();
    assert!(plan.host.is_empty() && plan.firmware.is_empty());
    assert_eq!(counts()[2] - c0[2], 2);
    let longer = chain(node, plain(1));
    let plan = q.plan_input_syncs(&[sync(longer)]).unwrap();
    assert_eq!(ptrs(&plan.host), vec![longer], "17 links exceed FW_DEP_LINKS_MAX");
}

#[test] fn ring_eviction_failed_ioctl_and_allocation_failure_are_unmatched() {
    let mut q = queue();
    let (a, _) = ioctl(&mut q, 1, 0);
    for _ in 0..g17_host_path::FW_DEP_RECENT { ioctl(&mut q, 1, 0); }
    assert_eq!(ptrs(&q.plan_input_syncs(&[sync(a)]).unwrap().host), vec![a], "evicted");
    // An ioctl that never reaches note_recent_submission (failed part-way).
    let unrecorded = plain(0);
    assert_eq!(ptrs(&q.plan_input_syncs(&[sync(unrecorded)]).unwrap().host), vec![unrecorded]);
    // Command-list allocation failure: the slot stays empty, list restored.
    q.ioctl_commands.as_mut().unwrap().clear();
    FAIL_PUSH.store(true, Ordering::SeqCst);
    q.note_ioctl_command(&Fence::of(plain(0)), false);
    FAIL_PUSH.store(false, Ordering::SeqCst);
    assert!(q.ioctl_commands.is_none());
    let lost = plain(0);
    let rb = q.render_sequence; q.render_sequence += 1;
    q.note_recent_submission(Fence::of(lost), rb, q.compute_sequence);
    assert!(q.ioctl_commands.is_some());
    assert_eq!(ptrs(&q.plan_input_syncs(&[sync(lost)]).unwrap().host), vec![lost]);
    // Slot vectors are reused: the ring never grows beyond FW_DEP_RECENT.
    assert_eq!(q.recent.len(), g17_host_path::FW_DEP_RECENT);
}

// j700 c209c3dc5: a command with host-copied user timestamps is ready for a
// consumer only at completion (the CPU copy precedes it), so FW_DEPS must never
// turn a wait on its ioctl into firmware ordering.
#[test] fn a_command_with_host_timestamps_keeps_its_ioctl_a_host_wait() {
    let mut q = queue();
    let (rb, cb) = (q.render_sequence, q.compute_sequence);
    q.ioctl_commands.as_mut().unwrap().clear();
    q.note_ioctl_command(&Fence::of(plain(0)), false);
    q.note_ioctl_command(&Fence::of(plain(0)), true);
    q.note_ioctl_command(&Fence::of(plain(0)), false);
    assert!(q.ioctl_commands.is_none());
    q.render_sequence += 3;
    let agg = plain(0);
    q.note_recent_submission(Fence::of(agg), rb, cb);
    assert!(q.ioctl_commands.is_some());
    assert_eq!(ptrs(&q.plan_input_syncs(&[sync(agg)]).unwrap().host), vec![agg]);
    // The next ioctl without host timestamps converts as usual.
    let (b, _) = ioctl(&mut q, 1, 0);
    assert!(q.plan_input_syncs(&[sync(b)]).unwrap().host.is_empty());
}

#[test] fn agg_first_depth_counts_a_render_retired_once_its_aggregate_signals() {
    let mut q = queue();
    let (a, hw) = ioctl(&mut q, 2, 0);
    for (i, f) in hw.iter().enumerate() {
        q.render_frontier.0.push((i as u64 + 1, g17_job::G17PCommandFences { published: Fence::of(plain(1)), completed: Fence::of(*f) }));
    }
    g17_host_path::MASK.store(0, Ordering::SeqCst);
    assert_eq!(q.outstanding_renders(), 2);
    g17_host_path::MASK.store(g17_host_path::AGG_FIRST, Ordering::SeqCst);
    assert_eq!(q.outstanding_renders(), 2, "aggregate pending");
    set(a, 1);
    assert_eq!(q.outstanding_renders(), 0, "aggregate signalled before hw fences");
    g17_host_path::MASK.store(0, Ordering::SeqCst);
    assert_eq!(q.outstanding_renders(), 2, "knob off: exactly the frontier length");
}
'''

# ---------------------------------------------------------------- program B
# Encoding and fault release: the production render predecessor resolution
# (runtime publish scan) with a converted (raised) prefix, and the firmware
# status check of a converted command's hardware fence.
constants = []
for name in ['KICK_TS_BITS', 'KICK_TS_MASK', 'KICK_QID_BITS', 'KICK_QID_MASK',
             'KICK_QID_SHIFT', 'KICK_STAMP_SHIFT', 'KICK_STAMP_INDEX_MASK',
             'KICK_RESERVED_MASK', 'KICK_SELECTOR_BIT']:
    constants.append(re.search(r'^(?:pub\(crate\) )?const ' + name + r': .*?;', CODEC, re.M | re.S).group())


def citem(name):
    match = re.search(r'^(?:pub\(crate\) )?(?:(?:const )?fn|enum|struct) ' + name + r'\b', CODEC, re.M)
    assert match, name
    return block(CODEC, match.group())


program_b = r'''
#![allow(dead_code, unused_imports)]
use std::sync::{Arc, atomic::{AtomicI32, AtomicU64, Ordering}};
#[derive(Debug, Copy, Clone, PartialEq)] struct Error(i32);
impl Error { fn from_errno(e: i32) -> Self { Self(e) } fn to_errno(self) -> i32 { self.0 } }
type Result<T=()> = std::result::Result<T, Error>;
const EIO: Error = Error(-5);
#[derive(Clone)] struct Fence { status: Arc<AtomicI32> }
impl Fence { fn raw(&self) -> *const Self { self } }
mod bindings { pub unsafe fn dma_fence_get_status(f: *const super::Fence) -> i32 { (*f).status.load(super::Ordering::Acquire) } }
mod g17_status { pub struct VmStatus; }
mod g17_host_path {
''' + span(HP, 'pub(crate) const fn fw_dep_raise(', 'pub(crate) const fn fw_dep_raise(').replace('pub(crate)', 'pub') + r'''
}
mod g17_submission {
''' + '\n'.join(constants)
for name in ['SubmissionError', 'KickTimestamp', 'SksmKickWord', 'G17PClBarrierDependency', 'G17PRenderOrder']:
    program_b += '\n#[derive(Debug,Copy,Clone,PartialEq,Eq)]\n' + citem(name)
program_b += '\n#[derive(Copy,Clone)]pub struct Geometry;impl Geometry{fn stamp_count(self)->u16{256}}\n'
program_b += '#[derive(Copy,Clone)]pub struct G17PClKickProducerState{pub base_timestamp:u64,pub geometry:Geometry}\n'
for name in ['encode_kick_word', 'decode_kick_word', 'encode_kick_timestamp',
             'g17p_render_barrier_from_timestamp', 'g17p_render_prefix_contains',
             'g17p_live_render_dependency']:
    program_b += '\n' + citem(name)
program_b += '\n}\n'
program_b += '\nstruct G17PQueue; impl G17PQueue {' + block(DRM, '    fn check_firmware_fence(') + '}\n'
# iso4: a failed predecessor is reported to the dependent through the
# production inheritance rule (EIO with the isolation mask clear).
program_b += block(DRM, 'pub(crate) fn inherited_dependency_error(').replace('pub(crate) ', '') + '\n'
program_b += recovery_isolation_mod()
# Counter-only resolver statistics (g17_dep_stats): no behaviour, stubbed.
program_b += '''
mod g17_dep_stats {
    pub fn note_resolved(_word: u64) {}
    pub fn note_error(_error: super::g17_submission::SubmissionError, _status: i32, _word: u64) {}
}
'''
program_b += r'''
struct Render { owner: u64, status: Arc<g17_status::VmStatus>, render_order: g17_submission::G17PRenderOrder,
                fragment_dependency: AtomicU64, fence: Fence }
impl Render { fn fence(&self) -> Fence { self.fence.clone() }
''' + block(DRM, '    pub(crate) fn predecessor_dependency(') + block(DRM, '    fn counted_dependency(') + r'''
}
fn fence(s: i32) -> Fence { Fence { status: Arc::new(AtomicI32::new(s)) } }
fn render(vm: &Arc<g17_status::VmStatus>, seq: u64, wait: Option<u64>, qid: u8, stamp: u64, s: i32) -> Render {
    Render { owner: 7, status: vm.clone(), render_order: g17_submission::G17PRenderOrder { sequence: seq, wait_through: wait },
        fragment_dependency: AtomicU64::new(g17_submission::encode_kick_word(stamp, qid).unwrap()), fence: fence(s) }
}
#[test] fn converted_prefix_becomes_a_native_token_and_failure_refuses_the_dependent() {
    let vm = Arc::new(g17_status::VmStatus);
    // Independent render (barrier NONE) whose input sync named render 41's ioctl.
    let raised = g17_host_path::fw_dep_raise(None, 41);
    let dependent = render(&vm, 43, raised, 9, 0x2000100, 0);
    let pending = render(&vm, 41, None, 7, 0x1ffffff, 0);
    let dep = dependent.predecessor_dependency(&pending).unwrap().expect("pending predecessor -> token");
    assert_eq!(dep.native_timestamp, Some(0x1ffffff));
    assert_eq!(dep.queue_id, 7);
    // Outside the converted prefix: unrelated.
    let later = render(&vm, 42, None, 9, 0x2000000, 0);
    assert!(dependent.predecessor_dependency(&later).unwrap().is_none());
    // Retired OK: no GPU wait needed.
    assert!(dependent.predecessor_dependency(&render(&vm, 41, None, 7, 1, 1)).unwrap().is_none());
    // Failed predecessor: publication of the dependent fails (never runs).
    assert_eq!(dependent.predecessor_dependency(&render(&vm, 41, None, 7, 1, -5)), Err(EIO));
    // iso4 INNOCENT: the dependent fails without a device-loss code, so its
    // own VM is not poisoned for its predecessor's failure.
    g17_recovery_isolation::MASK.store(g17_recovery_isolation::INNOCENT, Ordering::Relaxed);
    assert_eq!(dependent.predecessor_dependency(&render(&vm, 41, None, 7, 1, -5)), Err(Error(-125)));
    g17_recovery_isolation::MASK.store(0, Ordering::Relaxed);
    // Another queue (owner) or VM is never matched by the scan.
    let mut other = render(&vm, 41, None, 7, 1, 0); other.owner = 8;
    assert!(dependent.predecessor_dependency(&other).unwrap().is_none());
    // A converted command's hardware fence that failed before execution
    // (fail_unpublished leaves the VM clean for non-loss errors) is refused.
    assert!(G17PQueue::check_firmware_fence(&fence(0)).is_ok());
    assert!(G17PQueue::check_firmware_fence(&fence(1)).is_ok());
    assert_eq!(G17PQueue::check_firmware_fence(&fence(-16)), Err(Error(-16)));
}
'''

# ---------------------------------------------------------------- program C
# Statistics drain with the pstate sampler: identical consumer publications.
def res_item(marker):
    return block(RES, marker)


def res_static(marker):
    start = RES.index(marker)
    return RES[start:RES.index(';', start) + 1]


pstate = span(TB, 'pub(crate) const STATS_PSTATE_TYPE', 'pub(crate) fn stats_pstate_note(')
pstate = pstate.replace('pub(crate)', 'pub')
pstate = re.sub(r'^static ', 'pub static ', pstate, flags=re.M)
pstate = re.sub(r'^const ', 'pub const ', pstate, flags=re.M)
program_c = r'''
#![allow(dead_code, unused_variables, unused_macros, unused_imports)]
type Result<T=()> = std::result::Result<T,i32>;
type Error = i32;
const EINVAL:i32=22; const EIO:i32=5;
macro_rules! dev_err { ($($x:tt)*) => { } }
mod kernel { pub mod device { pub struct Device; } }
pub mod g17_timing_buffer {
    use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
''' + pstate + r'''
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)] enum InstanceRole { Primary, Secondary }
struct Instance { status_a:u64 }
struct Role { instance:Instance }
struct Handoff { primary:Role, secondary:Role }
const STATUS_A:u64 = 0x10_0000;
const STATE:usize = 0x280; const RING:usize = 0xa_fac0; const ENTRY:usize = 0x48;
struct Object { base:u64, bytes:Vec<u8>, publications:Vec<u32>, fail_at:Option<usize> }
impl Object {
 fn new() -> Self { Object { base: STATUS_A, bytes: vec![0; RING + 0x100*ENTRY + 0x100], publications: vec![], fail_at: None } }
 fn iova(&self)->u64 { self.base }
 fn read_live_u32(&mut self,address:u64)->Result<u32> {
  let offset=address.checked_sub(self.base).ok_or(EINVAL)? as usize;
  if self.fail_at == Some(offset) { return Err(EIO); }
  g17p_live_read_u32(&self.bytes,offset)
 }
 fn write_u32(&mut self,address:u64,value:u32)->Result {
  let offset=address.checked_sub(self.base).ok_or(EINVAL)? as usize;
  g17p_live_write_u32(&mut self.bytes,offset,value)?;
  if offset == STATE { self.publications.push(value); }
  Ok(())
 }
 fn set(&mut self, offset:usize, value:u32) { self.bytes[offset..offset+4].copy_from_slice(&value.to_le_bytes()); }
}
struct Dev; impl Dev { fn as_ref(&self) -> &kernel::device::Device { &kernel::device::Device } }
struct Resources { handoff:Handoff, objects:[Object;2], dev:Dev }
impl Resources {
 fn new()->Self {
  Self { handoff:Handoff { primary:Role{instance:Instance{status_a:STATUS_A}}, secondary:Role{instance:Instance{status_a:STATUS_A}} },
         objects:[Object::new(),Object::new()], dev:Dev }
 }
 fn dev(&self)->&Dev { &self.dev }
 fn ktrace_object(&mut self,role:InstanceRole)->&mut Object { &mut self.objects[match role{InstanceRole::Primary=>0,InstanceRole::Secondary=>1}] }
''' + res_item('    pub(crate) fn drain_statistics_ring_native(') + '\n}\n'
program_c += '#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]\n' + res_item('pub(crate) struct G17PNativeRingDrainStats {') + '\n'
program_c += res_static('pub(crate) static G17P_NATIVE_RING_QUARANTINE') + '\n'
for name in ['pub(crate) const fn g17p_native_ring_quarantine_bit(', 'fn g17p_native_ring_quarantine(',
             'fn g17p_live_word_bounds(', 'fn g17p_live_read_u32(', 'fn g17p_live_write_u32(']:
    program_c += res_item(name) + '\n'
program_c += '#[path="' + str(ROOT / 'g17_completion.rs') + '"] mod g17_completion;\n'
program_c += r'''
use core::sync::atomic::Ordering::Relaxed;
use g17_timing_buffer as tb;
fn fill(r: &mut Resources) {
    let o = r.ktrace_object(InstanceRole::Primary);
    for i in 0..6u32 {
        let e = RING + i as usize * ENTRY;
        o.set(e, if i % 2 == 0 { 0x10 } else { 0x19 });
        o.set(e + 4, 0x1000 + i); o.set(e + 8, 0x7);
        o.set(e + 0x18, 3 + i); o.set(e + 0x1c, 0xab);
    }
    o.set(STATE, 0); o.set(STATE + 0x20, 6);
}
#[test] fn sampler_is_read_only_and_captures_type_0x10_pstate() {
    let mut off = Resources::new(); fill(&mut off);
    tb::PSTATE_ON.store(0, Relaxed);
    let s_off = off.drain_statistics_ring_native(InstanceRole::Primary).unwrap();
    assert_eq!(tb::PSTATE_RECORDS.load(Relaxed), 0, "off: nothing recorded");
    let mut on = Resources::new(); fill(&mut on);
    tb::PSTATE_ON.store(1, Relaxed);
    let s_on = on.drain_statistics_ring_native(InstanceRole::Primary).unwrap();
    assert_eq!(s_on, s_off);
    assert_eq!(on.objects[0].publications, off.objects[0].publications);
    assert_eq!(on.objects[0].publications, vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(tb::PSTATE_RECORDS.load(Relaxed), 6);
    assert_eq!(tb::PSTATE_TYPE10.load(Relaxed), 3);
    assert_eq!(tb::PSTATE_LAST[0].load(Relaxed), 0x7_0000_1004);
    assert_eq!(tb::PSTATE_LAST[1].load(Relaxed), 0xab_0000_0007);
    // Buckets from the low byte of qword 3 (3, 5, 7); ticks attributed to the
    // previous record's pstate (0x1002-0x1000 to 3, 0x1004-0x1002 to 5).
    assert_eq!(tb::PSTATE_COUNT[3].load(Relaxed) + tb::PSTATE_COUNT[5].load(Relaxed) + tb::PSTATE_COUNT[7].load(Relaxed), 3);
    assert_eq!(tb::PSTATE_TICKS[3].load(Relaxed), 2);
    assert_eq!(tb::PSTATE_TICKS[5].load(Relaxed), 2);
    assert_eq!(tb::stats_pstate_bucket(0x20), 16);
    // A failed sample read is counted; the drain publishes exactly the same.
    let mut bad = Resources::new(); fill(&mut bad);
    bad.objects[0].fail_at = Some(RING + ENTRY + 0x18);
    let s_bad = bad.drain_statistics_ring_native(InstanceRole::Primary).unwrap();
    assert_eq!(s_bad, s_off);
    assert_eq!(bad.objects[0].publications, off.objects[0].publications);
    assert_eq!(tb::PSTATE_READ_FAILS.load(Relaxed), 1);
    tb::PSTATE_ON.store(0, Relaxed);
}
'''

run('v93_a', program_a)
run('v93_b', program_b)
run('v93_c', program_c)

# ---------------------------------------------------------------- static checks
default = re.search(r'const DEFAULT_MASK: u32 = (.*?);', HP, re.S).group(1)
for bit in ['FW_DEPS', 'AGG_FIRST', 'EARLY_DEPS']:
    assert bit not in default, bit                      # v92d behaviour by default
# Later stages append bits 16.. above the v93 bits; all three stay defined.
assert int(re.search(r'const ALL_BITS: u32 = \(1 << (\d+)\) - 1;', HP).group(1)) >= 16
for bit, shift in [('FW_DEPS', 13), ('AGG_FIRST', 14), ('EARLY_DEPS', 15)]:
    assert f'pub(crate) const {bit}: u32 = 1 << {shift};' in HP

render_complete = block(DRM, '    pub(crate) fn complete(\n        &self,\n        result: Result<g17_completion::PairedRenderCompletion>,')
order = ['destination.write(timestamp)?', 'self.status.record(error.to_errno())', 'drop(self.lease.lock().take())',
         'drop(self.vm_job.lock().take())', 'let agg_first =']
positions = [render_complete.index(x) for x in order]
assert positions == sorted(positions), 'timestamps, status, lease and VM job precede either signal'
agg = render_complete[render_complete.index('let agg_first ='):]
first = agg[agg.index('if agg_first {'):agg.index('} else {')]
assert first.index('self.aggregate_member.complete(result)') < first.index('self.fence.signal()')
rest = agg[agg.index('} else {'):]
assert rest.index('self.fence.signal()') < rest.index('self.aggregate_member.complete(result)')

job_complete = block(JOB, '    pub(crate) fn complete(&self, result: Result<[u64; 2]>)')
assert job_complete.index('destination.write(timestamp)?') < job_complete.index('drop(self.vm_job.lock().take())') \
    < job_complete.index('AGG_FIRST')
branch = job_complete[job_complete.index('AGG_FIRST'):]
assert branch.index('self.aggregate_member.complete(result)') < branch.index('self.fence.signal()')

base_loop = '''for sync in syncs {
                if let Some(fence) = &sync.fence {
                    checked_dependencies.push(fence.clone(), GFP_KERNEL)?;
                    dependencies.push(fence.clone(), GFP_KERNEL)?;
                }
            }'''
for name in ['    fn submit_render_async(', '    fn submit_compute_async(']:
    body = block(DRM, name)
    assert 'None => ' + base_loop in body, name              # FW_DEPS off: unchanged
    some = body[body.index('Some(plan) => {'):]
    assert 'for fence in plan.firmware.iter() {\n                    firmware_fences.push(' in some
    assert body.index('fw_dep_raise') < body.index('let prefix_count'), name
    assert body.count('fw_dep_raise') == (2 if 'render' in name else 1)
    assert 'if record' in body or 'record.then' in body
submit = DRM[DRM.index('    fn submit_guarded('):]
submit = submit[:submit.index('\n    }\n}\n') if '\n    }\n}\n' in submit else len(submit)]
assert submit.index('Self::install_output_fences(publication.outputs.drain(..), &aggregate.fence());') \
    < submit.index('self.plan_input_syncs(&syncs)') < submit.index('for index in 0..command_count {') \
    < submit.index('self.note_recent_submission(aggregate.fence(), render_before, compute_before);') \
    < submit.index('publication.publish_async(&aggregate.fence());')

early = block(DRM, '    fn try_publish_early(')
assert early.index('self.sched_backlog.load(Ordering::Acquire) != 0') < early.index('EARLY_DEPS') \
    < early.index('try_acquire_lease')
assert 'dma_fence_get_status(fence.raw()) } == 0' in early          # any unsignalled input declines

# Fault release: a converted dependent shares its producer's VM status, is
# refused at publication once that status is sticky, and is failed (with its
# queued successors) by the existing fail-outstanding/quarantine paths.
assert DRM.count('self.vm.status().clone(),') >= 2
publish = block(DRM, '    fn publish_with(')
assert publish.count('completion.vm_status().get() != 0') >= 3
assert publish.index('for fence in packet.firmware_fences()') < publish.index('acquire_lease')
fail_final = block(LIVE, '    fn fail_render_after_final_observation(')
assert 'self.quarantine_failed_render_completion(queued, error);' in fail_final  # stability 0006
assert 'self.fail_outstanding_jobs_for_vm(&status, error);' in fail_final
fail_vm = block(COMP, '    fn fail_outstanding_jobs_for_vm_after_compute_observation(')
assert 'completion.belongs_to_vm(status)' in fail_vm and 'completion.fail_while_owned(error)' in fail_vm
assert 'Arc::ptr_eq(&completion.status, status)' in fail_vm
walk = block(DRM, '    fn fw_dep_walk(')
assert 'dma_fence_chain_walk(node)' in walk and 'dma_fence_put(node)' in walk
assert walk.count('FwDepLeaf::Unsupported') >= 2

drain = block(RES, '    pub(crate) fn drain_stats_ring(')
assert 'MSG_STATS_RECORD_Q3' in drain and 'let mut raw = [0u8; 0x20];' in drain
assert 'static PSTATE_ON: AtomicU32 = AtomicU32::new(0);' in TB
print('v93 fw-deps / agg-first / early-deps / stats-pstate: ok')
