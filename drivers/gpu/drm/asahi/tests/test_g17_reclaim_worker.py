#!/usr/bin/env python3
"""Run actual reclaim queue/drain code with slow-unmap and cancellation races."""
from pathlib import Path
import subprocess, tempfile
root=Path(__file__).resolve().parents[1]
source=(root/'driver.rs').read_text()
def item(marker):
 a=source.index(marker);b=source.index('{',a)+1;depth=1
 while depth:depth+=(source[b]=='{')-(source[b]=='}');b+=1
 return source[a:b]
rust=r'''
#![allow(dead_code)]
use std::sync::{Arc,Mutex,MutexGuard,Barrier,atomic::{AtomicBool,AtomicUsize,Ordering}};
type ARef<T> = Arc<T>;
const GFP_KERNEL:usize=0;
static FAIL_ALLOC:AtomicBool=AtomicBool::new(false);
static QUEUED:AtomicUsize=AtomicUsize::new(0);
thread_local!{static LOCKS:std::cell::Cell<usize>=const{std::cell::Cell::new(0)};}
struct Lock<T>(Mutex<T>);
struct Guard<'a,T>(MutexGuard<'a,T>);
impl<T> Lock<T>{fn lock(&self)->Guard<'_,T>{let g=self.0.lock().unwrap();LOCKS.with(|v|v.set(v.get()+1));Guard(g)}}
impl<T> std::ops::Deref for Guard<'_,T>{type Target=T;fn deref(&self)->&T{&self.0}}
impl<T> std::ops::DerefMut for Guard<'_,T>{fn deref_mut(&mut self)->&mut T{&mut self.0}}
impl<T> Drop for Guard<'_,T>{fn drop(&mut self){LOCKS.with(|v|v.set(v.get()-1));}}
struct KVec<T>(Vec<T>);
impl<T> Default for KVec<T>{fn default()->Self{Self(Vec::new())}}
impl<T> KVec<T>{
 fn is_empty(&self)->bool{self.0.is_empty()}
 fn iter(&self)->std::slice::Iter<'_,T>{self.0.iter()}
 fn reserve(&mut self,n:usize,_:usize)->Result<(),()>{if FAIL_ALLOC.swap(false,Ordering::SeqCst){Err(())}else{self.0.reserve(n);Ok(())}}
 fn push_within_capacity(&mut self,v:T)->Result<(),PushError<T>>{if self.0.len()==self.0.capacity(){Err(PushError(v))}else{self.0.push(v);Ok(())}}
}
struct PushError<T>(T);
impl<T> IntoIterator for KVec<T>{type Item=T;type IntoIter=std::vec::IntoIter<T>;fn into_iter(self)->Self::IntoIter{self.0.into_iter()}}
#[derive(Default)]struct Audit{released:AtomicUsize,dropped:AtomicUsize}
mod g17_manager{
 use super::*;
 pub struct G17PLogicalQueueTeardown{pub audit:Arc<Audit>,pub barriers:Option<(Arc<Barrier>,Arc<Barrier>)>,pub released:bool}
 impl G17PLogicalQueueTeardown{pub fn release_retired_vm_mappings(&self){
  LOCKS.with(|v|assert_eq!(v.get(),0,"mapping cleanup under driver mutex"));
  if let Some((entered,release))=&self.barriers{entered.wait();release.wait();}
  self.audit.released.fetch_add(1,Ordering::SeqCst);
 }}
 impl Drop for G17PLogicalQueueTeardown{fn drop(&mut self){
  LOCKS.with(|v|assert_eq!(v.get(),0,"graph destruction under driver mutex"));
  assert!(self.audit.released.load(Ordering::SeqCst)>self.audit.dropped.load(Ordering::SeqCst));
  self.audit.dropped.fetch_add(1,Ordering::SeqCst);
 }}
}
struct AsahiDevice{g17_reclaims:Lock<KVec<KVec<g17_manager::G17PLogicalQueueTeardown>>>}
impl AsahiDevice{fn new()->Arc<Self>{Arc::new(Self{g17_reclaims:Lock(Mutex::new(KVec::default()))})}}
mod workqueue{
 pub struct Queue;
 pub fn system_unbound()->Queue{Queue}
 impl Queue{pub fn enqueue<T,const ID:u64>(&self,_dev:T)->bool{assert_eq!(ID,3);super::QUEUED.fetch_add(1,super::Ordering::SeqCst);true}}
}
fn batch(audit:Arc<Audit>,barriers:Option<(Arc<Barrier>,Arc<Barrier>)>)->KVec<g17_manager::G17PLogicalQueueTeardown>{
 KVec(vec![g17_manager::G17PLogicalQueueTeardown{audit,barriers,released:false}])
}
'''
rust += '\n'.join(item(m) for m in ['fn reclaim_g17p_graphs(', 'fn drain_g17p_reclaims(', 'fn queue_g17p_retired_graphs('])
rust += r'''
#[test]fn slow_reclamation_does_not_block_the_event_producer(){
 let dev=AsahiDevice::new();let audit=Arc::new(Audit::default());
 let entered=Arc::new(Barrier::new(2));let release=Arc::new(Barrier::new(2));
 queue_g17p_retired_graphs(dev.clone(),batch(audit.clone(),Some((entered.clone(),release.clone()))));
 assert_eq!(audit.released.load(Ordering::SeqCst),0,"event producer ran cleanup");
 let worker_dev=dev.clone();let worker=std::thread::spawn(move||drain_g17p_reclaims(&worker_dev));
 entered.wait();
 let producer_dev=dev.clone();let producer_audit=audit.clone();let(tx,rx)=std::sync::mpsc::channel();
 let producer=std::thread::spawn(move||{queue_g17p_retired_graphs(producer_dev,batch(producer_audit,None));tx.send(()).unwrap();});
 let progressed=rx.recv_timeout(std::time::Duration::from_secs(1)).is_ok();
 release.wait();worker.join().unwrap();producer.join().unwrap();assert!(progressed);
 assert_eq!(audit.dropped.load(Ordering::SeqCst),1);
 drain_g17p_reclaims(&dev);assert_eq!(audit.dropped.load(Ordering::SeqCst),2);
}
#[test]fn cancelled_queued_worker_is_drained_once_by_unbind(){
 let dev=AsahiDevice::new();let audit=Arc::new(Audit::default());
 for _ in 0..64{queue_g17p_retired_graphs(dev.clone(),batch(audit.clone(),None));}
 assert_eq!(audit.dropped.load(Ordering::SeqCst),0);
 // Model cancel_work_sync preventing callback entry; run the actual unbind drain.
 drain_g17p_reclaims(&dev);drain_g17p_reclaims(&dev);
 assert_eq!(audit.released.load(Ordering::SeqCst),64);assert_eq!(audit.dropped.load(Ordering::SeqCst),64);
}
#[test]fn allocation_failure_reclaims_outside_locks_without_queueing(){
 let dev=AsahiDevice::new();let audit=Arc::new(Audit::default());
 let before=QUEUED.load(Ordering::SeqCst);FAIL_ALLOC.store(true,Ordering::SeqCst);
 queue_g17p_retired_graphs(dev.clone(),batch(audit.clone(),None));
 assert_eq!(QUEUED.load(Ordering::SeqCst),before);
 assert_eq!(audit.released.load(Ordering::SeqCst),1);assert_eq!(audit.dropped.load(Ordering::SeqCst),1);
 drain_g17p_reclaims(&dev);assert_eq!(audit.dropped.load(Ordering::SeqCst),1);
}
#[test]fn empty_batch_does_not_queue_work(){let before=QUEUED.load(Ordering::SeqCst);queue_g17p_retired_graphs(AsahiDevice::new(),KVec::default());assert_eq!(QUEUED.load(Ordering::SeqCst),before);}
'''
with tempfile.TemporaryDirectory() as d:
 p=Path(d)/'test.rs';p.write_text(rust);binary=Path(d)/'test'
 subprocess.run(['rustc','--edition=2021','--test',str(p),'-o',str(binary)],check=True)
 subprocess.run([str(binary),'--test-threads=1'],check=True)
# The only producer is joined before cancellation/draining. No worker owns a
# raw runtime reference; cleanup completes before platform resources disappear.
assert source.count('=> queue_g17p_retired_graphs(')==1
cancel=item('    fn cancel_reclaim_work(')
assert cancel.index('disable_g17p_work::<3>') < cancel.index('drain_g17p_reclaims(')
lifecycle=(root/'g17_lifecycle.rs').read_text()
a=lifecycle.index('pub(crate) fn quiesce_driver_unbind<');b=lifecycle.index('\n}',a)
order=lifecycle[a:b]
assert order.index('owner.cancel_event_work()') < order.index('owner.cancel_reclaim_work()')
print('PASS acknowledged reclaim is independent of event ingress and joined at unbind')
