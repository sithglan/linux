#!/usr/bin/env python3
"""Run production per-QID completion/retirement methods against firmware witnesses."""
from pathlib import Path
from g17_config_test_support import with_g17_config
from g17_compute_inflight_test_support import completion_mod, queue_limits_mock, TIMING_BUFFER_MOCK, KVEC_MOCK
import subprocess,tempfile,sys
from g17_recovery_isolation_test_support import recovery_isolation_mod
p=Path(__file__).resolve().parents[1]
def item(s,needle):
 a=s.index(needle);start=s.index('{',a);depth=1;i=start+1
 while depth:
  depth+=(s[i]=='{')-(s[i]=='}');i+=1
 return s[a:i]
m=(p/'g17_manager.rs').read_text();r=(p/'g17_async_compute.rs').read_text();l=(p/'g17_live_boot.rs').read_text();resources=(p/'g17_resources.rs').read_text()
mask_helper=item(r,'const fn g17p_async_completion_mask_contains(')
batch_start=resources.index('#[derive(Debug, Copy, Clone, PartialEq, Eq)]', resources.index('const G17P_COMPUTE_COMPLETION_QID_COUNT'))
batch_impl=item(resources,'impl G17PComputeCompletionBatch {')
batch_code=resources[batch_start:resources.index('impl G17PComputeCompletionBatch {')]+batch_impl
retired_start=m.index('/// One exact retirement: the UAPI timestamp pair plus the raw firmware')
retired_code=m[retired_start:m.index('impl G17PComputeRetirementTicket {', retired_start)]
manager_batch='\n'.join(item(m,n) for n in [
 '    pub(crate) fn begin_independent_compute_completion_batch(',
 '    pub(crate) fn stage_independent_compute_completion(',
 '    pub(crate) fn take_staged_independent_compute_observation(',
 '    pub(crate) fn independent_compute_kick_finished(',
 '    pub(crate) fn scan_independent_compute_completion_batch(',
 '    pub(crate) fn poll_staged_independent_compute_completion(',
 '    pub(crate) fn independent_compute_retired_by_firmware('])
recordless_start=m.index('/// Whether a finished kick with no latched completion record may retire on')
recordless_code=m[recordless_start:m.index('/// One exact retirement: the UAPI timestamp pair plus the raw firmware')]
front_finished=item(r,'fn g17p_async_compute_front_finished(').replace('g17_manager::G17PManagerConstruction','Manager')
compute_src=(p/'g17_compute.rs').read_text()
stamp_code='\n'.join(item(compute_src,n) for n in [
 'pub(crate) const fn completion_stamp_value(',
 'pub(crate) const fn completion_stamp_covers('])
negative_control = '--negative-control' in sys.argv
if negative_control:
 stamp_code = stamp_code.replace(item(compute_src, 'pub(crate) const fn completion_stamp_covers('),
  'pub(crate) const fn completion_stamp_covers(stamp:u32,_kick:u64)->bool{stamp!=0}')
constant=[l for l in compute_src.splitlines() if l.strip().startswith('pub(crate) const COMPUTE_COMPLETION_STAMP_CONSTANT')][0]
assert 'completion_stamp_covers(\n                queue.compute.read_compute_stamp()?, ticket.kick_timestamp)' in m
methods='\n'.join(item(r,n) for n in [
 '    fn service_async_compute_completions_for_event(',
 '    fn service_async_compute_completion_masks(',
 '    pub(crate) fn service_async_compute_completions(',
 '    fn service_async_compute_completions_matching(',
 '    pub(crate) fn release_async_compute_queue(',
 '    pub(crate) fn cancel_async_compute(',
 '    pub(crate) fn quarantine_async_compute(',
 '    fn release_firmware_retired_quarantined_compute(',
 '    fn release_firmware_retired_quarantined_compute_nested(',
 '    fn retire_witnessed_quarantined_compute(',
 '    fn drop_unpublished_compute_queue('])
# Recovery isolation (iso1): spared zero-duration queues fail their fences here.
spared_helper=item(r,'fn fail_spared_deferred(')
publish = item(r, '    pub(crate) fn publish_async_compute(')
service = item(r, '    fn service_async_compute_completions_matching(')
assert 'queue_g17p_firmware_event_worker' not in publish
assert 'fsleep(' not in service
assert 'async_visibility_pending' not in service
# Records are latched before retirement and matched by exact kick identity.
assert service.index('take_staged_independent_compute_observation(') < service.index('poll_staged_independent_compute_completion(')
assert 'q.retire_pending' in service
# A retired kick is handed to the deferral list (signalled after the runtime mutex
# is released, see test_g17_deferred_completion_signal.py), never completed in place.
assert 'G17PDeferredCompletion::Compute {' in service and '.complete(Ok(timestamps))' not in service
arm=item(m,'    fn arm_compute_completion(')
# Native coalesces timestamp records: independent queues keep their stable
# address and advance the value without erasing any live completion witness.
assert 'let discarded = if queue.independent_context { 0 }' in arm
assert arm.index('if crate::g17_config::COMPLETION_STAMP != 0 {') < arm.index('arm_compute_stamp()')
for builder in ['    fn submit_translated_first_bind_channel(', '    fn submit_translated_repeat_channel(', '    pub(crate) fn submit_translated_compute(']:
    body=item(m,builder)
    assert body.index('let kick_timestamp = queue.producer.current_timestamp;') < body.index('set_completion_stamp_value(raw, kick_timestamp)')
dispatch=item(l, '    pub(crate) fn service_primary_firmware_events(')
assert 'self.service_async_compute_completion_masks(completion_masks)' in dispatch
inline=item(l, '    fn service_firmware_events_inline(')
drain=item(l, 'fn drain_primary_firmware_event_batch(')
assert '.firmware_event_notifications\n        .swap(0, Ordering::AcqRel)' in drain
inline_dispatch=inline.index('self.service_async_compute_completion_masks(completion_masks)?')
assert inline.index('drain_primary_firmware_event_batch(') < inline_dispatch
assert inline.index('let completion_masks = batch.completion_masks;') < inline_dispatch
assert inline_dispatch < inline.index('if let Some(release) = batch.uma_release')
assert inline_dispatch < inline.index('if let Some(generation) = batch.recovery_generation')
assert dispatch.count('self.service_async_compute_completion_masks(completion_masks)') == 1
assert inline.count('self.service_async_compute_completion_masks(completion_masks)') == 1
quarantine = item(r, '    pub(crate) fn quarantine_async_compute(')
assert quarantine.index('self.service_async_compute_completions()') < quarantine.index('q.quarantined = true')
source=r'''
#![allow(dead_code,unused_variables)]
use std::sync::{Arc,atomic::{AtomicUsize,Ordering}};
type Error=i32;type Result<T=()>=std::result::Result<T,Error>;
const EIO:i32=5;const EINVAL:i32=22;const EAGAIN:i32=11;const ENODEV:i32=19;const EBUSY:i32=16;const ERANGE:i32=34;
const ENODATA:i32=61;const ECANCELED:i32=125;const ENOSPC:i32=28;const EOVERFLOW:i32=75;
macro_rules! dev_info{($($t:tt)*)=>{()}}
macro_rules! dev_err{($($t:tt)*)=>{()}}
struct Delta;impl Delta{fn from_micros(_:u64)->Self{Self}}fn fsleep(_:Delta){}
mod util { pub fn g17p_verbose() -> bool { false } }
#[derive(Copy,Clone)]struct Geometry;
impl Geometry{fn stamp_count(&self)->u32{8}fn entry_stride(&self)->u32{0x170}}
#[derive(Copy,Clone)]struct Producer{submitted:u32,previous_valid:bool,previous_timestamp:u32,geometry:Geometry}
mod g17_submission {
 use super::Producer;
 pub struct Prepared{pub entry_offset:u32}
 pub fn prepare_kick_submission(_:u32,_:u8,_:u32,_:u32)->Result<Prepared,()>{Ok(Prepared{entry_offset:0})}
 pub fn complete_g17p_cl_kicks(mut p:Producer,n:u32)->Result<Producer,()>{p.submitted=p.submitted.checked_sub(n).ok_or(())?;Ok(p)}
}
'''+completion_mod('pub const fn g17p_ksm_completion_queue_id(control:u64)->u8{((control>>9)as u8)&0x7f}\npub const fn g17p_ksm_completion_timestamp(control:u64)->u64{(((control>>8)&0xffff_ffff_00)|((control>>48)&0xff))&0x00ff_ffff_ffff}')+queue_limits_mock()+TIMING_BUFFER_MOCK+KVEC_MOCK+r'''
mod g17_compute{
'''+constant+"\n"+stamp_code+r'''
}
mod g17_resources{
 pub(crate) use super::G17PComputeCompletionCandidate;
 pub struct G17PWorkStateLease { pub node:u64, pub counters:[u32;2] }
 impl G17PWorkStateLease {
  pub fn tsq_gpu_va(&self)->super::Result<u64>{Ok(self.node)}
  pub fn tsq_counters(&self)->super::Result<[u32;2]>{Ok(self.counters)}
 }
}
mod g17_manager{pub(crate) use super::G17PComputeRecordless;}
static G17P_COMPUTE_RECORDLESS_RETIRES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
thread_local! { static DESC_TS: std::cell::RefCell<std::collections::HashMap<(u8,u64),u64>> = std::cell::RefCell::new(std::collections::HashMap::new()); }
/// The kick timestamp the harness gave the kick published on this descriptor (1 when unknown).
fn desc_ts(qid:u8,descriptor:u64)->u64{DESC_TS.with(|m|m.borrow().get(&(qid,descriptor)).copied().unwrap_or(1))}
fn control(qid:u8,ts:u64)->u64{((ts&0xff)<<48)|(((ts>>8)&0xffff_ffff)<<16)|(u64::from(qid)<<9)|0x10}
const G17P_COMPUTE_COMPLETION_RECORD_SIZE:usize=0x40;const G17P_COMPUTE_COMPLETION_TIMESTAMP_MASK:u64=0x003f_ffff_ffff_ffff;const G17P_COMPUTE_COMPLETION_DESCRIPTOR_MASK:u64=!1u64;const G17P_COMPUTE_COMPLETION_QID_COUNT:usize=128;
'''+batch_code+r'''
thread_local! { static SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
thread_local! { static QOS_COMPLETIONS: std::cell::RefCell<Vec<(u8,u8,u8)>> = const { std::cell::RefCell::new(Vec::new()) }; }
type Record=(u8,u64,[u64;2]);
struct Resources{records:Vec<Record>,publish_after_scan:Option<Record>}
impl Resources{fn complete_qos_queue_records(&mut self,owners:&[(u8,u8,u8)])->Result{QOS_COMPLETIONS.with(|rows|rows.borrow_mut().extend_from_slice(owners));Ok(())}}
impl Resources{fn scan_compute_completion_batch(&mut self,batch:&mut G17PComputeCompletionBatch)->Result{SCANS.with(|p|p.set(p.get()+1));for(qid,descriptor,timestamps)in &self.records{let mut raw=[0u8;0x40];raw[0..8].copy_from_slice(&control(*qid,desc_ts(*qid,*descriptor)).to_le_bytes());raw[0x10..0x18].copy_from_slice(&timestamps[0].to_le_bytes());raw[0x20..0x28].copy_from_slice(&timestamps[0].to_le_bytes());raw[0x28..0x30].copy_from_slice(&timestamps[1].to_le_bytes());raw[0x30..0x38].copy_from_slice(&descriptor.to_le_bytes());raw[0x38..0x40].copy_from_slice(&1u64.to_le_bytes());batch.fold_record(&raw)?;}if let Some(record)=self.publish_after_scan.take(){self.records.push(record);}Ok(())}}
struct RingState{consumer:u32,producer:u32,count:u32}
/// Firmware witnesses: final stamp, [target, completed], (consumer, producer, count).
/// `.0` is the kick timestamp whose processing stored the stamp word (0: none).
struct Stamp(u32,[u32;2],(u32,u32,u32));impl Stamp{fn read_compute_stamp(&self)->Result<u32>{Ok(if self.0==0{0}else{g17_compute::completion_stamp_value(u64::from(self.0))})}fn read_compute_submitted_counters(&mut self)->Result<[u32;2]>{Ok(self.1)}fn read_compute_ring_state(&mut self)->Result<RingState>{Ok(RingState{consumer:self.2 .0,producer:self.2 .1,count:self.2 .2})}}
struct Storage;impl Storage{fn snapshot_compute_entry(&mut self,_:usize)->Result<[u64;46]>{Ok([0;46])}}
struct G17PSksmQueue{queue_id:u8,independent_context:bool,completion_descriptor:u64,completion_last_end:u64,completion_stamp_armed:bool,compute:Stamp,producer:Producer,storage:Storage}
impl G17PSksmQueue{fn queue_id(&self)->u8{self.queue_id}fn qos_hardware_buffer_id(&self)->u8{self.queue_id+20}fn bind_logical_channel_control(&mut self,_context:&Hold)->Result{Ok(())}}
#[derive(Debug,Copy,Clone,PartialEq,Eq)]struct G17PComputeRetirementTicket{tsq_node:u64,queue_id:u8,descriptor:u64,kick_timestamp:u64,submitted_target:u32,item_producer:u32,item_count:u32}
'''+recordless_code+retired_code+r'''
struct Manager{resources:Resources,compute_completion_batch:G17PComputeCompletionBatch}
fn manager(records:Vec<Record>)->Manager{Manager{resources:Resources{records,publish_after_scan:None},compute_completion_batch:G17PComputeCompletionBatch::new().unwrap()}}
thread_local! { static TIMEOUTS: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) }; }
thread_local! { static RECOVERY: std::cell::Cell<(u64,u64)> = const { std::cell::Cell::new((0,0)) }; }
struct Handshake { epoch:u64, state:u64 }
impl Manager {
 // This harness starts with installed queues; none is an unpublished close.
 fn cancel_unpublished_compute_queue(&mut self,_q:&G17PSksmQueue)->bool{false}
 fn firmware_recovery_handshake_snapshot(&mut self)->Result<Handshake>{let(epoch,state)=RECOVERY.with(|r|r.get());Ok(Handshake{epoch,state})}
 fn log_async_compute_timeout(&mut self,q:&mut G17PSksmQueue)->u128{TIMEOUTS.with(|t|t.borrow_mut().push(q.completion_descriptor));1u128 << q.queue_id}
 // Opt-in diagnostic: reads driver-owned DRAM and logs it, never a register.
 fn log_compute_usage_slot(&mut self,_q:&mut G17PSksmQueue,_phase:&str){}
 fn dump_compute_completion_records(&mut self,_label:&str)->Result{Ok(())}
 fn dump_queue_compute_completion_records(&mut self,_label:&str,_q:&mut G17PSksmQueue)->Result{Ok(())}
 fn return_witnessed_compute_queue(&mut self,_q:&mut G17PSksmQueue,_n:u32)->Result{Ok(())}
'''+manager_batch+r'''
}
mod g17_status { pub struct VmStatus; }
// Counter for retired-without-executing completions; the poller bumps it
// on the zero-duration arm so a boot's total is readable without a wedge.
static G17P_MARK_ZERO_DURATION: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);
// Every route into is_crashed() is raised through this helper so the
// transition names itself in the log; the flag itself is still what the
// assertions below read.
// Probe parameter; 0 is the default and the behaviour these tests assert.
mod module_parameters { pub struct P; impl P { pub fn value(&self)->&'static u32 { &0 } }
  pub static g17p_quarantine_scope: P = P; }
mod g17_live_boot { pub fn note_session_recovery_pending(pending:&mut bool,_pdev:Option<&()>,_reason:&'static str){ *pending = true; } }
struct Completion{signals:AtomicUsize,error:AtomicUsize,status:Arc<g17_status::VmStatus>,retired:AtomicUsize}
impl Completion{fn recovery_spared(&self)->bool{false}fn complete(&self,r:Result<[u64;2]>){if self.signals.compare_exchange(0,1,Ordering::SeqCst,Ordering::SeqCst).is_ok() && r.is_err(){self.error.fetch_add(1,Ordering::SeqCst);}}fn fail_while_owned(&self,e:Error){self.complete(Err(e));}fn cancel_while_owned(&self){self.complete(Err(ECANCELED));}fn release_after_retirement(&self){self.retired.fetch_add(1,Ordering::SeqCst);}
 // iso4 REPLAY inputs: this harness's kicks are never spared, so its
 // queues are never replay candidates (see test_g17_compute_replay.py).
 fn replay_witness_words(&self)->Option<[u64;2]>{None}fn has_native_consumer(&self)->bool{false}}
#[derive(Clone)]struct Context{context_id:u32}impl Context{fn context_id(&self)->u32{self.context_id}}
#[derive(Clone)]struct Packet{id:u64,context:Context,completion:Arc<Completion>}
impl Packet{fn submission_id(&self)->u64{self.id}fn context(&self)->&Context{&self.context}fn completion(&self)->Result<&Completion>{Ok(&self.completion)}}
#[derive(Clone)]struct Hold(Arc<AtomicUsize>);impl Hold{fn quarantine(&self){}fn release_retired_admission(&mut self){}fn retarget_logical_context(&mut self,_context:Hold)->Result{Ok(())}}impl Drop for Hold{fn drop(&mut self){self.0.fetch_add(1,Ordering::SeqCst);}}
#[derive(Debug,Copy,Clone,PartialEq,Eq)]struct G17PAsyncComputeRetirementTicket{owner:u64,submission:u64,context:u32,recovery_epoch:u64,firmware:G17PComputeRetirementTicket}
struct Active{_work_state:g17_resources::G17PWorkStateLease,packet:Packet,_lease:Hold,retirement:Option<G17PAsyncComputeRetirementTicket>,observed:Option<G17PComputeCompletionCandidate>}
struct G17PAsyncComputeReplay{packet:Packet,lease:Hold}
struct Previous{_binding:Option<Hold>,_context:Hold,publication:Option<(u64,u64)>}
type Inflight=g17_completion::PairFifo<Active,{g17_queue_limits::MAX_COMPUTE_IN_FLIGHT_PER_QUEUE as usize}>;
fn one(active:Active)->Inflight{let mut f=Inflight::new();if f.push(active).is_err(){unreachable!()}f}
type RetiredLeases=g17_completion::PairFifo<Hold,{g17_queue_limits::MAX_COMPUTE_IN_FLIGHT_PER_QUEUE as usize}>;
type G17PAsyncComputeQueue=Queue;
struct Queue{owner:Option<u64>,queue:G17PSksmQueue,binding:Option<Hold>,context:Option<Hold>,previous_binding:Option<Previous>,inflight:Inflight,released:bool,retirement_ready:bool,quarantined:bool,retired_by_teardown:bool,terminal_fanout:Option<(Arc<g17_status::VmStatus>,Error)>,retire_pending:bool,awaiting_retirement_witness:bool,retired_leases:RetiredLeases,
 spared_quarantine:bool,first_kick_watch:bool,spared_deferred:bool}
'''+spared_helper+'\n'+recovery_isolation_mod()+r'''
static DEFERRED: AtomicUsize = AtomicUsize::new(0);
mod g17_drm { use super::*; pub enum G17PDeferredCompletion { Compute { packet: Packet, timestamps: [u64;2] } }
 impl G17PDeferredCompletion { pub fn signal(self){ match self { Self::Compute{packet,timestamps} => packet.completion().unwrap().complete(Ok(timestamps)) } }
  // Production drains the list after the runtime mutex is released (test_g17_deferred_completion_signal.py);
  // this harness asserts retirement outcomes, so the mock signals on deferral.
  pub fn defer_into(self,_list:&mut Vec<Self>){ DEFERRED.fetch_add(1,Ordering::SeqCst); self.signal() } } }
fn kick_deferred_completion_drain_from(_deferred:&Vec<g17_drm::G17PDeferredCompletion>,_state:Option<&Arc<()>>,_drain_owned:bool){}
struct G17PLiveRuntime{pdev:Option<()>,manager:Option<Manager>,async_compute_queues:Vec<Queue>,session_recovery_pending:bool,deferred_completions:Vec<g17_drm::G17PDeferredCompletion>,deferred_completion_drain_owned:bool,gfx_state:Option<Arc<()>>}
'''+mask_helper+"\n"+front_finished+r'''
impl G17PLiveRuntime {
 fn replay_spared_compute(&mut self,_owner:u64,kept:KVec<G17PAsyncComputeReplay>){assert!(kept.is_empty(),"no replay in this harness");}
 fn log_timeout_scoreboard_after_power_hold(&mut self,_qids:u128){}
 fn fail_outstanding_jobs_for_vm_after_compute_observation(&mut self,_status:&Arc<g17_status::VmStatus>,_error:Error)->usize{0}
 fn fail_outstanding_jobs_for_vm(&mut self,_status:&Arc<g17_status::VmStatus>,_error:Error)->usize{0}
'''+methods+r'''
}
fn q(owner:u64,descriptor:u64,d:&Arc<AtomicUsize>)->(Queue,Arc<Completion>){let c=Arc::new(Completion{signals:AtomicUsize::new(0),error:AtomicUsize::new(0),status:Arc::new(g17_status::VmStatus),retired:AtomicUsize::new(0)});DESC_TS.with(|m|m.borrow_mut().insert((owner as u8,descriptor),1));let firmware=G17PComputeRetirementTicket{tsq_node:descriptor as u64,queue_id:owner as u8,descriptor,kick_timestamp:1,submitted_target:1,item_producer:1,item_count:8};let retirement=G17PAsyncComputeRetirementTicket{owner,submission:owner,context:owner as u32,recovery_epoch:0,firmware};(Queue{owner:Some(owner),queue:G17PSksmQueue{queue_id:owner as u8,independent_context:true,completion_descriptor:descriptor,completion_last_end:0,completion_stamp_armed:true,compute:Stamp(0,[1,1],(1,1,8)),producer:Producer{submitted:1,previous_valid:true,previous_timestamp:1,geometry:Geometry},storage:Storage},binding:Some(Hold(d.clone())),context:Some(Hold(d.clone())),previous_binding:None,inflight:one(Active{_work_state:g17_resources::G17PWorkStateLease{node:descriptor,counters:[1,1]},packet:Packet{id:owner,context:Context{context_id:owner as u32},completion:c.clone()},_lease:Hold(d.clone()),retirement:Some(retirement),observed:None}),released:false,retirement_ready:false,quarantined:false,retired_by_teardown:false,terminal_fanout:None,retire_pending:false,awaiting_retirement_witness:false,retired_leases:RetiredLeases::new(),spared_quarantine:false,first_kick_watch:false,spared_deferred:false},c)}
#[test]fn exact_retirement_is_handed_to_the_deferral_list(){
 let d=Arc::new(AtomicUsize::new(0));let(a,c)=q(1,0x100,&d);
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(1,0x100,[2,3])])),async_compute_queues:vec![a],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 let before=DEFERRED.load(Ordering::SeqCst);
 // Record latched, final stamp not yet visible: nothing retires, nothing is deferred.
 r.service_async_compute_completions().unwrap();
 assert_eq!(DEFERRED.load(Ordering::SeqCst),before);assert_eq!(c.signals.load(Ordering::SeqCst),0);assert!(r.async_compute_queues[0].inflight.is_some());
 r.async_compute_queues[0].queue.compute.0=1;
 r.service_async_compute_completions().unwrap();
 assert_eq!(DEFERRED.load(Ordering::SeqCst),before+1,"retirement bypassed the deferral list");assert_eq!(c.signals.load(Ordering::SeqCst),1);assert!(r.async_compute_queues[0].inflight.is_none());
}
#[test]fn cancelled_job_retires_on_later_unrelated_event_only_with_exact_proof(){
 let d=Arc::new(AtomicUsize::new(0));let(a,c)=q(1,0x100,&d);
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![])),async_compute_queues:vec![a],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 assert!(r.cancel_async_compute(1,1));assert_eq!(c.error.load(Ordering::SeqCst),1);assert_eq!(d.load(Ordering::SeqCst),0);assert!(!r.session_recovery_pending);
 r.manager.as_mut().unwrap().resources.records.push((1,0x100,[2,3]));
 r.service_async_compute_completion_masks([1<<2,0]).unwrap();
 assert_eq!(c.retired.load(Ordering::SeqCst),0,"record without final stamp cannot release");assert!(r.async_compute_queues[0].inflight.is_some());
 r.async_compute_queues[0].queue.compute.0=1;
 r.service_async_compute_completion_masks([1<<2,0]).unwrap();
 assert_eq!(c.retired.load(Ordering::SeqCst),1);assert_eq!(c.signals.load(Ordering::SeqCst),1);assert_eq!(d.load(Ordering::SeqCst),1);
 assert!(r.async_compute_queues[0].inflight.is_none());assert!(r.async_compute_queues[0].owner.is_none());assert!(r.async_compute_queues[0].retirement_ready);assert!(!r.async_compute_queues[0].quarantined);
 assert!(r.async_compute_queues[0].binding.is_some()&&r.async_compute_queues[0].context.is_some());
}
#[test]fn out_of_order_requires_matching_descriptor_and_final_visibility(){
 let drops=Arc::new(AtomicUsize::new(0));let(a,ca)=q(1,0x100,&drops);let(b,cb)=q(2,0x200,&drops);
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(2,0x200,[10,20])])),async_compute_queues:vec![a,b],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.service_async_compute_completions().unwrap();assert_eq!(ca.signals.load(Ordering::SeqCst),0);assert_eq!(cb.signals.load(Ordering::SeqCst),0);assert_eq!(drops.load(Ordering::SeqCst),0);
 r.async_compute_queues[1].queue.compute.0=7;r.service_async_compute_completions().unwrap();assert_eq!(cb.signals.load(Ordering::SeqCst),1);assert_eq!(ca.signals.load(Ordering::SeqCst),0);assert_eq!(r.async_compute_queues[1].queue.producer.submitted,0);assert_eq!(drops.load(Ordering::SeqCst),1);
 r.service_async_compute_completions().unwrap();assert_eq!(cb.signals.load(Ordering::SeqCst),1);
 r.manager.as_mut().unwrap().resources.records.push((1,0x100,[5,25]));r.async_compute_queues[0].queue.compute.0=8;r.service_async_compute_completions().unwrap();assert_eq!(ca.signals.load(Ordering::SeqCst),1);assert_eq!(drops.load(Ordering::SeqCst),2);
}
#[test]fn close_during_execution_defers_context_and_mapping_release(){
 let d=Arc::new(AtomicUsize::new(0));let(q,c)=q(1,0x100,&d);let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![])),async_compute_queues:vec![q],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.release_async_compute_queue(1);assert_eq!(d.load(Ordering::SeqCst),0);assert_eq!(r.async_compute_queues[0].owner,Some(1));
 r.manager.as_mut().unwrap().resources.records.push((1,0x100,[2,3]));r.async_compute_queues[0].queue.compute.0=1;r.service_async_compute_completions().unwrap();assert_eq!(c.signals.load(Ordering::SeqCst),1);assert_eq!(d.load(Ordering::SeqCst),1);assert_eq!(r.async_compute_queues[0].owner,None);
 // Only the completed packet's lease drops. The binding and context are what
 // the firmware still names through its installed QID, so they are retained
 // until a rebind reinstalls the roots or both CPUs stop -- see
 // test_g17_async_release_retains_firmware_state.
 assert!(r.async_compute_queues[0].binding.is_some()&&r.async_compute_queues[0].context.is_some());
}
#[test]fn timeout_and_zero_duration_quarantine_until_proven_stop(){
 let d=Arc::new(AtomicUsize::new(0));let(a,ca)=q(1,0x100,&d);let(b,cb)=q(2,0x200,&d);let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(2,0x200,[8,8])])),async_compute_queues:vec![a,b],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.quarantine_async_compute(1,999,110);assert_eq!(ca.signals.load(Ordering::SeqCst),0);r.quarantine_async_compute(1,1,110);r.quarantine_async_compute(1,1,110);assert_eq!(ca.error.load(Ordering::SeqCst),1);
 // QUARANTINE_SCOPE=1 (f6a71e767d55a) and the ENODATA zero-duration route
 // retire only the queue and fail its client: the session stays up.
 r.service_async_compute_completions().unwrap();assert_eq!(cb.error.load(Ordering::SeqCst),1);assert!(!r.session_recovery_pending);r.release_async_compute_queue(1);r.release_async_compute_queue(2);assert_eq!(d.load(Ordering::SeqCst),0);assert!(r.async_compute_queues.iter().all(|q|q.inflight.is_some()&&q.quarantined));
 r.async_compute_queues.clear();assert_eq!(d.load(Ordering::SeqCst),6);
}
'''
source += r'''
fn qid_mask(qid:u8)->[u64;2]{let mut mask=[0;2];mask[qid as usize/64]|=1u64<<(qid as usize%64);mask}
#[test]fn qos_retires_exactly_once_only_after_executed_final_completion(){
 QOS_COMPLETIONS.with(|rows|rows.borrow_mut().clear());
 let d=Arc::new(AtomicUsize::new(0));let(a,_)=q(4,0x400,&d);let(b,_)=q(5,0x500,&d);
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x400,[2,8]),(5,0x500,[9,9])])),async_compute_queues:vec![a,b],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.service_async_compute_completions().unwrap();
 assert!(QOS_COMPLETIONS.with(|rows|rows.borrow().is_empty()));
 r.async_compute_queues[0].queue.compute.0=1;
 r.service_async_compute_completions().unwrap();r.service_async_compute_completions().unwrap();
 assert_eq!(QOS_COMPLETIONS.with(|rows|rows.borrow().clone()),vec![(4,24,2)]);
}
#[test]fn completion_mask_selects_every_exact_qid_and_rejects_out_of_range(){
 for qid in 0u8..=127 {
  let mask=qid_mask(qid);
  for candidate in 0u8..=127 {assert_eq!(g17p_async_completion_mask_contains(mask,candidate),candidate==qid);}
 }
 assert!(!g17p_async_completion_mask_contains([u64::MAX;2],128));
 assert!(!g17p_async_completion_mask_contains([u64::MAX;2],255));
}
#[test]fn type1_event_polls_only_its_low_or_high_qid(){
 let d=Arc::new(AtomicUsize::new(0));let(a,ca)=q(4,0x400,&d);let(b,cb)=q(65,0x650,&d);
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x400,[2,8]),(65,0x650,[3,9])])),async_compute_queues:vec![a,b],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.async_compute_queues[0].queue.compute.0=1;r.async_compute_queues[1].queue.compute.0=1;
 SCANS.with(|p|p.set(0));r.service_async_compute_completions_for_event(qid_mask(65)).unwrap();
 assert_eq!(SCANS.with(|p|p.get()),1);assert_eq!(ca.signals.load(Ordering::SeqCst),0);assert_eq!(cb.signals.load(Ordering::SeqCst),1);
 r.service_async_compute_completions_for_event(qid_mask(4)).unwrap();assert_eq!(ca.signals.load(Ordering::SeqCst),1);
 assert!(!g17p_async_completion_mask_contains([u64::MAX;2],200));
}
#[test]fn shared_worker_and_inline_dispatch_keep_targeted_and_zero_mask_fallback(){
 let d=Arc::new(AtomicUsize::new(0));let(a,ca)=q(4,0x400,&d);let(b,cb)=q(65,0x650,&d);
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x400,[2,8]),(65,0x650,[3,9])])),async_compute_queues:vec![a,b],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.async_compute_queues[0].queue.compute.0=1;r.async_compute_queues[1].queue.compute.0=1;
 SCANS.with(|p|p.set(0));r.service_async_compute_completion_masks(qid_mask(65)).unwrap();assert_eq!(SCANS.with(|p|p.get()),1);assert_eq!(ca.signals.load(Ordering::SeqCst),0);assert_eq!(cb.signals.load(Ordering::SeqCst),1);
 r.service_async_compute_completion_masks([0;2]).unwrap();assert_eq!(SCANS.with(|p|p.get()),2);assert_eq!(ca.signals.load(Ordering::SeqCst),1);
}
#[test]fn coalesced_two_client_event_retires_both_and_visibility_retry_stays_armed(){
 let d=Arc::new(AtomicUsize::new(0));let(a,ca)=q(4,0x400,&d);let(b,cb)=q(5,0x500,&d);
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x400,[2,8]),(5,0x500,[3,9])])),async_compute_queues:vec![a,b],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.async_compute_queues[0].queue.compute.0=1;
 let mut both=qid_mask(4);both[0]|=qid_mask(5)[0];
 r.service_async_compute_completions_for_event(both).unwrap();
 assert_eq!(ca.signals.load(Ordering::SeqCst),1);assert_eq!(cb.signals.load(Ordering::SeqCst),0);assert!(r.async_compute_queues[1].inflight.is_some());
 r.async_compute_queues[1].queue.compute.0=1;r.service_async_compute_completions_for_event(qid_mask(5)).unwrap();assert_eq!(cb.signals.load(Ordering::SeqCst),1);
}
#[test]fn identityless_watchdog_fallback_still_covers_every_active_queue_in_one_scan(){
 let d=Arc::new(AtomicUsize::new(0));let(a,_)=q(4,0x400,&d);let(b,_)=q(65,0x650,&d);
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![])),async_compute_queues:vec![a,b],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 SCANS.with(|p|p.set(0));r.service_async_compute_completions().unwrap();assert_eq!(SCANS.with(|p|p.get()),1);
}
#[test]fn full_active_capacity_event_does_one_ring_scan_not_fifty_eight(){
 let d=Arc::new(AtomicUsize::new(0));let mut queues=Vec::new();
 for qid in 4u8..62 {queues.push(q(qid as u64,(qid as u64)<<12,&d).0);}
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![])),async_compute_queues:queues,session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 SCANS.with(|p|p.set(0));r.service_async_compute_completions_for_event(qid_mask(37)).unwrap();assert_eq!(SCANS.with(|p|p.get()),1);
 SCANS.with(|p|p.set(0));r.service_async_compute_completions().unwrap();assert_eq!(SCANS.with(|p|p.get()),1);
}
#[test]fn one_coalesced_batch_retires_fifty_eight_clients_with_one_ring_scan(){
 let d=Arc::new(AtomicUsize::new(0));let mut queues=Vec::new();let mut completions=Vec::new();let mut records=Vec::new();let mut masks=[0u64;2];
 for qid in 4u8..62 {let descriptor=(qid as u64)<<12;let(queue,completion)=q(qid as u64,descriptor,&d);queues.push(queue);completions.push(completion);records.push((qid,descriptor,[u64::from(qid),u64::from(qid)+100]));masks[0]|=1u64<<qid;}
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(records)),async_compute_queues:queues,session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 for queue in &mut r.async_compute_queues {queue.queue.compute.0=1;}
 SCANS.with(|p|p.set(0));r.service_async_compute_completions_for_event(masks).unwrap();
 assert_eq!(SCANS.with(|p|p.get()),1);assert!(completions.iter().all(|completion|completion.signals.load(Ordering::SeqCst)==1));assert!(r.async_compute_queues.iter().all(|queue|queue.inflight.is_none()));
}
#[test]fn publication_after_acquired_snapshot_waits_for_the_next_event(){
 let d=Arc::new(AtomicUsize::new(0));let(queue,completion)=q(4,0x400,&d);let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![])),async_compute_queues:vec![queue],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.async_compute_queues[0].queue.compute.0=1;r.manager.as_mut().unwrap().resources.publish_after_scan=Some((4,0x400,[2,8]));
 SCANS.with(|p|p.set(0));r.service_async_compute_completions_for_event(qid_mask(4)).unwrap();assert_eq!(completion.signals.load(Ordering::SeqCst),0);assert!(r.async_compute_queues[0].inflight.is_some());
 r.service_async_compute_completions_for_event(qid_mask(4)).unwrap();assert_eq!(SCANS.with(|p|p.get()),2);assert_eq!(completion.signals.load(Ordering::SeqCst),1);
}
#[test]fn unrelated_event_with_no_active_target_does_not_scan_the_ring(){
 let d=Arc::new(AtomicUsize::new(0));let(queue,_)=q(4,0x400,&d);let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![])),async_compute_queues:vec![queue],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 SCANS.with(|p|p.set(0));r.service_async_compute_completions_for_event(qid_mask(65)).unwrap();assert_eq!(SCANS.with(|p|p.get()),0);
}
#[test]fn early_notification_remains_armed_until_later_record(){
 let d=Arc::new(AtomicUsize::new(0));let(q,c)=q(1,0x100,&d);let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![])),async_compute_queues:vec![q],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.service_async_compute_completions().unwrap();assert_eq!(c.signals.load(Ordering::SeqCst),0);
 r.manager.as_mut().unwrap().resources.records.push((1,0x100,[2,8]));r.async_compute_queues[0].queue.compute.0=1;
 r.service_async_compute_completions().unwrap();assert_eq!(c.signals.load(Ordering::SeqCst),1);
}
#[test]fn timeout_retires_already_completed_work_without_quarantine(){
 let d=Arc::new(AtomicUsize::new(0));let(q,c)=q(1,0x100,&d);let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(1,0x100,[2,8])])),async_compute_queues:vec![q],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.async_compute_queues[0].queue.compute.0=1;
 assert!(!r.quarantine_async_compute(1,1,110));assert!(!r.async_compute_queues[0].quarantined);assert!(!r.session_recovery_pending);assert_eq!(c.signals.load(Ordering::SeqCst),1);assert_eq!(c.error.load(Ordering::SeqCst),0);
}
#[test]fn timeout_diagnostics_select_exact_owner_once_after_last_retirement_attempt(){
 TIMEOUTS.with(|t|t.borrow_mut().clear());
 let d=Arc::new(AtomicUsize::new(0));let(a,_)=q(4,0x400,&d);let(b,_)=q(7,0x700,&d);
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x400,[2,8])])),async_compute_queues:vec![a,b],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.async_compute_queues[0].queue.compute.0=1;
 assert!(!r.quarantine_async_compute(7,4,110)); // stale/foreign submission
 assert!(!r.quarantine_async_compute(4,4,110)); // successful final DRAM poll
 assert!(r.quarantine_async_compute(7,7,110));
 assert!(r.quarantine_async_compute(7,7,110)); // duplicate timer, no second dump
 TIMEOUTS.with(|t|assert_eq!(*t.borrow(),vec![0x700]));
 assert_eq!(d.load(Ordering::SeqCst),1); // only the completed QID4 packet dropped
 assert!(r.async_compute_queues[1].inflight.is_some());
}
'''
source += r'''
fn previous(q:&mut Queue,d:&Arc<AtomicUsize>,publication:Option<(u64,u64)>){
 let epoch=publication.map(|(_,epoch)|epoch).unwrap_or_else(||RECOVERY.with(|r|r.get().0));
 if let Some(active)=q.inflight.back_mut(){active.retirement.as_mut().unwrap().recovery_epoch=epoch;}
 q.previous_binding=Some(Previous{_binding:Some(Hold(d.clone())),_context:Hold(d.clone()),publication});
}
#[test]fn rebind_waits_for_exact_submission_and_final_stamp_without_blocking_other_queue(){
 RECOVERY.with(|r|r.set((7,0)));
 let current=Arc::new(AtomicUsize::new(0));let old=Arc::new(AtomicUsize::new(0));
 let(mut a,ca)=q(4,0x400,&current);let(mut b,cb)=q(5,0x500,&current);
 previous(&mut a,&old,Some((4,7)));b.queue.compute.0=1;
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x4010,[2,8]),(5,0x500,[3,9])])),async_compute_queues:vec![a,b],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.service_async_compute_completions().unwrap();assert_eq!(old.load(Ordering::SeqCst),0);assert_eq!(cb.signals.load(Ordering::SeqCst),1);assert_eq!(ca.signals.load(Ordering::SeqCst),0);
 r.manager.as_mut().unwrap().resources.records.push((4,0x400,[4,10]));
 r.service_async_compute_completions().unwrap();assert_eq!(old.load(Ordering::SeqCst),0);
 r.async_compute_queues[0].queue.compute.0=1;r.service_async_compute_completions().unwrap();
 // Two leases dropped: queue 5's at its first-pass retirement, queue 4's now.
 assert_eq!(old.load(Ordering::SeqCst),2);assert_eq!(current.load(Ordering::SeqCst),2);assert!(r.async_compute_queues[0].binding.is_some());assert_eq!(ca.signals.load(Ordering::SeqCst),1);
 RECOVERY.with(|r|r.set((0,0)));
}
#[test]fn rebind_rejects_wrong_publication_but_survives_a_closed_recovery(){
 // a9502decf205a: a closed recovery that moved the epoch retires normally and
 // releases the superseded binding; an open one leaves the job armed.
 {RECOVERY.with(|r|r.set((8,0)));let current=Arc::new(AtomicUsize::new(0));let old=Arc::new(AtomicUsize::new(0));let(mut a,c)=q(4,0x400,&current);a.queue.compute.0=1;previous(&mut a,&old,Some((4,7)));
  let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x400,[2,8])])),async_compute_queues:vec![a],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
  r.service_async_compute_completions().unwrap();assert_eq!(c.signals.load(Ordering::SeqCst),1);assert_eq!(c.error.load(Ordering::SeqCst),0);assert_eq!(old.load(Ordering::SeqCst),2);assert!(!r.async_compute_queues[0].quarantined);}
 {RECOVERY.with(|r|r.set((7,1)));let current=Arc::new(AtomicUsize::new(0));let old=Arc::new(AtomicUsize::new(0));let(mut a,c)=q(4,0x400,&current);a.queue.compute.0=1;previous(&mut a,&old,Some((4,7)));
  let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x400,[2,8])])),async_compute_queues:vec![a],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
  r.service_async_compute_completions().unwrap();assert_eq!(c.signals.load(Ordering::SeqCst),0);assert_eq!(old.load(Ordering::SeqCst),0);assert!(!r.async_compute_queues[0].quarantined);assert!(r.async_compute_queues[0].inflight.is_some());}
 for (publication,handshake) in [(Some((99,7)),(7,0)),(None,(7,0))] {
  RECOVERY.with(|r|r.set(handshake));let current=Arc::new(AtomicUsize::new(0));let old=Arc::new(AtomicUsize::new(0));let(mut a,c)=q(4,0x400,&current);a.queue.compute.0=1;previous(&mut a,&old,publication);
  let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x400,[2,8])])),async_compute_queues:vec![a],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
  r.service_async_compute_completions().unwrap();assert_eq!(old.load(Ordering::SeqCst),0);assert_eq!(current.load(Ordering::SeqCst),0);assert!(r.async_compute_queues[0].quarantined);assert_eq!(c.error.load(Ordering::SeqCst),1);
  r.async_compute_queues.clear();assert_eq!(old.load(Ordering::SeqCst),2);assert_eq!(current.load(Ordering::SeqCst),3);
 }
 RECOVERY.with(|r|r.set((0,0)));
}
#[test]fn zero_duration_rebind_keeps_both_bindings(){
 let current=Arc::new(AtomicUsize::new(0));let old=Arc::new(AtomicUsize::new(0));let(mut a,_)=q(4,0x400,&current);a.queue.compute.0=1;previous(&mut a,&old,Some((4,0)));
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x400,[8,8])])),async_compute_queues:vec![a],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.service_async_compute_completions().unwrap();assert_eq!(old.load(Ordering::SeqCst),0);assert_eq!(current.load(Ordering::SeqCst),0);assert!(r.async_compute_queues[0].previous_binding.is_some());
}
#[test]fn released_unpublished_rebind_restores_old_binding(){
 let current=Arc::new(AtomicUsize::new(0));let old=Arc::new(AtomicUsize::new(0));let(mut a,_)=q(4,0x400,&current);a.inflight=Inflight::new();previous(&mut a,&old,None);
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![])),async_compute_queues:vec![a],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.release_async_compute_queue(4);assert_eq!(current.load(Ordering::SeqCst),3);assert_eq!(old.load(Ordering::SeqCst),0);assert!(r.async_compute_queues[0].previous_binding.is_none());assert!(r.async_compute_queues[0].owner.is_none());assert!(r.async_compute_queues[0].binding.is_some());
}
'''
source += r'''
/// Several kicks of ONE queue in flight: publication never waited for a
/// retirement, so the host holds them oldest first.
fn stacked(owner:u64,descriptors:&[u64],d:&Arc<AtomicUsize>)->(Queue,Vec<Arc<Completion>>){
 let(mut queue,first)=q(owner,descriptors[0],d);let mut all=vec![first];
 queue.queue.producer.submitted=descriptors.len() as u32;queue.queue.completion_descriptor=*descriptors.last().unwrap();
 // Ring of 32 item records, four per kick; the host producer is past all of them.
 queue.queue.compute.2=(0,4*descriptors.len() as u32,32);
 queue.inflight.iter_mut().next().unwrap().retirement.as_mut().unwrap().firmware.item_producer=4;
 queue.inflight.iter_mut().next().unwrap().retirement.as_mut().unwrap().firmware.item_count=32;
 for (k,descriptor) in descriptors.iter().enumerate().skip(1){
  let c=Arc::new(Completion{signals:AtomicUsize::new(0),error:AtomicUsize::new(0),status:Arc::new(g17_status::VmStatus),retired:AtomicUsize::new(0)});
  let id=owner*100+k as u64;
  DESC_TS.with(|m|m.borrow_mut().insert((owner as u8,*descriptor),1+k as u64));
  let firmware=G17PComputeRetirementTicket{tsq_node:*descriptor,queue_id:owner as u8,descriptor:*descriptor,kick_timestamp:1+k as u64,submitted_target:1+k as u32,item_producer:4*(1+k as u32),item_count:32};
  let retirement=G17PAsyncComputeRetirementTicket{owner,submission:id,context:owner as u32,recovery_epoch:0,firmware};
  if queue.inflight.push(Active{_work_state:g17_resources::G17PWorkStateLease{node:*descriptor,counters:[3,1]},packet:Packet{id,context:Context{context_id:owner as u32},completion:c.clone()},_lease:Hold(d.clone()),retirement:Some(retirement),observed:None}).is_err(){unreachable!()}
  all.push(c);
 }
 (queue,all)
}
#[test]fn stacked_kicks_retire_in_order_from_one_batch(){
 let d=Arc::new(AtomicUsize::new(0));let(mut queue,c)=stacked(4,&[0x400,0x600,0x800],&d);
 // All three completed; the newest kick's processing stored its stamp.
 queue.queue.compute.0=3;queue.queue.compute.1=[3,3];queue.queue.compute.2 .0=12;
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x400,[2,8]),(4,0x600,[9,15]),(4,0x800,[16,20])])),async_compute_queues:vec![queue],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 SCANS.with(|p|p.set(0));r.service_async_compute_completions_for_event(qid_mask(4)).unwrap();assert_eq!(SCANS.with(|p|p.get()),1);
 assert!(c.iter().all(|c|c.signals.load(Ordering::SeqCst)==1&&c.error.load(Ordering::SeqCst)==0));
 assert!(r.async_compute_queues[0].inflight.is_none());assert_eq!(r.async_compute_queues[0].queue.producer.submitted,0);
 assert_eq!(r.async_compute_queues[0].queue.completion_last_end,20);assert_eq!(r.async_compute_queues[0].queue.completion_descriptor,0);
 assert!(r.async_compute_queues[0].retirement_ready);
}
#[test]fn older_kick_retires_while_newer_one_stays_armed(){
 let d=Arc::new(AtomicUsize::new(0));let(mut queue,c)=stacked(4,&[0x400,0x600],&d);
 // Firmware completed only the first; the second's records are consumed, it has not run.
 queue.queue.compute.0=1;queue.queue.compute.1=[2,1];queue.queue.compute.2 .0=8;
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x400,[2,8])])),async_compute_queues:vec![queue],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.service_async_compute_completions().unwrap();
 assert_eq!(c[0].signals.load(Ordering::SeqCst),1);assert_eq!(c[1].signals.load(Ordering::SeqCst),0);
 assert_eq!(r.async_compute_queues[0].inflight.len(),1);assert_eq!(r.async_compute_queues[0].queue.producer.submitted,1);
 assert_eq!(r.async_compute_queues[0].queue.completion_descriptor,0x600,"newest stays armed");assert!(!r.async_compute_queues[0].retirement_ready);
 r.manager.as_mut().unwrap().resources.records.push((4,0x600,[9,15]));r.async_compute_queues[0].queue.compute.1=[2,2];r.async_compute_queues[0].queue.compute.0=2;
 r.service_async_compute_completions().unwrap();assert_eq!(c[1].signals.load(Ordering::SeqCst),1);assert!(r.async_compute_queues[0].inflight.is_none());
}
#[test]fn a_later_record_never_retires_an_earlier_unfinished_kick(){
 let d=Arc::new(AtomicUsize::new(0));let(mut queue,c)=stacked(4,&[0x400,0x600],&d);
 // Firmware has not finished the older kick (completed 0): the newer
 // kick's record must not retire anything.
 queue.queue.compute.0=2;queue.queue.compute.1=[2,0];queue.queue.compute.2 .0=0;
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x600,[9,15])])),async_compute_queues:vec![queue],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.service_async_compute_completions().unwrap();
 assert!(c.iter().all(|c|c.signals.load(Ordering::SeqCst)==0));assert_eq!(r.async_compute_queues[0].inflight.len(),2);
 assert!(r.async_compute_queues[0].inflight.iter().nth(1).unwrap().observed.is_some(),"the newer record is latched anyway");
}
#[test]fn a_lost_older_record_retires_after_one_pass_bracketed_by_the_newer_record(){
 // Neo 2026-09-26 19:56 shape: both kicks finished (completed == target,
 // item ring drained, stamp stored) but the older kick's ring record was
 // overwritten before any scan latched it.
 let d=Arc::new(AtomicUsize::new(0));let(mut queue,c)=stacked(4,&[0x400,0x600],&d);
 queue.queue.completion_last_end=5;
 queue.queue.compute.0=2;queue.queue.compute.1=[2,2];queue.queue.compute.2 .0=8;
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x600,[9,15])])),async_compute_queues:vec![queue],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 // Its own event: one pass of grace in case the record only trails the event.
 r.service_async_compute_completions_for_event(qid_mask(4)).unwrap();
 assert!(c.iter().all(|c|c.signals.load(Ordering::SeqCst)==0));assert!(r.async_compute_queues[0].retire_pending);
 // Any later event, even for another QID, retires both: the older on counters
 // and stamp (end bracketed by the newer record), the newer with its record.
 r.service_async_compute_completions_for_event(qid_mask(9)).unwrap();
 assert!(c.iter().all(|c|c.signals.load(Ordering::SeqCst)==1&&c.error.load(Ordering::SeqCst)==0));
 let q=&r.async_compute_queues[0];assert!(q.inflight.is_none());assert!(!q.retire_pending);assert!(!q.quarantined);
 assert_eq!(q.queue.completion_last_end,15,"only a real record advances the floor");
 assert!(G17P_COMPUTE_RECORDLESS_RETIRES.load(core::sync::atomic::Ordering::Relaxed)>=1);
}
#[test]fn the_1956_timeout_shape_retires_on_the_final_observation_instead_of_quarantining(){
 let d=Arc::new(AtomicUsize::new(0));let(mut queue,c)=stacked(13,&[0x2d24000,0x2d28000],&d);
 queue.queue.completion_last_end=0xf8b1a066;
 queue.queue.compute.0=2;queue.queue.compute.1=[2,2];queue.queue.compute.2 .0=8;
 // Every record of both kicks was overwritten before the host looked.
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(13,0x2d20000,[0xf8b19efd,0xf8b1a066])])),async_compute_queues:vec![queue],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 assert!(!r.quarantine_async_compute(13,1301,110),"the final DRAM observation retired it");
 assert!(c.iter().all(|c|c.signals.load(Ordering::SeqCst)==1&&c.error.load(Ordering::SeqCst)==0));
 let q=&r.async_compute_queues[0];assert!(!q.quarantined);assert!(q.inflight.is_none());assert!(!r.session_recovery_pending);
 assert_eq!(q.queue.completion_last_end,0xf8b1a066);
}
#[test]fn a_recordless_finish_across_a_recovery_fails_submission_scoped(){
 let d=Arc::new(AtomicUsize::new(0));let(mut queue,c)=stacked(4,&[0x400,0x600],&d);
 queue.queue.compute.0=2;queue.queue.compute.1=[2,2];queue.queue.compute.2 .0=8;
 RECOVERY.with(|h|h.set((1,0)));
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![])),async_compute_queues:vec![queue],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.service_async_compute_completions().unwrap();
 RECOVERY.with(|h|h.set((0,0)));
 assert!(c.iter().all(|c|c.error.load(Ordering::SeqCst)==1));assert!(r.async_compute_queues[0].quarantined);assert!(!r.session_recovery_pending);
}
#[test]fn aggregate_or_obsolete_counters_do_not_gate_individual_completion(){
 let d=Arc::new(AtomicUsize::new(0));let(mut queue,c)=stacked(4,&[0x400,0x600],&d);
 // Other engines may still own this node, and the obsolete per-QID counters
 // are unrelated. Exact stamps, records and item consumption prove each CL.
 queue.queue.compute.0=2;queue.queue.compute.1=[200,0];queue.queue.compute.2 .0=8;
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x400,[2,8]),(4,0x600,[9,15])])),async_compute_queues:vec![queue],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.service_async_compute_completions_for_event(qid_mask(4)).unwrap();
 assert!(c.iter().all(|c|c.signals.load(Ordering::SeqCst)==1));
 assert!(r.async_compute_queues[0].inflight.is_none());
}
#[test]fn unconsumed_item_records_keep_a_kick_armed(){
 for (completed,consumer) in [(1u32,4u32),(2,4)] {
  let d=Arc::new(AtomicUsize::new(0));let(mut queue,c)=stacked(4,&[0x400,0x600],&d);
  queue.queue.compute.0=2;queue.queue.compute.1=[2,completed];queue.queue.compute.2 .0=consumer;
  let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x400,[2,8]),(4,0x600,[9,15])])),async_compute_queues:vec![queue],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
  r.service_async_compute_completions().unwrap();
  assert_eq!(c[0].signals.load(Ordering::SeqCst),1);assert_eq!(c[1].signals.load(Ordering::SeqCst),0,"completed={} consumer={}",completed,consumer);
 }
}
#[test]fn missing_final_stamp_keeps_every_stacked_kick_armed(){
 let d=Arc::new(AtomicUsize::new(0));let(mut queue,c)=stacked(4,&[0x400,0x600],&d);
 queue.queue.compute.1=[2,2];queue.queue.compute.2 .0=8;
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x400,[2,8]),(4,0x600,[9,15])])),async_compute_queues:vec![queue],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.service_async_compute_completions().unwrap();assert!(c.iter().all(|c|c.signals.load(Ordering::SeqCst)==0));
}
#[test]fn older_flush_does_not_release_completed_but_unflushed_successor(){
 let d=Arc::new(AtomicUsize::new(0));let(mut queue,c)=stacked(4,&[0x400,0x600],&d);
 // Both counters/records completed, but cache maintenance covers only kick1.
 queue.queue.compute.0=1;queue.queue.compute.1=[2,2];queue.queue.compute.2 .0=8;
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x400,[2,8]),(4,0x600,[9,15])])),async_compute_queues:vec![queue],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.service_async_compute_completions().unwrap();
 assert_eq!(c[0].signals.load(Ordering::SeqCst),1);assert_eq!(c[1].signals.load(Ordering::SeqCst),0);
 assert_eq!(r.async_compute_queues[0].inflight.len(),1);
 r.async_compute_queues[0].queue.compute.0=2;r.service_async_compute_completions().unwrap();
 assert_eq!(c[1].signals.load(Ordering::SeqCst),1);assert!(r.async_compute_queues[0].inflight.is_none());
}
#[test]fn quarantine_fails_every_stacked_kick_and_retains_them(){
 let d=Arc::new(AtomicUsize::new(0));let(queue,c)=stacked(4,&[0x400,0x600,0x800],&d);
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![])),async_compute_queues:vec![queue],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 assert!(r.quarantine_async_compute(4,401,110));
 assert!(c.iter().all(|c|c.error.load(Ordering::SeqCst)==1),"older and newer kicks fail with the timed-out one");
 assert!(r.async_compute_queues[0].quarantined);assert_eq!(r.async_compute_queues[0].inflight.len(),3);assert_eq!(d.load(Ordering::SeqCst),0);
}
#[test]fn zero_duration_front_fails_the_whole_queue_scoped(){
 let d=Arc::new(AtomicUsize::new(0));let(mut queue,c)=stacked(4,&[0x400,0x600],&d);queue.queue.compute.0=1;queue.queue.compute.1=[2,2];queue.queue.compute.2 .0=8;
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x400,[8,8])])),async_compute_queues:vec![queue],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.service_async_compute_completions().unwrap();
 assert!(c.iter().all(|c|c.error.load(Ordering::SeqCst)==1));assert!(r.async_compute_queues[0].quarantined);assert!(!r.session_recovery_pending);
}
#[test]fn zero_duration_quarantine_releases_stacked_packets_only_on_the_firmware_witness(){
 // recovery-leak Part A on the native-ordered pipeline: after a zero-duration
 // retire the queue is quarantined with every stacked packet (and its VM pin)
 // retained; the queue-local terminal words -- read against the NEWEST
 // ticket -- release them all at once, never earlier, and never the graph.
 let d=Arc::new(AtomicUsize::new(0));let(mut queue,c)=stacked(4,&[0x400,0x600],&d);
 queue.queue.compute.0=0;queue.queue.compute.1=[2,1];queue.queue.compute.2 .0=4;
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x400,[8,8])])),async_compute_queues:vec![queue],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.service_async_compute_completions().unwrap();
 {let q=&r.async_compute_queues[0];assert!(q.quarantined&&q.awaiting_retirement_witness);assert_eq!(q.inflight.len(),2);assert!(!r.session_recovery_pending);}
 assert!(c.iter().all(|c|c.error.load(Ordering::SeqCst)==1&&c.retired.load(Ordering::SeqCst)==0));
 // Stamp present but firmware completed only the front kick: still pinned.
 {let q=&mut r.async_compute_queues[0];q.queue.compute.0=1;q.queue.compute.1=[2,1];q.queue.compute.2 .0=4;}
 r.release_async_compute_queue(4);
 {let q=&r.async_compute_queues[0];assert_eq!(q.inflight.len(),2);assert!(q.awaiting_retirement_witness);assert_eq!(q.retired_leases.len(),0);}
 assert!(c.iter().all(|c|c.retired.load(Ordering::SeqCst)==0));
 // completed == the newest target and the item ring consumed to the newest
 // producer: every packet and VM pin goes; leases, binding, context and the
 // quarantine stay exactly as they were.
 {let q=&mut r.async_compute_queues[0];q.queue.compute.0=2;q.queue.compute.1=[2,2];q.queue.compute.2 .0=8;}
 r.service_async_compute_completions().unwrap();
 let q=&r.async_compute_queues[0];assert!(q.inflight.is_none());assert!(!q.awaiting_retirement_witness);assert!(q.quarantined);
 assert_eq!(q.retired_leases.len(),2);assert!(q.binding.is_some()&&q.context.is_some());
 assert!(c.iter().all(|c|c.retired.load(Ordering::SeqCst)==1&&c.signals.load(Ordering::SeqCst)==1));
 assert_eq!(d.load(Ordering::SeqCst),0,"no lease, binding or context dropped");
}
#[test]fn witness_is_refused_while_a_firmware_recovery_is_open(){
 let d=Arc::new(AtomicUsize::new(0));let(mut queue,c)=stacked(4,&[0x400,0x600],&d);
 queue.queue.compute.0=0;queue.queue.compute.1=[2,2];queue.queue.compute.2 .0=8;
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![(4,0x400,[8,8])])),async_compute_queues:vec![queue],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 r.service_async_compute_completions().unwrap();assert!(r.async_compute_queues[0].awaiting_retirement_witness);
 r.async_compute_queues[0].queue.compute.0=2;
 RECOVERY.with(|h|h.set((0,1)));r.release_async_compute_queue(4);
 assert_eq!(r.async_compute_queues[0].inflight.len(),2);assert!(c.iter().all(|c|c.retired.load(Ordering::SeqCst)==0));
 RECOVERY.with(|h|h.set((0,0)));r.release_async_compute_queue(4);
 assert!(r.async_compute_queues[0].inflight.is_none());assert!(c.iter().all(|c|c.retired.load(Ordering::SeqCst)==1));
}
#[test]fn teardown_cancels_any_stacked_kick_and_retires_it_exactly(){
 let d=Arc::new(AtomicUsize::new(0));let(queue,c)=stacked(4,&[0x400,0x600],&d);
 let mut r=G17PLiveRuntime{pdev:None,manager:Some(manager(vec![])),async_compute_queues:vec![queue],session_recovery_pending:false,deferred_completions:Vec::new(),deferred_completion_drain_owned:false,gfx_state:None};
 assert!(r.cancel_async_compute(4,401));assert!(r.cancel_async_compute(4,4));
 assert!(c.iter().all(|c|c.error.load(Ordering::SeqCst)==1));assert!(!r.async_compute_queues[0].quarantined);
 r.release_async_compute_queue(4);assert_eq!(r.async_compute_queues[0].owner,Some(4));
 { let queue=&mut r.async_compute_queues[0];queue.queue.compute.0=2;queue.queue.compute.1=[2,2];queue.queue.compute.2 .0=8; }
 r.manager.as_mut().unwrap().resources.records.extend([(4,0x400,[2,8]),(4,0x600,[9,15])]);
 r.service_async_compute_completion_masks([0,0]).unwrap();
 assert!(c.iter().all(|c|c.retired.load(Ordering::SeqCst)==1));assert!(r.async_compute_queues[0].inflight.is_none());assert_eq!(r.async_compute_queues[0].owner,None);
}
'''
with tempfile.TemporaryDirectory() as d:
 f=Path(d)/'test.rs';f.write_text(with_g17_config(source));exe=Path(d)/'test'
 subprocess.run(['rustc','--edition=2021','--test',str(f),'-o',str(exe)],check=True)
 if negative_control:
  result=subprocess.run([str(exe),'older_flush_does_not_release_completed_but_unflushed_successor','--exact','--test-threads=1'],capture_output=True,text=True)
  assert result.returncode != 0 and '1 failed' in result.stdout, result.stdout+result.stderr
  print('PASS negative control: historical nonzero stamp incorrectly releases an unflushed successor')
 else:
  subprocess.run([str(exe),'--test-threads=1'],check=True)
print('PASS production async retirement: per-descriptor routing, final stamp, out-of-order completion, close during execution, timeout/invalid timestamp quarantine, quarantined packets released only on the firmware retirement witness; no hardware claim')
