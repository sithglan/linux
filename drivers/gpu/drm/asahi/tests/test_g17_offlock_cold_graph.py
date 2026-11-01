#!/usr/bin/env python3
"""Run production cold reservations/returns with cancellation and install faults."""
from pathlib import Path
import subprocess, tempfile
root=Path(__file__).resolve().parents[1]
manager=(root/'g17_manager.rs').read_text(); drm=(root/'g17_drm.rs').read_text()
def item(source,marker):
 a=source.index(marker);b=source.index('{',a)+1;depth=1
 while depth:depth+=(source[b]=='{')-(source[b]=='}');b+=1
 return source[a:b]
capture=item(manager,'    pub(crate) fn take_warm_render_graph(')
# Host substitute for kernel in-place allocation only; ownership logic is exact.
capture=capture.replace('KBox::init(try_init!(G17PWarmRenderGraph {','Box::new(G17PWarmRenderGraph {').replace('        }), GFP_KERNEL)?;','        });')
returned=item(manager,'    pub(crate) fn return_warm_render_graph(')
# Kernel KBox::write consumes the allocation; std exposes the same operation as an associated function.
# The graph's storage is constructed in its final heap box (stack depth).
assert 'self.render_slots.get_mut(index).storage = Some(graph.into_storage());' in returned
request=item(manager,'pub(crate) struct G17PWarmRenderGraph {')
live=(root/'g17_live_boot.rs').read_text()
commit=item(live,'    pub(crate) fn return_warm_render_graph(')
assert commit.index('self.manager = Some(manager)') < commit.index('if result.is_err() && request.installed') < commit.index('self.request_session_rebuild(')
section=drm[drm.index('failure_stage = "render-warm-pairs";'):drm.index('// The 0x2e consumption witness')]
assert section.index('drop(runtime);') < section.index('request.build(&drm)') < section.index('runtime = self.runtime.mutex().lock();')
assert section.index('runtime.return_warm_render_graph(') < section.index('drop(request);') < section.index('returned?;')
assert 'built.is_ok() && current && !packet.cancel_requested()' in section
assert 'runtime.warm_render_pair(' not in drm
assert 'if restage.is_none() {\n                        drop(runtime);\n                        continue;' in drm
rust=r'''
#![allow(dead_code,unused_imports)]
use std::sync::{Arc,Mutex,Barrier,atomic::{AtomicUsize,AtomicBool,Ordering}};
use std::mem::MaybeUninit;
type Result<T=()> = std::result::Result<T,i32>;
type KBox<T> = Box<T>;
const EIO:i32=5;const EFAULT:i32=14;const EAGAIN:i32=11;const EBUSY:i32=16;const EINVAL:i32=22;
struct AsahiDevice;
mod regs{pub struct Resources;}
mod mmu{
 use super::*;
 pub struct Uat;
 #[derive(Clone)]pub struct Vm(pub Arc<AtomicUsize>);
 impl Vm{pub fn status(&self)->&Self{self}pub fn get(&self)->usize{self.0.load(Ordering::SeqCst)}}
}
mod g17_uapi{
 #[derive(Clone,Copy,Default)]pub struct Parameters{pub ta_hardware_buffer_id:u32,pub context_generation:u64}
 #[derive(Clone,Copy,Default)]pub struct TranslatedRenderCommand{pub parameters:Parameters}
}
mod g17_resources{
 use super::*;
 #[derive(Clone,Copy,PartialEq,Eq)]pub struct G17PRenderQueuePair(pub u8);
 impl G17PRenderQueuePair{pub fn slot(self)->u8{self.0}}
 pub struct G17PLogicalQueueContext{pub live:AtomicBool}
 impl G17PLogicalQueueContext{pub fn matches(&self,_:&mmu::Vm)->bool{true}pub fn is_current(&self)->bool{self.live.load(Ordering::SeqCst)}pub fn generation(&self)->u64{1}}
 pub struct G17PWorkStateLease;
 impl G17PWorkStateLease{pub fn word(&self)->u32{4}}
 pub type G17PPmPageMetricsBinding=u64;
 #[derive(Debug,PartialEq,Eq)]pub struct G17PRenderSksmBacking(pub u64);
 pub struct G17PUserRenderStorage{pub backing:Option<G17PRenderSksmBacking>,pub id:u32,pub dropped:Arc<AtomicUsize>}
 impl Drop for G17PUserRenderStorage{fn drop(&mut self){self.dropped.fetch_add(1,Ordering::SeqCst);}}
 pub struct G17PUninstalledRenderGraph{pub storage:Box<G17PUserRenderStorage>,pub retained:bool}
 impl G17PUninstalledRenderGraph{
  pub fn into_storage(self)->Box<G17PUserRenderStorage>{self.storage}
  pub fn reclaim_retained_sksm_backing(&mut self,to:&mut Option<G17PRenderSksmBacking>)->Result{
   if self.retained{if to.is_some(){return Err(EIO)}*to=self.storage.backing.take();self.retained=false;}Ok(())
  }
 }
}
use g17_resources::*;
'''+request+r'''
struct Queues{backing:Option<G17PRenderSksmBacking>}
#[derive(Default)]struct Slot{preparing:bool,storage:Option<Box<G17PUserRenderStorage>>,bind:Option<()>,context_aliases:Option<()>,tracker:Option<()>,queued_tracker:Option<()>,restaged:Option<()>,hardware_buffer_id:Option<u32>,submission_ordinal:u64,sksm:Option<Queues>}
struct Slots([Slot;2]);impl Slots{fn get(&self,i:u8)->&Slot{&self.0[i as usize]}fn get_mut(&mut self,i:u8)->&mut Slot{&mut self.0[i as usize]}}
struct Resources{fail:bool}
impl Resources{
 fn stage_render_region_views(&mut self)->Result{Ok(())}
 fn acquire_parameter_metrics_region(&mut self)->Result<u64>{Ok(1)}
 fn owned_uat(&self)->Arc<Box<mmu::Uat>>{Arc::new(Box::new(mmu::Uat))}
 fn finish_user_render_storage(&mut self,_:&mut G17PUninstalledRenderGraph,_:&mut Option<G17PRenderSksmBacking>)->Result{if self.fail{Err(EIO)}else{Ok(())}}
}
struct Ids{next:u32,live:[bool;16],reserved:[bool;16],releases:usize}
impl Ids{
 fn allocate_with_token(&mut self)->Result<(u32,u32)>{let i=self.next;self.next+=1;self.live[i as usize]=true;Ok((i,0))}
 fn release(&mut self,i:u32)->Result{assert!(self.live[i as usize]);self.live[i as usize]=false;self.releases+=1;Ok(())}
}
struct ChannelIds;impl ChannelIds{fn publish(&mut self,_:u32)->bool{true}}
struct Manager{render_slots:Slots,resources:Resources,num_clusters:u32,render_channel_leases:[Option<[u32;2]>;2],channel_ids:ChannelIds,render_hardware_buffer_ids:Ids,generation:u64,fail_finalizer:bool}
impl Manager{
 // v96: borrow ends retry parked firmware TVB grows (tested in test_g17_v96_grow_rekick.py).
 fn rekick_parked_render_grows(&self,_pair:Option<u8>){}
 fn new()->Self{Self{render_slots:Slots(std::array::from_fn(|_|Slot::default())),resources:Resources{fail:false},num_clusters:1,render_channel_leases:[None;2],channel_ids:ChannelIds,render_hardware_buffer_ids:Ids{next:0,live:[false;16],reserved:[false;16],releases:0},generation:1,fail_finalizer:false}}
 fn ensure_render_slot(&mut self,_:u8)->Result{Ok(())}
 fn render_queue_pair(&self,i:u8)->Result<G17PRenderQueuePair>{Ok(G17PRenderQueuePair(i))}
 fn render_pair_is_warm(&self,i:u8)->bool{let s=self.render_slots.get(i);s.storage.is_some()&&s.bind.is_some()}
 fn prepare_render_pm_manager_generation(&mut self)->Result<Option<u64>>{let g=self.generation;self.generation+=1;Ok(Some(g))}
 fn finish_warm_render_pair_storage(&mut self,_:&AsahiDevice,_:&mmu::Vm,p:G17PRenderQueuePair,_:Arc<G17PLogicalQueueContext>,_:Option<u64>,_:&regs::Resources,power:&mut impl FnMut(&mut Self)->Result)->Result{
  let slot=self.render_slots.get_mut(p.slot());let id=slot.storage.as_ref().unwrap().id;
  self.render_hardware_buffer_ids.reserved[id as usize]=true;slot.bind=Some(());
  power(self)?;if self.fail_finalizer{Err(EIO)}else{Ok(())}
 }
'''+capture+'\n'+returned+r'''
}
fn capture(m:&mut Manager,i:u8)->Box<G17PWarmRenderGraph>{
 m.take_warm_render_graph(&mmu::Vm(Arc::new(AtomicUsize::new(0))),G17PRenderQueuePair(i),Arc::new(G17PLogicalQueueContext{live:AtomicBool::new(true)}),Arc::new(G17PWorkStateLease),&g17_uapi::TranslatedRenderCommand::default()).unwrap().unwrap()
}
fn build(r:&mut G17PWarmRenderGraph)->Arc<AtomicUsize>{
 let dropped=Arc::new(AtomicUsize::new(0));
 let retained=r.retained_sksm_backing.is_some();
 r.graph=Some(G17PUninstalledRenderGraph{storage:Box::new(G17PUserRenderStorage{backing:r.retained_sksm_backing.take(),id:r.hardware_buffer_id.unwrap(),dropped:dropped.clone()}),retained});dropped
}
fn finish(m:&mut Manager,r:&mut G17PWarmRenderGraph,commit:bool)->Result{m.return_warm_render_graph(&AsahiDevice,r,commit,&regs::Resources,&mut |_|Ok(()))}
#[test]fn simultaneous_builders_release_runtime_and_commit_in_reverse_order(){
 let manager=Arc::new(Mutex::new(Manager::new()));let barrier=Arc::new(Barrier::new(3));
 let mut threads=Vec::new();for i in 0..2{
  let m=manager.clone();let b=barrier.clone();threads.push(std::thread::spawn(move||{
   let mut r=capture(&mut m.lock().unwrap(),i);build(&mut r);b.wait();b.wait();r
  }));
 }
 barrier.wait();{let m=manager.try_lock().expect("neither cold builder may hold runtime");assert!(m.render_slots.0.iter().all(|s|s.preparing));}
 barrier.wait();let mut requests:Vec<_>=threads.into_iter().map(|t|t.join().unwrap()).collect();
 let mut m=manager.lock().unwrap();for r in requests.iter_mut().rev(){finish(&mut m,r,true).unwrap();}
 assert!(m.render_slots.0.iter().all(|s|!s.preparing&&s.storage.is_some()));assert_eq!(m.render_hardware_buffer_ids.releases,2);
}
#[test]fn every_abort_returns_original_queue_backing_once_and_drops_graph_after_return(){
 for stage in 0..5{
  let mut m=Manager::new();m.render_slots.0[0].sksm=Some(Queues{backing:Some(G17PRenderSksmBacking(73))});
  let mut r=capture(&mut m,0);let dropped=if stage!=0{Some(build(&mut r))}else{None};
  if stage==2{m.resources.fail=true;}if stage==3{r.context.live.store(false,Ordering::SeqCst);}if stage==4{r.vm.0.store(1,Ordering::SeqCst);}
  let result=finish(&mut m,&mut r,stage>=2);assert_eq!(result.is_err(),stage>=2);
  assert!(!m.render_slots.0[0].preparing);assert!(m.render_slots.0[0].storage.is_none());
  assert_eq!(m.render_slots.0[0].sksm.as_ref().unwrap().backing,Some(G17PRenderSksmBacking(73)));
  assert_eq!(m.render_hardware_buffer_ids.releases,1);
  if let Some(d)=dropped{assert_eq!(d.load(Ordering::SeqCst),0);drop(r);assert_eq!(d.load(Ordering::SeqCst),1);}
 }
}
#[test]fn installed_graph_survives_late_registration_failure(){
 let mut m=Manager::new();let mut r=capture(&mut m,0);let dropped=build(&mut r);let id=r.hardware_buffer_id.unwrap();m.fail_finalizer=true;
 assert_eq!(finish(&mut m,&mut r,true),Err(EIO));assert!(r.installed);drop(r);
 assert_eq!(dropped.load(Ordering::SeqCst),0);assert!(m.render_slots.0[0].storage.is_some());assert!(!m.render_slots.0[0].preparing);
 assert!(m.render_hardware_buffer_ids.reserved[id as usize]);assert!(!m.render_hardware_buffer_ids.live[id as usize]);
}
#[test]fn busy_reservation_cannot_steal_identity_and_warm_pair_allocates_nothing(){
 let mut m=Manager::new();let mut r=capture(&mut m,0);
 assert!(matches!(m.take_warm_render_graph(&r.vm,r.queue_pair,r.context.clone(),r.work_state.clone(),&r.command),Err(EAGAIN)));
 assert_eq!(m.render_hardware_buffer_ids.next,1);build(&mut r);finish(&mut m,&mut r,true).unwrap();
 assert!(m.take_warm_render_graph(&r.vm,r.queue_pair,r.context.clone(),r.work_state.clone(),&r.command).unwrap().is_none());
 assert_eq!(m.render_hardware_buffer_ids.next,1);
}
'''
with tempfile.TemporaryDirectory() as d:
 p=Path(d)/'cold.rs';p.write_text(rust);b=Path(d)/'cold'
 subprocess.run(['rustc','--edition=2021','--test',str(p),'-o',str(b)],check=True)
 subprocess.run([str(b)],check=True,timeout=20)
print('PASS production cold capture/return: concurrent reservations, cancel/failed build/install, exact retained backing, late failure lifetime, idempotent warm path')
