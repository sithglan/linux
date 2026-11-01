#!/usr/bin/env python3
"""Compile unchanged production job/scheduler with a deterministic DRM/fence model."""
from pathlib import Path
import subprocess,tempfile
p=Path(__file__).resolve().parents[1]
s=(p/'g17_job.rs').read_text()
# Inner module docs retain their original placement inside actual.
mock=r'''
#![allow(dead_code,unused_variables)]
extern crate self as kernel;
pub mod time { pub type Msecs = u32; }
#[macro_export]macro_rules! pr_info { ($($arg:tt)*) => {{ let _ = format_args!($($arg)*); }}; }
#[macro_export]macro_rules! c_str{($s:literal)=>{$s}}
#[macro_export]macro_rules! new_mutex{($value:expr,$name:expr)=>{crate::sync::Mutex(std::sync::Mutex::new($value))}}
pub mod prelude{pub type Error=i32;pub trait Errno{fn to_errno(&self)->i32;}impl Errno for i32{fn to_errno(&self)->i32{-self.abs()}}pub type Result<T=()> = std::result::Result<T,i32>;pub type KVec<T>=Vec<T>;pub const EINVAL:i32=22;pub const ECANCELED:i32=125;pub const GFP_KERNEL:u32=0;}
pub mod sync{
 pub struct Mutex<T>(pub std::sync::Mutex<T>);
 impl<T>Mutex<T>{pub fn lock(&self)->std::sync::MutexGuard<'_,T>{self.0.lock().unwrap()}}
 pub struct Arc<T>(pub std::sync::Arc<T>);
 impl<T>Arc<T>{pub fn new(v:T,_flags:u32)->Result<Self,i32>{Ok(Self(std::sync::Arc::new(v)))}pub fn pin_init(v:T,flags:u32)->Result<Self,i32>{Self::new(v,flags)}}
 impl<T>Clone for Arc<T>{fn clone(&self)->Self{Self(self.0.clone())}}
 impl<T>std::ops::Deref for Arc<T>{type Target=T;fn deref(&self)->&T{&self.0}}
}
pub mod dma_fence{
 pub struct UserFence<T>(pub Fence,pub std::marker::PhantomData<T>);
 pub trait RawDmaFence{}
 impl<T>UserFence<T>{pub fn set_error(&self,_error:i32){}pub fn signal(&self){self.0.signal()}}

 use std::sync::{Arc,atomic::{AtomicBool,Ordering}};
 #[derive(Clone)]pub struct Fence(Arc<AtomicBool>);
 impl Fence{pub fn new()->Self{Self(Arc::new(AtomicBool::new(false)))}pub fn from_fence<T>(f:&UserFence<T>)->Self{f.0.clone()}pub fn done(&self)->bool{self.0.load(Ordering::Acquire)}pub fn signal(&self){self.0.store(true,Ordering::Release)}}
}
pub mod g17_status{pub struct VmStatus;impl VmStatus{pub fn record(&self,_e:i32){}pub fn record_if_device_loss(&self,_e:i32){}}}
pub mod g17_resources{pub struct G17PWorkStateLease;pub use crate::mmu::T8140ComputeExecutionContext as G17PLogicalQueueContext;pub struct G17PRenderQueuePair;impl G17PRenderQueuePair{pub const SLOT_COUNT:u8=2;}}
pub mod g17_drm{
 use crate::dma_fence::Fence;
 pub struct G17PSubmissionFence;
 pub struct G17PSubmitCompletionMember;
 impl G17PSubmitCompletionMember{pub fn complete(&self,_r:crate::prelude::Result){}pub fn work_state(&self)->crate::sync::Arc<crate::g17_resources::G17PWorkStateLease>{crate::sync::Arc::new(crate::g17_resources::G17PWorkStateLease,0).unwrap()}}
 pub struct G17PRenderCompletion(pub Fence);
 impl G17PRenderCompletion{pub fn fence(&self)->Fence{self.0.clone()}}
}
pub mod driver{pub struct AsahiDevice;impl AsRef<()> for AsahiDevice{fn as_ref(&self)->&(){&()}}}
pub mod g17_uapi{#[derive(Clone,Copy)]pub struct TranslatedComputeCommand{pub value:u64}#[derive(Clone,Copy)]pub struct TranslatedRenderCommand{pub value:u64}}
pub mod mmu{
 pub struct T8140VmJobGuard;
 impl T8140VmJobGuard{pub fn quarantine(&self){}}
 use std::sync::{Arc,Mutex,atomic::{AtomicUsize,Ordering}};
 pub struct KernelMapping{pub address:u64,pub bytes:Arc<Mutex<Vec<u8>>>,pub drops:Arc<AtomicUsize>}
 impl KernelMapping{pub fn iova(&self)->u64{self.address}pub fn size(&self)->usize{self.bytes.lock().unwrap().len()}pub fn with_cpu_bytes<R>(&self,f:impl FnOnce(&mut[u8])->Result<R,i32>)->Result<R,i32>{f(&mut self.bytes.lock().unwrap())}}
 impl Drop for KernelMapping{fn drop(&mut self){self.drops.fetch_add(1,Ordering::SeqCst);}}
 pub struct T8140ComputeExecutionContext{pub id:u32,pub drops:Arc<AtomicUsize>}
 impl Drop for T8140ComputeExecutionContext{fn drop(&mut self){self.drops.fetch_add(1,Ordering::SeqCst);}}
}
pub mod drm{pub mod sched{
 use crate::{dma_fence::Fence,prelude::Result};use std::sync::{Arc,Mutex};use std::ops::{Deref,DerefMut};
 pub enum Status{Nominal}pub enum Priority{Kernel}
 pub trait JobImpl:Sized{const MANAGED_TIMEOUT_RECOVERY:bool=false;fn false_timeout(_j:&mut Job<Self>)->bool{false}fn prepare(_j:&mut Job<Self>)->Option<Fence>{None}fn run(j:&mut Job<Self>)->Result<Option<Fence>>;fn timed_out(j:&mut Job<Self>)->Status;fn cancel(j:&mut Job<Self>);}
 pub struct Job<T:JobImpl>(T);impl<T:JobImpl>Deref for Job<T>{type Target=T;fn deref(&self)->&T{&self.0}}impl<T:JobImpl>DerefMut for Job<T>{fn deref_mut(&mut self)->&mut T{&mut self.0}}
 struct Entry<T:JobImpl>{job:Job<T>,deps:Vec<Fence>,scheduled:Fence,finished:Fence,hardware:Option<Fence>}
 pub struct Scheduler<T:JobImpl>{entries:Arc<Mutex<Vec<Entry<T>>>>}
 pub struct Entity<T:JobImpl>{entries:Arc<Mutex<Vec<Entry<T>>>>}
 pub struct PendingJob<T:JobImpl>{entry:Entry<T>,entries:Arc<Mutex<Vec<Entry<T>>>>}
 pub struct ArmedJob<T:JobImpl>(PendingJob<T>);
 pub struct Fences(Fence,Fence);impl Fences{pub fn finished(&mut self)->Fence{self.0.clone()}pub fn scheduled(&mut self)->Fence{self.1.clone()}}
 impl<T:JobImpl>Scheduler<T>{pub fn new(_d:&(),_r:u32,_c:u32,_h:u32,_t:u32,_n:&str)->Result<Self>{Ok(Self{entries:Arc::new(Mutex::new(Vec::new()))})}
 pub fn drive(&self){let mut entries=self.entries.lock().unwrap();for e in entries.iter_mut(){if e.hardware.is_none()&&e.deps.iter().all(Fence::done)&&T::prepare(&mut e.job).is_none(){e.hardware=T::run(&mut e.job).unwrap();e.scheduled.signal();}}
 entries.retain(|e|{if e.hardware.as_ref().is_some_and(Fence::done){e.finished.signal();false}else{true}});}
 pub fn cancel_all(&self){for mut e in self.entries.lock().unwrap().drain(..){T::cancel(&mut e.job);e.finished.signal();}}
 }
 impl<T:JobImpl>Entity<T>{pub fn new(s:&Scheduler<T>,_p:Priority)->Result<Self>{Ok(Self{entries:s.entries.clone()})}pub fn new_job(&mut self,_c:u32,j:T)->Result<PendingJob<T>>{Ok(PendingJob{entry:Entry{job:Job(j),deps:Vec::new(),scheduled:Fence::new(),finished:Fence::new(),hardware:None},entries:self.entries.clone()})}}
 impl<T:JobImpl>PendingJob<T>{pub fn add_dependency(&mut self,f:Fence)->Result{self.entry.deps.push(f);Ok(())}pub fn arm(self)->ArmedJob<T>{ArmedJob(self)}}
 impl<T:JobImpl>ArmedJob<T>{pub fn fences(&mut self)->Fences{Fences(self.0.entry.finished.clone(),self.0.entry.scheduled.clone())}pub fn push(self){self.0.entries.lock().unwrap().push(self.0.entry)}}
}}
'''
tests=r'''
#[cfg(test)]mod tests{
 use super::*;use std::sync::{Arc as StdArc,Mutex,atomic::{AtomicUsize,Ordering}};
 #[derive(Default)]struct Backend{active:Mutex<Vec<(Arc<G17PComputeJobPacket>,Fence)>>,cancelled:AtomicUsize}
 impl G17PQueueBackend for Backend{
  fn prepare(&self,_p:&G17PComputeJobPacket)->Option<Fence>{None}
  fn publish(&self,p:Arc<G17PComputeJobPacket>)->Result<Fence>{let f=p.completion()?.fence();self.active.lock().unwrap().push((p,f.clone()));Ok(f)}
  fn timed_out(&self,_p:Arc<G17PComputeJobPacket>)->sched::Status{sched::Status::Nominal}
  fn cancel(&self,_p:Arc<G17PComputeJobPacket>){self.cancelled.fetch_add(1,Ordering::SeqCst);/* active list deliberately quarantines hardware-owned packets */}
 }
 impl Backend{fn complete(&self,id:u64){let mut a=self.active.lock().unwrap();let i=a.iter().position(|(p,_)|p.submission_id()==id).unwrap();let (p,_)=a.remove(i);p.completion().unwrap().complete(Ok([1,2]));}}
 fn packet(id:u64,drops:&StdArc<AtomicUsize>,root_drops:&StdArc<AtomicUsize>)->Arc<G17PComputeJobPacket>{
  let context=Arc::new(mmu::T8140ComputeExecutionContext{id:id as u32+4,drops:root_drops.clone()},0).unwrap();
  let bytes=StdArc::new(Mutex::new(vec![0xa5;64]));let mut source=vec![id as u8;16];
  let sealed=G17PSealedCommand::new(mmu::KernelMapping{address:id*0x10000,bytes:bytes.clone(),drops:drops.clone()},&source).unwrap();
  source.fill(0xff);assert_eq!(&bytes.lock().unwrap()[..16],&[id as u8;16]);assert!(bytes.lock().unwrap()[16..].iter().all(|v|*v==0));
  let completion=G17PJobCompletion::new(
   UserFence(Fence::new(),std::marker::PhantomData),
   mmu::KernelMapping{address:0,bytes:StdArc::new(Mutex::new(vec![0;16])),drops:StdArc::new(AtomicUsize::new(0))},
   [None,None],Arc::new(crate::g17_status::VmStatus,0).unwrap(),
   mmu::T8140VmJobGuard,crate::g17_drm::G17PSubmitCompletionMember).unwrap();
  G17PComputeJobPacket::new(id,id as u8+4,context,g17_uapi::TranslatedComputeCommand{value:id},vec![sealed],vec![],vec![],vec![],Some(completion)).unwrap()
 }
 #[test]fn two_published_jobs_outlive_submitters_and_complete_out_of_order(){
  let d=StdArc::new(AtomicUsize::new(0));let r=StdArc::new(AtomicUsize::new(0));let b=Arc::new(Backend::default(),0).unwrap();
  let mut a=G17PJobScheduler::new(&AsahiDevice,b.clone(),16).unwrap();let mut c=G17PJobScheduler::new(&AsahiDevice,b.clone(),16).unwrap();
  let fa=a.enqueue(packet(1,&d,&r),vec![]).unwrap();let fb=c.enqueue(packet(2,&d,&r),vec![]).unwrap();assert!(!fa.completed.done()&&!fb.completed.done());assert!(!fa.published.done()&&!fb.published.done());
  a._scheduler.drive();c._scheduler.drive();assert_eq!(b.active.lock().unwrap().len(),2);assert!(fa.published.done()&&fb.published.done());assert!(!fa.completed.done()&&!fb.completed.done());assert_eq!(r.load(Ordering::SeqCst),0);
  b.complete(2);assert!(fb.completed.done()&&!fa.completed.done());assert_eq!(d.load(Ordering::SeqCst),0);c._scheduler.drive();assert_eq!(d.load(Ordering::SeqCst),1);assert_eq!(r.load(Ordering::SeqCst),1);
  b.complete(1);assert!(fa.completed.done());a._scheduler.drive();assert_eq!(d.load(Ordering::SeqCst),2);assert_eq!(r.load(Ordering::SeqCst),2);
 }
 #[test]fn dependencies_gate_publication_and_active_cancel_retains_ownership(){
  let d=StdArc::new(AtomicUsize::new(0));let r=StdArc::new(AtomicUsize::new(0));let b=Arc::new(Backend::default(),0).unwrap();
  let mut q=G17PJobScheduler::new(&AsahiDevice,b.clone(),16).unwrap();let dependency=Fence::new();let finished=q.enqueue(packet(1,&d,&r),vec![dependency.clone()]).unwrap();
  q._scheduler.drive();assert!(b.active.lock().unwrap().is_empty());assert!(!finished.completed.done()&&!finished.published.done());dependency.signal();q._scheduler.drive();assert_eq!(b.active.lock().unwrap().len(),1);assert!(finished.published.done()&&!finished.completed.done());
  q._scheduler.cancel_all();drop(q);assert_eq!(d.load(Ordering::SeqCst),0);assert_eq!(r.load(Ordering::SeqCst),0);
  b.complete(1);assert_eq!(d.load(Ordering::SeqCst),1);assert_eq!(r.load(Ordering::SeqCst),1);
 }
 #[test]fn invalid_sealing_drops_unpublished_resources(){
  let d=StdArc::new(AtomicUsize::new(0));let bytes=StdArc::new(Mutex::new(vec![0xa5;4]));
  assert!(G17PSealedCommand::new(mmu::KernelMapping{address:0,bytes:bytes.clone(),drops:d.clone()},&[1;5]).is_err());assert_eq!(*bytes.lock().unwrap(),vec![0xa5;4]);assert_eq!(d.load(Ordering::SeqCst),1);
 }
 #[test]fn compute_packets_accept_every_nonbootstrap_qid(){
  for qid in 0..=255u8 {
   let context=Arc::new(mmu::T8140ComputeExecutionContext{id:5,drops:StdArc::new(AtomicUsize::new(0))},0).unwrap();
   let packet=G17PComputeJobPacket::new(1,qid,context,g17_uapi::TranslatedComputeCommand{value:1},vec![],vec![],vec![],vec![],None);
   assert_eq!(packet.is_ok(),(2..128).contains(&qid),"qid {qid}");
  }
 }
}
'''
with tempfile.TemporaryDirectory() as d:
 f=Path(d)/'test.rs';f.write_text(mock+'mod actual{\n'+s+tests+'}\n');exe=Path(d)/'test'
 subprocess.run(['rustc','--edition=2021','--test',str(f),'-o',str(exe)],check=True)
 subprocess.run([str(exe),'--test-threads=1'],check=True)
print('PASS production packets/scheduler with modeled DRM: immutable images, two outstanding fences, dependencies, out-of-order retirement, active cancel/quarantine lifetime; NOT hardware parallel proof')
