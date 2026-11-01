#!/usr/bin/env python3
"""Actual status, ioctl handler and completion code with CPU/fence actuators."""
from pathlib import Path
import subprocess, tempfile
from g17_recovery_isolation_test_support import recovery_isolation_mod
p=Path(__file__).resolve().parents[1]
def item(text,needle):
 start=text.index(needle);op=text.index('{',start);depth=1;i=op+1
 while depth:
  depth+=(text[i]=='{')-(text[i]=='}');i+=1
 return text[start:i]
status=(p/'g17_status.rs').read_text().replace('//!','//')
job=(p/'g17_job.rs').read_text()
completion='\n'.join(item(job,n) for n in ['pub(crate) struct TimestampDestination','pub(crate) struct G17PJobCompletion','impl G17PJobCompletion'])
completion = completion.replace(item(job, '    pub(crate) fn new(\n        fence: UserFence<'), '')
completion += '\nconst G17P_COMMAND_TIMESTAMP_HZ:u64=24_000_000;\n' + item(job, 'impl TimestampDestination')
query=item((p/'file.rs').read_text(),'    fn get_vm_status(')
mock=r'''
#![allow(dead_code,unused_imports)]
extern crate self as kernel;
pub mod prelude {pub use crate::{Errno,Result,EINVAL,EBUSY};}  // v87: mirror API
pub mod error {pub mod code {pub const EIO:i32=-5;pub const ETIMEDOUT:i32=-110;pub const ENODEV:i32=-19;}}
macro_rules! pr_err {($($t:tt)*)=>{}}
macro_rules! dev_err {($($t:tt)*)=>{}}
use std::mem::ManuallyDrop;
struct G17PTimestampPool;
use std::sync::{Arc,Mutex as StdMutex};use std::sync::atomic::{AtomicU32,Ordering};use std::collections::BTreeMap;
struct Mutex<T>(StdMutex<T>);impl<T>Mutex<T>{fn new(t:T)->Self{Self(StdMutex::new(t))}fn lock(&self)->std::sync::MutexGuard<'_ ,T>{self.0.lock().unwrap()}}
type Error=i32;
use std::sync::atomic::fence;
pub type Result<T=()> = std::result::Result<T,i32>;
pub const EBUSY:i32=-16;pub const EINVAL:i32=-22;const EOPNOTSUPP:i32=-95;const ENOENT:i32=-2;const ECANCELED:i32=-125;
pub trait Errno{fn to_errno(&self)->i32;}impl Errno for i32{fn to_errno(&self)->i32{*self}}
mod g17_resources {pub struct G17PWorkStateLease;}
mod g17_host_path {pub const AGG_FIRST:u32=1<<14;pub static MASK:std::sync::atomic::AtomicU32=std::sync::atomic::AtomicU32::new(0);pub static AGG_FIRST_TAKEN:std::sync::atomic::AtomicU64=std::sync::atomic::AtomicU64::new(0);pub fn enabled(bit:u32)->bool{MASK.load(std::sync::atomic::Ordering::Relaxed)&bit!=0}pub fn count(c:&std::sync::atomic::AtomicU64){c.fetch_add(1,std::sync::atomic::Ordering::Relaxed);}}
mod g17_drm {
 pub struct G17PSubmissionFence;
 #[derive(Default)]pub struct G17PSubmitCompletionMember(pub std::sync::atomic::AtomicU32);
 impl G17PSubmitCompletionMember {pub fn complete(&self,_r:super::Result){self.0.fetch_add(1,std::sync::atomic::Ordering::AcqRel);}pub fn work_state(&self)->super::Result<std::sync::Arc<crate::g17_resources::G17PWorkStateLease>>{Ok(std::sync::Arc::new(crate::g17_resources::G17PWorkStateLease))}}
}
mod mmu {
 pub const T8140_CONTEXT_UNBOUND:i32=-108;
 pub fn is_context_unbound(e:i32)->bool{e==T8140_CONTEXT_UNBOUND}
 pub fn context_refusal_to_uapi(e:i32)->i32{if e==T8140_CONTEXT_UNBOUND{-2}else{e}}
 pub struct T8140VmJobGuard;
 impl T8140VmJobGuard {pub fn quarantine(&self){}}
 pub struct KernelMapping {pub fail:bool}
 impl KernelMapping {pub fn iova(&self)->u64{0}pub fn with_cpu_bytes<T>(&self,f:impl FnOnce(&mut[u8])->super::Result<T>)->super::Result<T>{if self.fail{return Err(-14)}f(&mut [0;16])}}
}
struct UserFence<T>{status:Arc<g17_status::VmStatus>,expected:i32,done:AtomicU32,_tag:std::marker::PhantomData<T>}
impl<T>UserFence<T>{fn set_error(&self,error:i32){if error!=ECANCELED && error!=-2{assert_eq!(self.status.get(),error)}}fn signal(&self){assert_eq!(self.status.get(),self.expected);self.done.fetch_add(1,Ordering::AcqRel);}}
struct Fence;impl Fence{fn from_fence<T>(_:&UserFence<T>)->Self{Self}}
mod uapi {pub struct drm_asahi_get_params{pub pointer:u64,pub size:u64,pub pad:u32}}
#[derive(Copy,Clone)]struct UserPtr(usize);impl UserPtr{fn from_addr(n:usize)->Self{Self(n)}}
struct UserSlice(UserPtr,usize);impl UserSlice{fn new(p:UserPtr,n:usize)->Self{Self(p,n)}fn reader(self)->Self{self}fn writer(self)->Self{self}fn read_slice(&self,b:&mut[u8])->Result{unsafe{b.copy_from_slice(std::slice::from_raw_parts(self.0.0 as *const u8,self.1))}Ok(())}fn write_slice(&self,b:&[u8])->Result{unsafe{std::slice::from_raw_parts_mut(self.0.0 as *mut u8,self.1).copy_from_slice(b)}Ok(())}}
struct TestVm(Arc<g17_status::VmStatus>);impl TestVm{fn status(&self)->&Arc<g17_status::VmStatus>{&self.0}}
struct FileVm{vm:TestVm}struct Vms(Mutex<BTreeMap<usize,FileVm>>);impl Vms{fn lock(&self)->VmsGuard<'_>{VmsGuard(self.0.lock())}}
struct VmsGuard<'a>(std::sync::MutexGuard<'a,BTreeMap<usize,FileVm>>);impl VmsGuard<'_>{fn get(&self,id:usize)->Option<&FileVm>{self.0.get(&id)}}
struct DrmFile{vms:Vms}impl DrmFile{fn inner(&self)->&Self{self}fn vms(&self)->&Vms{&self.vms}}
struct Gpu{supported:bool,error:i32}impl Gpu{fn supports_vm_status(&self)->bool{self.supported}fn submission_error(&self)->i32{self.error}}
struct AsahiDevice{gpu:Gpu}
'''
tests=r'''
#[cfg(test)]mod tests{
 use super::*;
 fn file()->DrmFile{DrmFile{vms:Vms(Mutex::new([(7,FileVm{vm:TestVm(Arc::new(g17_status::VmStatus::new()))})].into()))}}
 fn query(f:&DrmFile,vm:u32,error:i32,flags:u32,pad:u32,size:u64)->(Result<u32>,[u8;16]){let mut b=g17_status::encode_response(vm,0);b[4..8].copy_from_slice(&flags.to_ne_bytes());b[12..].copy_from_slice(&pad.to_ne_bytes());let d=uapi::drm_asahi_get_params{pointer:b.as_mut_ptr() as u64,size,pad:0};let result=get_vm_status(&AsahiDevice{gpu:Gpu{supported:true,error}},&d,f);(result,b)}
 #[test]fn missing_context_refusal_is_signalled_without_vm_error(){let c=completion(0,false);c.complete(Err(mmu::T8140_CONTEXT_UNBOUND));assert_eq!(c.status.get(),0);assert_eq!(c.fence.done.load(Ordering::SeqCst),1);assert_eq!(mmu::context_refusal_to_uapi(mmu::T8140_CONTEXT_UNBOUND),-2);assert_eq!(mmu::context_refusal_to_uapi(-5),-5);}
 #[test]fn first_failure_sticks_and_vms_are_isolated(){let a=g17_status::VmStatus::new();let b=g17_status::VmStatus::new();a.record(-110);a.record(-5);a.record(0);assert_eq!(a.get(),-110);assert_eq!(b.get(),0);}
 #[test]fn query_enforces_file_vm_ownership_and_reserved_fields(){let f=file();assert_eq!(query(&f,7,0,0,0,16).0,Ok(0));assert_eq!(query(&f,8,0,0,0,16).0,Err(ENOENT));assert_eq!(query(&f,7,0,1,0,16).0,Err(EINVAL));assert_eq!(query(&f,7,0,0,1,16).0,Err(EINVAL));assert_eq!(query(&f,7,0,0,0,15).0,Err(EINVAL));assert_eq!(query(&f,7,0,0,0,32).0,Ok(0));}
 #[test]fn query_preserves_domain_failure_after_recovery(){let f=file();assert_eq!(query(&f,7,-5,0,0,16).1,g17_status::encode_response(7,-5));assert_eq!(query(&f,7,0,0,0,16).1,g17_status::encode_response(7,-5));assert_eq!(query(&file(),7,0,0,0,16).1,g17_status::encode_response(7,0));}
 fn completion(expected:i32,fail_copy:bool)->G17PJobCompletion{let status=Arc::new(g17_status::VmStatus::new());G17PJobCompletion{recovery_class:Default::default(),status:status.clone(),fence:UserFence{status,expected,done:AtomicU32::new(0),_tag:std::marker::PhantomData},timestamps:ManuallyDrop::new(mmu::KernelMapping{fail:false}),ts_pool:None,destinations:[Some(TimestampDestination{mapping:Arc::new(mmu::KernelMapping{fail:fail_copy}),offset:0}),None],vm_job:Arc::new(Mutex::new(Some(mmu::T8140VmJobGuard))),signalled:AtomicU32::new(0),aggregate_member:Default::default(),replays:Default::default(),replay_timeout_credit:Default::default(),native_consumer:Default::default()}}
 #[test]fn cancellation_signals_once_without_poisoning_sibling_queues(){let c=completion(0,false);c.cancel_while_owned();c.cancel_while_owned();assert_eq!(c.status.get(),0);assert!(c.vm_job.lock().is_some());assert_eq!(c.fence.done.load(Ordering::Acquire),1);assert_eq!(c.aggregate_member.0.load(Ordering::Acquire),1);c.complete(Ok([1,2]));assert!(c.vm_job.lock().is_some());c.release_after_retirement();assert!(c.vm_job.lock().is_none());}
 #[test]fn admission_errors_are_not_sticky_device_loss(){let s=g17_status::VmStatus::new();for e in [-16,-22,-4,ECANCELED]{s.record_if_device_loss(e);assert_eq!(s.get(),0)}s.record_if_device_loss(-110);assert_eq!(s.get(),-110);s.record_if_device_loss(-5);assert_eq!(s.get(),-110);}
 #[test]fn error_is_visible_before_error_fence_and_timestamp_failure(){let a=completion(-110,false);a.complete(Err(-110));a.complete(Err(-110));assert_eq!(a.fence.done.load(Ordering::Acquire),1);let b=completion(-14,true);b.complete(Ok([1,2]));assert_eq!(b.status.get(),-14);let c=completion(0,false);c.complete(Ok([1,2]));assert_eq!(c.status.get(),0);}
 #[test]fn concurrent_failures_publish_one_first_error(){let s=Arc::new(g17_status::VmStatus::new());let threads:Vec<_>=(1..17).map(|n|{let s=s.clone();std::thread::spawn(move||s.record(-n))}).collect();for t in threads{t.join().unwrap()}let first=s.get();assert!((-16..=-1).contains(&first));for n in 1..17{s.record(-n);assert_eq!(s.get(),first)}}
}
'''
with tempfile.TemporaryDirectory() as d:
 f=Path(d)/'status.rs';f.write_text(mock+'\n'+recovery_isolation_mod()+'\nmod g17_status{'+status+'}\n'+completion+'\n'+query+'\n'+tests)
 subprocess.run(['rustc','--edition=2021','--test',str(f),'-o',str(Path(d)/'test')],check=True)
 subprocess.run([str(Path(d)/'test'),'--nocapture'],check=True)
