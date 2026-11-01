#!/usr/bin/env python3
"""Actual atomic admission, lease Drop and join with host synchronization adapters."""
from pathlib import Path
import subprocess,tempfile
r=Path(__file__).resolve().parents[1];s=(r/'g17_preparation.rs').read_text()
def item(source,marker):
 a=source.index(marker);b=source.index('{',a)+1;d=1
 while d:d+=(source[b]=='{')-(source[b]=='}');b+=1
 return source[a:b]
core=s[s.index('use core::'):s.index('#[cfg(not(test))]')]
control=item(s,'pub(crate) struct G17PPreparations {').replace('        #[pin]\n','')
lease=item(s,'pub(crate) struct G17PPreparationLease {')
methods='\n'.join(item(s,marker) for marker in ['        pub(crate) fn enter(', '        pub(crate) fn enter_wait(', '        pub(crate) fn fail(', '        pub(crate) fn close(', '        pub(crate) fn is_removed(', '        pub(crate) fn is_drained(', '        pub(crate) fn reopen(', '        pub(crate) fn wait_for_preparers('])
finish=item(s,'    impl G17PPreparationLease {');drop=item(s,'    impl Drop for G17PPreparationLease {')
rust=r'''
#![allow(dead_code,unused_imports)]
use std::sync::{Arc,Mutex as StdMutex,Condvar as StdCondvar,mpsc};
use std::time::Duration;
#[derive(Debug,Clone,Copy,PartialEq,Eq)]struct Error(i32);
const ERESTARTSYS:Error=Error(-512);const EAGAIN:Error=Error(-11);const ENODEV:Error=Error(-19);const ENOSPC:Error=Error(-28);const ECANCELED:Error=Error(-125);
type Result<T=(),E=Error>=std::result::Result<T,E>;
'''+core+r'''
struct Mutex<T>(StdMutex<T>);
struct Guard<'a,T>{guard:Option<std::sync::MutexGuard<'a,T>>}
impl<T>Mutex<T>{fn lock(&self)->Guard<'_,T>{Guard{guard:Some(self.0.lock().unwrap())}}}
struct CondVar(StdCondvar);
impl CondVar{fn wait<T>(&self,g:&mut Guard<'_,T>){g.guard=Some(self.0.wait(g.guard.take().unwrap()).unwrap());}fn wait_interruptible<T>(&self,g:&mut Guard<'_,T>)->bool{self.wait(g);false}fn notify_all(&self){self.0.notify_all();}}
mod runtime{
 use super::*;
'''+control+lease+'impl G17PPreparations {\n'+r'''
 fn new()->Arc<Self>{Arc::new(Self{state:PreparationState::new(),join:Mutex(StdMutex::new(())),drained:CondVar(StdCondvar::new())})}
'''+methods+'}\n'+finish+drop+r'''
 #[test]fn independent_admission_and_stale_commit_refusal(){
  let p=G17PPreparations::new();let a=p.enter().unwrap();let b=p.enter().unwrap();
  p.close(false);assert!(matches!(p.enter(),Err(EAGAIN)));assert!(!p.reopen());
  assert_eq!(a.finish(),Err(ECANCELED));assert!(!p.is_drained());drop(b);
  assert!(p.is_drained());assert!(p.reopen());assert!(p.enter().unwrap().finish().is_ok());
 }
 #[test]fn removed_epoch_never_reopens(){let p=G17PPreparations::new();let a=p.enter().unwrap();p.close(true);drop(a);assert!(p.is_removed());assert!(!p.reopen());assert!(matches!(p.enter(),Err(ENODEV)));}
 #[test]fn join_observes_resource_destruction_before_last_lease(){
  struct Resource(Arc<std::sync::atomic::AtomicBool>);impl Drop for Resource{fn drop(&mut self){self.0.store(true,Ordering::Release);}}
  struct Build{resource:Resource,lease:G17PPreparationLease}
  let p=G17PPreparations::new();let freed=Arc::new(std::sync::atomic::AtomicBool::new(false));
  let tx=Build{resource:Resource(freed.clone()),lease:p.enter().unwrap()};p.close(false);
  let waiter=p.clone();let done=freed.clone();let(tx_done,rx)=mpsc::channel();
  let t=std::thread::spawn(move||{waiter.wait_for_preparers();assert!(done.load(Ordering::Acquire));tx_done.send(()).unwrap();});
  assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());drop(tx);rx.recv_timeout(Duration::from_secs(2)).unwrap();t.join().unwrap();
 }
 #[test]fn cancellation_can_take_runtime_while_recovery_joins(){
  let runtime=Arc::new(StdMutex::new(()));let p=G17PPreparations::new();let lease=p.enter().unwrap();
  {let _r=runtime.lock().unwrap();p.close(false);}
  let join=p.clone();let t=std::thread::spawn(move||join.wait_for_preparers());
  {let _r=runtime.lock().unwrap();assert_eq!(lease.finish(),Err(ECANCELED));}t.join().unwrap();
 }
 #[test]fn admission_waiter_retries_only_after_reopen(){
  let p=G17PPreparations::new();p.close(false);let waiter=p.clone();let(tx,rx)=mpsc::channel();
  let t=std::thread::spawn(move||{let lease=waiter.enter_wait().unwrap();tx.send(()).unwrap();lease.finish().unwrap();});
  assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());assert!(p.is_drained());p.reopen();rx.recv_timeout(Duration::from_secs(2)).unwrap();t.join().unwrap();
 }
 #[test]fn failed_recovery_wakes_admission_with_device_loss(){
  let p=G17PPreparations::new();p.close(false);let waiter=p.clone();let(tx,rx)=mpsc::channel();
  let t=std::thread::spawn(move||{assert!(matches!(waiter.enter_wait(),Err(ENODEV)));tx.send(()).unwrap();});
  p.fail();rx.recv_timeout(Duration::from_secs(2)).unwrap();t.join().unwrap();assert!(p.reopen());assert!(p.enter().is_ok());
 }
 #[test]fn check_sleep_release_races_do_not_lose_final_wakeup(){
  for _ in 0..256{let p=G17PPreparations::new();let lease=p.enter().unwrap();p.close(false);
   let waiter=p.clone();let(tx,rx)=mpsc::channel();let t=std::thread::spawn(move||{waiter.wait_for_preparers();tx.send(()).unwrap();});
   std::thread::yield_now();drop(lease);rx.recv_timeout(Duration::from_secs(2)).unwrap();t.join().unwrap();}
 }
}
#[test]fn epoch_and_count_exhaustion_never_wrap(){
 let s=PreparationState(AtomicU64::new(EPOCH_STEP|COUNT_MASK));assert_eq!(s.enter(),Err(AdmissionError::Full));
 let s=PreparationState(AtomicU64::new(u64::MAX & !(EPOCH_STEP-1)));let epoch=s.enter().unwrap();s.close(false);
 assert!(!s.current(epoch));assert!(!s.reopen());assert!(s.leave());assert_eq!(s.enter(),Err(AdmissionError::Removed));
}
#[test]fn close_admission_races_preserve_counts(){
 let s=Arc::new(PreparationState::new());let mut threads=Vec::new();
 for _ in 0..8{let s=s.clone();threads.push(std::thread::spawn(move||{for _ in 0..20000{if s.enter().is_ok(){s.leave();}}}));}
 for _ in 0..20000{s.close(false);s.reopen();}
 for t in threads{t.join().unwrap();}assert!(s.drained());assert!(s.reopen());assert!(s.current(s.enter().unwrap()));s.leave();
}
'''
with tempfile.TemporaryDirectory() as d:
 p=Path(d)/'epoch.rs';p.write_text(rust);b=Path(d)/'epoch'
 subprocess.run(['rustc','--edition=2021','--test',str(p),'-o',str(b)],check=True)
 subprocess.run([str(b)],check=True,timeout=30)
# Integration: runtime exclusion ends before a blocking join; every whole
# session reset routes through the check, and both production engines lease.
live=(r/'g17_live_boot.rs').read_text();drm=(r/'g17_drm.rs').read_text();driver=(r/'driver.rs').read_text()
assert live.count('g17_lifecycle::recover_persistent_runtime(self)')==1
reset=item(live,'    fn rebuild_session_or_defer(')
assert reset.index('self.preparations.close(false)') < reset.index('!self.preparations.is_drained()') < reset.index('g17_lifecycle::recover_persistent_runtime(self)')
worker=item(drm,'    pub(crate) fn service_primary_recovery(')
assert worker.index('        };') < worker.index('wait_for_preparers()') < worker.index('let (result, completed)')
unbind=item(driver,'    fn disarm_ingress(')
assert unbind.index('        };') < unbind.index('wait_for_preparers()')
assert drm.count('self.runtime.preparations.enter_wait()?')==2
assert drm.count('preparation.finish().is_err()')==1
assert drm.count('!preparation.is_current()')==3
assert 'let current = preparation.is_current();' in drm
assert 'if retry_warm || !preparation.is_current()' in drm
print('PASS actual epoch admission/lease/join: independent readers, cancellation, removal, overflow, wake races, reset wiring')
