#!/usr/bin/env python3
"""Compile actual admission/query methods; mock waits and GPU observations only.

This proves host exclusion/rollback, not a firmware ordering requirement.
"""
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def item(source, marker):
    start = source.index(marker)
    end = source.index('{', start) + 1
    depth = 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[start:end]


drm = (ROOT / 'g17_drm.rs').read_text()
manager = (ROOT / 'g17_manager.rs').read_text()
runtime = (ROOT / 'g17_live_boot.rs').read_text()
async_source = (ROOT / 'g17_async_compute.rs').read_text()
params = (ROOT / 'asahi.rs').read_text()
assert 'default: 0' in item(params, 'g17p_render_transition_probe: u32')
assert 'default: 0' in item(params, 'g17p_render_transition_class: u32')
submit = drm[drm.index('let mut transition = None;'):]
assert submit.index('g17p_render_transition_probe.value() != 0') < submit.index('runtime.render_transition_reasons(')
assert submit.index('drop(runtime);') < submit.index('acquire_render_transition(render_slot)')
assert submit.index('drop(transition);') > submit.index('break runtime.submit_translated_render(')
assert submit.index('drop(transition);') < submit.index('packet.mark_published()')
service = item(async_source, 'fn service_async_compute_completions_matching(')
error_path = service[service.index('Err(error) => {'):]
assert error_path.index('admission-at-retirement-error') < error_path.index('error == ENODATA')
assert 'active._lease.admission_snapshot()' in error_path

preamble = r'''
#![allow(dead_code)]
use std::sync::{Arc, Mutex as StdMutex, Condvar as StdCondvar};
use std::sync::atomic::{AtomicU32, Ordering};
use std::{thread, time::Duration, cell::Cell, ops::{Deref,DerefMut}};
type Result<T=()> = std::result::Result<T, i32>;
const EIO:i32=-5; const EBUSY:i32=-16; const ERESTARTSYS:i32=-512;
const ENOSPC:i32=-28; const ENODEV:i32=-19; const EOVERFLOW:i32=-75; const EINVAL:i32=-22; const ETIMEDOUT:i32=-110;
const G17P_RENDER_SLOTS:usize=64; const G17P_ACTIVE_RENDER_SLOTS:usize=64;
macro_rules! pr_info { ($($arg:tt)*) => { let _ = format_args!($($arg)*); }; }
struct Monotonic;
struct Instant<T>(std::time::Instant, std::marker::PhantomData<T>);
impl<T> Copy for Instant<T> {}
impl<T> Clone for Instant<T> {fn clone(&self)->Self{*self}}
impl<T> Instant<T> {
 fn now()->Self { Self(std::time::Instant::now(),std::marker::PhantomData) }
 fn elapsed(&self)->Duration { self.0.elapsed() }
}
struct Delta;impl Delta{fn from_millis(_:i64)->Self{Self}}
fn fsleep(_:Delta){}
#[derive(Default)]struct Mutex<T>(StdMutex<T>);
struct Guard<'a,T>(Option<std::sync::MutexGuard<'a,T>>);
impl<T> Mutex<T> { fn lock(&self)->Guard<'_,T> { Guard(Some(self.0.lock().unwrap())) } }
impl<T> Deref for Guard<'_,T> { type Target=T; fn deref(&self)->&T { self.0.as_ref().unwrap() } }
impl<T> DerefMut for Guard<'_,T> { fn deref_mut(&mut self)->&mut T { self.0.as_mut().unwrap() } }
thread_local! { static INTERRUPT: Cell<bool> = const {Cell::new(false)}; }
#[derive(Default)]struct Condvar { cv:StdCondvar, waits:AtomicU32, wakes:AtomicU32 }
impl Condvar {
 fn wait_interruptible<T>(&self,g:&mut Guard<'_,T>)->bool {
  self.waits.fetch_add(1,Ordering::SeqCst);
  if INTERRUPT.with(|v|v.replace(false)) { return true; }
  let (held,timed)=self.cv.wait_timeout(g.0.take().unwrap(),Duration::from_secs(3)).unwrap();
  g.0=Some(held);
  assert!(!timed.timed_out(),"admission deadlocked");
  false
 }
 fn notify_all(&self) { self.wakes.fetch_add(1,Ordering::SeqCst); self.cv.notify_all(); }
}
#[derive(Default)]struct G17PQueueRegistry {
 lease:Mutex<G17PQueueLeaseState>, lease_free:Condvar,
 compute_readers:AtomicU32, compute_faulted:AtomicU32,
}
fn until(mut condition:impl FnMut()->bool) {
 let deadline=std::time::Instant::now()+Duration::from_secs(2);
 while !condition() { assert!(std::time::Instant::now()<deadline,"condition did not arrive"); thread::yield_now(); }
}
fn wait_entered(r:&G17PQueueRegistry, after:u32) {
 until(||r.lease_free.waits.load(Ordering::SeqCst)>after);
 // Taking the mutex also proves the waiter released it into the condvar.
 drop(r.lease.lock());
}
mod mmu {
 pub struct Vm(pub u32);
 pub struct T8140ComputeExecutionContext(pub u32);
 impl T8140ComputeExecutionContext { pub fn context_id(&self)->u32 {self.0} }
}
mod g17_uapi { pub struct TranslatedRenderCommand { pub parameters:usize } }
mod g17_resources {
 pub const G17P_RENDER_MAX_IN_FLIGHT:usize=2;
 pub use crate::mmu::T8140ComputeExecutionContext as G17PLogicalQueueContext;
 pub struct G17PRenderQueuePair;
 impl G17PRenderQueuePair {pub const SLOT_COUNT:u8=64;}
 pub fn g17p_render_tvb_mode()->u32 {2}
 pub fn required_render_tvb_blocks(p:&usize,_clusters:u32)->Option<usize> {
  (*p!=usize::MAX).then_some(*p)
 }
}
mod g17_queue_limits { pub const MAX_IN_FLIGHT_PER_QUEUE:u32=2; }
struct Bind(u32);impl Bind {fn matches(&self,vm:&mmu::Vm)->bool {self.0==vm.0}}
struct Queue {registered:bool}
struct Queues {tiling:Queue,fragment:Queue}
struct Resources {owned:bool}
impl Resources {fn render_flist_owned(&self)->bool{self.owned} fn uat(&self){} }
struct Storage {context:u16,context_update_pending:bool,qos_update_pending:bool,unreachable:bool,desynced:bool,capacity:usize,error:bool}
impl Storage {
 fn descriptor_context_id(&self)->u16{self.context}
 fn context_update_pending(&self)->bool{self.context_update_pending}
 fn first_unreachable(&self,_uat:(),_vm:&mmu::Vm)->Option<()> {self.unreachable.then_some(())}
 fn restage_admission_desynced(&mut self,_ordinal:u64)->Result<bool>{if self.error{Err(EIO)}else{Ok(self.desynced)}}
 fn tvb_capacity(&self)->usize{self.capacity}
}
struct Manager {
 render_bind:Option<Bind>,render_sksm:Option<Queues>,render_storage:Option<Storage>,
 resources:Resources,num_clusters:u32,render_submission_ordinal:u64,selected:u8,
 retired_render_handoff_graph:Option<Graph>,retired_render_handoff_graphs:Vec<Graph>,
}
struct Graph {storage:Storage}
impl Manager {
 fn stable()->Self {Self{
  render_bind:Some(Bind(1)),render_sksm:Some(Queues{tiling:Queue{registered:true},fragment:Queue{registered:true}}),
  render_storage:Some(Storage{context:7,context_update_pending:false,qos_update_pending:false,unreachable:false,desynced:false,capacity:16,error:false}),
  resources:Resources{owned:true},num_clusters:1,render_submission_ordinal:4,selected:0,
  retired_render_handoff_graph:None,retired_render_handoff_graphs:Vec::new(),
 }}
 fn select_render_slot(&mut self,slot:u8)->Result {if slot>=2{return Err(EINVAL)}self.selected=slot;Ok(())}
 fn retained_render_has_other_owner(&self,vm:&mmu::Vm,context:u16)->bool {
  self.render_storage.as_ref().is_some_and(|s|s.context!=context || !self.render_bind.as_ref().is_some_and(|b|b.matches(vm)))
 }
 fn select_cached_render_graph(&self,_vm:&mmu::Vm,_context:u16)->(bool,Option<usize>) {
  (self.retired_render_handoff_graph.is_some(),(!self.retired_render_handoff_graphs.is_empty()).then_some(0))
 }
 fn query(&mut self)->Result<u32> {
  self.render_transition_reasons(&mmu::Vm(1),1,&mmu::T8140ComputeExecutionContext(7),&g17_uapi::TranslatedRenderCommand{parameters:8})
 }
}
struct Runtime {crashed:bool,manager:Option<Manager>}
impl Runtime {fn is_crashed(&self)->bool{self.crashed}}
'''

production = item(manager, 'pub(crate) mod render_transition {')
production += 'mod g17_manager {pub(crate) use super::render_transition;}\n'
production += '#[derive(Default)]\n' + item(drm, 'struct G17PComputeBurstState {')
production += item(drm, 'struct G17PQueueLeaseState {')
production += item(drm, 'impl Default for G17PQueueLeaseState {')
production += item(drm, 'impl G17PQueueLeaseState {')
production += 'impl G17PQueueRegistry {\n' + '\n'.join(item(drm, n) for n in [
    'fn acquire_compute(', 'fn acquire_render_transition(', 'fn acquire_lease(',
]) + '\n}\n'
production += '\n'.join(item(drm, n) for n in [
    'pub(crate) struct G17PComputeLeaseGuard', 'impl G17PComputeLeaseGuard {',
    'impl Drop for G17PComputeLeaseGuard {', 'struct G17PLeaseGuard {',
    'impl Drop for G17PLeaseGuard {', 'struct G17PRenderTransitionGuard {',
    'impl Drop for G17PRenderTransitionGuard {',
])
production += 'impl Manager {\n' + item(manager, 'pub(crate) fn render_transition_reasons(') + '\n}\n'
production += 'impl Runtime {\n' + item(runtime, 'pub(crate) fn render_transition_reasons(') + '\n}\n'
production += 'impl Storage {\n' + item((ROOT / 'g17_resources.rs').read_text(), 'pub(crate) fn complete_context_update(') + '\n}\n'
loop_start = drm.index('let mut transition = None;')
loop_end = drm.index('drop(transition);', loop_start) + len('drop(transition);')
publish_loop = drm[loop_start:loop_end]
publish_loop = 'let mut failure_stage = "model";\n' + publish_loop
publish_loop = publish_loop.replace('packet.cancel_requested()', 'false')
publish_loop = publish_loop.replace('packet.command()', '&translated')
publish_loop = publish_loop.replace('ECANCELED', 'EIO')
production += '''
mod module_parameters {
 pub struct Param(pub super::AtomicU32);
 impl Param {pub fn value(&self)->Box<u32>{Box::new(self.0.load(super::Ordering::SeqCst))}}
 #[allow(non_upper_case_globals)]
 pub static g17p_render_transition_probe:Param=Param(super::AtomicU32::new(0));
 #[allow(non_upper_case_globals)]
 pub static g17p_render_transition_class:Param=Param(super::AtomicU32::new(0));
 #[allow(non_upper_case_globals)]
 pub static g17p_compute_burst_probe:Param=Param(super::AtomicU32::new(0));
}
struct PublishRuntime {registry:Arc<G17PQueueRegistry>,query_count:u32,publish_count:u32,failure:bool,control_backpressured:bool,reasons:Result<u32>,after_drain:Option<Result<u32>>}
impl PublishRuntime {
 fn render_usc_freelist_release_pending_for_slot(&mut self,_slot:u8)->Result<bool>{Ok(false)}
 fn primary_device_control_backpressured(&self)->bool{self.control_backpressured}
 fn render_transition_reasons(&mut self,_vm:&mmu::Vm,_slot:u8,_ctx:&mmu::T8140ComputeExecutionContext,_cmd:&g17_uapi::TranslatedRenderCommand)->Result<u32>{
  self.query_count+=1;
  if self.query_count>1 {if let Some(next)=self.after_drain.take(){self.reasons=next;}}
  self.reasons
 }
 fn submit_translated_render(&mut self,_vm:&mmu::Vm,_slot:u8,_defer:bool,_ctx:Arc<mmu::T8140ComputeExecutionContext>,_cmd:&g17_uapi::TranslatedRenderCommand,_completion:())->Result {
  let enabled=*module_parameters::g17p_render_transition_probe.value()!=0 && render_transition::selected(self.reasons.unwrap_or(render_transition::UNKNOWN),*module_parameters::g17p_render_transition_class.value());
  assert_eq!(self.registry.lease.lock().render_transition,enabled);
  assert_eq!(self.registry.compute_readers.load(Ordering::Acquire),u32::from(self.query_count<2));
  self.publish_count+=1;if self.failure {Err(EIO)}else{Ok(())}
 }
}
struct RuntimeRef(Mutex<PublishRuntime>);
impl RuntimeRef {fn mutex(&self)->&Mutex<PublishRuntime>{&self.0}}
struct Submitter {runtime:RuntimeRef,registry:Arc<G17PQueueRegistry>,vm:mmu::Vm,context:Arc<mmu::T8140ComputeExecutionContext>}
impl Submitter {
 fn new(registry:Arc<G17PQueueRegistry>,failure:bool)->Self {
  Self{runtime:RuntimeRef(Mutex(StdMutex::new(PublishRuntime{registry:registry.clone(),query_count:0,publish_count:0,failure,control_backpressured:false,reasons:Ok(2),after_drain:None}))),registry,vm:mmu::Vm(1),context:Arc::new(mmu::T8140ComputeExecutionContext(7))}
 }
 fn publish(&self)->Result {
  let translated=g17_uapi::TranslatedRenderCommand{parameters:8};let render_slot=1;let completion=();let submission_id=1;
''' + publish_loop + '\n submit\n}\n}\n'

tests = r'''
#[test]fn stable_jobs_overlap_without_an_execution_cap() {
 let r=Arc::new(G17PQueueRegistry::default());
 let a=r.acquire_lease(1).unwrap();let b=r.acquire_lease(2).unwrap();
 let readers:Vec<_>=(0..12).map(|_|r.acquire_compute(false).unwrap()).collect();
 assert_eq!(readers[0].admission_snapshot(),(12,6,false,false,0));
 drop(readers);drop(a);drop(b);
 assert_eq!(r.compute_readers.load(Ordering::Acquire),0);
}
#[test]fn intent_blocks_new_readers_before_existing_readers_drain() {
 let r=Arc::new(G17PQueueRegistry::default());let render=r.acquire_lease(1).unwrap();
 let old=r.acquire_compute(false).unwrap();
 let writer={let r=r.clone();thread::spawn(move||r.acquire_render_transition(1))};
 until(||r.lease.lock().render_transition);
 let n=r.lease_free.waits.load(Ordering::SeqCst);
 let (tx,rx)=std::sync::mpsc::channel();
 let newcomer={let r=r.clone();thread::spawn(move||{let lease=r.acquire_compute(false).unwrap();tx.send(()).unwrap();lease})};
 wait_entered(&r,n);assert!(rx.try_recv().is_err());
 drop(old);let transition=writer.join().unwrap().unwrap();
 assert_eq!(r.compute_readers.load(Ordering::Acquire),0);assert!(rx.try_recv().is_err());
 // Another stable render is allowed; only graph mutations exclude compute.
 let other_render=r.acquire_lease(2).unwrap();
 drop(transition);rx.recv_timeout(Duration::from_secs(2)).unwrap();
 let new=newcomer.join().unwrap();assert_eq!(new.admission_snapshot(),(1,6,false,false,0));
 drop(new);drop(render);drop(other_render);
}
#[test]fn waiting_first_install_cannot_deadlock_render_transition() {
 let r=Arc::new(G17PQueueRegistry::default());let render=r.acquire_lease(1).unwrap();
 let compute=r.acquire_compute(false).unwrap();
 let installer={let r=r.clone();thread::spawn(move||r.acquire_compute(true))};
 wait_entered(&r,0);assert!(!r.lease.lock().compute_install);
 let writer={let r=r.clone();thread::spawn(move||r.acquire_render_transition(1))};
 until(||r.lease.lock().render_transition);drop(compute);
 let transition=writer.join().unwrap().unwrap();
 assert!(!r.lease.lock().compute_install);
 drop(transition);assert!(!r.lease.lock().compute_install);
 drop(render);let installed=installer.join().unwrap().unwrap();
 assert!(r.lease.lock().compute_install);drop(installed);
}
#[test]fn interruption_during_drain_rolls_back_intent_and_wakes() {
 let r=Arc::new(G17PQueueRegistry::default());let _render=r.acquire_lease(1).unwrap();
 let old=r.acquire_compute(false).unwrap();let wakes=r.lease_free.wakes.load(Ordering::SeqCst);
 INTERRUPT.with(|v|v.set(true));assert!(matches!(r.acquire_render_transition(1),Err(ERESTARTSYS)));
 assert!(!r.lease.lock().render_transition);assert!(r.lease_free.wakes.load(Ordering::SeqCst)>wakes);
 let next=r.acquire_compute(false).unwrap();assert_eq!(next.admission_snapshot().0,2);drop(old);
}
#[test]fn interrupted_competing_writer_cannot_clear_current_writer() {
 let r=Arc::new(G17PQueueRegistry::default());let _a=r.acquire_lease(1).unwrap();let _b=r.acquire_lease(2).unwrap();
 let first=r.acquire_render_transition(1).unwrap();let wakes=r.lease_free.wakes.load(Ordering::SeqCst);
 INTERRUPT.with(|v|v.set(true));assert!(matches!(r.acquire_render_transition(2),Err(ERESTARTSYS)));
 assert!(r.lease.lock().render_transition);assert_eq!(r.lease_free.wakes.load(Ordering::SeqCst),wakes);
 drop(first);assert!(!r.lease.lock().render_transition);
}
#[test]fn second_writer_wakes_after_first_publication() {
 let r=Arc::new(G17PQueueRegistry::default());let _a=r.acquire_lease(1).unwrap();let _b=r.acquire_lease(2).unwrap();
 let first=r.acquire_render_transition(1).unwrap();
 let second={let r=r.clone();thread::spawn(move||r.acquire_render_transition(2))};
 wait_entered(&r,0);drop(first);let second=second.join().unwrap().unwrap();
 assert!(r.lease.lock().render_transition);drop(second);
}
#[test]fn fault_while_draining_rolls_back_but_preserves_reader_ownership() {
 let r=Arc::new(G17PQueueRegistry::default());let _render=r.acquire_lease(1).unwrap();
 let compute=r.acquire_compute(false).unwrap();
 let writer={let r=r.clone();thread::spawn(move||r.acquire_render_transition(1))};
 wait_entered(&r,0);compute.quarantine();assert!(matches!(writer.join().unwrap(),Err(EIO)));
 assert_eq!(compute.admission_snapshot(),(1,2,false,false,0));
 assert!(matches!(r.acquire_compute(false),Err(EIO)));
 compute.rehabilitate_after_processor_stop();drop(r.acquire_compute(false).unwrap());
}
fn publish(r:&Arc<G17PQueueRegistry>,failure:bool)->Result {
 let _transition=r.acquire_render_transition(1)?;
 if failure{return Err(EIO)}Ok(())
}
#[test]fn host_publication_success_and_failure_release_before_render_retirement() {
 for failure in [false,true] {
  let r=Arc::new(G17PQueueRegistry::default());let _render=r.acquire_lease(1).unwrap();
  assert_eq!(publish(&r,failure),if failure{Err(EIO)}else{Ok(())});
  let compute=r.acquire_compute(false).unwrap();assert_eq!(compute.admission_snapshot(),(1,2,false,false,0));
 }
}
#[test]fn missing_or_invalid_render_slot_cannot_claim_transition() {
 let r=Arc::new(G17PQueueRegistry::default());
 for slot in [0,2,255] {assert!(matches!(r.acquire_render_transition(slot),Err(EBUSY)));}
 assert!(!r.lease.lock().render_transition);
}
#[test]fn actual_query_leaves_stable_graph_unrestricted_and_detects_mutations() {
 let mut stable=Manager::stable();assert_eq!(stable.query(),Ok(0));assert_eq!(stable.selected,1);
 for cause in 0..12 {
  let mut m=Manager::stable();
  match cause {
   0=>m.render_bind=None,1=>m.render_bind=Some(Bind(2)),2=>m.render_sksm=None,
   3=>m.render_sksm.as_mut().unwrap().tiling.registered=false,
   4=>m.render_sksm.as_mut().unwrap().fragment.registered=false,
   5=>m.resources.owned=false,6=>m.render_storage=None,
   7=>m.render_storage.as_mut().unwrap().context=8,
   8=>m.render_storage.as_mut().unwrap().context_update_pending=true,
   9=>m.render_storage.as_mut().unwrap().unreachable=true,
   10=>m.render_storage.as_mut().unwrap().desynced=true,
   11=>m.render_storage.as_mut().unwrap().capacity=7,_=>unreachable!(),
  }
  // Cause 5 toggles the retired device-global FList flag. Private render
  // owners are graph-local now, so that flag must not force a transition.
  let expected=[6,6,1,1,1,0,12,2,2,64,36,16][cause];
  assert_eq!(m.query(),Ok(expected),"wrong transition cause {cause}");
 }
 let mut m=Manager::stable();m.render_submission_ordinal=u64::MAX;assert_eq!(m.query(),Ok(128));
}
#[test]fn query_propagates_unavailable_or_invalid_observations() {
 let mut m=Manager::stable();m.render_storage.as_mut().unwrap().error=true;assert_eq!(m.query(),Err(EIO));
 let mut r=Runtime{crashed:false,manager:None};let vm=mmu::Vm(1);let ctx=mmu::T8140ComputeExecutionContext(7);
 let cmd=g17_uapi::TranslatedRenderCommand{parameters:8};
 assert_eq!(r.render_transition_reasons(&vm,0,&ctx,&cmd),Err(ENODEV));
 r.crashed=true;assert_eq!(r.render_transition_reasons(&vm,0,&ctx,&cmd),Ok(128));
 r.crashed=false;r.manager=Some(Manager::stable());
 assert_eq!(r.render_transition_reasons(&vm,2,&ctx,&cmd),Err(EINVAL));
 assert_eq!(r.render_transition_reasons(&vm,0,&mmu::T8140ComputeExecutionContext(65536),&cmd),Err(EOVERFLOW));
 assert_eq!(r.render_transition_reasons(&vm,0,&ctx,&g17_uapi::TranslatedRenderCommand{parameters:usize::MAX}),Err(EINVAL));
 assert_eq!(r.render_transition_reasons(&vm,0,&ctx,&cmd),Ok(0));
}
#[test]fn production_submit_loop_defaults_off_and_drops_runtime_lock_before_drain() {
 // Only this test changes the parameter. All other tests exercise registry or
 // query methods directly, independently of the global qualification switch.
 for mode in [0,1,2] { for failure in [false,true] {
  let enabled=mode==2;
  module_parameters::g17p_render_transition_probe.0.store(u32::from(mode!=0),Ordering::SeqCst);
  module_parameters::g17p_render_transition_class.0.store(if enabled{2}else{0},Ordering::SeqCst);
  let r=Arc::new(G17PQueueRegistry::default());let _render=r.acquire_lease(1).unwrap();
  let old=r.acquire_compute(false).unwrap();let s=Arc::new(Submitter::new(r.clone(),failure));
  let publication={let s=s.clone();thread::spawn(move||s.publish())};
  if enabled {
   until(||r.lease.lock().render_transition);
   assert!(s.runtime.0.0.try_lock().is_ok(),"runtime lock must be free while draining compute");
   drop(old);
  }
  assert_eq!(publication.join().unwrap(),if failure{Err(EIO)}else{Ok(())});
  assert!(!r.lease.lock().render_transition);
  let state=s.runtime.mutex().lock();assert_eq!(state.query_count,mode);assert_eq!(state.publish_count,1);drop(state);
  // The render still owns its physical slot, yet compute can enter again.
  drop(r.acquire_compute(false).unwrap());
 }}
 // Neither compound observations nor query errors may drain or reject work.
 module_parameters::g17p_render_transition_probe.0.store(1,Ordering::SeqCst);
 module_parameters::g17p_render_transition_class.0.store(2,Ordering::SeqCst);
 for reasons in [Ok(0),Ok(1),Ok(6),Ok(128),Err(EIO)] {
  let r=Arc::new(G17PQueueRegistry::default());let _render=r.acquire_lease(1).unwrap();
  let _old=r.acquire_compute(false).unwrap();let s=Submitter::new(r.clone(),false);
  s.runtime.mutex().lock().reasons=reasons;
  assert_eq!(s.publish(),Ok(()));assert_eq!(s.runtime.mutex().lock().query_count,1);
 }
 // A second observation after draining can expose a compound change. Drop
 // the guard before publication rather than silently widening the experiment.
 for next in [Ok(6),Ok(0),Err(EIO)] {
  let r=Arc::new(G17PQueueRegistry::default());let _render=r.acquire_lease(1).unwrap();
  let old=r.acquire_compute(false).unwrap();let s=Arc::new(Submitter::new(r.clone(),false));
  s.runtime.mutex().lock().after_drain=Some(next);
  let publisher={let s=s.clone();thread::spawn(move||s.publish())};
  until(||r.lease.lock().render_transition);drop(old);
  assert_eq!(publisher.join().unwrap(),Ok(()));assert_eq!(s.runtime.mutex().lock().query_count,3);
  assert!(!r.lease.lock().render_transition);
 }
 module_parameters::g17p_render_transition_probe.0.store(0,Ordering::SeqCst);
 module_parameters::g17p_render_transition_class.0.store(0,Ordering::SeqCst);
}
#[test]fn completed_retained_render_has_no_mutation() {
 let mut m=Manager::stable();m.render_storage.as_mut().unwrap().context_update_pending=true;
 m.render_storage.as_mut().unwrap().qos_update_pending=true;
 assert_eq!(m.query(),Ok(render_transition::CONTEXT_HANDOFF));
 // Use the actual completion method, not a hand-written pending=false model.
 m.render_storage.as_mut().unwrap().complete_context_update();
 assert!(!m.render_storage.as_ref().unwrap().qos_update_pending);
 for ordinal in 5..1005 {m.render_submission_ordinal=ordinal;assert_eq!(m.query(),Ok(0));}
}
#[test]fn cached_handoff_uses_incoming_capacity_and_does_not_invent_pb_creation_or_desync() {
 for immediate in [false,true] {
  let mut m=Manager::stable();m.render_bind=Some(Bind(2));
  m.render_storage.as_mut().unwrap().capacity=1;
  m.render_storage.as_mut().unwrap().desynced=true;
  let graph=Graph{storage:Manager::stable().render_storage.take().unwrap()};
  if immediate {m.retired_render_handoff_graph=Some(graph)}else{m.retired_render_handoff_graphs.push(graph)}
  assert_eq!(m.query(),Ok(render_transition::CONTEXT_HANDOFF));
  if immediate {m.retired_render_handoff_graph.as_mut().unwrap().storage.capacity=1;}
  else {m.retired_render_handoff_graphs[0].storage.capacity=1;}
  assert_eq!(m.query(),Ok(render_transition::CONTEXT_HANDOFF|render_transition::TVB_GROWTH));
 }
}
#[test]fn only_exact_single_known_class_can_drain() {
 for class in [1,2,4,8,16,32] {
  for observed in 0..512 {
   assert_eq!(render_transition::selected(observed,class),observed==class);
  }
 }
 for class in [0,3,7,63,64,128,256,u32::MAX] {
  assert!(!render_transition::selected(class,class));
 }
}
'''

if __name__ == '__main__':
    with tempfile.TemporaryDirectory() as directory:
        source = Path(directory) / 'transition.rs'
        source.write_text(preamble + production + tests)
        binary = Path(directory) / 'transition'
        subprocess.run(['rustc', '--edition=2021', '--test', str(source), '-o', str(binary)], check=True)
        subprocess.run([str(binary), '--nocapture'], check=True, timeout=20)
