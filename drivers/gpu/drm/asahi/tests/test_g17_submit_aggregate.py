#!/usr/bin/env python3
"""Run the actual counted aggregate with fake fences; no GPU or wait thread."""
from pathlib import Path
from g17_work_state_test_support import tsq_backing
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
source = (root / 'g17_drm.rs').read_text()


def block(marker):
    start = source.index(marker)
    end = source.index('{', start) + 1
    depth = 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[start:end]


preamble = r'''
#![allow(dead_code)]
use std::sync::atomic::{AtomicU32,AtomicI32,Ordering};
#[derive(Clone,Copy,Debug,PartialEq)]struct Error(i32);
impl Error {fn to_errno(self)->i32{self.0}fn from_errno(e:i32)->Self{Self(e)}}
type Result<T=()>=std::result::Result<T,Error>;
const EINVAL:Error=Error(-22);const ENOSPC:Error=Error(-28);const EOVERFLOW:Error=Error(-75);
const ECANCELED:Error=Error(-125);const EBUSY:Error=Error(-16);const GFP_KERNEL:()=();
struct Arc<T>(std::sync::Arc<T>);
impl<T>Arc<T>{
 fn new(value:T,_flags:())->Result<Self>{Ok(Self(std::sync::Arc::new(value)))}
 fn pin_init(value:T,flags:())->Result<Self>{Self::new(value,flags)}
}
struct Mutex<T>(std::sync::Mutex<T>);
impl<T>Mutex<T>{fn lock(&self)->std::sync::MutexGuard<'_,T>{self.0.lock().unwrap()}}
macro_rules! new_mutex {($value:expr,$name:expr)=>{Mutex(std::sync::Mutex::new($value))}}
macro_rules! pin_init {($($value:tt)*)=>{$($value)*}}
impl<T>Clone for Arc<T>{fn clone(&self)->Self{Self(self.0.clone())}}
impl<T>std::ops::Deref for Arc<T>{type Target=T;fn deref(&self)->&T{&self.0}}
#[derive(Default)]struct Signals{signals:AtomicU32,error:AtomicI32}
struct UserFence<T>{state:std::sync::Arc<Signals>,_p:std::marker::PhantomData<T>}
impl<T>UserFence<T>{
 fn set_error(&self,error:Error){self.state.error.store(error.0,Ordering::Release);}
 fn signal(&self){self.state.signals.fetch_add(1,Ordering::AcqRel);}
}
struct Fence(std::sync::Arc<Signals>);
impl Fence{fn from_fence<T>(f:&UserFence<T>)->Self{Self(f.state.clone())}}
struct G17PSubmissionFence;
'''
actual = '\n'.join(block(marker) for marker in [
    'pub(crate) struct G17PSubmitCompletion {', 'impl G17PSubmitCompletion {',
    'pub(crate) struct G17PSubmitCompletionMember {', 'impl G17PSubmitCompletionMember {',
    'impl Drop for G17PSubmitCompletionMember {',
])
# Kernel pin initialization is compile-checked by the module build. Host tests
# use std mutex backing while retaining the production completion/lease bodies.
actual = actual.replace('#[pin]', '').replace(
    'work_state <- new_mutex!', 'work_state: new_mutex!')
saved_source = source
source = (root / 'g17_resources.rs').read_text()
lease = '\n'.join(block(marker) for marker in [
    'pub(crate) struct G17PWorkStateLease {', 'impl G17PWorkStateLease {',
    'impl Drop for G17PWorkStateLease {',
])
source = saved_source
# The e2e timeline ring is disarmed here (stamp() == 0), as by default.
preamble += '\nmod g17_e2e {pub const K_AGG_SIGNAL:u8=19;pub fn stamp()->u64{0}pub fn now()->u64{0}pub fn record(_:u8,_:u32,_:u64,_:[u64;5]){}}\n'
preamble += f'\n#[path = "{root / "g17_work_state.rs"}"] mod g17_work_state;\n'
preamble += tsq_backing()
preamble += 'mod g17_resources {use super::*;pub struct G17PLogicalQueueContext {pub work_states:g17_work_state::WorkStatePool,pub tsq:TsqLock,pub tsq_base:u64}\n' + lease + '\nimpl G17PLogicalQueueContext {pub fn tsq_head_gpu_va(&self)->u64{0x18000}pub fn tsq_head_snapshot(&self)->Result<[u64;2]>{Ok([0,0])}}\n}\n'
tests = r'''
fn context()->Arc<g17_resources::G17PLogicalQueueContext>{Arc::new(g17_resources::G17PLogicalQueueContext{work_states:g17_work_state::WorkStatePool::new(),tsq:TsqLock::new(),tsq_base:0x10000},()).unwrap()}
fn aggregate_with_context(c:Arc<g17_resources::G17PLogicalQueueContext>)->Arc<G17PSubmitCompletion>{G17PSubmitCompletion::new(UserFence{
 state:std::sync::Arc::new(Signals::default()),_p:std::marker::PhantomData},c,0).unwrap()}
fn aggregate()->Arc<G17PSubmitCompletion>{aggregate_with_context(context())}
#[test]fn concurrent_engine_acquisition_installs_one_key_and_returns_losing_reservations(){
 let context=context();let state=g17_resources::G17PWorkStateLease::deferred(context.clone());
 let barrier=std::sync::Barrier::new(16);
 std::thread::scope(|scope| {let mut workers=Vec::new();for _ in 0..16 {
  let state=&state;let barrier=&barrier;workers.push(scope.spawn(move||{barrier.wait();state.acquire().unwrap()}));
 }let keys:Vec<_>=workers.into_iter().map(|worker|worker.join().unwrap()).collect();assert!(keys.iter().all(|key|*key==state.word()));});
 let rest:Vec<_>=(0..62).map(|_|g17_resources::G17PWorkStateLease::new(context.clone()).unwrap()).collect();
 assert!(!context.work_states.has_free());drop(state);assert!(context.work_states.has_free());drop(rest);
}
#[test]fn engines_share_one_lazy_key_and_error_fence_does_not_release_it(){
 let context=context();let a=aggregate_with_context(context.clone());
 let render=G17PSubmitCompletionMember::new(a.clone());let compute=G17PSubmitCompletionMember::new(a.clone());
 let untouched:Vec<_>=(0..63).map(|_|g17_resources::G17PWorkStateLease::new(context.clone()).unwrap()).collect();drop(untouched);
 let key=render.work_state().unwrap().acquire().unwrap();assert_eq!(compute.work_state().unwrap().acquire(),Ok(key));
 let runtime_owner=render.work_state().unwrap();
 a.finish_enqueue(Ok(()));render.complete(Err(Error(-5)));compute.complete(Err(Error(-5)));
 assert_eq!(signals(&a),1);drop(render);drop(compute);drop(a);
 let others:Vec<_>=(0..62).map(|_|g17_resources::G17PWorkStateLease::new(context.clone()).unwrap()).collect();
 assert!(!context.work_states.has_free());drop(runtime_owner);
 assert!(g17_resources::G17PWorkStateLease::new(context.clone()).is_ok());drop(others);
}
fn signals(a:&G17PSubmitCompletion)->u32{a.fence.state.signals.load(Ordering::Acquire)}
fn error(a:&G17PSubmitCompletion)->i32{a.fence.state.error.load(Ordering::Acquire)}
#[test]fn all_commands_across_both_engines_must_complete_in_any_order(){
 let a=aggregate();let render_a=G17PSubmitCompletionMember::new(a.clone());
 let render_b=G17PSubmitCompletionMember::new(a.clone());let compute=G17PSubmitCompletionMember::new(a.clone());
 a.finish_enqueue(Ok(()));compute.complete(Ok(()));render_b.complete(Ok(()));assert_eq!(signals(&a),0);
 render_a.complete(Ok(()));assert_eq!(signals(&a),1);assert_eq!(error(&a),0);
 drop((render_a,render_b,compute));assert_eq!(signals(&a),1);
}
#[test]fn completion_before_enqueue_finishes_cannot_signal_early(){
 let a=aggregate();let r=G17PSubmitCompletionMember::new(a.clone());r.complete(Ok(()));
 assert_eq!(signals(&a),0);a.finish_enqueue(Ok(()));assert_eq!(signals(&a),1);
}
#[test]fn earlier_error_survives_later_success(){
 let a=aggregate();let first=G17PSubmitCompletionMember::new(a.clone());let last=G17PSubmitCompletionMember::new(a.clone());
 first.complete(Err(Error(-5)));last.complete(Ok(()));a.finish_enqueue(Ok(()));
 assert_eq!(signals(&a),1);assert_eq!(error(&a),-5);
}
#[test]fn partial_enqueue_failure_covers_queued_work_and_actual_error(){
 let a=aggregate();let queued=G17PSubmitCompletionMember::new(a.clone());
 drop(G17PSubmitCompletionMember::new(a.clone())); // failed construction
 a.finish_enqueue(Err(Error(-12)));assert_eq!(signals(&a),0);
 queued.complete(Ok(()));assert_eq!(signals(&a),1);assert_eq!(error(&a),-12);
}
#[test]fn first_enqueue_failure_does_not_strand_exported_fence(){
 let a=aggregate();drop(G17PSubmitCompletionMember::new(a.clone()));a.finish_enqueue(Err(Error(-12)));
 assert_eq!(signals(&a),1);assert_eq!(error(&a),-12);
}
#[test]fn cancellation_and_duplicate_callback_count_once(){
 let a=aggregate();let r=G17PSubmitCompletionMember::new(a.clone());a.finish_enqueue(Ok(()));
 r.complete(Err(ECANCELED));r.complete(Ok(()));drop(r);assert_eq!(signals(&a),1);assert_eq!(error(&a),-125);
}
#[test]fn concurrent_callbacks_preserve_failure_and_signal_once(){
 let a=aggregate();let members=(0..64).map(|_|G17PSubmitCompletionMember::new(a.clone())).collect::<Vec<_>>();
 a.finish_enqueue(Ok(()));std::thread::scope(|scope|{for (i,m) in members.into_iter().enumerate(){
 scope.spawn(move||m.complete(if i==13{Err(Error(-5))}else{Ok(())}));}});
 assert_eq!(signals(&a),1);assert_eq!(error(&a),-5);
}

#[test]fn completed_packets_do_not_exhaust_63_native_states(){
 let context=context();let mut retained=Vec::new();
 for _ in 0..4096 {
  let a=aggregate_with_context(context.clone());let m=G17PSubmitCompletionMember::new(a.clone());
  m.work_state().unwrap().acquire().unwrap();a.finish_enqueue(Ok(()));m.complete(Ok(()));
  assert_eq!(signals(&a),1);assert_eq!(m.work_state().err(),Some(ECANCELED));
  // Models deferred scheduler free_job: both packet member and aggregate live.
  retained.push((a,m));
 }
 assert!(context.work_states.has_free());assert_eq!(retained.len(),4096);
}
#[test]fn success_keeps_runtime_owner_until_its_exact_retirement(){
 let context=context();let a=aggregate_with_context(context.clone());
 let m=G17PSubmitCompletionMember::new(a.clone());let runtime=m.work_state().unwrap();runtime.acquire().unwrap();
 a.finish_enqueue(Ok(()));m.complete(Ok(()));
 let others:Vec<_>=(0..62).map(|_|g17_resources::G17PWorkStateLease::new(context.clone()).unwrap()).collect();
 assert!(!context.work_states.has_free());drop(runtime);assert!(context.work_states.has_free());drop(others);
}
#[test]fn enqueue_gate_keeps_successful_state_for_later_members(){
 let context=context();let a=aggregate_with_context(context.clone());let first=G17PSubmitCompletionMember::new(a.clone());
 let word=first.work_state().unwrap().acquire().unwrap();first.complete(Ok(()));
 let last=G17PSubmitCompletionMember::new(a.clone());assert_eq!(last.work_state().unwrap().acquire(),Ok(word));
 a.finish_enqueue(Ok(()));assert_eq!(signals(&a),0);last.complete(Ok(()));
 assert_eq!(last.work_state().err(),Some(ECANCELED));assert!(context.work_states.has_free());
}
#[test]fn concurrent_success_releases_state_once_with_members_retained(){
 let context=context();let a=aggregate_with_context(context.clone());
 let members=(0..32).map(|_|G17PSubmitCompletionMember::new(a.clone())).collect::<Vec<_>>();
 members[0].work_state().unwrap().acquire().unwrap();a.finish_enqueue(Ok(()));
 std::thread::scope(|scope|{for member in &members {scope.spawn(move||member.complete(Ok(())));}});
 assert_eq!(signals(&a),1);assert_eq!(error(&a),0);assert_eq!(members[0].work_state().err(),Some(ECANCELED));
 assert!(context.work_states.has_free());
}
'''

# Production callback wiring and failure gate, including both engines.
# v93 AGG_FIRST (g17p_host_path bit14, default off) orders the aggregate
# before or after the hardware fence: two exclusive call sites in complete().
render = block('impl G17PRenderCompletion {')
assert render.count('self.aggregate_member.complete(') == 4
compute = (root / 'g17_job.rs').read_text()
assert compute.count('self.aggregate_member.complete(') == 4
for engine in (render, compute):
    first = engine.index('crate::g17_host_path::enabled(crate::g17_host_path::AGG_FIRST)')
    ordered = engine[first:first + 900]
    assert ordered.count('self.aggregate_member.complete(') == 2 and '} else {' in ordered
assert 'self.aggregate_member.complete(Err(ECANCELED));' in compute
guard = block("impl Drop for G17POutputFencePublication<'_> {")
assert 'aggregate.finish_enqueue(if self.succeeded { Ok(()) } else { Err(self.failure) });' in guard
dispatch = block('    fn submit_guarded(')
# Commands are published by index in place (kernel stack depth), not drained.
assert dispatch.index('publication.aggregate = Some(aggregate.clone());') < dispatch.index('for index in 0..command_count {')
assert dispatch.count('G17PSubmitCompletionMember::new(aggregate.clone())') == 2
assert 'publication.publish_async(&aggregate.fence());' in dispatch
assert 'publication.publish_async(&finished);' not in source
with tempfile.TemporaryDirectory() as directory:
    path = Path(directory) / 'aggregate.rs'
    binary = Path(directory) / 'aggregate'
    path.write_text(preamble + actual + tests)
    subprocess.run(['rustc', '--edition=2021', '--test', str(path), '-o', str(binary)], check=True)
    subprocess.run([str(binary), '--test-threads=1'], check=True, timeout=60)
