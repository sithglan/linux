#!/usr/bin/env python3
"""Run production VM failure/retirement methods against observable owners.

No hardware, kernel callbacks, fault reproduction or unsafe pointer access.
The firmware predicate is an explicit mock witness; its detailed validation is
covered by test_g17_async_retirement.py. This test covers routing into that
predicate, exact ticket identity, retained pins, and once-only notification.
Set NEO_FANOUT_SOURCE to another driver directory for the negative control.
"""
import os
from pathlib import Path
import subprocess
import tempfile

from test_g17_compute_scs_encoding import block
from g17_render_fanout_test_support import fence_state_source, retention_method, KVEC
from g17_compute_inflight_test_support import completion_mod, queue_limits_mock
from g17_recovery_isolation_test_support import recovery_isolation_mod

ROOT = Path(__file__).resolve().parents[1]
SOURCE = Path(os.environ.get("NEO_FANOUT_SOURCE", ROOT))
compute = (SOURCE / "g17_async_compute.rs").read_text()
job = (SOURCE / "g17_job.rs").read_text()
methods = "\n".join(block(compute, marker) for marker in [
    "    fn fail_outstanding_jobs_for_vm_after_compute_observation(",
    "    pub(crate) fn fail_outstanding_jobs_for_vm(",
    "    fn release_firmware_retired_quarantined_compute(",
    "    fn release_firmware_retired_quarantined_compute_nested(",
    "    fn retire_witnessed_quarantined_compute(",
])
completion_methods = "\n".join(block(job, marker) for marker in [
    "    pub(crate) fn fail_while_owned(",
    "    pub(crate) fn release_after_retirement(",
    "    pub(crate) fn recovery_spared(",
])
# Recovery isolation (iso1): spared zero-duration queues fail their fences here.
spared_helper = block(compute, "fn fail_spared_deferred(").replace("G17PAsyncComputeQueue", "Queue")

drm = (SOURCE / "g17_drm.rs").read_text()
live = (SOURCE / "g17_live_boot.rs").read_text()
runtime_render_methods = "\n".join(block(live, marker) for marker in ["    fn reclaim_drained_quarantined_render_slots(", "    fn release_quarantined_render_after_processor_stop(", "    fn fail_render_after_final_observation("])
render_methods = "\n".join(block(drm, marker) for marker in [
    "    pub(crate) fn belongs_to_vm(", "    pub(crate) fn vm_status(",
    "    pub(crate) fn fail_while_owned(", "    pub(crate) fn release_after_processor_stop(",
    "    pub(crate) fn release_after_firmware_drain(",
    "    pub(crate) fn recovery_spared(", "    pub(crate) fn set_recovery_class(",
])
aggregate_done = block(drm, "    fn member_done(")
member_complete = block(block(drm, "impl G17PSubmitCompletionMember {"), "    pub(crate) fn complete(")
source = r'''
#![allow(dead_code,unused_variables)]
use std::sync::{Arc,Mutex as StdMutex};
use std::sync::atomic::{AtomicI32,AtomicU32,AtomicUsize,Ordering};
#[derive(Debug,Copy,Clone,PartialEq,Eq)] struct Error(i32);
impl Error {fn to_errno(self)->i32{self.0} fn from_errno(n:i32)->Self{Self(n)}}
const EIO:Error=Error(-5);
const ENODATA:Error=Error(-61);
type Result<T=()> = std::result::Result<T,Error>;
macro_rules! dev_info {($($t:tt)*)=>{()}}
macro_rules! dev_err {($($t:tt)*)=>{()}}
macro_rules! pr_err {($($t:tt)*)=>{()}}
macro_rules! pr_warn {($($t:tt)*)=>{()}}
macro_rules! dev_warn {($($t:tt)*)=>{()}}
struct Mutex<T>(StdMutex<T>);
impl<T> Mutex<T>{fn lock(&self)->std::sync::MutexGuard<'_,T>{self.0.lock().unwrap()}}
mod g17_status {
 use super::*;
 #[derive(Default)] pub struct VmStatus(pub AtomicI32);
 impl VmStatus {pub fn record(&self,e:i32){let _=self.0.compare_exchange(0,e,Ordering::AcqRel,Ordering::Acquire);}}
}
#[derive(Default)] struct Counts {pins:AtomicUsize,graphs_dropped:AtomicUsize,leases_dropped:AtomicUsize}
struct Pin(Arc<Counts>);
impl Pin {fn quarantine(&self){}}
impl Drop for Pin {fn drop(&mut self){self.0.pins.fetch_sub(1,Ordering::SeqCst);}}
struct Graph(Arc<Counts>);
impl Drop for Graph {fn drop(&mut self){self.0.graphs_dropped.fetch_add(1,Ordering::SeqCst);}}
#[derive(Default)] struct Fence {signals:AtomicUsize,error:AtomicI32}
impl Fence {
 fn set_error(&self,e:Error){self.error.store(e.0,Ordering::SeqCst);}
 fn signal(&self){self.signals.fetch_add(1,Ordering::SeqCst);}
}
#[derive(Default)] struct Aggregate(AtomicUsize);
impl Aggregate {fn complete(&self,_:Result){self.0.fetch_add(1,Ordering::SeqCst);}}
struct Completion {signalled:AtomicU32,vm_job:Mutex<Option<Pin>>,status:Arc<g17_status::VmStatus>,fence:Fence,aggregate_member:Aggregate,recovery_class:std::sync::atomic::AtomicU8}
impl Completion {
''' + completion_methods + r'''
 // iso4 REPLAY inputs. The default mask (0) never makes a replay candidate;
 // the decision itself is tested in test_g17_compute_replay.py.
 fn replay_witness_words(&self)->Option<[u64;2]>{None}
 fn has_native_consumer(&self)->bool{false}
 // Only reached for a spared queue (recovery isolation): fail and release.
 fn complete(&self,r:Result){if let Err(e)=r{self.fence.set_error(e)}self.signalled.store(1,Ordering::SeqCst);drop(self.vm_job.lock().take());self.fence.signal();self.aggregate_member.complete(r);}
}
struct Context(u32);
impl Context {fn context_id(&self)->u32{self.0}}
struct Packet {id:u64,context:Context,completion:Arc<Completion>}
impl Packet {
 fn submission_id(&self)->u64{self.id}
 fn context(&self)->&Context{&self.context}
 fn completion(&self)->Result<&Completion>{Ok(&self.completion)}
}
struct Lease(Arc<Counts>);
impl Lease {fn quarantine(&self){}}
impl Drop for Lease {fn drop(&mut self){self.0.leases_dropped.fetch_add(1,Ordering::SeqCst);}}
#[derive(Clone,Copy)] struct FirmwareTicket(u64);
#[derive(Clone,Copy)] struct Ticket {owner:u64,submission:u64,context:u32,firmware:FirmwareTicket}
struct Active {packet:Arc<Packet>,_lease:Lease,retirement:Option<Ticket>}
struct G17PAsyncComputeReplay {packet:Arc<Packet>,lease:Lease}
''' + completion_mod(fence_state_source()) + queue_limits_mock() + KVEC + r'''
type Inflight=g17_completion::PairFifo<Active,{g17_queue_limits::MAX_COMPUTE_IN_FLIGHT_PER_QUEUE as usize}>;
type RetiredLeases=g17_completion::PairFifo<Lease,{g17_queue_limits::MAX_COMPUTE_IN_FLIGHT_PER_QUEUE as usize}>;
struct HardwareQueue(u8);
impl HardwareQueue {fn queue_id(&self)->u8{self.0}}
struct Previous {publication:Option<()>}
struct Queue {owner:Option<u64>,queue:HardwareQueue,inflight:Inflight,quarantined:bool,awaiting_retirement_witness:bool,retired_leases:RetiredLeases,_graph:Graph,
 previous_binding:Option<Previous>,retirement_ready:bool,retire_pending:bool,spared_quarantine:bool,first_kick_watch:bool,spared_deferred:bool,released:bool}
''' + spared_helper + r'''
''' + recovery_isolation_mod() + r'''
struct Device;
impl Device {fn as_ref(&self)->&Self{self}}
mod g17_resources {
 pub const G17P_RENDER_QUEUED_PER_PAIR:usize=15;
 pub struct G17PRenderQueuePair;
 impl G17PRenderQueuePair {pub const SLOT_COUNT:u8=2;}
}
mod g17_e2e {pub fn stamp()->u64{0} pub const K_AGG_SIGNAL:u32=0;pub fn record(_:u32,_:u32,_:u64,_:[u64;5]){}}
struct RenderAggregate {remaining:AtomicU32,error:AtomicI32,fence:Fence,e2e_id:u64}
impl RenderAggregate {
''' + aggregate_done + r'''
}
struct RenderMember {aggregate:Arc<RenderAggregate>,completed:AtomicU32}
impl RenderMember {
''' + member_complete + r'''
}
#[derive(Default)] struct Registry {quarantined:AtomicUsize,rehabilitated:AtomicUsize}
impl Registry {
 fn quarantine_render_slot(&self,_:u8){self.quarantined.fetch_add(1,Ordering::SeqCst);}
 fn rehabilitate_render_slot_after_processor_stop(&self,_:u8){self.rehabilitated.fetch_add(1,Ordering::SeqCst);}
}
struct RenderLease {slot:u8,registry:Arc<Registry>,counts:Arc<Counts>}
impl Drop for RenderLease {fn drop(&mut self){self.counts.leases_dropped.fetch_add(1,Ordering::SeqCst);}}
struct Render {recovery_class:std::sync::atomic::AtomicU8,id:u64,status:Arc<g17_status::VmStatus>,signalled:AtomicU32,vm_job:Mutex<Option<Pin>>,lease:Mutex<Option<RenderLease>>,fence:Fence,aggregate_member:RenderMember}
impl Render {
''' + render_methods + r'''
 fn render_slot(&self)->Option<u8>{self.lease.lock().as_ref().map(|l|l.slot)}
 fn complete(&self,_:Result){unreachable!("this harness never proves a direct single-member retirement")}
}
mod g17_drm {
 pub(super) type G17PRenderCompletion=super::Render;
 pub(super) enum G17PDeferredCompletion{ReleaseAfterDrain{completion:super::Arc<super::Render>}}
}
mod kernel {pub mod time {
 pub struct Monotonic;pub struct Instant<T>(std::marker::PhantomData<T>);
 impl<T> Instant<T>{pub fn now()->Self{Self(std::marker::PhantomData)}}
}}
mod g17_recovery_path {
 use std::sync::atomic::{AtomicBool,AtomicU64,Ordering};
 pub const RECLAIM_DRAINED:u32=1;
 pub static ENABLED:AtomicBool=AtomicBool::new(false);
 pub static RECLAIM_ATTEMPTS:AtomicU64=AtomicU64::new(0);
 pub static RECLAIM_PAIRS:AtomicU64=AtomicU64::new(0);
 pub static RECLAIM_RELEASED:AtomicU64=AtomicU64::new(0);
 pub enum ReclaimBlock{Recovery,Error,Draining}
 pub fn enabled(_:u32)->bool{ENABLED.load(Ordering::SeqCst)}
 pub fn note_blocked(_:ReclaimBlock){}
}
mod module_parameters {
 pub struct Param;impl Param{pub fn value(&self)->&u32{&0}}
 #[allow(non_upper_case_globals)]pub const g17p_fault_report:Param=Param;
}
mod g17_config{pub const QUARANTINE_SCOPE:u32=2;}
fn note_session_recovery_pending(pending:&mut bool,_:Option<&Device>,_:&str){*pending=true;}

type RenderFifo=g17_completion::PairFifo<Arc<Render>,15>;
struct Handshake {state:u64}
struct Manager {state:u64,proof:Result<bool>,queries:Vec<(u8,u64,u32)>}
impl Manager {
 fn report_queued_render_failure(&mut self,_:u8)->Result{Ok(())}
 fn firmware_recovery_handshake_snapshot(&mut self)->Result<Handshake>{Ok(Handshake{state:self.state})}
 fn independent_compute_retired_by_firmware(&mut self,q:&mut HardwareQueue,t:&FirmwareTicket,n:u32)->Result<bool>{
  self.queries.push((q.0,t.0,n));self.proof
 }
 fn return_witnessed_compute_queue(&mut self,_:&mut HardwareQueue,_:u32)->Result{Ok(())}
 fn render_slot_head_pass_started(&mut self,_:u8)->Option<bool>{None}
}
struct Runtime {async_compute_queues:Vec<Queue>,pending_render:[Option<Arc<Render>>;2],pending_render_queued:[RenderFifo;2],render_fence_state:g17_completion::RenderFenceState,render_event_masks:[[u32;2];2],quarantined_render_completions:KVec<Arc<Render>>,render_ready:[bool;2],reclaim_pending_since:Option<kernel::time::Instant<kernel::time::Monotonic>>,session_recovery_pending:bool,reclaim_queries:Vec<u8>,deferred:Vec<Arc<Render>>,manager:Option<Manager>,pdev:Option<Device>,scans:usize}
impl Runtime {
 fn replay_spared_compute(&mut self,_:u64,kept:KVec<G17PAsyncComputeReplay>){assert!(kept.is_empty(),"no replay in this harness");}
 fn service_async_compute_completions(&mut self)->Result{self.scans+=1;Ok(())}
 fn poll_deferred_render_completion(&mut self,slot:u8)->Result<Option<()>>{Ok(self.render_ready[slot as usize].then_some(()))}
 // Mock the already-proven success boundary; the fanout and error completion
 // are production methods. Promotion mirrors the runtime's FIFO transition.
 fn settle_deferred_render_completion(&mut self,slot:u8,_:())->Result<bool>{
  let i=slot as usize;self.render_ready[i]=false;
  let done=self.pending_render[i].take().unwrap();
  done.release_after_processor_stop();
  done.signalled.store(1,Ordering::SeqCst);done.fence.signal();done.aggregate_member.complete(Ok(()));
  self.pending_render[i]=self.pending_render_queued[i].take();
  if self.pending_render[i].is_none(){self.render_fence_state.settle(slot);self.render_event_masks[i]=[0;2];}
  Ok(true)
 }
''' + methods + retention_method(SOURCE) + runtime_render_methods + r'''
 fn is_crashed(&self)->bool{false}
 fn primary_recovery_pending(&self)->bool{self.manager.as_ref().unwrap().state!=0}
 // Whole-pair witness is explicit. The production predicate and revalidation
 // around inline events are separately executed by test_g17_recovery_reclaim.
 fn reclaim_drained_render_slot(&mut self,slot:u8,_:&mut bool)->Result<Option<g17_recovery_path::ReclaimBlock>>{
  self.reclaim_queries.push(slot);
  self.manager.as_ref().unwrap().proof.map(|drained|if drained{None}else{Some(g17_recovery_path::ReclaimBlock::Draining)})
 }
 fn defer_completion(&mut self,c:g17_drm::G17PDeferredCompletion){match c{g17_drm::G17PDeferredCompletion::ReleaseAfterDrain{completion}=>self.deferred.push(completion)}}
 fn retire_failed_deferred_render(&mut self,_:u8,_:bool)->Result<bool>{Ok(false)}

}
fn fixture()->(Runtime,Arc<g17_status::VmStatus>,Arc<Counts>,Vec<Arc<Completion>>) {
 g17_recovery_path::ENABLED.store(false,Ordering::SeqCst);
 let status=Arc::new(g17_status::VmStatus::default());let counts=Arc::new(Counts::default());
 let mut inflight=Inflight::new();let mut completions=Vec::new();
 for id in [100,101] {
  counts.pins.fetch_add(1,Ordering::SeqCst);
  let c=Arc::new(Completion{signalled:AtomicU32::new(0),vm_job:Mutex(StdMutex::new(Some(Pin(counts.clone())))),status:status.clone(),fence:Fence::default(),aggregate_member:Aggregate::default(),recovery_class:Default::default()});
  completions.push(c.clone());
  let active=Active{packet:Arc::new(Packet{id,context:Context(10),completion:c}),_lease:Lease(counts.clone()),retirement:Some(Ticket{owner:65,submission:id,context:10,firmware:FirmwareTicket(id)})};
  assert!(inflight.push(active).is_ok());
 }
 let q=Queue{owner:Some(65),queue:HardwareQueue(13),inflight,quarantined:false,awaiting_retirement_witness:false,retired_leases:RetiredLeases::new(),_graph:Graph(counts.clone()),previous_binding:None,retirement_ready:false,retire_pending:false,spared_quarantine:false,first_kick_watch:false,spared_deferred:false,released:false};
 (Runtime{async_compute_queues:vec![q],pending_render:[None,None],pending_render_queued:std::array::from_fn(|_|RenderFifo::new()),render_fence_state:Default::default(),render_event_masks:[[0;2];2],quarantined_render_completions:KVec::with_capacity(32),render_ready:[false;2],reclaim_pending_since:None,session_recovery_pending:false,reclaim_queries:Vec::new(),deferred:Vec::new(),manager:Some(Manager{state:0,proof:Ok(false),queries:Vec::new()}),pdev:None,scans:0},status,counts,completions)
}
fn check_retained(rt:&Runtime,c:&Counts){
 assert_eq!(rt.async_compute_queues[0].inflight.len(),2);
 assert_eq!(c.pins.load(Ordering::SeqCst),2);
 assert_eq!(c.graphs_dropped.load(Ordering::SeqCst),0);
 assert_eq!(c.leases_dropped.load(Ordering::SeqCst),0);
}
#[test] fn sibling_loss_arms_the_existing_witness_without_releasing_pins(){
 let(mut rt,status,c,completions)=fixture();
 assert_eq!(rt.fail_outstanding_jobs_for_vm(&status,EIO),2);
 assert!(rt.async_compute_queues[0].quarantined);
 assert!(rt.async_compute_queues[0].awaiting_retirement_witness,"VM fanout stranded ticketed owners outside retirement observation");
 assert_eq!(rt.scans,1);assert!(rt.manager.as_ref().unwrap().queries.is_empty());
 check_retained(&rt,&c);
 for completion in completions {assert_eq!(completion.fence.signals.load(Ordering::SeqCst),1);assert_eq!(completion.fence.error.load(Ordering::SeqCst),-5);}
}
#[test] fn later_exact_retirement_releases_pins_even_with_scheduler_references(){
 let(mut rt,status,c,completions)=fixture();
 rt.fail_outstanding_jobs_for_vm(&status,EIO);
 rt.manager.as_mut().unwrap().proof=Ok(true);
 rt.release_firmware_retired_quarantined_compute("test");
 assert!(rt.async_compute_queues[0].inflight.is_none(),"ticketed failed packets stayed pinned after proof");
 assert!(!rt.async_compute_queues[0].awaiting_retirement_witness);
 assert!(rt.async_compute_queues[0].quarantined);
 assert_eq!(rt.async_compute_queues[0].retired_leases.len(),2);
 assert_eq!(c.pins.load(Ordering::SeqCst),0);
 assert_eq!(c.graphs_dropped.load(Ordering::SeqCst),0);
 assert_eq!(c.leases_dropped.load(Ordering::SeqCst),0);
 assert_eq!(rt.manager.as_ref().unwrap().queries,vec![(13,101,2)]);
 rt.release_firmware_retired_quarantined_compute("again");
 assert_eq!(rt.manager.as_ref().unwrap().queries.len(),1);
 for completion in completions {assert_eq!(completion.fence.signals.load(Ordering::SeqCst),1);assert_eq!(completion.aggregate_member.0.load(Ordering::SeqCst),1);}
 assert_eq!(status.0.load(Ordering::SeqCst),-5);
}
#[test] fn unretired_or_recovering_firmware_keeps_every_owner(){
 let(mut rt,status,c,_)=fixture();rt.fail_outstanding_jobs_for_vm(&status,EIO);
 rt.manager.as_mut().unwrap().state=1;rt.manager.as_mut().unwrap().proof=Ok(true);
 rt.release_firmware_retired_quarantined_compute("recovery");check_retained(&rt,&c);
 assert!(rt.manager.as_ref().unwrap().queries.is_empty());
 rt.manager.as_mut().unwrap().state=0;rt.manager.as_mut().unwrap().proof=Ok(false);
 rt.release_firmware_retired_quarantined_compute("not-retired");check_retained(&rt,&c);
 assert_eq!(rt.manager.as_ref().unwrap().queries.len(),1);
}
#[test] fn incomplete_publication_identity_does_not_arm_a_partial_release(){
 let(mut rt,status,c,_)=fixture();
 rt.async_compute_queues[0].inflight.iter_mut().next().unwrap().retirement=None;
 rt.fail_outstanding_jobs_for_vm(&status,EIO);
 assert!(!rt.async_compute_queues[0].awaiting_retirement_witness);
 rt.manager.as_mut().unwrap().proof=Ok(true);
 rt.release_firmware_retired_quarantined_compute("missing-ticket");check_retained(&rt,&c);
 assert!(rt.manager.as_ref().unwrap().queries.is_empty());
}
#[test] fn mismatched_owner_submission_or_context_never_releases(){
 for field in 0..3 {
  let(mut rt,status,c,_)=fixture();rt.fail_outstanding_jobs_for_vm(&status,EIO);
  let ticket=rt.async_compute_queues[0].inflight.iter_mut().next().unwrap().retirement.as_mut().unwrap();
  match field {0=>ticket.owner+=1,1=>ticket.submission+=1,_=>ticket.context+=1}
  rt.manager.as_mut().unwrap().proof=Ok(true);
  rt.release_firmware_retired_quarantined_compute("wrong-identity");check_retained(&rt,&c);
  assert!(rt.manager.as_ref().unwrap().queries.is_empty());
  assert!(!rt.async_compute_queues[0].awaiting_retirement_witness);
 }
}
#[test] fn witness_errors_fail_closed_without_dropping_packets(){
 let(mut rt,status,c,_)=fixture();rt.fail_outstanding_jobs_for_vm(&status,EIO);
 rt.manager.as_mut().unwrap().proof=Err(EIO);
 rt.release_firmware_retired_quarantined_compute("error");check_retained(&rt,&c);
 assert_eq!(rt.manager.as_ref().unwrap().queries.len(),1);
 assert!(!rt.async_compute_queues[0].awaiting_retirement_witness);
}
#[test] fn unrelated_vm_and_empty_queues_are_unchanged(){
 let(mut rt,_,c,_)=fixture();let other=Arc::new(g17_status::VmStatus::default());
 assert_eq!(rt.fail_outstanding_jobs_for_vm(&other,EIO),0);check_retained(&rt,&c);
 assert!(!rt.async_compute_queues[0].quarantined);
 assert!(!rt.async_compute_queues[0].awaiting_retirement_witness);
 let empty=Queue{owner:None,queue:HardwareQueue(14),inflight:Inflight::new(),quarantined:false,awaiting_retirement_witness:false,retired_leases:RetiredLeases::new(),_graph:Graph(c.clone()),previous_binding:None,retirement_ready:false,retire_pending:false,spared_quarantine:false,first_kick_watch:false,spared_deferred:false,released:false};
 rt.async_compute_queues.push(empty);
 assert_eq!(rt.fail_outstanding_jobs_for_vm(&other,EIO),0);
 assert!(!rt.async_compute_queues[1].awaiting_retirement_witness);
}
#[test] fn duplicate_failure_does_not_reset_the_witness_or_signal_twice(){
 let(mut rt,status,c,completions)=fixture();rt.fail_outstanding_jobs_for_vm(&status,EIO);
 assert!(rt.async_compute_queues[0].awaiting_retirement_witness);
 assert_eq!(rt.fail_outstanding_jobs_for_vm(&status,EIO),0);check_retained(&rt,&c);
 assert!(rt.async_compute_queues[0].awaiting_retirement_witness);
 for completion in completions {assert_eq!(completion.fence.signals.load(Ordering::SeqCst),1);}
}
fn render_pair(rt:&mut Runtime,status:Arc<g17_status::VmStatus>,slot:u8,n:usize)->(Arc<Counts>,Arc<Registry>,Arc<RenderAggregate>,Vec<Arc<Render>>){
 let counts=Arc::new(Counts::default());let registry=Arc::new(Registry::default());
 let aggregate=Arc::new(RenderAggregate{remaining:AtomicU32::new(n as u32),error:AtomicI32::new(0),fence:Fence::default(),e2e_id:1});
 let mut completions=Vec::new();
 for id in 0..n {
  counts.pins.fetch_add(1,Ordering::SeqCst);
  let c=Arc::new(Render{recovery_class:Default::default(),id:id as u64,status:status.clone(),signalled:AtomicU32::new(0),vm_job:Mutex(StdMutex::new(Some(Pin(counts.clone())))),lease:Mutex(StdMutex::new(Some(RenderLease{slot,registry:registry.clone(),counts:counts.clone()}))),fence:Fence::default(),aggregate_member:RenderMember{aggregate:aggregate.clone(),completed:AtomicU32::new(0)}});
  if id==0{rt.pending_render[slot as usize]=Some(c.clone());}else{assert!(rt.pending_render_queued[slot as usize].push(c.clone()).is_ok());}
  completions.push(c);
 }
 assert!(rt.render_fence_state.arm(slot));rt.render_event_masks[slot as usize]=[1,2];
 (counts,registry,aggregate,completions)
}
#[test] fn every_render_member_signals_and_the_aggregate_finishes(){
 let(mut rt,status,_,_)=fixture();rt.async_compute_queues.clear();
 let(counts,registry,aggregate,completions)=render_pair(&mut rt,status.clone(),0,16);
 assert_eq!(rt.fail_outstanding_jobs_for_vm(&status,EIO),16);
 assert_eq!(aggregate.remaining.load(Ordering::SeqCst),0);
 assert_eq!(aggregate.fence.signals.load(Ordering::SeqCst),1);
 assert_eq!(aggregate.fence.error.load(Ordering::SeqCst),-5);
 assert!(completions.iter().all(|c|c.fence.signals.load(Ordering::SeqCst)==1));
 assert_eq!(counts.pins.load(Ordering::SeqCst),16);
 assert_eq!(counts.leases_dropped.load(Ordering::SeqCst),0);
 assert_eq!(registry.quarantined.load(Ordering::SeqCst),16);
 assert_eq!(registry.rehabilitated.load(Ordering::SeqCst),0);
}
#[test] fn failed_fifo_moves_in_order_to_the_existing_retirement_collection(){
 let(mut rt,status,_,_)=fixture();rt.async_compute_queues.clear();
 render_pair(&mut rt,status.clone(),0,16);rt.fail_outstanding_jobs_for_vm(&status,EIO);
 assert!(rt.pending_render[0].is_none());assert!(rt.pending_render_queued[0].is_none());
 assert!(!rt.render_fence_state.pending(0));assert_eq!(rt.render_event_masks[0],[0;2]);
 assert_eq!(rt.quarantined_render_completions.iter().map(|c|c.id).collect::<Vec<_>>(),(0..16).collect::<Vec<_>>());
}
#[test] fn losing_render_vm_does_not_touch_healthy_pair_owners_or_event_state(){
 let(mut rt,status,_,_)=fixture();rt.async_compute_queues.clear();
 render_pair(&mut rt,status.clone(),0,16);
 let healthy=Arc::new(g17_status::VmStatus::default());
 let(counts,registry,aggregate,cs)=render_pair(&mut rt,healthy.clone(),1,16);
 rt.fail_outstanding_jobs_for_vm(&status,EIO);
 assert!(rt.pending_render[1].is_some());assert_eq!(rt.pending_render_queued[1].len(),15);
 assert!(rt.render_fence_state.pending(1));assert_eq!(rt.render_event_masks[1],[1,2]);
 assert_eq!(counts.pins.load(Ordering::SeqCst),16);assert_eq!(registry.quarantined.load(Ordering::SeqCst),0);
 assert_eq!(aggregate.fence.signals.load(Ordering::SeqCst),0);assert_eq!(healthy.0.load(Ordering::SeqCst),0);
 assert!(cs.iter().all(|c|c.fence.signals.load(Ordering::SeqCst)==0));
}
#[test] fn repeated_render_failure_retains_each_owner_exactly_once(){
 let(mut rt,status,_,_)=fixture();rt.async_compute_queues.clear();
 let(_,_,aggregate,cs)=render_pair(&mut rt,status.clone(),0,16);
 rt.fail_outstanding_jobs_for_vm(&status,EIO);
 assert_eq!(rt.fail_outstanding_jobs_for_vm(&status,Error(-110)),0);
 assert_eq!(rt.quarantined_render_completions.len(),16);
 assert_eq!(aggregate.fence.signals.load(Ordering::SeqCst),1);
 assert!(cs.iter().all(|c|c.fence.signals.load(Ordering::SeqCst)==1&&c.fence.error.load(Ordering::SeqCst)==-5));
}
#[test] fn an_already_signalled_head_does_not_hide_its_queued_successors(){
 let(mut rt,status,_,_)=fixture();rt.async_compute_queues.clear();
 let(counts,_,aggregate,cs)=render_pair(&mut rt,status.clone(),0,16);
 assert!(cs[0].fail_while_owned(Error(-110)));
 assert_eq!(rt.fail_outstanding_jobs_for_vm(&status,EIO),15);
 assert_eq!(rt.quarantined_render_completions.len(),16);
 assert_eq!(aggregate.fence.error.load(Ordering::SeqCst),-110);
 assert_eq!(aggregate.fence.signals.load(Ordering::SeqCst),1);
 assert_eq!(counts.pins.load(Ordering::SeqCst),16);
}
#[test] fn one_proven_render_completion_wins_before_remaining_members_fail(){
 let(mut rt,status,_,_)=fixture();rt.async_compute_queues.clear();
 let(counts,_,aggregate,cs)=render_pair(&mut rt,status.clone(),0,16);rt.render_ready[0]=true;
 assert_eq!(rt.fail_outstanding_jobs_for_vm(&status,EIO),15);
 assert_eq!(cs[0].fence.error.load(Ordering::SeqCst),0);
 assert_eq!(rt.quarantined_render_completions.len(),15);
 assert_eq!(counts.pins.load(Ordering::SeqCst),15);
 assert_eq!(aggregate.fence.signals.load(Ordering::SeqCst),1);
}
#[test] fn scheduler_drops_cannot_release_the_runtime_quarantines(){
 let(mut rt,status,_,_)=fixture();rt.async_compute_queues.clear();
 let(counts,_,_,cs)=render_pair(&mut rt,status.clone(),0,16);
 rt.fail_outstanding_jobs_for_vm(&status,EIO);drop(cs);
 assert_eq!(counts.pins.load(Ordering::SeqCst),16);
 assert_eq!(counts.leases_dropped.load(Ordering::SeqCst),0);
 assert_eq!(rt.quarantined_render_completions.len(),16);
}
#[test] fn full_pair_capacity_accepts_all_owners_without_allocating(){
 let(mut rt,status,_,_)=fixture();rt.async_compute_queues.clear();
 render_pair(&mut rt,status.clone(),0,16);render_pair(&mut rt,status.clone(),1,16);
 let capacity=rt.quarantined_render_completions.0.capacity();
 assert_eq!(rt.fail_outstanding_jobs_for_vm(&status,EIO),32);
 assert_eq!(rt.quarantined_render_completions.len(),32);
 assert_eq!(rt.quarantined_render_completions.0.capacity(),capacity);
}
#[test] fn unexpected_retention_exhaustion_keeps_firmware_pins(){
 let(mut rt,status,_,_)=fixture();rt.async_compute_queues.clear();
 rt.quarantined_render_completions=KVec::with_capacity(0);
 let(counts,_,aggregate,cs)=render_pair(&mut rt,status.clone(),0,2);
 rt.fail_outstanding_jobs_for_vm(&status,EIO);drop(cs);
 assert_eq!(counts.pins.load(Ordering::SeqCst),2);
 assert_eq!(counts.leases_dropped.load(Ordering::SeqCst),0);
 assert_eq!(aggregate.fence.signals.load(Ordering::SeqCst),1);
}

#[test] fn blamed_pair_and_vm_sibling_use_one_retained_owner_collection(){
 let(mut rt,status,_,_)=fixture();rt.async_compute_queues.clear();
 let(c0,_,a0,_)=render_pair(&mut rt,status.clone(),0,16);
 let(c1,_,a1,_)=render_pair(&mut rt,status.clone(),1,16);
 rt.fail_render_after_final_observation(0,None,Some(15),EIO);
 assert_eq!(rt.quarantined_render_completions.len(),32);
 assert!(!rt.render_fence_state.pending(0));assert!(!rt.render_fence_state.pending(1));
 assert_eq!(c0.pins.load(Ordering::SeqCst),16);assert_eq!(c1.pins.load(Ordering::SeqCst),16);
 assert_eq!(a0.fence.signals.load(Ordering::SeqCst),1);assert_eq!(a1.fence.signals.load(Ordering::SeqCst),1);
 assert!(!rt.session_recovery_pending);
}
#[test] fn whole_pair_retirement_releases_guards_only_after_deferred_dispatch(){
 let(mut rt,status,_,_)=fixture();rt.async_compute_queues.clear();
 let(counts,registry,aggregate,cs)=render_pair(&mut rt,status.clone(),0,16);
 rt.fail_outstanding_jobs_for_vm(&status,EIO);
 rt.manager.as_mut().unwrap().proof=Ok(true);
 rt.reclaim_drained_quarantined_render_slots(); // unchanged default-off control
 assert!(rt.reclaim_queries.is_empty());assert_eq!(counts.pins.load(Ordering::SeqCst),16);
 g17_recovery_path::ENABLED.store(true,Ordering::SeqCst);
 rt.reclaim_drained_quarantined_render_slots();
 assert_eq!(rt.reclaim_queries,vec![0]);assert!(rt.quarantined_render_completions.is_empty());
 assert_eq!(rt.deferred.len(),16);assert_eq!(counts.pins.load(Ordering::SeqCst),16);
 for c in rt.deferred.drain(..){c.release_after_firmware_drain();}
 assert_eq!(counts.pins.load(Ordering::SeqCst),0);assert_eq!(counts.leases_dropped.load(Ordering::SeqCst),16);
 assert_eq!(registry.rehabilitated.load(Ordering::SeqCst),16);
 assert_eq!(aggregate.fence.signals.load(Ordering::SeqCst),1);
 assert!(cs.iter().all(|c|c.fence.signals.load(Ordering::SeqCst)==1));
 assert_eq!(status.0.load(Ordering::SeqCst),-5);
}
#[test] fn render_drain_pending_error_or_active_recovery_keeps_owners(){
 let(mut rt,status,_,_)=fixture();rt.async_compute_queues.clear();
 let(counts,_,_,_)=render_pair(&mut rt,status.clone(),0,16);
 rt.fail_outstanding_jobs_for_vm(&status,EIO);g17_recovery_path::ENABLED.store(true,Ordering::SeqCst);
 for proof in [Ok(false),Err(EIO)] {
  rt.manager.as_mut().unwrap().proof=proof;rt.reclaim_drained_quarantined_render_slots();
  assert_eq!(rt.quarantined_render_completions.len(),16);assert!(rt.deferred.is_empty());
  assert_eq!(counts.pins.load(Ordering::SeqCst),16);
 }
 rt.reclaim_queries.clear();rt.manager.as_mut().unwrap().proof=Ok(true);rt.manager.as_mut().unwrap().state=1;
 rt.reclaim_drained_quarantined_render_slots();
 assert!(rt.reclaim_queries.is_empty());assert_eq!(counts.pins.load(Ordering::SeqCst),16);
}
#[test] fn processor_stop_also_releases_every_vm_sibling_owner(){
 let(mut rt,status,_,_)=fixture();rt.async_compute_queues.clear();
 let(counts,registry,aggregate,cs)=render_pair(&mut rt,status.clone(),0,16);
 rt.fail_outstanding_jobs_for_vm(&status,EIO);
 rt.release_quarantined_render_after_processor_stop();
 assert!(rt.quarantined_render_completions.is_empty());assert_eq!(counts.pins.load(Ordering::SeqCst),0);
 assert_eq!(counts.leases_dropped.load(Ordering::SeqCst),16);assert_eq!(registry.rehabilitated.load(Ordering::SeqCst),16);
 assert_eq!(aggregate.fence.signals.load(Ordering::SeqCst),1);
 assert!(cs.iter().all(|c|c.fence.signals.load(Ordering::SeqCst)==1));
}

'''

with tempfile.TemporaryDirectory() as directory:
    directory = Path(directory)
    test = directory / "test.rs"
    test.write_text(source)
    executable = directory / "test"
    subprocess.run(["rustc", "--edition=2021", "--test", str(test), "-o", str(executable)], check=True)
    subprocess.run([str(executable), "--test-threads=1"], check=True, timeout=60)
