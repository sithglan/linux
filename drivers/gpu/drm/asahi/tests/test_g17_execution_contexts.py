#!/usr/bin/env python3
"""Compile production independent-context ownership against a DRAM TTBAT model."""
from pathlib import Path
import re,subprocess,tempfile
s=(Path(__file__).resolve().parents[1]/'mmu.rs').read_text()
def block(marker):
 a=s.index(marker);b=s.index('{',a);d=1;e=b+1
 while d:d+=(s[e]=='{')-(s[e]=='}');e+=1
 return s[a:e]
production='\n'.join(block(x) for x in [
 'struct T8140ContextRootLease {','pub(crate) struct T8140ComputeExecutionContext {',
 'impl T8140ComputeExecutionContext {','impl Drop for T8140ContextRootLease {'])
method=block('    pub(crate) fn allocate_t8140_compute_execution_context(')+block('    pub(crate) fn allocate_t8140_app_context(')
from g17_queue_limits_test_support import queue_limits_source
limits=queue_limits_source()
constants='\n'.join(re.search(r'const '+n+r': [^;]+;',s)[0] for n in [
 'T8140_APP_CONTEXT_START','T8140_APP_CONTEXT_RESERVED_MASK','T8140_APP_CONTEXT_AVAILABLE_MASK'])
mock=r'''
#![allow(dead_code,unused_variables)]
use std::sync::{Mutex,MutexGuard};
use std::sync::atomic::{AtomicU8,AtomicU64,AtomicBool,Ordering,fence};
use std::ops::Deref;
type Result<T=()> = std::result::Result<T,i32>;
const EINVAL:i32=22;const ENOSPC:i32=28;const EBUSY:i32=16;const GFP_KERNEL:u32=0;
static FAIL_ALLOC:AtomicBool=AtomicBool::new(false);
struct Arc<T>(std::sync::Arc<T>);
impl<T>Arc<T>{fn new(v:T,_flags:u32)->Result<Self>{if FAIL_ALLOC.swap(false,Ordering::SeqCst){drop(v);Err(12)}else{Ok(Self(std::sync::Arc::new(v)))}}fn as_ptr(v:&Self)->*const T{std::sync::Arc::as_ptr(&v.0)}}
impl<T>Clone for Arc<T>{fn clone(&self)->Self{Self(self.0.clone())}}
impl<T>Deref for Arc<T>{type Target=T;fn deref(&self)->&T{&self.0}}
macro_rules! pr_err {($($t:tt)*)=>{}}
macro_rules! dev_info {($($t:tt)*)=>{}}
#[derive(Clone)]struct VmBind(Vm);impl VmBind{fn slot(&self)->u32{1}}
struct T8140AppContextLease{root:T8140ContextRootLease,_bind:VmBind}
struct TTBS{ttb0:AtomicU64,ttb1:AtomicU64}
struct Handoff;
impl Handoff{fn lock(&self){}fn unlock(&self){}fn current_slot(&self)->Option<u32>{None}}
struct State{table:Vec<TTBS>}
impl State{fn ttbs(&self)->&[TTBS]{&self.table}fn handoff(&self)->Handoff{Handoff}}
struct UatInner{state:Mutex<State>,t8140_app_contexts:AtomicU64,t8140_context_generations:[AtomicU8;64]}
impl UatInner{fn lock(&self)->MutexGuard<'_,State>{self.state.lock().unwrap()}}
#[derive(PartialEq)]enum UatConfig{T8140,Other}
struct VmOwner{is_kernel:bool,uat_inner:Arc<UatInner>}
struct GpuVm{owner:Mutex<VmOwner>}
impl GpuVm{fn exec_lock(&self,_none:Option<()>,_flag:bool)->Result<MutexGuard<'_,VmOwner>>{Ok(self.owner.lock().unwrap())}}
#[derive(Clone)]struct Vm{id:u64,inner:Arc<GpuVm>,root:u64}
impl Vm{fn ttb(&self)->u64{self.root}}
struct Uat{cfg:UatConfig,inner:Arc<UatInner>,kernel_vm:Vm}
fn t8140_tagged_root(root:u64,id:usize)->u64{root|1|((id as u64)<<48)}
mod mem{use std::sync::Mutex;pub static TLBIS:Mutex<Vec<u8>>=Mutex::new(Vec::new());pub fn tlbi_asid(i:u8){TLBIS.lock().unwrap().push(i)}pub fn sync(){}}
impl Uat{
 fn install_t8140_app_context_alias(&self,b:&VmBind,id:usize)->Result<(u64,u64)>{let low=t8140_tagged_root(b.0.ttb(),id);let high=t8140_tagged_root(self.kernel_vm.ttb(),id);let state=self.inner.lock();state.ttbs()[id].ttb0.store(low,Ordering::Release);state.ttbs()[id].ttb1.store(high,Ordering::Release);Ok((low,high))}
 fn new()->Self{let inner=Arc::new(UatInner{state:Mutex::new(State{table:(0..64).map(|_|TTBS{ttb0:AtomicU64::new(0),ttb1:AtomicU64::new(0)}).collect()}),t8140_app_contexts:AtomicU64::new(31),t8140_context_generations:core::array::from_fn(|_|AtomicU8::new(0))},0).unwrap();let kernel_vm=Self::vm_in(&inner,0,0xff0000,true);Self{cfg:UatConfig::T8140,inner,kernel_vm}}
 fn vm_in(inner:&Arc<UatInner>,id:u64,root:u64,is_kernel:bool)->Vm{Vm{id,root,inner:Arc::new(GpuVm{owner:Mutex::new(VmOwner{is_kernel,uat_inner:inner.clone()})},0).unwrap()}}
 fn vm(&self,id:u64)->Vm{Self::vm_in(&self.inner,id,id*0x10000,false)}
 fn roots(&self,i:usize)->(u64,u64){let s=self.inner.lock();(s.ttbs()[i].ttb0.load(Ordering::Acquire),s.ttbs()[i].ttb1.load(Ordering::Acquire))}
}
'''
tests=r'''
#[test]fn simultaneous_roots_and_job_references_do_not_touch_slot1(){
 mem::TLBIS.lock().unwrap().clear();
 let u=Uat::new();{let s=u.inner.lock();s.ttbs()[1].ttb0.store(0x12340001,Ordering::Release);s.ttbs()[1].ttb1.store(0x56780001,Ordering::Release);}
 let a=u.allocate_t8140_compute_execution_context(&u.vm(1)).unwrap();
 let b=u.allocate_t8140_compute_execution_context(&u.vm(2)).unwrap();
 assert_eq!((a.context_id(),b.context_id()),(5,6));assert_ne!(a.root(),b.root());
 assert_eq!((a.generation(),b.generation()),(1,1));
 assert_eq!(u.roots(1),(0x12340001,0x56780001));
 let job_a=a.clone();drop(a);assert_ne!(u.roots(5),(0,0));
 drop(b);assert_eq!(u.roots(6),(0,0));assert_ne!(u.roots(5),(0,0));
 let c=u.allocate_t8140_compute_execution_context(&u.vm(3)).unwrap();assert_eq!(c.context_id(),6);
 assert_eq!((job_a.generation(),c.generation()),(1,2));
 let before=mem::TLBIS.lock().unwrap().len();drop(job_a);assert_eq!(u.roots(5),(0,0));assert_eq!(mem::TLBIS.lock().unwrap().len(),before+1);
 assert_eq!(u.roots(1),(0x12340001,0x56780001));drop(c);
 assert_eq!(u.inner.t8140_app_contexts.load(Ordering::Acquire),31);
}
#[test]fn context_id_reuse_stays_ordered_beyond_the_shared_namespace(){
 mem::TLBIS.lock().unwrap().clear();
 let u=Uat::new();
 for generation in 0..10_001u64{
  let c=u.allocate_t8140_compute_execution_context(&u.vm(generation+1)).unwrap();
  assert_eq!(c.context_id(),5);assert!(c.is_current());assert_eq!(c.generation(),(generation+1) as u8);drop(c);
  assert_eq!(u.roots(5),(0,0));
 }
 assert_eq!(mem::TLBIS.lock().unwrap().iter().filter(|&&id|id==5).count(),20_002);
 assert_eq!(u.inner.t8140_app_contexts.load(Ordering::Acquire),31);
}
#[test]fn exhaustion_and_failed_host_allocation_do_not_publish_or_leak(){
 let u=Uat::new();let vm=u.vm(1);FAIL_ALLOC.store(true,Ordering::Release);
 assert!(matches!(u.allocate_t8140_compute_execution_context(&vm),Err(12)));
 assert_eq!(u.inner.t8140_app_contexts.load(Ordering::Acquire),31);assert_eq!(u.roots(5),(0,0));
 let first=crate::g17_queue_limits::COMPUTE_CONTEXT_FIRST;let last=first+crate::g17_queue_limits::MAX_QUEUES;
 assert_eq!(crate::g17_queue_limits::COMPUTE_CONTEXT_MASK&crate::g17_queue_limits::BIND_CONTEXT_MASK,0);
 let mut live=Vec::new();for id in first..last{let c=u.allocate_t8140_compute_execution_context(&vm).unwrap();assert_eq!(c.context_id(),id);live.push(c);}
 assert!(matches!(u.allocate_t8140_compute_execution_context(&vm),Err(ENOSPC)));
 drop(live);assert_eq!(u.inner.t8140_app_contexts.load(Ordering::Acquire),31);
}
#[test]fn foreign_vm_and_unexpected_root_are_not_overwritten(){
 let u=Uat::new();let other=Uat::new();assert!(matches!(u.allocate_t8140_compute_execution_context(&other.vm(1)),Err(EINVAL)));
 assert!(matches!(u.allocate_t8140_compute_execution_context(&u.kernel_vm),Err(EINVAL)));
 {u.inner.lock().ttbs()[5].ttb0.store(0xdead0001,Ordering::Release);}
 assert!(matches!(u.allocate_t8140_compute_execution_context(&u.vm(1)),Err(EBUSY)));
 assert_eq!(u.roots(5),(0xdead0001,0));assert_eq!(u.inner.t8140_app_contexts.load(Ordering::Acquire)&32,32);
 let next=u.allocate_t8140_compute_execution_context(&u.vm(2)).unwrap();assert_eq!(next.context_id(),6);
}
'''
tests+=r'''
#[test]fn full_compute_capacity_preserves_render_context_in_either_order(){
 for render_first in [true,false]{let u=Uat::new();let vm=u.vm(1);let bind=VmBind(vm.clone());let mut render=if render_first{Some(u.allocate_t8140_app_context(&bind).unwrap())}else{None};let jobs:Vec<_>=(0..crate::g17_queue_limits::MAX_QUEUES).map(|_|u.allocate_t8140_compute_execution_context(&vm).unwrap()).collect();assert!(matches!(u.allocate_t8140_compute_execution_context(&vm),Err(ENOSPC)));if render.is_none(){render=Some(u.allocate_t8140_app_context(&bind).unwrap())}assert_eq!(render.as_ref().unwrap().root.context_id,63);drop(jobs);assert_ne!(u.roots(63),(0,0));drop(render);assert_eq!(u.inner.t8140_app_contexts.load(Ordering::Acquire),31);}
}
'''
with tempfile.TemporaryDirectory() as d:
 f=Path(d)/'test.rs';f.write_text(mock+'mod g17_queue_limits{'+limits+'}'+constants+production+'impl Uat{'+method+'}'+tests);exe=Path(d)/'test'
 subprocess.run(['rustc','--edition=2021','--test',str(f),'-o',str(exe)],check=True)
 subprocess.run([str(exe),'--test-threads=1'],check=True)
print('PASS production execution-context allocation/drop: simultaneous isolated roots, outstanding reference lifetime, exhaustion, allocation rollback, foreign root protection')
