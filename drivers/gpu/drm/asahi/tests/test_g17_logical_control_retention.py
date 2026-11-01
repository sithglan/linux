#!/usr/bin/env python3
"""Compile actual scheduler-page close/publication/consumption ownership paths.

Native successful close waits for opcode0x14 consumption before destroying
logical channels; physical KSM backing survives. This replaces the obsolete
test of a removed session-global scheduler-page retention cache.
"""
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
manager = (ROOT / "g17_manager.rs").read_text()
runtime = (ROOT / "g17_live_boot.rs").read_text()


def item(source, marker):
    start = source.index(marker)
    end = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


# The behavioral harness below replaces hardware storage and graph detachment,
# not the production release state machine or cursor-consumption predicate.
methods = "\n".join(item(manager, marker) for marker in [
    "pub(crate) fn release_render_context_owner(",
    "fn find_logical_queue_teardown(",
    "fn push_logical_queue_teardown(",
    "pub(crate) fn publish_logical_queue_release(",
    "pub(crate) fn settle_logical_queue_teardowns(",
])
code = r'''
#![allow(dead_code,unused_variables,private_interfaces)]
extern crate self as kernel;
use std::sync::Arc;
type Result<T=()> = std::result::Result<T,i32>;
const EIO:i32=5;const EINVAL:i32=22;const EOVERFLOW:i32=75;
const ENOSPC:i32=28;const ENOMEM:i32=12;const EAGAIN:i32=11;
const EBUSY:i32=16;const GFP_KERNEL:()=();
macro_rules! dev_info {($($arg:tt)*)=>{{}}}
macro_rules! dev_err {($($arg:tt)*)=>{{}}}
mod util {pub fn g17p_verbose()->bool{false}}
pub mod time {
 pub struct Monotonic;
 pub struct Instant<T>(std::marker::PhantomData<T>);
 impl<T> Instant<T>{pub fn now()->Self{Self(std::marker::PhantomData)}}
}
struct KVec<T>{items:Vec<T>,fail_reserve:bool,fail_push:bool}
impl<T>KVec<T>{
 fn new()->Self{Self{items:vec![],fail_reserve:false,fail_push:false}}
 fn reserve(&mut self,n:usize,_:())->Result{
  if self.fail_reserve{return Err(ENOMEM)}self.items.reserve(n);Ok(())}
 fn push_within_capacity(&mut self,t:T)->std::result::Result<(),T>{
  if self.fail_push{return Err(t)}self.items.push(t);Ok(())}
 fn remove(&mut self,i:usize)->Result<T>{
  if i<self.items.len(){Ok(self.items.remove(i))}else{Err(EINVAL)}}
}
impl<T> std::ops::Deref for KVec<T>{type Target=Vec<T>;
 fn deref(&self)->&Vec<T>{&self.items}}
impl<T> std::ops::DerefMut for KVec<T>{fn deref_mut(&mut self)->&mut Vec<T>{&mut self.items}}
impl<T> IntoIterator for KVec<T>{type Item=T;type IntoIter=std::vec::IntoIter<T>;
 fn into_iter(self)->Self::IntoIter{self.items.into_iter()}}
mod g17_initdata {
 pub const CONTROL_RING_INDEX_MASK:u32=255;
 pub const CHANNEL_STATE_CONSUMER:usize=0;pub const CHANNEL_STATE_PRODUCER:usize=1;
 pub const CONTROL_RECORD_SIZE:usize=32;
 pub fn encode_scheduler_state_release_record(va:u64,id:[u8;4],out:&mut[u8])->Result<(),()>{
  out[0]=0x14;out[4..8].copy_from_slice(&id);out[8..16].copy_from_slice(&va.to_le_bytes());Ok(())}
}
mod g17_resources {
 use super::*;
 pub struct Control{pub published:bool,pub va:u64,pub identity:[u8;4]}
 impl Control {
  pub fn is_published(&self)->bool{self.published}
  pub fn release_identity(&self)->Result<[u8;4]>{Ok(self.identity)}
  pub fn gpu_va(&self)->u64{self.va}
 }
 pub struct G17PLogicalQueueContext{pub id:u32,pub channel_control:Arc<Control>}
 impl G17PLogicalQueueContext{pub fn context_id(&self)->u32{self.id}}
 pub struct G17PDeviceControlPublication{pub producer_after:u32,pub producer_before:u32,pub slot:u32}
}
use g17_resources::*;
struct Counters{primary:[u32;2]}
struct Resources{consumer:u32,producer:u32,full:bool,records:Vec<[u8;32]>}
impl Resources {
 fn control_counters(&mut self)->Result<Counters>{
  Ok(Counters{primary:[self.consumer,self.producer]})}
 fn publish_primary_device_control_raw(&mut self,r:&[u8;32])->Result<G17PDeviceControlPublication>{
  if self.full{return Err(EAGAIN)}
  let before=self.producer;self.producer=(before+1)&255;self.records.push(*r);
  Ok(G17PDeviceControlPublication{producer_before:before,producer_after:self.producer,slot:before})
 }
}
struct Vm{cleared:std::cell::Cell<u32>}
impl Vm{fn clear_render_pool_mappings_if_owner(&self,_:u8,pool:u64){assert_eq!(pool,42);self.cleared.set(self.cleared.get()+1)}}
struct Bind{vm:Arc<Vm>}
impl Bind{fn vm(&self)->&Vm{&self.vm}}
struct Pair;
impl Pair{fn slot(&self)->u8{0}}
struct Storage;
impl Storage{
 fn retained_hardware_buffer_id(&self)->Option<u32>{Some(3)}
 fn queue_pair(&self)->Pair{Pair}
 fn render_pool_id(&self)->u64{42}
}
struct G17PRetiredRenderGraph{bind:Bind,storage:Storage}
struct HardwareIds{unreserved:Vec<u32>}
impl HardwareIds{fn unreserve(&mut self,id:u32){self.unreserved.push(id)}}
''' + item(manager, "enum G17PBootstrapUmaLifecycleError {").replace(
    "enum G17PBootstrapUmaLifecycleError", "#[derive(Debug,PartialEq)]\nenum G17PBootstrapUmaLifecycleError"
) + "\n" + item(manager, "fn g17p_control_ring_target_consumed(") + "\n" + (
    "#[derive(Debug,Copy,Clone,PartialEq,Eq)]\n"
    + item(manager, "enum G17PLogicalQueueTeardownPhase {")
) + "\n" + item(manager, "pub(crate) struct G17PLogicalQueueTeardown {") + "\n" + item(manager, "impl G17PLogicalQueueTeardown {") + r'''
struct Manager {
 logical_queue_teardowns:KVec<G17PLogicalQueueTeardown>,
 pending_primary_control:Option<()>,resources:Resources,
 render_hardware_buffer_ids:HardwareIds,detach_calls:usize,detach_busy:bool,
 detached:KVec<G17PRetiredRenderGraph>,
 // Shared FList: settlement also waits until the teardown's pools are
 // released (production predicate displaced_graphs_flists_released).
 flists_released:bool,
}
impl Manager {
 fn new(cursor:u32)->Self{Self{
  logical_queue_teardowns:KVec::new(),pending_primary_control:None,
  resources:Resources{consumer:cursor,producer:cursor,full:false,records:vec![]},
  render_hardware_buffer_ids:HardwareIds{unreserved:vec![]},detach_calls:0,
  detach_busy:false,detached:KVec::new(),flists_released:true,
 }}
 fn displaced_graphs_flists_released(&self,_:usize)->bool{self.flists_released}
 fn detach_render_graphs_of_context(&mut self,_:&Arc<G17PLogicalQueueContext>,_:u16)
   ->Result<KVec<G17PRetiredRenderGraph>>{
  self.detach_calls+=1;if self.detach_busy{return Err(EBUSY)}
  Ok(std::mem::replace(&mut self.detached,KVec::new()))
 }
''' + methods + r'''
}
fn context(id:u32,published:bool)->Arc<G17PLogicalQueueContext>{
 Arc::new(G17PLogicalQueueContext{id,channel_control:Arc::new(Control{
  published,va:0x10000+u64::from(id)*0x4000,identity:[3,2,1,4]})})
}
#[test]fn unpublished_close_does_not_retain_or_submit(){
 let mut m=Manager::new(0);let c=context(9,false);let w=Arc::downgrade(&c);
 m.release_render_context_owner(&c).unwrap();drop(c);
 assert!(w.upgrade().is_none());assert!(m.logical_queue_teardowns.is_empty());
 assert!(m.resources.records.is_empty());
}
#[test]fn publication_is_not_consumption_and_settled_drop_is_deferred(){
 let mut m=Manager::new(147);let c=context(9,true);
 let w=Arc::downgrade(&c);let page=Arc::downgrade(&c.channel_control);
 // An installed physical queue may retain just the scheduler page.
 let installed=c.channel_control.clone();
 m.release_render_context_owner(&c).unwrap();
 m.release_render_context_owner(&c).unwrap();
 assert_eq!(m.logical_queue_teardowns.len(),1);
 m.publish_logical_queue_release(0).unwrap();drop(c);
 assert_eq!(m.resources.records[0][0],0x14);
 assert_eq!(&m.resources.records[0][4..8],&[3,2,1,4]);
 assert!(m.settle_logical_queue_teardowns().unwrap().is_empty());
 assert_eq!(m.detach_calls,0);assert!(w.upgrade().is_some());
 m.resources.consumer=148;
 let settled=m.settle_logical_queue_teardowns().unwrap();
 assert_eq!(settled.len(),1);assert!(w.upgrade().is_some());
 assert!(m.logical_queue_teardowns.is_empty());drop(settled);
 assert!(w.upgrade().is_none());assert!(page.upgrade().is_some());
 drop(installed);assert!(page.upgrade().is_none());
}
#[test]fn control_wrap_and_later_pending_records_do_not_strand_old_release(){
 let mut m=Manager::new(254);let c=context(9,true);
 m.release_render_context_owner(&c).unwrap();m.publish_logical_queue_release(0).unwrap();
 // An unrelated control has been published after this release.
 m.resources.producer=0;
 assert!(m.settle_logical_queue_teardowns().unwrap().is_empty());
 m.resources.consumer=255;
 assert_eq!(m.settle_logical_queue_teardowns().unwrap().len(),1);
}
#[test]fn wrapped_target_waits_until_consumer_passes_zero(){
 let mut m=Manager::new(255);let c=context(9,true);
 m.release_render_context_owner(&c).unwrap();m.publish_logical_queue_release(0).unwrap();
 assert!(m.settle_logical_queue_teardowns().unwrap().is_empty());
 m.resources.consumer=0;assert_eq!(m.settle_logical_queue_teardowns().unwrap().len(),1);
}
#[test]fn full_ring_and_mandatory_reply_preserve_draining_owner(){
 let mut m=Manager::new(0);let c=context(9,true);m.release_render_context_owner(&c).unwrap();
 m.resources.full=true;
 assert!(matches!(m.publish_logical_queue_release(0),Err(EAGAIN)));
 assert_eq!(m.logical_queue_teardowns[0].phase,G17PLogicalQueueTeardownPhase::Draining);
 m.resources.full=false;m.pending_primary_control=Some(());
 assert!(matches!(m.publish_logical_queue_release(0),Err(EAGAIN)));
 m.pending_primary_control=None;m.publish_logical_queue_release(0).unwrap();
 assert!(matches!(m.publish_logical_queue_release(0),Err(EINVAL)));
 assert_eq!(m.resources.records.len(),1);
}
#[test]fn pointer_identity_not_recycled_numeric_id_selects_teardown(){
 let mut m=Manager::new(0);let a=context(9,true);let b=context(9,true);
 m.release_render_context_owner(&a).unwrap();m.release_render_context_owner(&b).unwrap();
 assert_eq!(m.find_logical_queue_teardown(&a),Some(0));
 assert_eq!(m.find_logical_queue_teardown(&b),Some(1));
 m.publish_logical_queue_release(0).unwrap();m.resources.consumer=1;
 assert_eq!(m.settle_logical_queue_teardowns().unwrap().len(),1);
 assert_eq!(m.find_logical_queue_teardown(&b),Some(0));
 assert_eq!(m.logical_queue_teardowns[0].phase,G17PLogicalQueueTeardownPhase::Draining);
}
#[test]fn deferred_graph_owner_arms_once_at_actual_close(){
 let mut m=Manager::new(0);let c=context(9,true);
 m.push_logical_queue_teardown(&c,G17PLogicalQueueTeardownPhase::Deferred,time::Instant::now()).unwrap();
 assert!(matches!(m.publish_logical_queue_release(0),Err(EINVAL)));
 m.release_render_context_owner(&c).unwrap();
 assert_eq!(m.logical_queue_teardowns.len(),1);
 assert_eq!(m.logical_queue_teardowns[0].phase,G17PLogicalQueueTeardownPhase::Draining);
}
#[test]fn unexpected_busy_graph_is_retained_after_control_consumption(){
 let mut m=Manager::new(0);let c=context(9,true);let w=Arc::downgrade(&c);
 let vm=Arc::new(Vm{cleared:std::cell::Cell::new(0)});
 m.detached.items.push(G17PRetiredRenderGraph{bind:Bind{vm:vm.clone()},storage:Storage});
 m.release_render_context_owner(&c).unwrap();m.publish_logical_queue_release(0).unwrap();drop(c);
 m.resources.consumer=1;m.detach_busy=true;
 assert!(m.settle_logical_queue_teardowns().unwrap().is_empty());
 assert!(w.upgrade().is_some());assert_eq!(vm.cleared.get(),0);
 m.detach_busy=false;let settled=m.settle_logical_queue_teardowns().unwrap();
 assert_eq!(vm.cleared.get(),0);
 for teardown in settled.iter(){teardown.release_retired_vm_mappings();}
 assert_eq!(vm.cleared.get(),1);assert!(w.upgrade().is_some());drop(settled);
 assert!(w.upgrade().is_none());assert_eq!(m.render_hardware_buffer_ids.unreserved,vec![3]);
}
#[test]fn unreleased_pool_keeps_a_consumed_teardown_draining(){
 let mut m=Manager::new(0);let c=context(9,true);
 m.release_render_context_owner(&c).unwrap();m.publish_logical_queue_release(0).unwrap();
 m.resources.consumer=1;m.flists_released=false;
 assert!(m.settle_logical_queue_teardowns().unwrap().is_empty());
 assert_eq!(m.detach_calls,0);assert_eq!(m.logical_queue_teardowns.len(),1);
 m.flists_released=true;assert_eq!(m.settle_logical_queue_teardowns().unwrap().len(),1);
}
#[test]fn malformed_control_indices_do_not_detach(){
 let mut m=Manager::new(0);let c=context(9,true);
 m.release_render_context_owner(&c).unwrap();m.publish_logical_queue_release(0).unwrap();
 m.resources.consumer=256;
 assert!(matches!(m.settle_logical_queue_teardowns(),Err(EIO)));
 assert_eq!(m.detach_calls,0);assert_eq!(m.logical_queue_teardowns.len(),1);
}
'''

# Wire the tested consumption state machine to the real hardware-work gates.
service = item(runtime, "pub(crate) fn service_logical_queue_teardowns(")
assert service.index("logical_queue_teardown_render_ready(") < service.index("publish_logical_queue_release(")
assert service.index("compute_idle_for_context(") < service.index("publish_logical_queue_release(")
ready = item(manager, "pub(crate) fn logical_queue_teardown_render_ready(")
assert "render_pairs_idle_for_context(" in ready
assert "displaced_graphs_flists_released(" in ready
detach = item(manager, "fn detach_render_slot_graph(")
assert detach.index("Arc::ptr_eq(owner, context)") < detach.index("state.storage.take()")
assert detach.index("render_slot_idle_for_context(") < detach.index("state.storage.take()")
assert detach.index("queues.backing = Some(storage.take_sksm_backing()?)") < detach.index("state.storage.take()")
stop = item(manager, "pub(crate) fn release_runtime_objects_after_processor_stop(")
assert "logical_queue_teardowns.clear()" in stop

negative_control = "--negative-control" in sys.argv
if negative_control:
    # Simulate mistaking successful publication for firmware consumption.
    predicate = "Ok(target_ahead == 0 || target_ahead > pending)"
    assert code.count(predicate) == 1
    code = code.replace(predicate, "Ok(true)")

with tempfile.TemporaryDirectory(prefix="g17-control-retention-") as temp:
    path = Path(temp)
    (path / "test.rs").write_text(code)
    subprocess.run(["rustc", "--edition=2021", "--test", str(path / "test.rs"),
                    "-o", str(path / "test")], check=True)
    result = subprocess.run([str(path / "test"), "--test-threads=1"],
                            capture_output=True, text=True)
    if negative_control:
        assert result.returncode != 0
        assert "publication_is_not_consumption_and_settled_drop_is_deferred ... FAILED" in result.stdout
        print("PASS negative control: premature scheduler release detected")
        sys.exit(0)
    print(result.stdout, end="")
    if result.returncode:
        print(result.stderr, end="", file=sys.stderr)
        result.check_returncode()
print("PASS production logical close/publication/consumption ownership: 9 behavioral cases; hardware gate and retained SKSM wiring")
