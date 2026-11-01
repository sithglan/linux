#!/usr/bin/env python3
"""Actual bounded admission rendezvous with host clock/condvar adapters.

No firmware claim: this tests distinct participants, aborts and admission
ordering without holding a runtime mutex or GPU lease during collection.
"""
from pathlib import Path
import subprocess
import tempfile
from test_g17_render_transition_probe import preamble, production, item, drm, ROOT

params = (ROOT / 'asahi.rs').read_text()
assert 'default: 0' in item(params, 'g17p_compute_burst_probe: u32')
backend = item(drm, 'impl g17_job::G17PQueueBackend for G17PAsyncQueueBackend {')
assert backend.index('async_compute_needs_install') < backend.index('wait_compute_burst(')
assert backend.index('wait_compute_burst(') < backend.index('acquire_compute(')
assert backend.index('publish_async_compute(') < backend.index('G17P compute burst probe:')
burst = item(drm, 'fn wait_compute_burst(')
assert 'runtime' not in burst and 'acquire_compute(' not in burst

adapters = r'''
struct EligibilityHardware(bool);
impl EligibilityHardware {fn requires_first_install(&self)->bool{self.0}}
struct EligibilityContext(bool);
impl EligibilityContext {fn is_current(&self)->bool{self.0}}
struct EligibilityQueue {
 owner:Option<u64>,queue:EligibilityHardware,previous_binding:Option<()>,binding:Option<()>,
 context:Option<EligibilityContext>,active:Option<()>,released:bool,quarantined:bool,
}
struct EligibilityRuntime {async_compute_queues:Vec<EligibilityQueue>}
impl EligibilityRuntime {
 fn steady()->Self {Self{async_compute_queues:vec![EligibilityQueue{
  owner:Some(10),queue:EligibilityHardware(false),previous_binding:None,binding:Some(()),
  context:Some(EligibilityContext(true)),active:None,released:false,quarantined:false,
 }]}}
}
mod kernel {
 pub mod time {pub fn msecs_to_jiffies(v:u32)->u64{u64::from(v)}}
 pub mod sync {
  pub enum CondVarTimeoutResult {Timeout,Woken{jiffies:u64},Signal{jiffies:u64}}
 }
}
impl Condvar {
 fn wait_interruptible_timeout<T>(&self,g:&mut Guard<'_,T>,ms:u64)->kernel::sync::CondVarTimeoutResult {
  use kernel::sync::CondVarTimeoutResult::*;
  self.waits.fetch_add(1,Ordering::SeqCst);
  if INTERRUPT.with(|v|v.replace(false)){return Signal{jiffies:ms};}
  let (held,timed)=self.cv.wait_timeout(g.0.take().unwrap(),Duration::from_millis(ms)).unwrap();
  g.0=Some(held);
  if timed.timed_out(){Timeout}else{Woken{jiffies:ms}}
 }
}
fn registry(mode:u32)->Arc<G17PQueueRegistry> {
 module_parameters::g17p_compute_burst_probe.0.store(mode,Ordering::SeqCst);
 Arc::new(G17PQueueRegistry::default())
}
fn member(r:&Arc<G17PQueueRegistry>,owner:u64)->thread::JoinHandle<Result<Option<G17PComputeBurstTicket>>> {
 let r=r.clone();thread::spawn(move||r.wait_compute_burst(owner,false))
}
fn arrived(r:&G17PQueueRegistry,n:u8) {until(||r.lease.lock().burst.arrived==n);}
'''

tests = r'''
#[test]fn only_current_installed_idle_unrebound_queues_are_eligible() {
 assert!(EligibilityRuntime::steady().async_compute_burst_steady(10));
 assert!(!EligibilityRuntime::steady().async_compute_burst_steady(20));
 for cause in 0..8 {
  let mut r=EligibilityRuntime::steady();let q=&mut r.async_compute_queues[0];
  match cause {0=>q.queue.0=true,1=>q.previous_binding=Some(()),2=>q.binding=None,
   3=>q.context=None,4=>q.context=Some(EligibilityContext(false)),5=>q.active=Some(()),
   6=>q.released=true,7=>q.quarantined=true,_=>unreachable!()}
  assert!(!r.async_compute_burst_steady(10),"ineligible case {cause} admitted");
 }
}
#[test]fn off_first_install_and_unknown_mode_do_not_wait() {
 for mode in [0,3,u32::MAX] {let r=registry(mode);assert!(r.wait_compute_burst(1,false).unwrap().is_none());assert_eq!(r.lease_free.waits.load(Ordering::Acquire),0);}
 let r=registry(2);assert!(r.wait_compute_burst(1,true).unwrap().is_none());assert!(r.wait_compute_burst(0,false).unwrap().is_none());
}
#[test]fn observation_mode_has_matching_log_ticket_without_waits_and_is_bounded() {
 let r=registry(1);
 for _ in 0..192 {let t=r.wait_compute_burst(1,false).unwrap().unwrap();assert_eq!(t.outcome,5);assert_eq!(t.parties,1);assert_eq!(t.wait_ns,0);}
 assert!(r.wait_compute_burst(1,false).unwrap().is_none());assert_eq!(r.lease_free.waits.load(Ordering::Acquire),0);
}
#[test]fn three_distinct_steady_queues_release_together_then_execute_in_parallel() {
 let r=registry(2);let a=member(&r,10);arrived(&r,1);let b=member(&r,20);arrived(&r,2);
 // Gatherers have not acquired a GPU lease; even an unrelated steady job
 // can acquire and retire while they wait.
 assert_eq!(r.compute_readers.load(Ordering::Acquire),0);
 let unrelated=r.acquire_compute(false).unwrap();drop(unrelated);
 let c=r.wait_compute_burst(30,false).unwrap().unwrap();
 let a=a.join().unwrap().unwrap().unwrap();let b=b.join().unwrap().unwrap().unwrap();
 for t in [&a,&b,&c] {assert_eq!((t.cohort,t.parties,t.outcome),(1,3,0));assert_eq!(t.render_epoch,0);}
 assert_eq!(a.released.0,b.released.0);assert_eq!(a.released.0,c.released.0);
 let jobs:Vec<_>=(0..3).map(|_|r.acquire_compute(false).unwrap()).collect();
 assert_eq!(jobs[0].admission_snapshot().0,3);drop(jobs);
 assert_eq!(r.lease.lock().burst.generation,1);
}
#[test]fn duplicate_owner_does_not_complete_the_cohort() {
 let r=registry(2);let a=member(&r,10);arrived(&r,1);
 assert!(r.wait_compute_burst(10,false).unwrap().is_none());assert_eq!(r.lease.lock().burst.arrived,1);
 let b=member(&r,20);arrived(&r,2);let c=r.wait_compute_burst(30,false).unwrap().unwrap();
 assert_eq!(c.outcome,0);a.join().unwrap().unwrap();b.join().unwrap().unwrap();
}
#[test]fn partial_cohort_timeout_releases_every_participant_without_failing_work() {
 let r=registry(2);let a=member(&r,10);arrived(&r,1);let b=member(&r,20);
 for t in [a.join().unwrap().unwrap().unwrap(),b.join().unwrap().unwrap().unwrap()] {
  assert_eq!((t.parties,t.outcome),(2,1));assert!(t.wait_ns>=1_000_000);
 }
 assert_eq!(r.lease.lock().burst.generation,1);drop(r.acquire_compute(false).unwrap());
}
#[test]fn signal_cancels_only_signaller_and_releases_peer() {
 let r=registry(2);let a=member(&r,10);arrived(&r,1);
 INTERRUPT.with(|v|v.set(true));assert!(matches!(r.wait_compute_burst(20,false),Err(ERESTARTSYS)));
 let t=a.join().unwrap().unwrap().unwrap();assert_eq!(t.outcome,2);
 assert_eq!(r.lease.lock().burst.generation,1);drop(r.acquire_compute(false).unwrap());
}
#[test]fn render_epoch_change_aborts_even_if_render_retired_before_wakeup() {
 let r=registry(2);let a=member(&r,10);arrived(&r,1);
 {let _render=r.acquire_lease(44).unwrap();}
 let t=a.join().unwrap().unwrap().unwrap();assert_eq!(t.outcome,3);
 assert_eq!(r.lease.lock().render_slots,0);
}
#[test]fn install_and_render_owners_bypass_collection() {
 let r=registry(2);let render=r.acquire_lease(1).unwrap();
 assert!(r.wait_compute_burst(10,false).unwrap().is_none());drop(render);
 let installer=r.acquire_compute(true).unwrap();assert!(r.wait_compute_burst(10,false).unwrap().is_none());drop(installer);
 let render=r.acquire_lease(1).unwrap();let transition=r.acquire_render_transition(1).unwrap();
 assert!(r.wait_compute_burst(10,false).unwrap().is_none());drop(transition);drop(render);
 assert_eq!(r.lease.lock().burst.arrived,0);
}
#[test]fn install_that_finishes_before_gatherers_wake_still_aborts() {
 let r=registry(2);let a=member(&r,10);arrived(&r,1);
 {let _installer=r.acquire_compute(true).unwrap();}
 let t=a.join().unwrap().unwrap().unwrap();assert_eq!(t.outcome,3);
 assert!(!r.lease.lock().compute_install);
}
#[test]fn fault_wakes_collection_but_does_not_rehabilitate_device() {
 let r=registry(2);let job=r.acquire_compute(false).unwrap();
 let a=member(&r,10);arrived(&r,1);job.quarantine();
 let t=a.join().unwrap().unwrap().unwrap();assert_eq!(t.outcome,4);
 assert!(matches!(r.acquire_compute(false),Err(EIO)));assert_eq!(job.admission_snapshot().0,1);
}
#[test]fn spurious_wakeup_does_not_release_a_partial_cohort() {
 let r=registry(2);let a=member(&r,10);arrived(&r,1);
 let waits=r.lease_free.waits.load(Ordering::Acquire);r.lease_free.notify_all();wait_entered(&r,waits);
 assert!(r.lease.lock().burst.released.is_none());
 let b=member(&r,20);arrived(&r,2);assert_eq!(r.wait_compute_burst(30,false).unwrap().unwrap().outcome,0);
 a.join().unwrap().unwrap();b.join().unwrap().unwrap();
}
#[test]fn final_cohort_limit_preserves_normal_admission() {
 let r=registry(2);r.lease.lock().burst.generation=63;
 let a=member(&r,10);arrived(&r,1);let b=member(&r,20);arrived(&r,2);
 assert_eq!(r.wait_compute_burst(30,false).unwrap().unwrap().cohort,64);
 a.join().unwrap().unwrap();b.join().unwrap().unwrap();
 assert!(r.wait_compute_burst(40,false).unwrap().is_none());drop(r.acquire_compute(false).unwrap());
}
'''

with tempfile.TemporaryDirectory() as directory:
    source = Path(directory) / 'burst.rs'
    source.write_text(preamble + production + adapters
                      + 'impl EligibilityRuntime {\n'
                      + item((ROOT / 'g17_async_compute.rs').read_text(), 'pub(crate) fn async_compute_burst_steady(') + '\n}\n'
                      + item(drm, 'struct G17PComputeBurstTicket {')
                      + 'impl G17PQueueRegistry {\n' + burst + '\n}\n' + tests)
    binary = Path(directory) / 'burst'
    subprocess.run(['rustc', '--edition=2021', '--test', str(source), '-o', str(binary)], check=True)
    subprocess.run([str(binary), '--test-threads=1', '--nocapture'], check=True, timeout=20)
