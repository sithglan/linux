#!/usr/bin/env python3
"""Execute actual lazy compute-queue allocation/drop with fault injection."""
from pathlib import Path
import subprocess,tempfile
p=Path(__file__).resolve().parents[1];s=(p/'g17_drm.rs').read_text()
def block(text,needle,start=0):
 a=text.index(needle,start);b=text.index('{',a);i=b+1;d=1
 while d:d+=(text[i]=='{')-(text[i]=='}');i+=1
 return text[a:i]
constructor=block(s,'    fn new(\n',s.index('impl G17PQueue {'))
ensure=block(s,'    fn ensure_async_compute_queue(')
production='\n'.join(block(s,n) for n in ['struct G17PQueue {','enum G17PSubmissionEngine {','impl Drop for G17PQueue {','pub(crate) struct G17PAsyncQueueBackend {','impl Drop for G17PAsyncQueueBackend {'])
limits=(p/'g17_queue_limits.rs').read_text().replace('//!','//')
reserve=block(s,'    fn reserve_queue(');release=block(s,'    fn release_queue(')
mock=r'''
#![allow(dead_code,unused_imports)]
use std::sync::{Mutex,MutexGuard};use std::sync::atomic::{AtomicBool,AtomicU32,Ordering};use std::ops::{Deref,Range};
type Result<T=()> = std::result::Result<T,i32>;const EINVAL:i32=22;const ENOSPC:i32=28;const EIO:i32=5;const GFP_KERNEL:u32=0;
static FAIL_CONTEXT:AtomicBool=AtomicBool::new(false);static FAIL_PREPARE:AtomicBool=AtomicBool::new(false);static FAIL_BACKEND:AtomicBool=AtomicBool::new(false);static FAIL_SCHEDULER:AtomicBool=AtomicBool::new(false);static FAIL_FENCE:AtomicBool=AtomicBool::new(false);
struct Arc<T>(std::sync::Arc<T>);impl<T>Arc<T>{fn new(v:T,_flags:u32)->Result<Self>{if std::any::type_name::<T>().ends_with("G17PAsyncQueueBackend")&&FAIL_BACKEND.swap(false,Ordering::SeqCst){drop(v);Err(12)}else{Ok(Self(std::sync::Arc::new(v)))}}}impl<T>Clone for Arc<T>{fn clone(&self)->Self{Self(self.0.clone())}}impl<T>Deref for Arc<T>{type Target=T;fn deref(&self)->&T{&self.0}}
struct G17PQueueRegistry{live_queues:AtomicU32}
impl G17PQueueRegistry{fn release_render_owner(&self,_owner:u64){}}
#[derive(Clone)]struct AsahiDevice;type AsahiDevRef=AsahiDevice;impl From<&AsahiDevice> for AsahiDevice{fn from(d:&AsahiDevice)->Self{d.clone()}}
mod mmu{#[derive(Clone)]pub struct Vm;pub struct T8140ComputeExecutionContext;impl T8140ComputeExecutionContext{pub fn context_id(&self)->u32{5}}}
mod g17_resources{pub use crate::mmu::T8140ComputeExecutionContext as G17PLogicalQueueContext;}
struct Fence;
struct FenceContexts;impl FenceContexts{fn new(_n:u32,_name:&str,_key:u32)->Result<Self>{if FAIL_FENCE.swap(false,Ordering::SeqCst){Err(12)}else{Ok(Self)}}}
const QUEUE_NAME:&str="queue";const QUEUE_CLASS_KEY:u32=0;const T8140_USER_START:u64=0x4000;const T8140_UNKNOWN_PAGE:u64=1<<42;
mod g17_uapi{pub struct QueueUscWindow{pub base:u64,pub user_start:u64,pub user_end:u64}impl QueueUscWindow{pub fn validate(&self)->Result<(),()>{Ok(())}}}
struct State{owners:Vec<u64>,released:Vec<u64>,prepares:usize}
#[derive(Clone,Copy)]struct G17PRuntimeRef(&'static Mutex<State>);
impl G17PRuntimeRef{fn new()->Self{Self(Box::leak(Box::new(Mutex::new(State{owners:vec![],released:vec![],prepares:0}))))}fn mutex(&self)->&Self{self}fn lock(&self)->Guard<'_>{Guard(self.0.lock().unwrap())}fn release_compute_owner(&self,_id:u64){}}
struct Guard<'a>(MutexGuard<'a,State>);impl Guard<'_>{fn prepare_compute_execution_context(&mut self,context:&Arc<mmu::T8140ComputeExecutionContext>)->Result<Arc<mmu::T8140ComputeExecutionContext>>{Ok(context.clone())}fn prepare_queue_execution_context(&mut self,_vm:&mmu::Vm)->Result<Arc<mmu::T8140ComputeExecutionContext>>{if FAIL_CONTEXT.swap(false,Ordering::SeqCst){Err(12)}else{Arc::new(mmu::T8140ComputeExecutionContext,0)}}fn prepare_async_compute_queue(&mut self,id:u64,_vm:&mmu::Vm,context:Arc<mmu::T8140ComputeExecutionContext>)->Result<(u8,Arc<mmu::T8140ComputeExecutionContext>)>{if FAIL_PREPARE.swap(false,Ordering::SeqCst){return Err(12)}assert!(!self.0.owners.contains(&id));self.0.owners.push(id);self.0.prepares+=1;Ok((4,context))}fn async_compute_queue_matches(&self,id:u64,hardware_queue:u8,context:&Arc<mmu::T8140ComputeExecutionContext>)->Result<bool>{if !self.0.owners.contains(&id){return Ok(false)}if hardware_queue!=4||context.context_id()!=5{return Err(EIO)}Ok(true)}fn release_async_compute_queue(&mut self,id:u64){if let Some(i)=self.0.owners.iter().position(|x|*x==id){self.0.owners.remove(i);self.0.released.push(id)}}fn release_render_owner(&mut self,_id:u64,_context:u16){}}
struct G17PRenderQueueBackend{runtime:G17PRuntimeRef,registry:Arc<G17PQueueRegistry>,owner:u64,vm:mmu::Vm,context:Arc<mmu::T8140ComputeExecutionContext>}
mod g17_job{use super::*;pub struct G17PJobScheduler<T>(Arc<T>);impl<T>G17PJobScheduler<T>{pub fn new(_d:&AsahiDevice,b:Arc<T>,_credits:u32)->Result<Self>{if FAIL_SCHEDULER.swap(false,Ordering::SeqCst){Err(12)}else{Ok(Self(b))}}}pub struct G17PRenderScheduler<T>(Arc<T>);impl<T>G17PRenderScheduler<T>{pub fn new(_d:&AsahiDevice,b:Arc<T>,_credits:u32)->Result<Self>{Ok(Self(b))}}}
'''
tests=r'''
#[test]fn idle_logical_queue_consumes_no_hardware_qid(){let rt=G17PRuntimeRef::new();let reg=Arc::new(G17PQueueRegistry{live_queues:AtomicU32::new(0)},0).unwrap();reg.reserve_queue().unwrap();let q=G17PQueue::new(&AsahiDevice,rt,reg.clone(),0,0,0,mmu::Vm,7,0,0).unwrap();assert!(rt.0.lock().unwrap().owners.is_empty());drop(q);let state=rt.0.lock().unwrap();assert!(state.owners.is_empty());assert!(state.released.is_empty());assert_eq!(reg.live_queues.load(Ordering::Acquire),0);}
#[test]fn first_compute_allocates_and_drop_releases_once(){let rt=G17PRuntimeRef::new();let reg=Arc::new(G17PQueueRegistry{live_queues:AtomicU32::new(0)},0).unwrap();reg.reserve_queue().unwrap();let mut q=G17PQueue::new(&AsahiDevice,rt,reg.clone(),0,0,0,mmu::Vm,7,0,0).unwrap();q.ensure_async_compute_queue().unwrap();assert_eq!(rt.0.lock().unwrap().owners,vec![7]);q.ensure_async_compute_queue().unwrap();assert_eq!(rt.0.lock().unwrap().owners,vec![7]);drop(q);let state=rt.0.lock().unwrap();assert!(state.owners.is_empty());assert_eq!(state.released,vec![7]);}
#[test]fn surviving_frontend_rebuilds_runtime_graph_after_processor_stop(){let rt=G17PRuntimeRef::new();let reg=Arc::new(G17PQueueRegistry{live_queues:AtomicU32::new(0)},0).unwrap();reg.reserve_queue().unwrap();let mut q=G17PQueue::new(&AsahiDevice,rt,reg,0,0,0,mmu::Vm,17,0,0).unwrap();q.ensure_async_compute_queue().unwrap();assert_eq!(rt.0.lock().unwrap().prepares,1);rt.0.lock().unwrap().owners.clear();q.ensure_async_compute_queue().unwrap();let state=rt.0.lock().unwrap();assert_eq!(state.owners,vec![17]);assert_eq!(state.prepares,2);drop(state);q.ensure_async_compute_queue().unwrap();assert_eq!(rt.0.lock().unwrap().prepares,2);}
#[test]fn allocation_failures_do_not_leak_hardware_qid(){for failure in [&FAIL_FENCE,&FAIL_CONTEXT]{let rt=G17PRuntimeRef::new();let reg=Arc::new(G17PQueueRegistry{live_queues:AtomicU32::new(0)},0).unwrap();failure.store(true,Ordering::Release);assert!(G17PQueue::new(&AsahiDevice,rt,reg,0,0,0,mmu::Vm,9,0,0).is_err());assert!(rt.0.lock().unwrap().owners.is_empty());}for failure in [&FAIL_PREPARE,&FAIL_BACKEND,&FAIL_SCHEDULER]{let rt=G17PRuntimeRef::new();let reg=Arc::new(G17PQueueRegistry{live_queues:AtomicU32::new(0)},0).unwrap();let mut q=G17PQueue::new(&AsahiDevice,rt,reg,0,0,0,mmu::Vm,9,0,0).unwrap();failure.store(true,Ordering::Release);assert!(q.ensure_async_compute_queue().is_err());assert!(rt.0.lock().unwrap().owners.is_empty());assert!(rt.0.lock().unwrap().released.len()<=1);}}
#[test]fn capacity_is_shared_and_released_slots_are_reusable(){let reg=G17PQueueRegistry{live_queues:AtomicU32::new(0)};for _ in 0..G17P_MAX_COMPUTE_QUEUES{reg.reserve_queue().unwrap()}assert_eq!(reg.reserve_queue(),Err(ENOSPC));reg.release_queue();assert_eq!(reg.reserve_queue(),Ok(()));assert_eq!(reg.live_queues.load(Ordering::Acquire),g17_queue_limits::MAX_QUEUES);assert_eq!(g17_queue_limits::MAX_QUEUES,58);}
'''
with tempfile.TemporaryDirectory() as d:
 f=Path(d)/'test.rs';f.write_text(mock+'mod g17_queue_limits{'+limits+'}\nconst G17P_MAX_COMPUTE_QUEUES:u32=g17_queue_limits::MAX_QUEUES;\nimpl G17PQueueRegistry{'+reserve+release+'}\n'+production+'\nimpl G17PQueue{'+constructor+ensure+'}\n'+tests)
 Path('/tmp/g17-create-model.rs').write_text(f.read_text())
 subprocess.run(['rustc','--edition=2021','--test',str(f),'-o',str(Path(d)/'test')],check=True)
 subprocess.run([str(Path(d)/'test'),'--test-threads=1'],check=True)
print('PASS actual lazy queue allocation/drop: idle logical queues consume no hardware QID, first compute allocates once, failures unwind, shared pool exhaustion/reuse')
