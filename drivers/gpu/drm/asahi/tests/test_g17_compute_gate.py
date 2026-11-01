#!/usr/bin/env python3
"""Actual gate methods: parallel retained compute, first install without any drain, fault wake."""
from pathlib import Path
import subprocess,tempfile
s=(Path(__file__).resolve().parents[1]/'g17_drm.rs').read_text()
def item(needle):
 a=s.index(needle);start=s.index('{',a);d=1;i=start+1
 while d:d+=(s[i]=='{')-(s[i]=='}');i+=1
 return s[a:i]
methods='\n'.join(item(n) for n in ['    fn acquire_compute(', '    fn acquire_lease(', '    fn compute_poison_is_device_wide('])
# The slot-count constant lives beside the state, not inside it, so extract
# it too rather than re-declaring a copy that could drift.
slots=s[s.index('const G17P_RENDER_SLOTS: usize ='):]
slots=slots[:slots.index('const _: () = assert!(G17P_ACTIVE_RENDER_SLOTS > 0);')]
state='\n'.join(item(n) for n in ['struct G17PQueueLeaseState {','impl Default for G17PQueueLeaseState {','impl G17PQueueLeaseState {'])
guards='\n'.join(item(n) for n in ['pub(crate) struct G17PComputeLeaseGuard {','impl G17PComputeLeaseGuard {','impl Drop for G17PComputeLeaseGuard {','struct G17PLeaseGuard {','impl G17PLeaseGuard {','impl Drop for G17PLeaseGuard {'])
code=r'''
#![allow(dead_code)]
use std::sync::{Arc,atomic::{AtomicU32,Ordering}};use std::ops::{Deref,DerefMut};use std::time::Duration;
type Result<T=()>=std::result::Result<T,i32>;const EINVAL:i32=22;const ENOSPC:i32=28;const EIO:i32=5;const EBUSY:i32=16;const ERESTARTSYS:i32=512;
mod g17_resources { pub const G17P_RENDER_MAX_IN_FLIGHT:usize=2; pub struct G17PRenderQueuePair; impl G17PRenderQueuePair { pub const SLOT_COUNT:u8=64; } }
// Production is scoped (bit 0 set); the device-wide variant is compiled
// separately with `--cfg device_wide` to keep that mode's contract covered.
#[cfg(not(device_wide))] mod g17_config { pub const QUARANTINE_SCOPE:u32=1; }
#[cfg(device_wide)] mod g17_config { pub const QUARANTINE_SCOPE:u32=0; }
'''+slots+r'''
struct Mutex<T>(std::sync::Mutex<T>);struct Guard<'a,T>(Option<std::sync::MutexGuard<'a,T>>);
impl<T>Mutex<T>{fn lock(&self)->Guard<'_,T>{Guard(Some(self.0.lock().unwrap()))}}
impl<T>Deref for Guard<'_,T>{type Target=T;fn deref(&self)->&T{self.0.as_ref().unwrap()}}
impl<T>DerefMut for Guard<'_,T>{fn deref_mut(&mut self)->&mut T{self.0.as_mut().unwrap()}}
struct CondVar(std::sync::Condvar);impl CondVar{fn wait_interruptible<T>(&self,g:&mut Guard<'_,T>)->bool{let(inner,t)=self.0.wait_timeout(g.0.take().unwrap(),Duration::from_secs(2)).unwrap();g.0=Some(inner);t.timed_out()}fn notify_all(&self){self.0.notify_all()}}
'''+state+r'''
struct G17PQueueRegistry{lease:Mutex<G17PQueueLeaseState>,lease_free:CondVar,compute_readers:AtomicU32,compute_faulted:AtomicU32}
impl G17PQueueRegistry{fn new()->Arc<Self>{Arc::new(Self{lease:Mutex(std::sync::Mutex::new(G17PQueueLeaseState::default())),lease_free:CondVar(std::sync::Condvar::new()),compute_readers:AtomicU32::new(0),compute_faulted:AtomicU32::new(0)})}
'''+methods+'\n}\n'+guards+r'''
#[test]fn two_retained_compute_owners_are_simultaneously_admitted(){
 let r=G17PQueueRegistry::new();let a=r.acquire_compute().unwrap();let b=r.acquire_compute().unwrap();assert_eq!(r.compute_readers.load(Ordering::SeqCst),2);drop(b);assert_eq!(r.compute_readers.load(Ordering::SeqCst),1);drop(a);assert_eq!(r.compute_readers.load(Ordering::SeqCst),0);
}
#[test]fn first_install_never_drains_renders_or_compute(){
 let r=G17PQueueRegistry::new();let old=r.acquire_compute().unwrap();let renders:Vec<_>=(1..=4).map(|owner|r.acquire_lease(owner).unwrap()).collect();
 // A queue's first install is an ordinary reader lease: it is admitted with four renders and a compute job live.
 let install=r.acquire_compute().unwrap();assert_eq!(r.compute_readers.load(Ordering::SeqCst),2);assert_eq!(r.lease.lock().render_slots.count_ones(),4);
 // And renders keep admitting while it is held.
 let more=r.acquire_lease(5).unwrap();assert_eq!(r.lease.lock().render_slots.count_ones(),5);
 drop(more);drop(install);drop(renders);drop(old);assert_eq!(r.compute_readers.load(Ordering::SeqCst),0);assert_eq!(r.lease.lock().render_slots,0);
}
#[test]fn steady_compute_overlaps_many_independently_owned_renders(){
 let r=G17PQueueRegistry::new();let q=r.acquire_compute().unwrap();
 let leases:Vec<_>=(1..=12).map(|owner|r.acquire_lease(owner).unwrap()).collect();
 assert_eq!(r.lease.lock().render_slots.count_ones(),12);
 assert_eq!(r.compute_readers.load(Ordering::SeqCst),1);
 // Warm second pair: an owner claims its sibling slot at first acquisition, so owners take slots 1,3,5,...
 for (index,lease) in leases.iter().enumerate(){assert_eq!(lease.slot() as usize,2*index+1);}
 drop(leases);drop(q);
}
#[cfg(not(device_wide))]
#[test]fn compute_quarantine_neither_fails_nor_frees_render_waiters(){
 let r=G17PQueueRegistry::new();let q=r.acquire_compute().unwrap();
 // Owner 7 fills its two physical pairs to the per-pair command depth (2 x RENDER_KICKS_PER_PAIR); only then does another kick wait.
 let mut a:Vec<_>=(0..2*g17_queue_limits::RENDER_KICKS_PER_PAIR).map(|_|r.acquire_lease(7).unwrap()).collect();let _b=r.acquire_lease(8).unwrap();
 let(tx,rx)=std::sync::mpsc::channel();let rr=r.clone();let w=std::thread::spawn(move||{tx.send(rr.acquire_lease(7).map(|l|l.slot())).unwrap();});
 assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
 // A compute fault is scoped to its own queue (QUARANTINE_SCOPE bit 0): the parked render neither fails nor is woken by it, and live slots stay live.
 q.quarantine();assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());assert_eq!(r.lease.lock().render_slots,14);
 // Only a retired kick on one of its own pairs admits it.
 drop(a.pop());assert!(rx.recv_timeout(Duration::from_secs(1)).unwrap().is_ok());w.join().unwrap();drop(a);drop(q);
}
#[cfg(not(device_wide))]
#[test]fn one_clients_compute_quarantine_never_refuses_another_clients_compute(){
 // Neo 2026-09-26 19:56:58: one qid-13 timeout latched the admission poison
 // and every later client's first compute publication failed with EIO,
 // poisoning each fresh VM. Scoped quarantine admits every other client.
 let r=G17PQueueRegistry::new();let q=r.acquire_compute().unwrap();q.quarantine();
 assert_eq!(r.compute_faulted.load(Ordering::SeqCst),1,"the latch is still recorded for diagnostics");
 let fresh=r.acquire_compute().unwrap();let render=r.acquire_lease(9).unwrap();
 assert_eq!(r.compute_readers.load(Ordering::SeqCst),2);drop(fresh);drop(render);drop(q);
}
#[cfg(device_wide)]
#[test]fn processor_stop_clears_compute_poison_before_the_old_lease_drops(){
 let r=G17PQueueRegistry::new();let q=r.acquire_compute().unwrap();q.quarantine();
 assert_eq!(r.acquire_compute().err(),Some(EIO));
 q.rehabilitate_after_processor_stop();drop(q);
 let recovered=r.acquire_compute().unwrap();drop(recovered);
 let render=r.acquire_lease(7).unwrap();drop(render);
}
'''
code += f'\n#[path = "{Path(__file__).resolve().parents[1] / "g17_queue_limits.rs"}"] mod g17_queue_limits;\n'
with tempfile.TemporaryDirectory() as d:
 f=Path(d)/'test.rs';f.write_text(code)
 for extra in ([],['--cfg','device_wide']):
  exe=Path(d)/'test';subprocess.run(['rustc','--edition=2021','--test',*extra,str(f),'-o',str(exe)],check=True);subprocess.run([str(exe),'--test-threads=1'],check=True,timeout=10)
print('PASS queue gate with host threads: compute overlap incl. first install, independent render owners, per-owner backpressure; scoped quarantine never refuses another client, device-wide poison cleared at processor stop')
