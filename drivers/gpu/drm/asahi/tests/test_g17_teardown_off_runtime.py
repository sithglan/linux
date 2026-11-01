#!/usr/bin/env python3
"""Execute the actual runtime wrapper; mapping removal must observe it unlocked."""
from pathlib import Path
import subprocess,tempfile
s=(Path(__file__).resolve().parents[1]/'g17_drm.rs').read_text()
a=s.index('    pub(crate) fn service_logical_queue_teardowns(');b=s.index('{',a)+1;depth=1
while depth:depth+=(s[b]=='{')-(s[b]=='}');b+=1
method=s[a:b]
driver=(Path(__file__).resolve().parents[1]/'driver.rs').read_text()
a=driver.index('fn reclaim_g17p_graphs(');b=driver.index('{',a)+1;depth=1
while depth:depth+=(driver[b]=='{')-(driver[b]=='}');b+=1
reclaim=driver[a:b]
rust=r'''
use std::sync::{Arc,Mutex,MutexGuard,atomic::{AtomicBool,AtomicUsize,Ordering}};
type Result<T=()>=std::result::Result<T,i32>;type KVec<T>=Vec<T>;
mod g17_manager {
 use super::*;
 pub struct G17PLogicalQueueTeardown{pub held:Arc<AtomicBool>,pub cleaned:Arc<AtomicUsize>}
 impl G17PLogicalQueueTeardown{
  pub fn release_retired_vm_mappings(&self){assert!(!self.held.load(Ordering::SeqCst));self.cleaned.fetch_add(1,Ordering::SeqCst);}
 }
 impl Drop for G17PLogicalQueueTeardown{fn drop(&mut self){assert!(!self.held.load(Ordering::SeqCst));}}
}
struct Runtime{error:bool,held:Arc<AtomicBool>,cleaned:Arc<AtomicUsize>}
impl Runtime{fn service_logical_queue_teardowns(&mut self)->Result<KVec<g17_manager::G17PLogicalQueueTeardown>>{
 assert!(self.held.load(Ordering::SeqCst));if self.error{return Err(5)}
 Ok(vec![g17_manager::G17PLogicalQueueTeardown{held:self.held.clone(),cleaned:self.cleaned.clone()}])
}}
struct Lock{inner:Mutex<Runtime>,held:Arc<AtomicBool>}
struct Guard<'a>{inner:MutexGuard<'a,Runtime>,held:&'a AtomicBool}
impl Lock{fn lock(&self)->Guard<'_>{let inner=self.inner.lock().unwrap();self.held.store(true,Ordering::SeqCst);Guard{inner,held:&self.held}}}
impl std::ops::Deref for Guard<'_>{type Target=Runtime;fn deref(&self)->&Runtime{&self.inner}}
impl std::ops::DerefMut for Guard<'_>{fn deref_mut(&mut self)->&mut Runtime{&mut self.inner}}
impl Drop for Guard<'_>{fn drop(&mut self){self.held.store(false,Ordering::SeqCst);}}
struct Ref{lock:Lock}
impl Ref{fn new(error:bool)->Self{
 let held=Arc::new(AtomicBool::new(false));let cleaned=Arc::new(AtomicUsize::new(0));
 Self{lock:Lock{inner:Mutex::new(Runtime{error,held:held.clone(),cleaned}),held}}
}fn mutex(&self)->&Lock{&self.lock}
'''+method+r'''
}
'''+reclaim+r'''
#[test]fn acknowledged_mapping_cleanup_and_graph_drop_run_unlocked(){
 let r=Ref::new(false);let retired=r.service_logical_queue_teardowns().unwrap();assert_eq!(retired.len(),1);
 assert_eq!(retired[0].cleaned.load(Ordering::SeqCst),0);
 let cleaned=retired[0].cleaned.clone();reclaim_g17p_graphs(retired);assert_eq!(cleaned.load(Ordering::SeqCst),1);
}
#[test]fn failed_settlement_does_not_release_unacknowledged_graphs(){
 let r=Ref::new(true);assert!(r.service_logical_queue_teardowns().is_err());
 assert!(!r.lock.held.load(Ordering::SeqCst));assert_eq!(r.lock.inner.lock().unwrap().cleaned.load(Ordering::SeqCst),0);
}
'''
with tempfile.TemporaryDirectory() as d:
 p=Path(d)/'test.rs';p.write_text(rust);binary=Path(d)/'test'
 subprocess.run(['rustc','--edition=2021','--test',str(p),'-o',str(binary)],check=True)
 subprocess.run([str(binary)],check=True)
