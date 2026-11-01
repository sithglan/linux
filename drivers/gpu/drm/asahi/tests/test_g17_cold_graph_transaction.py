#!/usr/bin/env python3
"""Exercise production cold-graph commit and ownership against failing primitives."""
from pathlib import Path
import subprocess, tempfile
root=Path(__file__).resolve().parents[1]
source=(root/'g17_resources.rs').read_text()
def block(marker):
 a=source.index(marker);b=source.index('{',a)+1;d=1
 while d:d+=(source[b]=='{')-(source[b]=='}');b+=1
 return source[a:b]
commit=block('    fn install_user_render_storage(')+'\n'+block('    pub(crate) fn finish_user_render_storage(')
prepared=block('struct G17PUninstalledRenderGraph {')
build=block('impl G17PUninstalledRenderGraph {')
build=build[build.index('    pub(crate) fn build('):]
for forbidden in ['append_driver_mappings(', 'install_render_pool_mappings(', 'stage_render_parameter_buffer', '&mut self']:
 assert forbidden not in build, forbidden
assert 'vm: vm.clone()' in build
rust=r'''
#![allow(dead_code)]
use std::{sync::{Arc,Mutex,atomic::{AtomicUsize,Ordering}},ops::{Deref,DerefMut}};
#[derive(Clone,Copy,Debug,PartialEq,Eq)]struct Error(i32);
impl Error{fn to_errno(self)->i32{self.0}}
type Result<T=()>=std::result::Result<T,Error>;
type KBox<T>=Box<T>;
const EIO:Error=Error(-5);const ENOENT:Error=Error(-2);const EBUSY:Error=Error(-16);const GFP_KERNEL:u32=0;
static FAIL:AtomicUsize=AtomicUsize::new(0);
fn fault(stage:usize)->Result{if FAIL.load(Ordering::SeqCst)==stage{Err(Error(-(stage as i32)-100))}else{Ok(())}}
#[derive(Default)]struct KVec<T>(Vec<T>);
impl<T>KVec<T>{fn reserve(&mut self,n:usize,_:u32)->Result{fault(1)?;self.0.reserve(n);Ok(())}fn push(&mut self,t:T,_:u32)->Result{assert!(self.0.len()<self.0.capacity());self.0.push(t);Ok(())}}
impl<T>Deref for KVec<T>{type Target=Vec<T>;fn deref(&self)->&Vec<T>{&self.0}}
impl<T>DerefMut for KVec<T>{fn deref_mut(&mut self)->&mut Vec<T>{&mut self.0}}
impl<T>IntoIterator for KVec<T>{type Item=T;type IntoIter=std::vec::IntoIter<T>;fn into_iter(self)->Self::IntoIter{self.0.into_iter()}}
#[derive(Clone,Copy)]struct Pair(u8);impl Pair{fn slot(self)->u8{self.0}}
#[derive(Default)]struct Audit{live:AtomicUsize,graphs:AtomicUsize}
#[derive(Default)]struct Mapping(Option<Arc<Audit>>);
impl Mapping{fn new(a:&Arc<Audit>)->Self{a.live.fetch_add(1,Ordering::SeqCst);Self(Some(a.clone()))}}
impl Drop for Mapping{fn drop(&mut self){if let Some(a)=&self.0{a.live.fetch_sub(1,Ordering::SeqCst);}}}
#[derive(Default)]struct State{pool:Option<(u64,KVec<Mapping>)>,driver:KVec<Mapping>,status:i32}
mod mmu{
 use super::*;pub type KernelMapping=Mapping;pub type Uat=super::Uat;
 #[derive(Clone,Default)]pub struct Vm(pub Arc<Mutex<State>>);
 impl Vm{
  pub fn status(&self)->Self{self.clone()}pub fn record(&self,e:i32){self.0.lock().unwrap().status=e;}
  pub fn install_render_pool_mappings(&self,_:u8,id:u64,_:usize,m:KVec<Mapping>)->Result{fault(2)?;let mut s=self.0.lock().unwrap();if s.pool.is_some(){return Err(EBUSY)}s.pool=Some((id,m));Ok(())}
  pub fn append_driver_mappings(&self,m:KVec<Mapping>)->Result{fault(6)?;self.0.lock().unwrap().driver.0.extend(m);Ok(())}
  pub fn clear_render_pool_mappings_if_owner(&self,_:u8,id:u64){let mut s=self.0.lock().unwrap();if s.pool.as_ref().map(|p|p.0)==Some(id){s.pool=None;}}
 }
}
struct Tvb{block_count:usize}
struct G17PRenderSksmBacking;
struct G17PUserRenderStorage{queue_pair:Pair,sksm_vm_aliases:KVec<Mapping>,persistent:KVec<Mapping>,render_tvb:Option<Tvb>,audit:Arc<Audit>}
impl G17PUserRenderStorage{
 fn render_pool_id(&self)->u64{7}
 fn take_sksm_vm_aliases(&mut self)->KVec<Mapping>{std::mem::take(&mut self.sksm_vm_aliases)}
 fn take_persistent_client_mappings(&mut self)->Result<KVec<Mapping>>{Ok(std::mem::take(&mut self.persistent))}
 fn parameter_buffer_descriptor(&self)->Result<u64>{Ok(700)}
 fn take_sksm_backing(&mut self)->Result<G17PRenderSksmBacking>{Ok(G17PRenderSksmBacking)}
}
impl Drop for G17PUserRenderStorage{fn drop(&mut self){
 assert_eq!(self.audit.live.load(Ordering::SeqCst),self.sksm_vm_aliases.len()+self.persistent.len(),"tentative aliases outlived graph");self.audit.graphs.fetch_add(1,Ordering::SeqCst);
}}
struct Uat;impl Uat{fn flush_t8140_vm_contexts(&self,_:&mmu::Vm)->Result{fault(3)}}
struct AddressSpace{uat:Uat}
struct Owner{address_space:AddressSpace,pbdesc:u64}
impl Owner{
 fn stage_render_parameter_buffer_page_list(&mut self,_:&mut G17PUserRenderStorage)->Result{fault(4)}
 fn stage_render_parameter_buffer_descriptor(&mut self,v:u64)->Result<Option<u64>>{fault(5)?;let old=self.pbdesc;self.pbdesc=v;Ok(Some(old))}
 fn restore_render_parameter_buffer_descriptor(&mut self,v:Option<u64>)->Result{if let Some(v)=v{self.pbdesc=v;}Ok(())}
'''+commit+'}\n'+prepared+'\nimpl G17PUninstalledRenderGraph {\n'+block('    pub(crate) fn into_storage(')+'\n'+block('    pub(crate) fn reclaim_retained_sksm_backing(')+'\n}\n'+r'''
fn prepare()->(G17PUninstalledRenderGraph,Arc<Audit>,mmu::Vm,Owner){
 let audit=Arc::new(Audit::default());let vm=mmu::Vm::default();
 let aliases=|n|KVec((0..n).map(|_|Mapping::new(&audit)).collect());
 let p=G17PUninstalledRenderGraph{driver_aliases:aliases(1),storage:Box::new(G17PUserRenderStorage{queue_pair:Pair(3),sksm_vm_aliases:aliases(2),persistent:aliases(1),render_tvb:Some(Tvb{block_count:17}),audit:audit.clone()}),vm:vm.clone(),had_retained_sksm_backing:true,_uat:Arc::new(Box::new(Uat))};
 (p,audit,vm,Owner{address_space:AddressSpace{uat:Uat},pbdesc:99})
}
#[test]fn uninstalled_graph_pins_uat_after_external_owner_drops(){let(p,a,_v,_o)=prepare();let owner=p._uat.clone();let weak=Arc::downgrade(&owner);drop(owner);assert!(weak.upgrade().is_some());drop(p);assert!(weak.upgrade().is_none());assert_eq!(a.graphs.load(Ordering::SeqCst),1);}
#[test]fn dropping_uninstalled_graph_does_not_retain_vm_aliases(){FAIL.store(0,Ordering::SeqCst);let(p,a,v,o)=prepare();drop(p);assert_eq!(a.live.load(Ordering::SeqCst),0);assert_eq!(a.graphs.load(Ordering::SeqCst),1);assert_eq!(o.pbdesc,99);assert!(v.0.lock().unwrap().pool.is_none());}
#[test]fn every_install_failure_retracts_exact_graph_and_restores_descriptor(){
 for stage in 1..=6{FAIL.store(stage,Ordering::SeqCst);let(p,a,v,mut o)=prepare();let mut retained=None;
 assert!(o.install_user_render_storage(p,&mut retained).is_err());assert!(retained.is_some());
 assert_eq!(o.pbdesc,99);let s=v.0.lock().unwrap();assert!(s.pool.is_none());assert!(s.driver.is_empty());assert_eq!(a.live.load(Ordering::SeqCst),0);assert_eq!(a.graphs.load(Ordering::SeqCst),1);}
}
#[test]fn conflicting_pool_survives_rejected_commit(){FAIL.store(0,Ordering::SeqCst);let(p,a,v,mut o)=prepare();v.0.lock().unwrap().pool=Some((88,KVec::default()));assert!(matches!(o.install_user_render_storage(p,&mut None),Err(EBUSY)));assert_eq!(v.0.lock().unwrap().pool.as_ref().unwrap().0,88);assert_eq!(o.pbdesc,99);assert_eq!(a.live.load(Ordering::SeqCst),0);}
#[test]fn successful_install_transfers_all_aliases_once(){FAIL.store(0,Ordering::SeqCst);let(p,a,v,mut o)=prepare();let storage=o.install_user_render_storage(p,&mut None).unwrap();assert_eq!(o.pbdesc,700);{let s=v.0.lock().unwrap();assert_eq!(s.pool.as_ref().unwrap().0,7);assert_eq!(s.driver.len(),3);}assert_eq!(a.live.load(Ordering::SeqCst),4);v.clear_render_pool_mappings_if_owner(3,7);v.0.lock().unwrap().driver.0.clear();drop(storage);assert_eq!(a.graphs.load(Ordering::SeqCst),1);}
'''
with tempfile.TemporaryDirectory() as d:
 p=Path(d)/'cold.rs';p.write_text(rust);b=Path(d)/'cold'
 subprocess.run(['rustc','--edition=2021','--test',str(p),'-o',str(b)],check=True)
 subprocess.run([str(b),'--test-threads=1'],check=True)
print('PASS production cold graph commit: abort, six failed stages, foreign pool, successful ownership transfer')
