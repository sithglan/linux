#!/usr/bin/env python3
"""A client that loses its device must not leave a syncobj waiter behind.

The submit ioctl publishes an output fence on every one of its own exit paths.
That says nothing about work the same client had ALREADY handed to the
hardware: once its VM is gone nothing completes those jobs, so their fences
never signal and the client waits on the syncobj forever. Measured shape:
gnome-shell alive at idle with one thread parked in
`drm_syncobj_array_wait_timeout` and the session permanently black.

Two failure modes this must avoid while fixing that:

  * Signalling a job that is merely queued. A DRM syncobj treats an error fence
    as signalled, so a waiter woken by one goes on to read the destination
    buffer. Only jobs belonging to the lost VM may be settled, and the VM's
    sticky status has to be recorded before the fence signals.
  * Retiring a client for a recoverable refusal. Lease contention, an
    interrupted wait and a malformed command buffer all leave a healthy VM.
"""
from pathlib import Path
from g17_render_fanout_test_support import fence_state_source, retention_method, KVEC
from g17_compute_inflight_test_support import completion_mod, queue_limits_mock
import subprocess, tempfile

SOURCE = Path(__file__).resolve().parents[1]
drm = (SOURCE / 'g17_drm.rs').read_text()
async_compute = (SOURCE / 'g17_async_compute.rs').read_text()
live = (SOURCE / 'g17_live_boot.rs').read_text()
file_rs = (SOURCE / 'file.rs').read_text()


def item(source, needle):
    a = source.index(needle)
    i = source.index('{', a) + 1
    depth = 1
    while depth:
        depth += (source[i] == '{') - (source[i] == '}')
        i += 1
    return source[a:i]


# --- source-level ordering guarantees -------------------------------------
# Once an output syncobj has been looked up, userspace is waiting on it and the
# driver owes it a fence. `queue.submit()` guarantees that for itself, so no
# fallible step may sit between the lookup and the call.
submit_ioctl = item(file_rs, '    pub(crate) fn submit(')
assert submit_ioctl.index('reader.read_all(') < submit_ioctl.index('SyncItem::parse_array('), \
    'the command-buffer copy can still fail after the output syncobjs are known'
between = submit_ioctl[submit_ioctl.index('SyncItem::parse_array('):submit_ioctl.index('.submit(id, syncs,')]
for fallible in ('read_all(', 'copy_from_user', 'UserSlice::new('):
    assert fallible not in between, f'{fallible} runs after the syncobjs are parsed'

# The trait entry point must be the wrapper, so no submission path can skip the
# teardown.
wrapper = item(drm, 'impl queue::Queue for G17PQueue {')
assert 'self.submit_guarded(' in wrapper
guarded = item(drm, '    fn submit_guarded(')
assert 'self.note_submission_failure(' in guarded
assert guarded.index('self.note_submission_failure(submission_id, error);') \
    < guarded.index('publication.failed(error);'), \
    'the output fence can wake before terminal VM state is recorded'

note = item(drm, '    fn note_submission_failure(')
sweep = item(async_compute, '    pub(crate) fn fail_outstanding_jobs_for_vm(')
sweep_after_observation = item(
    async_compute, '    fn fail_outstanding_jobs_for_vm_after_compute_observation(')
completion_service = item(async_compute, '    fn service_async_compute_completions_matching(')
quarantine_compute = item(async_compute, '    pub(crate) fn quarantine_async_compute(')
# v92d: the watchdog and the recovery settle share one final-observation sink.
timeout_wrapper = item(live, '    pub(crate) fn timeout_deferred_render(')
assert 'self.fail_render_after_final_observation(' in timeout_wrapper
assert 'self.fail_render_after_final_observation(' in item(live, '    fn settle_failed_deferred_render(')
timeout_render = item(live, '    fn fail_render_after_final_observation(')
publish_compute = item(drm, '    fn publish(&self, packet: Arc<g17_job::G17PComputeJobPacket>)')
# The packet stays retained: firmware may still be executing it.
assert 'inflight = ' not in sweep_after_observation and \
    '.inflight.take()' not in sweep_after_observation, \
    'the sweep releases a packet the firmware may still own'
assert 'pending_render' in sweep_after_observation and \
    'belongs_to_vm(status)' in sweep_after_observation, \
    'terminal VM settlement does not cover outstanding renders'
assert completion_service.index('completion.fail_while_owned(error)') < \
    completion_service.index('q.terminal_fanout = Some((') < \
    completion_service.index('// Finish the acquired batch before failing peers.') < \
    completion_service.index(
        'self.fail_outstanding_jobs_for_vm_after_compute_observation('), \
    'completion-poll failure fanout runs before the acquired batch is drained'
assert quarantine_compute.index('completion.fail_while_owned(error)') < \
    quarantine_compute.index('self.fail_outstanding_jobs_for_vm(&status, error)'), \
    'a terminal compute timeout does not fan out to its VM peers'
assert timeout_render.index('self.quarantine_failed_render_completion(signal, error)') < \
    timeout_render.index('self.fail_outstanding_jobs_for_vm(&status, error)'), \
    'a terminal render timeout does not fan out to its VM peers'
assert '.fail_outstanding_jobs_for_vm(&status, *error)' in publish_compute, \
    'a scheduler-time compute publication failure strands prior VM work'
assert publish_compute.index('if lost {') < \
    publish_compute.index('.fail_outstanding_jobs_for_vm(&status, *error)'), \
    'a recoverable compute publication refusal poisons the VM'

preamble = r'''
#![allow(dead_code)]
#![allow(unused_variables)]
use std::sync::Arc;
use std::sync::atomic::{AtomicI32,AtomicU32,Ordering};
use std::cell::RefCell;
#[derive(Debug,PartialEq,Eq,Copy,Clone)]struct Error(i32);
impl Error{fn to_errno(self)->i32{self.0}}
type Result<T=()>=std::result::Result<T,Error>;
const EIO:Error=Error(-5);
const ETIMEDOUT:Error=Error(-110);
const ENODEV:Error=Error(-19);
const EBUSY:Error=Error(-16);
const EINVAL:Error=Error(-22);
const ERESTARTSYS:Error=Error(-512);
const ECANCELED:Error=Error(-125);
macro_rules! dev_err{($($t:tt)*)=>{{}}}
macro_rules! pr_err{($($t:tt)*)=>{{}}}

// `crate::g17_status::VmStatus` is what the extracted code names, and in a
// standalone test crate `crate` is this file.
pub mod g17_status{
 use super::*;
 pub struct VmStatus{error:AtomicI32}
 impl VmStatus{
  pub fn new()->Self{Self{error:AtomicI32::new(0)}}
  pub fn record(&self,error:i32){if error<0{let _=self.error.compare_exchange(0,error,Ordering::AcqRel,Ordering::Acquire);}}
  pub fn get(&self)->i32{self.error.load(Ordering::Acquire)}
 }
}

/// Records whether the sticky VM status was already set when the fence
/// signalled. A waiter woken by an error fence asks the status why, so a fence
/// that signals first is a fence that can hand back stale data.
struct Completion{
 status:Arc<crate::g17_status::VmStatus>,
 signalled:RefCell<Option<i32>>,
 status_at_signal:RefCell<i32>,
}
impl Completion{
 fn belongs_to_vm(&self,status:&Arc<crate::g17_status::VmStatus>)->bool{Arc::ptr_eq(&self.status,status)}
 fn fail_while_owned(&self,error:Error)->bool{
  if self.signalled.borrow().is_some(){return false;}self.complete(Err(error));true
 }
 fn complete(&self,result:Result){
  if self.signalled.borrow().is_some(){return;}
  if let Err(error)=result{ self.status.record(error.to_errno()); }
  *self.status_at_signal.borrow_mut()=self.status.get();
  *self.signalled.borrow_mut()=Some(result.err().map(|e|e.to_errno()).unwrap_or(0));
 }
}
struct Context(u32);
impl Context{fn context_id(&self)->u32{self.0}}
struct Packet{submission:u64,context:Context,completion:Option<Completion>}
impl Packet{
 fn submission_id(&self)->u64{self.submission}
 fn context(&self)->&Context{&self.context}
 fn completion(&self)->Result<&Completion>{self.completion.as_ref().ok_or(EINVAL)}
}
#[derive(Default)]
struct Lease{quarantined:Arc<AtomicU32>}
impl Lease{fn quarantine(&self){self.quarantined.fetch_add(1,Ordering::SeqCst);}}
struct G17PAsyncComputeActive{packet:Arc<Packet>,_lease:Lease,retirement:Option<()>}
struct Queue(u8);
impl Queue{fn queue_id(&self)->u8{self.0}}
'''+completion_mod(fence_state_source())+queue_limits_mock()+KVEC+r'''
mod g17_drm{pub(super) type G17PRenderCompletion=super::Completion;}
type RenderFifo=g17_completion::PairFifo<Arc<Completion>,15>;
type Inflight=g17_completion::PairFifo<G17PAsyncComputeActive,{g17_queue_limits::MAX_COMPUTE_IN_FLIGHT_PER_QUEUE as usize}>;
fn one(active:G17PAsyncComputeActive)->Inflight{let mut f=Inflight::new();if f.push(active).is_err(){unreachable!()}f}
struct G17PAsyncComputeQueue{queue:Queue,inflight:Inflight,quarantined:bool,awaiting_retirement_witness:bool}
mod g17_resources{
 pub struct G17PRenderQueuePair;
 impl G17PRenderQueuePair{pub const SLOT_COUNT:u8=2;}
}
struct Pdev;
mod g17_live_boot {
 pub fn note_session_recovery_pending(pending:&mut bool,_pdev:Option<&super::Pdev>,_reason:&str){*pending=true;}
}
impl Pdev{fn as_ref(&self)->&Self{self}}
#[derive(Default)]
struct Serviced(u32);
struct G17PLiveRuntime{
 async_compute_queues:Vec<G17PAsyncComputeQueue>,
 pdev:Option<Pdev>,
 serviced:u32,
 render_observations:u32,
 render_completed:[bool;2],
 pending_render:[Option<Arc<Completion>>;2],
 pending_render_queued:[RenderFifo;2],render_fence_state:g17_completion::RenderFenceState,render_event_masks:[[u32;2];2],quarantined_render_completions:KVec<Arc<Completion>>,
 session_recovery_pending:bool,
}
impl G17PLiveRuntime{
 fn service_async_compute_completions(&mut self)->Result{self.serviced+=1;Ok(())}
 fn poll_deferred_render_completion(&mut self,slot:u8)->Result<Option<u8>>{
  self.render_observations+=1;Ok(self.render_completed[slot as usize].then_some(slot))
 }
 fn settle_deferred_render_completion(&mut self,slot:u8,_observed:u8)->Result<bool>{
  if let Some(completion)=self.pending_render[slot as usize].take(){completion.complete(Ok(()));}
  Ok(true)
 }
'''

harness = r'''
}

/// The queue-side settle: which errors mean "this client's device is gone".
struct Vm{status:Arc<crate::g17_status::VmStatus>,id:u64}
impl Vm{
 fn status(&self)->&Arc<crate::g17_status::VmStatus>{&self.status}
 fn id(&self)->u64{self.id}
}
struct Dev;
impl Dev{fn as_ref(&self)->&Self{self}}
struct RuntimeRef(RefCell<G17PLiveRuntime>);
impl RuntimeRef{
 fn mutex(&self)->&Self{self}
 fn lock(&self)->std::cell::RefMut<'_,G17PLiveRuntime>{self.0.borrow_mut()}
}
struct G17PQueue{vm:Vm,runtime:RuntimeRef,dev:Dev}
'''

note_impl = 'impl G17PQueue{\n' + note + '\n}\n'

tests = r'''
fn runtime(owners:&[Arc<crate::g17_status::VmStatus>])->(G17PLiveRuntime,Vec<Arc<AtomicU32>>){
 let mut queues=Vec::new();let mut leases=Vec::new();
 for (index,status) in owners.iter().enumerate(){
  let flag=Arc::new(AtomicU32::new(0));
  leases.push(flag.clone());
  queues.push(G17PAsyncComputeQueue{
   queue:Queue(4+index as u8),
   inflight:one(G17PAsyncComputeActive{
    packet:Arc::new(Packet{
     submission:100+index as u64,
     context:Context(10+index as u32),
     completion:Some(Completion{status:status.clone(),signalled:RefCell::new(None),status_at_signal:RefCell::new(0)}),
    }),
    _lease:Lease{quarantined:flag},retirement:Some(()),
   }),
   quarantined:false,awaiting_retirement_witness:false,
  });
 }
 (G17PLiveRuntime{async_compute_queues:queues,pdev:Some(Pdev),serviced:0,
   render_observations:0,render_completed:[false;2],pending_render:[None,None],pending_render_queued:std::array::from_fn(|_|RenderFifo::new()),render_fence_state:Default::default(),render_event_masks:[[0;2];2],quarantined_render_completions:KVec::with_capacity(32),
   session_recovery_pending:false},leases)
}

fn completion(status:Arc<crate::g17_status::VmStatus>)->Arc<Completion>{
 Arc::new(Completion{status,signalled:RefCell::new(None),status_at_signal:RefCell::new(0)})
}

#[test]fn only_the_lost_vm_s_jobs_are_failed(){
 let lost=Arc::new(crate::g17_status::VmStatus::new());
 let healthy=Arc::new(crate::g17_status::VmStatus::new());
 let (mut rt,leases)=runtime(&[lost.clone(),healthy.clone(),lost.clone()]);
 lost.record(EIO.to_errno());
 assert_eq!(rt.fail_outstanding_jobs_for_vm(&lost,EIO),2);
 let signalled:Vec<Option<i32>>=rt.async_compute_queues.iter()
  .map(|q|*q.inflight.front().unwrap().packet.completion().unwrap().signalled.borrow()).collect();
 assert_eq!(signalled,vec![Some(EIO.to_errno()),None,Some(EIO.to_errno())]);
 assert_eq!(rt.async_compute_queues.iter().map(|q|q.quarantined).collect::<Vec<_>>(),vec![true,false,true]);
 assert_eq!(leases.iter().map(|l|l.load(Ordering::SeqCst)).collect::<Vec<_>>(),vec![1,0,1]);
 assert_eq!(healthy.get(),0,"a healthy client's VM was retired");
 // Nothing is released: the firmware may still be running these packets.
 assert!(rt.async_compute_queues.iter().all(|q|q.inflight.is_some()));
}

#[test]fn one_terminal_failure_settles_same_vm_render_and_compute_only(){
 let lost=Arc::new(crate::g17_status::VmStatus::new());
 let healthy=Arc::new(crate::g17_status::VmStatus::new());
 let (mut rt,leases)=runtime(&[lost.clone(),healthy.clone()]);
 rt.pending_render=[Some(completion(lost.clone())),Some(completion(healthy.clone()))];
 assert_eq!(rt.fail_outstanding_jobs_for_vm(&lost,ETIMEDOUT),2);
 assert_eq!(*rt.quarantined_render_completions[0].signalled.borrow(),Some(ETIMEDOUT.to_errno()));
 assert_eq!(*rt.pending_render[1].as_ref().unwrap().signalled.borrow(),None);
 assert_eq!(leases[0].load(Ordering::SeqCst),1);
 assert_eq!(leases[1].load(Ordering::SeqCst),0);
 assert_eq!(healthy.get(),0,"the peer VM was poisoned");
 // A VM-scoped loss quarantines that VM's render slots and compute queue; it never declares the session crashed.
 assert!(!rt.session_recovery_pending);
 assert_eq!(rt.fail_outstanding_jobs_for_vm(&lost,EIO),0,"same owners failed twice");
}

#[test]fn compute_poll_failure_settles_same_vm_peers_without_a_second_ring_scan(){
 let lost=Arc::new(crate::g17_status::VmStatus::new());
 let healthy=Arc::new(crate::g17_status::VmStatus::new());
 let (mut rt,leases)=runtime(&[lost.clone(),lost.clone(),healthy.clone()]);
 rt.pending_render=[Some(completion(lost.clone())),Some(completion(healthy.clone()))];
 {
  let failed=&mut rt.async_compute_queues[0];
  failed.quarantined=true;
  let active=failed.inflight.front().unwrap();
  active._lease.quarantine();
  active.packet.completion().unwrap().fail_while_owned(EIO);
 }
 assert_eq!(rt.fail_outstanding_jobs_for_vm_after_compute_observation(&lost,EIO),2);
 assert_eq!(rt.serviced,0,"poll-error fanout rescanned the compute completion ring");
 assert_eq!(rt.async_compute_queues.iter().map(|q|q.quarantined).collect::<Vec<_>>(),
  vec![true,true,false]);
 assert_eq!(leases.iter().map(|l|l.load(Ordering::SeqCst)).collect::<Vec<_>>(),vec![1,1,0]);
 assert_eq!(*rt.quarantined_render_completions[0].signalled.borrow(),Some(EIO.to_errno()));
 assert_eq!(*rt.pending_render[1].as_ref().unwrap().signalled.borrow(),None);
 assert_eq!(healthy.get(),0,"the completion-poll failure poisoned a peer VM");
}

#[test]fn every_stacked_kick_of_the_lost_vm_is_failed_and_retained(){
 let lost=Arc::new(crate::g17_status::VmStatus::new());
 let (mut rt,leases)=runtime(&[lost.clone()]);
 let flag=leases[0].clone();
 for k in 1..3u64 {
  let extra=G17PAsyncComputeActive{packet:Arc::new(Packet{submission:100+10*k,context:Context(10),
   completion:Some(Completion{status:lost.clone(),signalled:RefCell::new(None),status_at_signal:RefCell::new(0)})}),
   _lease:Lease{quarantined:flag.clone()},retirement:Some(())};
  if rt.async_compute_queues[0].inflight.push(extra).is_err(){unreachable!()}
 }
 assert_eq!(rt.fail_outstanding_jobs_for_vm(&lost,EIO),3);
 assert!(rt.async_compute_queues[0].inflight.iter().all(|a|*a.packet.completion().unwrap().signalled.borrow()==Some(EIO.to_errno())));
 assert_eq!(leases[0].load(Ordering::SeqCst),3);assert_eq!(rt.async_compute_queues[0].inflight.len(),3);
}

#[test]fn render_completion_visible_at_terminal_boundary_wins(){
 let lost=Arc::new(crate::g17_status::VmStatus::new());
 let (mut rt,_)=runtime(&[]);
 rt.pending_render[0]=Some(completion(lost.clone()));
 rt.render_completed[0]=true;
 assert_eq!(rt.fail_outstanding_jobs_for_vm(&lost,EIO),0);
 assert!(rt.pending_render[0].is_none());
 assert_eq!(rt.render_observations,1);
}

#[test]fn a_woken_waiter_can_always_see_why(){
 let lost=Arc::new(crate::g17_status::VmStatus::new());
 let (mut rt,_)=runtime(&[lost.clone()]);
 rt.fail_outstanding_jobs_for_vm(&lost,ETIMEDOUT);
 let q=&rt.async_compute_queues[0];
 let completion=q.inflight.front().unwrap().packet.completion().unwrap();
 assert_eq!(*completion.signalled.borrow(),Some(ETIMEDOUT.to_errno()));
 assert_ne!(*completion.status_at_signal.borrow(),0,
  "the fence signalled while the VM status still read healthy");
}

#[test]fn a_completion_already_in_dram_is_taken_first(){
 let lost=Arc::new(crate::g17_status::VmStatus::new());
 let (mut rt,_)=runtime(&[lost.clone()]);
 rt.fail_outstanding_jobs_for_vm(&lost,EIO);
 assert_eq!(rt.serviced,1,"the sweep discarded a job the GPU may have finished");
}

#[test]fn an_already_quarantined_queue_is_not_failed_twice(){
 let lost=Arc::new(crate::g17_status::VmStatus::new());
 let (mut rt,_)=runtime(&[lost.clone()]);
 assert_eq!(rt.fail_outstanding_jobs_for_vm(&lost,EIO),1);
 assert_eq!(rt.fail_outstanding_jobs_for_vm(&lost,EIO),0);
}

fn queue(status:Arc<crate::g17_status::VmStatus>,jobs:&[Arc<crate::g17_status::VmStatus>])->G17PQueue{
 let (rt,_)=runtime(jobs);
 G17PQueue{vm:Vm{status,id:7},runtime:RuntimeRef(RefCell::new(rt)),dev:Dev}
}

#[test]fn contention_does_not_retire_a_healthy_client(){
 for error in [EBUSY,ERESTARTSYS,EINVAL,ECANCELED]{
  let status=Arc::new(crate::g17_status::VmStatus::new());
  let mut q=queue(status.clone(),&[status.clone()]);
  q.note_submission_failure(1,error);
  assert_eq!(status.get(),0,"{:?} retired a healthy VM",error);
  let rt=q.runtime.lock();
  assert!(!rt.async_compute_queues[0].quarantined,"{:?} failed outstanding work",error);
 }
}

#[test]fn a_terminal_failure_settles_the_client_s_outstanding_work(){
 for error in [EIO,ETIMEDOUT,ENODEV]{
  let status=Arc::new(crate::g17_status::VmStatus::new());
  let mut q=queue(status.clone(),&[status.clone()]);
  q.note_submission_failure(1,error);
  assert_eq!(status.get(),error.to_errno());
  let rt=q.runtime.lock();
  assert!(rt.async_compute_queues[0].quarantined);
  let completion=rt.async_compute_queues[0].inflight.front().unwrap().packet.completion().unwrap();
  assert_eq!(*completion.signalled.borrow(),Some(error.to_errno()));
 }
}

#[test]fn a_dead_vm_settles_even_on_a_recoverable_looking_error(){
 // Once the VM is already dead, every later refusal is terminal for it: the
 // work it has in flight can never complete.
 let status=Arc::new(crate::g17_status::VmStatus::new());
 status.record(EIO.to_errno());
 let mut q=queue(status.clone(),&[status.clone()]);
 q.note_submission_failure(1,EBUSY);
 let rt=q.runtime.lock();
 assert!(rt.async_compute_queues[0].quarantined);
}
'''

code = preamble + retention_method(SOURCE) + sweep_after_observation + sweep + harness + note_impl + tests
with tempfile.TemporaryDirectory() as d:
    d = Path(d)
    f = d / 'teardown.rs'
    f.write_text(code)
    exe = d / 'teardown'
    subprocess.run(['rustc', '--edition=2021', '--test', str(f), '-o', str(exe)], check=True)
    subprocess.run([str(exe), '--test-threads=1'], check=True, timeout=60)
print('PASS lost-device teardown: only the lost VM is settled, its status is set '
      'before the fence signals, packets stay retained, contention stays recoverable')
