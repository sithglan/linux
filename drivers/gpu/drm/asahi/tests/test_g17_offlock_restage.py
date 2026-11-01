#!/usr/bin/env python3
"""Exercise actual graph borrow/return/cancel code with concurrent host owners."""
from pathlib import Path
import subprocess, tempfile
r=Path(__file__).resolve().parents[1]
m=(r/'g17_manager.rs').read_text();d=(r/'g17_drm.rs').read_text();live=(r/'g17_live_boot.rs').read_text()
def item(s,key):
 a=s.index(key);b=s.index('{',a)+1;n=1
 while n:n+=(s[b]=='{')-(s[b]=='}');b+=1
 return s[a:b]
code=r'''
#![allow(dead_code,unused_variables)]
use std::sync::{Arc,Mutex,Barrier};
type KBox<T> = Box<T>;
type Result<T=()> = std::result::Result<T,i32>;
const EIO:i32=5;const EBUSY:i32=16;
struct AsahiDevice;
mod g17_submission {pub const G17P_CL_EXPLICIT_DEPENDENCIES_MAX:usize=31;pub type G17PClBarrierDependency=u8;pub type G17PClKickProducerState=u8;}
mod g17_config {pub const RENDER_SKSM:u32=1;pub const RENDER_NATIVE_DOORBELLS:u32=1;pub const RENDER_SPLIT_LAUNCH:u32=0;}
struct Queue{producer:u8}struct Queues{tiling:Queue,fragment:Queue}

mod mmu {pub struct Vm;pub struct Uat;}
mod g17_uapi {#[derive(Clone,Copy)]pub struct TranslatedRenderCommand;}
mod g17_resources {
 use super::*;
 #[derive(Clone,Copy)]pub struct G17PRenderQueuePair(pub u8);
 impl G17PRenderQueuePair {pub fn slot(self)->u8{self.0}}
 pub struct G17PLogicalQueueContext;
 pub struct G17PWorkStateLease;
 pub struct G17PHostTvbGrow;
 pub struct G17PUserRenderStorage {pub id:u64,pub generation:Option<u64>,pub grown:std::cell::Cell<bool>,pub drops:Arc<std::sync::atomic::AtomicUsize>}
 impl G17PUserRenderStorage {
 pub fn mark_render_flist_grown(&self){self.grown.set(true)}
 pub fn render_flist_id(&self)->u32{self.id as u32}
 pub fn render_flist_pointer(&self)->u64{1}
 pub fn render_flist_operand_table(&self)->u64{2}
 pub fn render_flist_cookie(&self)->u64{3}
 pub fn render_flist_serial(&self)->u64{4}
 pub fn ensure_sksm_dependency_buffer(&mut self)->Result{Ok(())}
 pub fn take_sksm_dependency_buffer(&mut self)->Box<G17PRenderDependencies>{Box::new(G17PRenderDependencies{entries:[0;31],count:0})}
 pub fn return_sksm_dependency_buffer(&mut self,_:Box<G17PRenderDependencies>){}
 pub fn cancel_unpublished_command(&mut self,o:u64)->Result{assert_eq!(self.generation,Some(o));self.generation=None;Ok(())}}
 impl Drop for G17PUserRenderStorage{fn drop(&mut self){self.drops.fetch_add(1,std::sync::atomic::Ordering::SeqCst);}}
}
enum G17PBootstrapUmaRequestState{Submitted,Ready}
struct Uma{request:Option<()>,state:G17PBootstrapUmaRequestState,id:u32}
impl Uma{fn matches_owner(&self,id:u32,_:u64,_:u64,_:u64)->bool{self.id==id}fn same_pool_object(&self,_:u64)->bool{true}}
struct Slot{sksm:Option<Queues>,storage:Option<Box<g17_resources::G17PUserRenderStorage>>,preparing:bool,restaged:Option<G17PRenderReservation>,
 flist_index:usize,submission_ordinal:u64,hardware_buffer_id:Option<u32>,hardware_buffer_published:bool,prepared_render_flist:Option<()>,auxiliary_timestamp:u64}
struct Slots(Vec<Slot>);
impl Slots{fn allocated(&self,s:u8)->Result<&Slot>{self.0.get(s as usize).ok_or(EIO)}fn get(&self,s:u8)->&Slot{&self.0[s as usize]}fn get_mut(&mut self,s:u8)->&mut Slot{&mut self.0[s as usize]}}
struct Ids(Vec<u32>);impl Ids{fn release(&mut self,id:u32)->Result{self.0.push(id);Ok(())}}
''' + item(m,'pub(crate) struct G17PRenderDependencies {') + '#[derive(Clone,Copy)]\n' + item(m,'struct G17PRenderPreparedSksm {') + item(m,'struct G17PRenderReservation {') + item(m,'pub(crate) struct G17PRenderRestage {') + r'''
struct Resources{fail_commit:bool,commits:usize}impl Resources{fn commit_host_tvb_grow(&mut self,_:&g17_resources::G17PHostTvbGrow)->Result{self.commits+=1;if self.fail_commit{Err(EIO)}else{Ok(())}}fn owned_uat(&self)->Arc<KBox<mmu::Uat>>{Arc::new(Box::new(mmu::Uat))}}
// Shared FList: the UMA lifecycle lives in the manager, indexed by each pair's FList pool.
struct Manager{resources:Resources,render_slots:Slots,num_clusters:u32,render_hardware_buffer_ids:Ids,released_states:usize,render_flist_owners:Vec<Option<Uma>>}
impl Manager{
 // v96: borrow ends retry parked firmware TVB grows (tested in test_g17_v96_grow_rekick.py).
 fn rekick_parked_render_grows(&self,_pair:Option<u8>){}
 fn new(drops:Arc<std::sync::atomic::AtomicUsize>)->Self{Self{resources:Resources{fail_commit:false,commits:0},render_slots:Slots((0..2).map(|i|Slot{sksm:None,storage:Some(Box::new(g17_resources::G17PUserRenderStorage{id:i,generation:None,grown:std::cell::Cell::new(false),drops:drops.clone()})),preparing:false,restaged:None,flist_index:i as usize,submission_ordinal:7,hardware_buffer_id:None,hardware_buffer_published:false,prepared_render_flist:None,auxiliary_timestamp:0}).collect()),num_clusters:1,render_hardware_buffer_ids:Ids(vec![]),released_states:0,render_flist_owners:vec![None,None]}}
 fn reserve_translated_render(&mut self,_:&AsahiDevice,_:&mmu::Vm,p:g17_resources::G17PRenderQueuePair,c:Arc<g17_resources::G17PLogicalQueueContext>,_:Arc<g17_resources::G17PWorkStateLease>,cmd:&g17_uapi::TranslatedRenderCommand)->Result<G17PRenderReservation>{let s=self.render_slots.get_mut(p.slot());s.hardware_buffer_id=Some(p.slot() as u32);Ok(G17PRenderReservation{hardware_buffer_id:p.slot() as u32,parameter_buffer_token:1,command_buffer_state_word:1,descriptor_context_id:1,execution_context:c,queue_pair:p,ordinal:s.submission_ordinal,prepared_sksm:None,host_tvb_grow:None})}
 fn release_active_render_command_buffer_states(&mut self,_:u8)->Result{self.released_states+=1;Ok(())}
''' + item(m,'    pub(crate) fn take_render_restage(') + item(m,'    pub(crate) fn return_render_restage(') + item(m,'    pub(crate) fn cancel_render_restage(') + r'''
}
fn take(m:&mut Manager,slot:u8)->Result<Option<G17PRenderRestage>>{m.take_render_restage(&AsahiDevice,&mmu::Vm,g17_resources::G17PRenderQueuePair(slot),Arc::new(g17_resources::G17PLogicalQueueContext),Arc::new(g17_resources::G17PWorkStateLease),&g17_uapi::TranslatedRenderCommand,&G17PRenderDependencies{entries:[0;31],count:0})}
#[test]fn independent_preparers_hold_no_global_mutex_and_return_exact_graphs(){
 let drops=Arc::new(std::sync::atomic::AtomicUsize::new(0));let m=Arc::new(Mutex::new(Manager::new(drops.clone())));let barrier=Arc::new(Barrier::new(3));let mut threads=vec![];
 for slot in 0..2{let m=m.clone();let barrier=barrier.clone();threads.push(std::thread::spawn(move||{
  let mut graph=take(&mut m.lock().unwrap(),slot).unwrap().unwrap();
  barrier.wait();barrier.wait();graph.storage.generation=Some(7);
  m.lock().unwrap().return_render_restage(graph);
 }));}
 barrier.wait();{
  let mut state=m.lock().unwrap();for slot in 0..2{assert!(state.render_slots.get(slot).preparing);assert!(state.render_slots.get(slot).storage.is_none());assert!(matches!(take(&mut state,slot),Err(EBUSY)));}
  assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst),0);
 }barrier.wait();for t in threads{t.join().unwrap();}
 {let mut state=m.lock().unwrap();for slot in 0..2{let s=state.render_slots.get(slot);assert!(!s.preparing);assert_eq!(s.storage.as_ref().unwrap().id,slot as u64);assert!(s.restaged.is_some());state.cancel_render_restage(slot).unwrap();assert_eq!(state.render_slots.get(slot).storage.as_ref().unwrap().generation,None);}
 assert_eq!(state.released_states,2);assert_eq!(state.render_hardware_buffer_ids.0,vec![0,1]);}
 assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst),0);drop(m);assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst),2);
}

#[test]fn host_grow_commit_failure_returns_graph_and_reservation_before_error(){
 let drops=Arc::new(std::sync::atomic::AtomicUsize::new(0));let mut m=Manager::new(drops.clone());
 let mut graph=take(&mut m,0).unwrap().unwrap();graph.storage.generation=Some(7);
 graph.reservation.host_tvb_grow=Some(g17_resources::G17PHostTvbGrow);
 m.resources.fail_commit=true;
 assert_eq!(m.return_render_restage(graph),(0,Err(EIO)));
 assert_eq!(m.resources.commits,1);assert!(!m.render_slots.get(0).preparing);
 assert!(m.render_slots.get(0).storage.is_some());assert!(m.render_slots.get(0).restaged.is_some());
 m.cancel_render_restage(0).unwrap();assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst),0);
 assert_eq!(m.render_slots.get(0).storage.as_ref().unwrap().generation,None);
}
#[test]fn pending_uma_allows_owned_preparation_without_dropping_graph(){let drops=Arc::new(std::sync::atomic::AtomicUsize::new(0));let mut m=Manager::new(drops);m.render_flist_owners[0]=Some(Uma{request:Some(()),state:G17PBootstrapUmaRequestState::Submitted,id:0});let graph=take(&mut m,0).unwrap().unwrap();assert!(m.render_slots.get(0).storage.is_none());assert!(m.render_slots.get(0).preparing);m.return_render_restage(graph);assert!(m.render_slots.get(0).storage.is_some());}
#[test]fn failed_preparation_returns_installed_graph_before_cancellation(){let drops=Arc::new(std::sync::atomic::AtomicUsize::new(0));let mut m=Manager::new(drops.clone());let mut graph=take(&mut m,0).unwrap().unwrap();graph.storage.generation=Some(7);m.return_render_restage(graph);m.cancel_render_restage(0).unwrap();assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst),0);assert!(m.render_slots.get(0).storage.is_some());assert!(!m.render_slots.get(0).preparing);}
#[test]fn grow_completion_during_borrow_marks_only_the_same_returned_owner(){
 for identity in [0,99]{
  let drops=Arc::new(std::sync::atomic::AtomicUsize::new(0));let mut m=Manager::new(drops);
  m.render_flist_owners[0]=Some(Uma{request:Some(()),state:G17PBootstrapUmaRequestState::Submitted,id:identity});
  let graph=take(&mut m,0).unwrap().unwrap();assert!(!graph.storage.grown.get());
  m.render_flist_owners[0].as_mut().unwrap().state=G17PBootstrapUmaRequestState::Ready;
  assert!(!graph.storage.grown.get());m.return_render_restage(graph);
  assert_eq!(m.render_slots.get(0).storage.as_ref().unwrap().grown.get(),identity==0);
 }
}
'''
with tempfile.TemporaryDirectory() as tmp:
 src=Path(tmp)/'restage.rs';src.write_text(code);binary=Path(tmp)/'restage'
 subprocess.run(['rustc','--edition=2021','--test',str(src),'-o',str(binary)],check=True)
 subprocess.run([str(binary)],check=True,timeout=10)
# Actual production return precedes every fallible/cancel/epoch branch.
a=d.index('if let Some((drm, mut prepared)) = restage');b=d.index('let result = runtime.submit_translated_render',a);body=d[a:b]
assert body.index('drop(runtime)')<body.index('prepared.prepare(')<body.index('runtime = self.runtime.mutex().lock()')<body.index('runtime.return_render_restage(prepared)')<body.index('!preparation.is_current()')<body.index('prepared_result {')
assert '?' not in body[body.index('drop(runtime)'):body.index('runtime.return_render_restage(prepared)')]
for name in ['handoff_retained_render_vm','require_retired_render_owner_inner','render_slot_idle_for_context','detach_render_slot_graph','commit_render_observation','retire_abandoned_render_if_firmware_retired']:
 assert 'preparing' in item(m,'fn '+name+'(')
assert '.storage' not in item(m,'fn render_completion_masks_for_slot(') # manager identity, not borrowed storage
assert 'queue_g17p_firmware_event_worker' in item(live,'fn return_render_restage(')
print('PASS actual graph borrow/return/cancel: simultaneous independent preparers, busy exclusion, failed prepare retention, return wake, guarded teardown/completion')

# Cold sibling warm-up must encode the allocated PB owner, not the template
# command's ID. Otherwise different pairs reserve different allocator IDs but
# both install ID zero in firmware and trigger PBState rebound assertions.
warm=item(m,'pub(crate) fn warm_render_pair_storage(')
assert warm.index('command.parameters.ta_hardware_buffer_id = hardware_buffer_id') < warm.index('self.warm_render_pair_storage_inner(')
assert warm.index('command.parameters.context_generation = execution_context.generation()') < warm.index('self.warm_render_pair_storage_inner(')
assert 'command.parameters.ta_hardware_buffer_id = r.hardware_buffer_id' in item(m,'impl G17PRenderRestage {')

# Host pre-grow is captured under the local idle proof, allocated in detached
# preparation, and committed even when later descriptor restaging fails.
reserve=item(m,'    fn reserve_translated_render(')
assert 'self.resources.grow_render_tvb(' not in reserve
assert reserve.index('self.require_retired_render_owner(pair_slot)?;',reserve.index('let mut host_tvb_grow')) < reserve.index('self.resources.prepare_host_tvb_grow(')
prepare=item(m,'impl G17PRenderRestage {')
assert prepare.index('grow.grow(') < prepare.index('self.storage.restage_command(')
returned=item(m,'    pub(crate) fn return_render_restage(')
assert returned.index('slot.storage = Some(storage)') < returned.index('self.resources.commit_host_tvb_grow(')
assert 'request_session_rebuild' in item(live,'    pub(crate) fn return_render_restage(')
