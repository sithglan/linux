#!/usr/bin/env python3
"""Production completion graph ownership and unlocked observation boundaries."""
from pathlib import Path
import subprocess,tempfile
r=Path(__file__).resolve().parents[1]
m=(r/'g17_manager.rs').read_text();d=(r/'g17_drm.rs').read_text();l=(r/'g17_live_boot.rs').read_text()
def item(s,key):
 a=s.index(key);b=s.index('{',a)+1;n=1
 while n:n+=(s[b]=='{')-(s[b]=='}');b+=1
 return s[a:b]
worker=item(d,'    fn service_render_observations(')
assert worker.index('drop(runtime)') < worker.index('observation.observe()') < worker.index('runtime.return_render_observation(observation)')
ret=worker.index('runtime.return_render_observation(observation)')
assert '?' not in worker[worker.index('drop(runtime)'):ret]
assert ret < worker.index('!epoch.is_current()',ret) < worker.index('runtime.commit_render_observation(')
assert worker.index('drop(runtime);',ret)<worker.index('completion.signal()')
assert 'enter_wait' not in worker
assert 'retain_render_completion_masks(completion_masks)' in item(l,'    pub(crate) fn service_primary_firmware_events(')
assert 'service_async_render_completions' not in item(l,'    pub(crate) fn service_primary_firmware_events(')
assert 'render_pair_preparing(render_slot) { return Err(EAGAIN); }' in item(l,'    pub(crate) fn take_render_restage(')
commit=item(l,'    pub(crate) fn commit_render_observation(')
assert commit.index('!self.render_fence_state.pending(slot)') < commit.index('.commit_render_observation(slot, Some(observed))')
code=r'''
#![allow(dead_code)]
use std::sync::{Arc,Mutex,Barrier};
use std::sync::atomic::{fence,Ordering,AtomicUsize};
type KBox<T>=Box<T>;type Result<T=()>=std::result::Result<T,i32>;
const EIO:i32=5;const ENODEV:i32=19;const EAGAIN:i32=11;
mod g17_completion{
 #[derive(Clone,Copy)]pub struct QueueIndices(pub u32);
 #[derive(Clone,Copy)]pub struct RenderQueueWindow;impl RenderQueueWindow{pub fn target(self)->u32{21}}
 #[derive(Clone,Copy)]pub struct PairedRenderCompletion;
 pub struct RenderExecutionState{pub job_list_empty:bool,pub tiling_timestamps:[u64;2],pub fragment_timestamps:[u64;2]}
 pub fn observe_paired_render_window_depth(_:RenderQueueWindow,_:u32,_:QueueIndices,s:QueueIndices,_:QueueIndices,_:QueueIndices,_:RenderExecutionState)->std::result::Result<PairedRenderCompletion,()>{if s.0==0{Err(())}else{Ok(PairedRenderCompletion)}}
}
mod g17_resources{
 use super::*;
 pub const G17P_RENDER_MAX_IN_FLIGHT:usize=32;
 pub fn g17p_render_completion_window(_:u64)->Result<g17_completion::RenderQueueWindow>{Ok(g17_completion::RenderQueueWindow)}
 #[derive(Clone,Copy)]pub struct G17PUserRenderQueueState{pub tiling:g17_completion::QueueIndices,pub fragment:g17_completion::QueueIndices,pub job_list_empty:bool,pub tiling_timestamps:[u64;2],pub fragment_timestamps:[u64;2]}
 pub struct G17PUserRenderStorage{pub id:usize,pub reads:usize,pub fail:bool,pub torn:bool,pub drops:Arc<AtomicUsize>}
 impl G17PUserRenderStorage{pub fn snapshot(&mut self,o:u64)->Result<G17PUserRenderQueueState>{assert_eq!(o,7);self.reads+=1;if self.fail{return Err(EIO)};Ok(G17PUserRenderQueueState{tiling:g17_completion::QueueIndices(u32::from(!self.torn)),fragment:g17_completion::QueueIndices(1),job_list_empty:true,tiling_timestamps:[1;2],fragment_timestamps:[2;2]})}}
 impl Drop for G17PUserRenderStorage{fn drop(&mut self){self.drops.fetch_add(1,Ordering::SeqCst);}}
}
enum G17PBootstrapUmaRequestState{Submitted,Ready}struct Uma{state:G17PBootstrapUmaRequestState}
struct Tracker{ordinal:u64,tiling_prefix:u32,fragment_prefix:u32,tiling_previous:g17_completion::QueueIndices,fragment_previous:g17_completion::QueueIndices}
struct Slot{preparing:bool,restaged:Option<()>,tracker:Option<Tracker>,bootstrap_uma:Option<Uma>,storage:Option<Box<g17_resources::G17PUserRenderStorage>>}
struct Slots(Vec<Slot>);impl Slots{fn allocated(&self,s:u8)->Result<&Slot>{self.0.get(s as usize).ok_or(EIO)}fn get_mut(&mut self,s:u8)->&mut Slot{&mut self.0[s as usize]}}
''' + item(m,'pub(crate) struct G17PRenderObservation {') + item(m,'impl G17PRenderObservation {') + '''
struct Manager{render_slots:Slots}
impl Manager{
''' + item(m,'    pub(crate) fn take_render_observation(') + item(m,'    pub(crate) fn return_render_observation(') + r'''
}
fn manager(drops:Arc<AtomicUsize>)->Manager{Manager{render_slots:Slots((0..2).map(|id|Slot{preparing:false,restaged:None,tracker:Some(Tracker{ordinal:7,tiling_prefix:21,fragment_prefix:21,tiling_previous:g17_completion::QueueIndices(1),fragment_previous:g17_completion::QueueIndices(1)}),bootstrap_uma:None,storage:Some(Box::new(g17_resources::G17PUserRenderStorage{id,reads:0,fail:false,torn:false,drops:drops.clone()}))}).collect())}}
#[test]fn independent_owners_observe_concurrently_without_global_lock(){
 let drops=Arc::new(AtomicUsize::new(0));let m=Arc::new(Mutex::new(manager(drops.clone())));let barrier=Arc::new(Barrier::new(3));let mut threads=vec![];
 for slot in 0..2{let m=m.clone();let b=barrier.clone();threads.push(std::thread::spawn(move||{let mut read=m.lock().unwrap().take_render_observation(slot).unwrap().unwrap();b.wait();b.wait();read.observe().unwrap();m.lock().unwrap().return_render_observation(read);}));}
 barrier.wait();{let mut m=m.lock().unwrap();for slot in 0..2{assert!(m.render_slots.allocated(slot).unwrap().preparing);assert!(m.take_render_observation(slot).unwrap().is_none());}assert_eq!(drops.load(Ordering::SeqCst),0);}barrier.wait();for t in threads{t.join().unwrap();}
 let m=m.lock().unwrap();for (id,s) in m.render_slots.0.iter().enumerate(){assert!(!s.preparing);assert_eq!(s.storage.as_ref().unwrap().id,id);assert_eq!(s.storage.as_ref().unwrap().reads,1);}
}
#[test]fn read_failure_and_torn_snapshots_preserve_exact_graph(){for torn in [false,true]{let drops=Arc::new(AtomicUsize::new(0));let mut m=manager(drops.clone());{let s=m.render_slots.get_mut(0).storage.as_mut().unwrap();s.fail=!torn;s.torn=torn;}let mut read=m.take_render_observation(0).unwrap().unwrap();assert!(matches!(read.observe(),Err(e) if e==if torn{EAGAIN}else{EIO}));m.return_render_observation(read);assert_eq!(drops.load(Ordering::SeqCst),0);assert_eq!(m.render_slots.get_mut(0).storage.as_ref().unwrap().reads,if torn{2}else{1});}}
#[test]fn submitted_grow_and_unpublished_restage_exclude_only_their_pair(){let mut m=manager(Arc::new(AtomicUsize::new(0)));m.render_slots.get_mut(0).bootstrap_uma=Some(Uma{state:G17PBootstrapUmaRequestState::Submitted});assert!(m.take_render_observation(0).unwrap().is_none());let read=m.take_render_observation(1).unwrap().unwrap();m.return_render_observation(read);m.render_slots.get_mut(0).bootstrap_uma=None;m.render_slots.get_mut(0).restaged=Some(());assert!(m.take_render_observation(0).unwrap().is_none());}
'''
with tempfile.TemporaryDirectory() as tmp:
 p=Path(tmp)/'test.rs';p.write_text(code);exe=Path(tmp)/'test'
 subprocess.run(['rustc','--edition=2021','--test',str(p),'-o',str(exe)],check=True)
 subprocess.run([str(exe)],check=True,timeout=15)
print('PASS production off-runtime completion borrow/observe/return; independent owners, bounded read retry, error lifetime, grow exclusion, epoch/commit ordering')
# Exercise the actual event-worker orchestration, including epoch invalidation
# during an unlocked read and draining successors across more than one pair.
program=r'''
#![allow(dead_code)]
use std::sync::atomic::{AtomicBool,AtomicUsize,Ordering};
use std::cell::RefCell;
type Result<T=()> = std::result::Result<T,i32>;
const EAGAIN:i32=11;const ENODEV:i32=19;const EIO:i32=5;
static LOCKED:AtomicBool=AtomicBool::new(false);
static CURRENT:AtomicBool=AtomicBool::new(true);
static LEASES:AtomicUsize=AtomicUsize::new(0);
static SIGNALS:AtomicUsize=AtomicUsize::new(0);
struct Mutex<T>(RefCell<T>);struct Guard<'a,T>(std::cell::RefMut<'a,T>);
impl<T> Mutex<T>{fn lock(&self)->Guard<'_,T>{assert!(!LOCKED.swap(true,Ordering::SeqCst));Guard(self.0.borrow_mut())}}
impl<T> std::ops::Deref for Guard<'_,T>{type Target=T;fn deref(&self)->&T{&self.0}}
impl<T> std::ops::DerefMut for Guard<'_,T>{fn deref_mut(&mut self)->&mut T{&mut self.0}}
impl<T> Drop for Guard<'_,T>{fn drop(&mut self){LOCKED.store(false,Ordering::SeqCst);}}
struct Preparations;struct Epoch;
impl Preparations{fn enter(&self)->Result<Epoch>{if !CURRENT.load(Ordering::SeqCst){return Err(EAGAIN)}LEASES.fetch_add(1,Ordering::SeqCst);Ok(Epoch)}}
impl Epoch{fn is_current(&self)->bool{CURRENT.load(Ordering::SeqCst)}}
impl Drop for Epoch{fn drop(&mut self){LEASES.fetch_sub(1,Ordering::SeqCst);}}
mod g17_resources{pub const G17P_RENDER_MAX_IN_FLIGHT:usize=2;pub struct G17PRenderQueuePair;impl G17PRenderQueuePair{pub const SLOT_COUNT:u8=4;}}
struct Observation{pair_slot:u8,fail:bool,invalidate:bool}
impl Observation{fn observe(&mut self)->Result<bool>{assert!(!LOCKED.load(Ordering::SeqCst));assert_eq!(LEASES.load(Ordering::SeqCst),1);if self.invalidate{CURRENT.store(false,Ordering::SeqCst);}if self.fail{Err(EIO)}else{Ok(true)}}}
struct Completion;impl Completion{fn signal(self){assert!(!LOCKED.load(Ordering::SeqCst));SIGNALS.fetch_add(1,Ordering::SeqCst);}}
struct Runtime{queued:[usize;4],returned:usize,committed:usize,borrowed:bool,fail:bool,invalidate:bool,completed:Vec<Completion>}
impl Runtime{
 fn next_render_observation(&mut self,start:u8)->Result<Option<Observation>>{assert!(!self.borrowed);for s in start..4{if self.queued[s as usize]>0{self.borrowed=true;return Ok(Some(Observation{pair_slot:s,fail:self.fail,invalidate:self.invalidate}));}}Ok(None)}
 fn return_render_observation(&mut self,_:Observation){assert!(self.borrowed);self.borrowed=false;self.returned+=1;}
 fn begin_deferred_completion_drain(&mut self){}
 fn commit_render_observation(&mut self,s:u8,observed:Result<bool>)->Result<bool>{assert!(!self.borrowed);let success=observed?;self.committed+=1;self.queued[s as usize]-=1;if success{self.completed.push(Completion);}Ok(success)}
 fn take_deferred_completions(&mut self)->Vec<Completion>{std::mem::take(&mut self.completed)}
}
struct Wrapper{runtime:Mutex<Runtime>,preparations:Preparations}
impl Wrapper{fn mutex(&self)->&Mutex<Runtime>{&self.runtime}
''' + worker + r'''
}
#[test]fn actual_wrapper_returns_before_failure_or_stale_epoch_and_drains_each_pair(){
 for scenario in 0..3{
 CURRENT.store(true,Ordering::SeqCst);SIGNALS.store(0,Ordering::SeqCst);
 let w=Wrapper{preparations:Preparations,runtime:Mutex(RefCell::new(Runtime{queued:[2,2,2,2],returned:0,committed:0,borrowed:false,fail:scenario==1,invalidate:scenario==2,completed:vec![]}))};
 let result=w.service_render_observations();assert_eq!(LEASES.load(Ordering::SeqCst),0);
 let r=w.runtime.0.borrow();assert!(!r.borrowed);
 if scenario==0{assert!(result.is_ok());assert_eq!(r.returned,8);assert_eq!(r.committed,8);assert_eq!(SIGNALS.load(Ordering::SeqCst),8);}
 else{assert_eq!(r.returned,1);assert_eq!(r.committed,0);assert_eq!(SIGNALS.load(Ordering::SeqCst),0);assert_eq!(result,if scenario==1{Err(EIO)}else{Ok(())});}
 }
}
'''
with tempfile.TemporaryDirectory() as tmp:
 p=Path(tmp)/'worker.rs';p.write_text(program);exe=Path(tmp)/'worker'
 subprocess.run(['rustc','--edition=2021','--test',str(p),'-o',str(exe)],check=True)
 subprocess.run([str(exe)],check=True,timeout=15)
print('PASS actual worker unlock/return/commit/signal lifecycle, epoch invalidation and per-pair budget reset')
