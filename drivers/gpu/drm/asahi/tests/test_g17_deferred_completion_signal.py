#!/usr/bin/env python3
"""Success completions are signalled with the runtime mutex released.

Compiles the actual deferral type, the actual runtime deferral methods and the
actual `G17PRuntimeRef` worker wrappers against a mock mutex that records
whether it is held, and checks: every proven completion is signalled exactly
once, never while the runtime mutex is held, even when the service pass fails
after settling; a settle outside a worker wrapper queues the event worker
exactly once (only while armed) and is signalled by that worker's drain or,
at teardown, in place; a deferral without memory still signals
exactly once. Text checks pin the settle sites to the deferral and the
failure paths to their immediate signalling.
"""
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]


def item(text, marker):
    start = text.index(marker)
    end = text.index('{', start) + 1
    depth = 1
    while depth:
        depth += (text[end] == '{') - (text[end] == '}')
        end += 1
    return text[start:end]


drm = (root / 'g17_drm.rs').read_text()
live = (root / 'g17_live_boot.rs').read_text()
compute = (root / 'g17_async_compute.rs').read_text()

# --- settle sites defer; failure paths still signal in place ---------------
wrapper = item(live, '    fn settle_deferred_render_completion(')
assert 'self.settle_retired_render_completion(render_slot, Ok(observed))' in wrapper
settle = item(live, '    fn settle_retired_render_completion(')
assert 'completion: signal, observed,' in settle
assert '.complete(Ok(' not in settle
assert settle.index('completion: signal, observed,') < settle.index(
    'retire_render_usc_freelist_at_completion(render_slot)')
retire = item(compute, '    fn service_async_compute_completions_matching(')
assert 'G17PDeferredCompletion::Compute {' in retire
assert '.complete(Ok(timestamps))' not in retire
assert retire.index('.defer_into(deferred);') < retire.index(
    'kick_deferred_completion_drain_from(deferred, gfx_state, drain_owned);') < retire.index('drop(active);')
# The admission lease still drops under the mutex, before the fence wakes.
assert retire.index('kick_deferred_completion_drain_from(deferred, gfx_state, drain_owned);') < retire.index('drop(active);')
assert 'completion.fail_while_owned(error)' in retire
timeout = item(live, '    fn fail_render_after_final_observation(')
assert 'self.quarantine_failed_render_completion(signal, error);' in timeout
assert 'signal.complete(Err(EIO));' in timeout
render_impl = item(drm, 'impl G17PRenderCompletion {')
fail_owned = item(render_impl, '    pub(crate) fn fail_while_owned(')
assert 'lease.registry.quarantine_render_slot(lease.slot);' in fail_owned
# Rebuild/teardown signal what was deferred first, with or without a proven stop.
for marker in ('    fn release_runtime_objects(', '    fn drop_queue('):
    body = item(live, marker)
    assert body.index('self.signal_deferred_completions_in_place();') < body.index(
        'completion.complete(Err(EIO));'), marker
retain = item(live, '    fn retain_after_stop_failure(')
assert retain.index('self.signal_deferred_completions_in_place();') < retain.index('core::mem::forget(')

# --- the worker wrappers signal after the lock scope closes -----------------
for name in ('service_primary_firmware_events', 'service_primary_recovery'):
    wrapper = item(drm, f'    pub(crate) fn {name}(&self) -> Result {{')
    scope_end = wrapper.index('};', wrapper.index('take_deferred_completions()'))
    assert wrapper.index('self.mutex().lock()') < wrapper.index('begin_deferred_completion_drain()') \
        < wrapper.index(f'runtime.{name}()') < wrapper.index('take_deferred_completions()') < scope_end
    assert scope_end < wrapper.index('completion.signal();'), name

# --- compiled model with the actual code ------------------------------------
enum = item(drm, 'pub(crate) enum G17PDeferredCompletion {')
enum_impl = item(drm, 'impl G17PDeferredCompletion {')
wrappers = '\n'.join(item(drm, f'    pub(crate) fn {n}(&self) -> Result {{')
                     for n in ('service_primary_firmware_events', 'service_primary_recovery'))
runtime_methods = '\n'.join(item(live, n) for n in (
    '    fn defer_completion(',
    '    pub(crate) fn begin_deferred_completion_drain(',
    '    pub(crate) fn take_deferred_completions(',
    '    fn signal_deferred_completions_in_place('))
kick = item(live, 'fn kick_deferred_completion_drain_from(')

source = r'''
#![allow(dead_code)]
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[derive(Clone, Copy, Debug, PartialEq)] struct Error(i32);
type Result<T = ()> = std::result::Result<T, Error>;
const EIO: Error = Error(-5);
struct Flags; const GFP_KERNEL: Flags = Flags;
struct AllocError;
static NO_MEMORY: AtomicBool = AtomicBool::new(false);
mod kernel { pub mod alloc { pub mod kvec { pub struct PushError<T>(pub T); } } }
struct KVec<T>(Vec<T>);
impl<T> Default for KVec<T> { fn default() -> Self { KVec(Vec::new()) } }
impl<T> KVec<T> {
    fn reserve(&mut self, n: usize, _: Flags) -> std::result::Result<(), AllocError> {
        if NO_MEMORY.load(Ordering::SeqCst) { return Err(AllocError); }
        self.0.reserve(n); Ok(())
    }
    fn push_within_capacity(&mut self, v: T) -> std::result::Result<(), kernel::alloc::kvec::PushError<T>> {
        if self.0.len() == self.0.capacity() { return Err(kernel::alloc::kvec::PushError(v)); }
        self.0.push(v); Ok(())
    }
    fn is_empty(&self) -> bool { self.0.is_empty() }
    fn len(&self) -> usize { self.0.len() }
}
impl<T> IntoIterator for KVec<T> { type Item = T; type IntoIter = std::vec::IntoIter<T>; fn into_iter(self) -> Self::IntoIter { self.0.into_iter() } }

// The runtime mutex: records whether it is held on this (single) thread.
static LOCKED: AtomicBool = AtomicBool::new(false);
struct Mutex<T>(std::cell::RefCell<T>);
struct Guard<'a, T>(std::cell::RefMut<'a, T>);
impl<T> Mutex<T> {
    fn lock(&self) -> Guard<'_, T> { assert!(!LOCKED.swap(true, Ordering::SeqCst), "runtime mutex taken twice"); Guard(self.0.borrow_mut()) }
}
impl<T> std::ops::Deref for Guard<'_, T> { type Target = T; fn deref(&self) -> &T { &self.0 } }
impl<T> std::ops::DerefMut for Guard<'_, T> { fn deref_mut(&mut self) -> &mut T { &mut self.0 } }
impl<T> Drop for Guard<'_, T> { fn drop(&mut self) { LOCKED.store(false, Ordering::SeqCst); } }

mod g17_completion { #[derive(Clone, Copy, Default, Debug, PartialEq)] pub struct PairedRenderCompletion { pub complete: bool } }
// Completions: count signals and assert the runtime mutex is NOT held.
struct G17PRenderCompletion { signals: AtomicUsize, complete_seen: AtomicUsize }
impl G17PRenderCompletion {
    fn new() -> Arc<Self> { Arc::new(Self { signals: AtomicUsize::new(0), complete_seen: AtomicUsize::new(0) }) }
    fn release_after_firmware_drain(&self) { assert!(!LOCKED.load(Ordering::SeqCst)); }
    fn complete(&self, result: Result<g17_completion::PairedRenderCompletion>) {
        assert!(!LOCKED.load(Ordering::SeqCst), "render fence signalled under the runtime mutex");
        self.signals.fetch_add(1, Ordering::SeqCst);
        if result.map(|c| c.complete).unwrap_or(false) { self.complete_seen.fetch_add(1, Ordering::SeqCst); }
    }
}
struct G17PJobCompletion { signals: AtomicUsize, timestamps: std::cell::Cell<[u64; 2]> }
impl G17PJobCompletion {
    fn complete(&self, result: Result<[u64; 2]>) {
        assert!(!LOCKED.load(Ordering::SeqCst), "compute fence signalled under the runtime mutex");
        self.signals.fetch_add(1, Ordering::SeqCst); self.timestamps.set(result.unwrap());
    }
}
mod g17_job {
    pub struct G17PComputeJobPacket { pub completion: Option<super::G17PJobCompletion> }
    impl G17PComputeJobPacket { pub fn completion(&self) -> super::Result<&super::G17PJobCompletion> { self.completion.as_ref().ok_or(super::EIO) } }
}
#[derive(Clone)] struct Dev;
static KICKS: AtomicUsize = AtomicUsize::new(0);
mod driver { pub fn queue_g17p_firmware_event_worker(_dev: super::Dev) { super::KICKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst); } }
struct G17PRoleRtkitState { dev: Dev, event_worker_armed: AtomicBool }
mod g17_drm { pub(crate) use crate::{G17PDeferredCompletion, G17PRenderCompletion}; }
''' + enum + '\n' + enum_impl + '\n' + kick + r'''
mod g17_e2e {
    pub const K_WORKER:u32=0;
    pub fn stamp()->u64{0} pub fn now()->u64{0}
    pub fn record(_:u32,_:u32,_:u64,_:[u64;5]){}
}
mod g17_host_path {
    use std::sync::atomic::AtomicU64;
    pub const OBS_SKIP:u32=0;
    pub static OBS_SKIPPED:AtomicU64=AtomicU64::new(0);
    pub fn enabled(_:u32)->bool{false} pub fn count(_:&AtomicU64){}
}
mod g17_timing_buffer {
    pub fn enabled()->bool{false} pub fn claim_completion_snapshot()->bool{false}
    pub fn finish_completion_snapshot(_:super::Result){}
}
struct G17PLiveRuntime {
    deferred_completions: KVec<crate::g17_drm::G17PDeferredCompletion>,
    deferred_completion_drain_owned: bool,
    gfx_state: Option<Arc<G17PRoleRtkitState>>,
    pending: Vec<Arc<G17PRenderCompletion>>,
    fail_after_settle: bool,
}
impl G17PLiveRuntime {
    fn new(armed: bool) -> Self {
        Self { deferred_completions: KVec::default(), deferred_completion_drain_owned: false,
               gfx_state: Some(Arc::new(G17PRoleRtkitState { dev: Dev, event_worker_armed: AtomicBool::new(armed) })),
               pending: Vec::new(), fail_after_settle: false }
    }
    // Test-only producer: the enum and its signal/deferral methods below
    // are extracted verbatim from production, including cancelled results.
    fn defer_render_completion(&mut self, completion: Arc<G17PRenderCompletion>,
        observed: g17_completion::PairedRenderCompletion) {
        self.defer_completion(G17PDeferredCompletion::Render {completion, observed: Ok(observed)});
    }
    // A service pass: settles everything pending, then possibly fails.
    fn note_render_observation_error(&mut self,_:Error) {}
    fn service_compute_witness_top(&mut self,_:&str) {}
    fn render_observation_possible(&self)->bool{true}
    fn capture_completion_ring_snapshot(&mut self)->Result{Ok(())}
    fn service_primary_firmware_events(&mut self) -> Result {
        assert!(LOCKED.load(Ordering::SeqCst));
        for completion in std::mem::take(&mut self.pending) {
            self.defer_render_completion(completion, g17_completion::PairedRenderCompletion { complete: true });
        }
        if self.fail_after_settle { Err(EIO) } else { Ok(()) }
    }
    fn primary_recovery_pending(&self)->bool{true}
    fn service_primary_recovery(&mut self) -> Result { self.service_primary_firmware_events() }
''' + runtime_methods + r'''
}
struct Preparations;impl Preparations{fn close(&self,_:bool){assert!(LOCKED.load(Ordering::SeqCst));}fn wait_for_preparers(&self){assert!(!LOCKED.load(Ordering::SeqCst));}}
struct G17PRuntimeRef { preparations:Preparations, mutex: Arc<Mutex<G17PLiveRuntime>> }
impl G17PRuntimeRef {
 fn service_render_observations(&self)->Result {Ok(())}

    fn mutex(&self) -> &Mutex<G17PLiveRuntime> { &self.mutex }
''' + wrappers + r'''
}
fn runtime(armed: bool) -> (G17PRuntimeRef, Vec<Arc<G17PRenderCompletion>>) {
    let completions: Vec<_> = (0..5).map(|_| G17PRenderCompletion::new()).collect();
    let mut rt = G17PLiveRuntime::new(armed);
    rt.pending = completions.clone();
    (G17PRuntimeRef { preparations:Preparations, mutex: Arc::new(Mutex(std::cell::RefCell::new(rt))) }, completions)
}
fn signalled_once(completions: &[Arc<G17PRenderCompletion>]) -> bool {
    completions.iter().all(|c| c.signals.load(Ordering::SeqCst) == 1 && c.complete_seen.load(Ordering::SeqCst) == 1)
}
#[test] fn worker_pass_signals_every_settled_render_after_unlocking_without_a_kick() {
    KICKS.store(0, Ordering::SeqCst);
    let (rt, completions) = runtime(true);
    assert!(rt.service_primary_firmware_events().is_ok());
    assert!(signalled_once(&completions));
    assert!(!LOCKED.load(Ordering::SeqCst));
    assert_eq!(KICKS.load(Ordering::SeqCst), 0, "a worker pass queued itself again");
    let guard = rt.mutex().lock();
    assert!(guard.deferred_completions.is_empty() && !guard.deferred_completion_drain_owned);
}
#[test] fn failed_pass_still_signals_what_it_settled() {
    let (rt, completions) = runtime(true);
    rt.mutex().lock().fail_after_settle = true;
    assert_eq!(rt.service_primary_firmware_events(), Err(EIO));
    assert!(signalled_once(&completions));
    let (rt, completions) = runtime(true);
    rt.mutex().lock().fail_after_settle = true;
    assert_eq!(rt.service_primary_recovery(), Err(EIO));
    assert!(signalled_once(&completions));
}
#[test] fn settle_outside_a_worker_kicks_the_worker_once_and_that_worker_signals() {
    KICKS.store(0, Ordering::SeqCst);
    let (rt, completions) = runtime(true);
    {
        // An inline drain / watchdog settle: the caller holds the mutex and
        // will release it without draining.
        let mut guard = rt.mutex().lock();
        for completion in std::mem::take(&mut guard.pending) {
            guard.defer_render_completion(completion, g17_completion::PairedRenderCompletion { complete: true });
        }
        assert_eq!(guard.deferred_completions.len(), 5);
        assert!(completions.iter().all(|c| c.signals.load(Ordering::SeqCst) == 0), "signalled under the mutex");
    }
    assert!(KICKS.load(Ordering::SeqCst) >= 1, "nobody was asked to signal the deferred fences");
    assert!(completions.iter().all(|c| c.signals.load(Ordering::SeqCst) == 0));
    // The queued worker turn drains them.
    assert!(rt.service_primary_firmware_events().is_ok());
    assert!(signalled_once(&completions));
}
#[test] fn settle_after_disarm_is_signalled_under_processor_stop() {
    KICKS.store(0, Ordering::SeqCst);
    let (rt, completions) = runtime(false);
    {
        let mut guard = rt.mutex().lock();
        for completion in std::mem::take(&mut guard.pending) {
            guard.defer_render_completion(completion, g17_completion::PairedRenderCompletion { complete: true });
        }
    }
    assert_eq!(KICKS.load(Ordering::SeqCst), 0, "disarmed worker was queued");
    LOCKED.store(false, Ordering::SeqCst);
    // Teardown/rebuild signals in place, the pre-deferral behaviour; the
    // mock mutex is not held.
    rt.mutex.0.borrow_mut().signal_deferred_completions_in_place();
    assert!(signalled_once(&completions));
}
#[test] fn compute_retirement_signals_its_job_once_with_the_timestamps() {
    let packet = Arc::new(g17_job::G17PComputeJobPacket { completion: Some(G17PJobCompletion { signals: AtomicUsize::new(0), timestamps: std::cell::Cell::new([0; 2]) }) });
    let mut list = KVec::default();
    G17PDeferredCompletion::Compute { packet: packet.clone(), timestamps: [7, 9] }.defer_into(&mut list);
    assert_eq!(packet.completion().unwrap().signals.load(Ordering::SeqCst), 0);
    for c in list { c.signal(); }
    assert_eq!(packet.completion().unwrap().signals.load(Ordering::SeqCst), 1);
    assert_eq!(packet.completion().unwrap().timestamps.get(), [7, 9]);
}
#[test] fn cancelled_retirement_defers_an_error_and_signals_once_after_unlocking() {
    let (rt, _) = runtime(true);
    let completion = G17PRenderCompletion::new();
    let deferred = {
        let mut guard = rt.mutex.lock();
        guard.begin_deferred_completion_drain();
        guard.defer_completion(G17PDeferredCompletion::Render {
            completion: completion.clone(), observed: Err(EIO),
        });
        assert_eq!(completion.signals.load(Ordering::SeqCst), 0);
        guard.take_deferred_completions()
    };
    for c in deferred { c.signal(); }
    assert_eq!(completion.signals.load(Ordering::SeqCst), 1);
    assert_eq!(completion.complete_seen.load(Ordering::SeqCst), 0);
}
#[test] fn no_memory_to_defer_signals_exactly_once_in_place() {
    let completion = G17PRenderCompletion::new();
    let mut list = KVec::default();
    NO_MEMORY.store(true, Ordering::SeqCst);
    G17PDeferredCompletion::Render { completion: completion.clone(), observed: Ok(g17_completion::PairedRenderCompletion { complete: true }) }.defer_into(&mut list);
    NO_MEMORY.store(false, Ordering::SeqCst);
    assert!(list.is_empty());
    assert_eq!(completion.signals.load(Ordering::SeqCst), 1);
}
'''
with tempfile.TemporaryDirectory() as tmp:
    tmp = Path(tmp)
    src, exe = tmp / 'deferred.rs', tmp / 'deferred'
    src.write_text(source)
    subprocess.run(['rustc', '--edition=2021', '--test', str(src), '-o', str(exe)], check=True)
    subprocess.run([str(exe), '--test-threads=1'], check=True)
print('PASS deferred completion signalling: success fences signal after the runtime mutex is released, '
      'exactly once, drained by worker wrappers or a kicked worker turn, or in place at teardown')
