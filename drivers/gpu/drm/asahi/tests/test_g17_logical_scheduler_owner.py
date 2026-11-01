#!/usr/bin/env python3
"""Exercise production logical QoS leases, graph retargets and bootstrap isolation."""
from pathlib import Path
import subprocess
import tempfile
import re
from g17_config_test_support import with_g17_config

root = Path(__file__).resolve().parents[1]
resources = (root / "g17_resources.rs").read_text()
manager = (root / "g17_manager.rs").read_text()
submission = (root / "g17_submission.rs").read_text()


def item(source, marker):
    start = source.index(marker)
    end = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


lease = "\n".join(item(resources, marker) for marker in [
    "pub(crate) struct G17PLogicalQosLease {",
    "impl G17PLogicalQosLease {",
    "impl Drop for G17PLogicalQosLease {",
])
context = "\n".join(item(resources, marker) for marker in [
    "pub(crate) struct G17PLogicalQueueContext {",
    "impl G17PLogicalQueueContext {",
    "impl core::ops::Deref for G17PLogicalQueueContext {",
])
# This harness models backing addresses, not firmware-mutated memory reads.
context = context.replace(item(resources, "    fn tsq_head_snapshot("), "")
retarget = item(resources, "    pub(crate) fn retarget_command_context(")
compute_retarget = item(resources, "    pub(crate) fn retarget_channel_control(")
bind = item(manager, "    pub(crate) fn bind_logical_channel_control(")
b2 = item(manager, "    fn publish_compute_b2_once(")
qos_id = item(resources, "    pub(crate) fn qos_hardware_buffer_id(")

# Ensure retained production staging, not just first construction, supplies
# the logical pointer/ID while PB identity remains a separate field.
optional = item(resources, "    fn optional_addresses(")
assert "addresses.channel_control = control.gpu_va()" in optional
assert "self.ta_hardware_buffer_id," in optional
# v92d: both stages' config updates carry the logical QoS HardwareBufferID
# through final_config_update (reached via prepare_render_items); late tiling
# passes it directly.
assert "self.qos_hardware_buffer_id()?" in item(resources, "    fn final_config_update(")
assert "self.prepare_render_items(ordinal)?" in item(resources, "    fn stage_fragment_config_group(")
assert "self.qos_hardware_buffer_id()?" in item(resources, "    pub(crate) fn prepare_late_tiling_publication_with_stamp(")
# v91 WORD_STORES: the +0x52 QoS-update word is part of the aligned +0x50 store.
assert "((u64::from(fields.qos_update) & 0xffffffff) << 16)" in resources
assert "qos_update: u32::from(ordinal == 0 || qos_update) }" in resources
assert "self.descriptor_context_id, self.context_update_pending, self.qos_update_pending," in resources
prepare = item(manager, "    pub(crate) fn prepare_logical_queue_context(")
assert "G17PLogicalQueueContext::new(" in prepare
assert "execution, control, qos, owner_pid" in prepare
assert "channel_control: Option<Arc<g17_resources::G17PClChannelControl>>" in item(manager, "struct G17PRenderSlotState {")
assert "logical_channel_controls.push" not in manager, "retained pages must be bounded by graph owners"

rust = r'''
#![allow(dead_code, unused_variables)]
use std::{cell::Cell,ops::Deref};
type Result<T=()>=std::result::Result<T,i32>;
const EIO:i32=5;const EINVAL:i32=22;const EFAULT:i32=14;const EOVERFLOW:i32=75;const ENOSPC:i32=28;const GFP_KERNEL:u32=0;
thread_local!{static FAIL_ALLOC:Cell<bool>=const{Cell::new(false)};}
struct Arc<T>(std::sync::Arc<T>);
impl<T> Clone for Arc<T>{fn clone(&self)->Self{Self(self.0.clone())}}
impl<T> Deref for Arc<T>{type Target=T;fn deref(&self)->&T{&self.0}}
impl<T> Arc<T>{fn new(value:T,_flags:u32)->Result<Self>{if FAIL_ALLOC.with(|f|f.replace(false)){Err(EIO)}else{Ok(Self(std::sync::Arc::new(value)))}}
 fn pin_init(value:T,flags:u32)->Result<Self>{Self::new(value,flags)}
 fn ptr_eq(a:&Self,b:&Self)->bool{std::sync::Arc::ptr_eq(&a.0,&b.0)}}
mod kernel{pub mod sync{pub struct Mutex<T>(pub std::sync::Mutex<T>);impl<T> Mutex<T>{pub fn lock(&self)->std::sync::MutexGuard<'_,T>{self.0.lock().unwrap()}}}
 macro_rules! new_mutex{($v:expr,$n:expr)=>{$crate::kernel::sync::Mutex(std::sync::Mutex::new($v))}}
 pub(crate) use new_mutex;}
// Shared FList: a logical context owns its render FList pool(s).
struct KVec<T>(Vec<T>);
impl<T> KVec<T>{fn with_capacity(n:usize,_:u32)->Result<Self>{Ok(Self(Vec::with_capacity(n)))}fn push(&mut self,v:T,_:u32)->Result{self.0.push(v);Ok(())}}
impl<T> Deref for KVec<T>{type Target=Vec<T>;fn deref(&self)->&Vec<T>{&self.0}}
impl<T> std::ops::DerefMut for KVec<T>{fn deref_mut(&mut self)->&mut Vec<T>{&mut self.0}}
struct AsahiDevice;
thread_local!{static NEXT_POOL:Cell<u16>=const{Cell::new(129)};}
struct G17PRenderFlist{id:u16}
impl G17PRenderFlist{fn new(_:&AsahiDevice,_:&mmu::Uat,_:&mmu::Vm)->Result<Self>{Ok(Self{id:NEXT_POOL.with(|n|{let id=n.get();n.set(id+1);id})})}
 fn id(&self)->u16{self.id} fn control_shared_va(&self)->u64{u64::from(self.id)<<20}}
mod g17_host_path{pub const SHARED_FLIST:u32=1<<19;
 thread_local!{pub static MASK:std::cell::Cell<u32>=const{std::cell::Cell::new(SHARED_FLIST)};}
 pub fn enabled(bit:u32)->bool{MASK.with(|m|m.get()&bit!=0)}}
mod mmu{pub const UAT_PGSZ:usize=0x4000;
 pub struct Vm{pub id:u64}
 pub struct T8140ComputeExecutionContext{pub vm:Vm,pub id:u32,pub current:bool}
 pub struct Uat;
 impl T8140ComputeExecutionContext{pub fn vm(&self)->&Vm{&self.vm}pub fn context_id(&self)->u32{self.id}pub fn matches(&self,vm:&Vm)->bool{self.vm.id==vm.id}pub fn is_current(&self)->bool{self.current}}
}
struct G17PWorkStateStorage{storage:TestAddress}
struct TestAddress(u64);impl TestAddress{fn iova(&self)->u64{self.0}}
struct G17PClChannelControl{address:u64,firmware_byte:Cell<u8>,published:Cell<bool>}
impl G17PClChannelControl{fn gpu_va(&self)->u64{self.address}fn mark_published(&self){self.published.set(true)}}
''' + lease + context + r'''
mod g17_resources{pub(super) use super::{G17PLogicalQueueContext,G17PLogicalQosLease,G17PClChannelControl};}
struct Bytes{raw:Vec<u8>,fail:bool}
impl Bytes{fn with_bytes_mut<T>(&mut self,f:impl FnOnce(&mut[u8])->Result<T>)->Result<T>{if self.fail{return Err(EIO)}f(&mut self.raw)}}
fn put_u64(raw:&mut[u8],offset:usize,value:u64){raw[offset..offset+8].copy_from_slice(&value.to_le_bytes());}
fn put_u32(raw:&mut[u8],offset:usize,value:u32){raw[offset..offset+4].copy_from_slice(&value.to_le_bytes());}
fn pid(raw:&[u8],offset:usize)->u32{u32::from_le_bytes(raw[offset..offset+4].try_into().unwrap())}
fn word(raw:&[u8],offset:usize)->u64{u64::from_le_bytes(raw[offset..offset+8].try_into().unwrap())}
struct Compute{queue_graph:Bytes}
impl Compute{''' + compute_retarget + r'''}
struct Qos{queue_byte_32:u8}
struct Producer{qos:Qos}
struct G17PSksmQueue{channel_control_gpu_va:u64,logical_channel_control:Option<Arc<G17PClChannelControl>>,logical_qos:Option<Arc<G17PLogicalQosLease>>,compute:Compute,producer:Producer}
impl G17PSksmQueue{''' + bind + r'''}
enum G17PRenderCommandContext{Job(Arc<G17PLogicalQueueContext>)}
struct Pair(u8);impl Pair{fn slot(&self)->u8{self.0}}
struct Render{queue_pair:Pair,render_flist:Arc<G17PRenderFlist>,graph:Bytes,logical_channel_control:Option<Arc<G17PClChannelControl>>,logical_qos:Option<Arc<G17PLogicalQosLease>>,_command_context:Option<G17PRenderCommandContext>,descriptor_context_id:u16,render_root_slot:u8,context_update_pending:bool,qos_update_pending:bool,ta_hardware_buffer_id:u32,owner_pid:u32}
impl Render{
 fn descriptor_context_valid(id:u16)->bool{(5..63).contains(&id)}
 fn queue_record_offset(&self,tiling:bool)->usize{if tiling{0}else{0xc0}}
''' + retarget + qos_id + r'''}
struct B2Resources{raw:Vec<u8>,writes:usize}
impl B2Resources{fn stage_compute_region_views(&mut self)->Result{Ok(())}fn with_b2_computed_mut<T>(&mut self,f:impl FnOnce(&mut[u8])->Result<T>)->Result<T>{self.writes+=1;f(&mut self.raw)}}
struct Manager{resources:B2Resources,compute_b2_activation:g17_submission::G17PClB2ActivationState,compute_channel_control:Option<G17PClChannelControl>}
impl Manager{''' + b2 + r'''}
fn allocator()->Arc<kernel::sync::Mutex<u128>>{Arc::new(kernel::sync::Mutex(std::sync::Mutex::new(3u128)),0).unwrap()}
fn context(ids:&Arc<kernel::sync::Mutex<u128>>,id:u32,address:u64)->Arc<G17PLogicalQueueContext>{
 let root=Arc::new(mmu::T8140ComputeExecutionContext{vm:mmu::Vm{id:7},id,current:true},0).unwrap();
 let page=Arc::new(G17PClChannelControl{address,firmware_byte:Cell::new(0xff),published:Cell::new(false)},0).unwrap();
 let qos=G17PLogicalQosLease::new(ids.clone()).unwrap();
 Arc::new(G17PLogicalQueueContext::new(root,page,qos,0x10000+id,Arc::new(kernel::sync::Mutex(std::sync::Mutex::new(G17PWorkStateStorage{storage:TestAddress(address+0x10000)})),0).unwrap()).unwrap(),0).unwrap()
}
fn compute()->G17PSksmQueue{G17PSksmQueue{channel_control_gpu_va:0,logical_channel_control:None,logical_qos:None,compute:Compute{queue_graph:Bytes{raw:vec![0xa5;0x400],fail:false}},producer:Producer{qos:Qos{queue_byte_32:0}}}}
fn render()->Render{render_on(0)}
fn render_on(slot:u8)->Render{Render{queue_pair:Pair(slot),render_flist:Arc::new(G17PRenderFlist{id:0},0).unwrap(),graph:Bytes{raw:vec![0x5a;0x400],fail:false},logical_channel_control:None,logical_qos:None,_command_context:None,descriptor_context_id:0,render_root_slot:0,context_update_pending:false,qos_update_pending:false,ta_hardware_buffer_id:73,owner_pid:0}}
#[test]fn logical_cross_engine_identity_is_stable_and_pb_identity_is_separate(){
 let ids=allocator();let owner=context(&ids,9,0x12340000);let mut cl=compute();let mut r=render();let before=r.graph.raw.clone();
 cl.bind_logical_channel_control(&owner).unwrap();r.retarget_command_context(&AsahiDevice,&mmu::Uat,&mmu::Vm{id:7},owner.clone()).unwrap();
 assert_eq!(word(&cl.compute.queue_graph.raw,0x9c),0x12340000);
 for at in [0x9c,0xc0+0x9c]{assert_eq!(word(&r.graph.raw,at),0x12340000);}
 for at in [0x10,0xd0]{assert_eq!(word(&r.graph.raw,at),owner.tsq_head_gpu_va());}
 assert_eq!(word(&cl.compute.queue_graph.raw,0x10),owner.tsq_head_gpu_va());
 for i in 0..before.len(){if !(0x10..0x18).contains(&i)&&!(0xd0..0xd8).contains(&i)&&!(0x9c..0xa4).contains(&i)&&!(0x15c..0x164).contains(&i)&&!(0x48..0x4c).contains(&i)&&!(0x108..0x10c).contains(&i){assert_eq!(r.graph.raw[i],before[i]);}}
 assert_eq!(pid(&cl.compute.queue_graph.raw,0x48),owner.owner_pid);
 for at in [0x48,0x108]{assert_eq!(pid(&r.graph.raw,at),owner.owner_pid);}
 assert_eq!(r.owner_pid,owner.owner_pid);
 assert_eq!(r.qos_hardware_buffer_id().unwrap(),u16::from(cl.producer.qos.queue_byte_32));assert_eq!(r.ta_hardware_buffer_id,73);
 owner.channel_control.firmware_byte.set(17);cl.bind_logical_channel_control(&owner).unwrap();r.retarget_command_context(&AsahiDevice,&mmu::Uat,&mmu::Vm{id:7},owner.clone()).unwrap();assert_eq!(owner.channel_control.firmware_byte.get(),17);assert!(owner.channel_control.published.get());
 let other=context(&ids,10,0x56780000);assert_ne!(owner.qos.id(),other.qos.id());assert_ne!(owner.channel_control_gpu_va(),other.channel_control_gpu_va());
}
#[test]fn old_qos_credit_survives_each_engine_reference_but_not_inert_page_retention(){
 let ids=allocator();let owner=context(&ids,9,0x12340000);let id=owner.qos.id();let retained_page=owner.channel_control.clone();
 let mut cl=compute();let mut r=render();cl.bind_logical_channel_control(&owner).unwrap();r.retarget_command_context(&AsahiDevice,&mmu::Uat,&mmu::Vm{id:7},owner.clone()).unwrap();drop(owner);
 let next=context(&ids,10,0x56780000);cl.bind_logical_channel_control(&next).unwrap();assert_ne!(*ids.lock()&(1u128<<id),0);
 r.retarget_command_context(&AsahiDevice,&mmu::Uat,&mmu::Vm{id:7},next).unwrap();assert_eq!(*ids.lock()&(1u128<<id),0);assert_eq!(retained_page.gpu_va(),0x12340000);
}
#[test]fn lease_churn_exceeds_namespace_without_leaking_credits(){
 let ids=allocator();for _ in 0..1024{let lease=G17PLogicalQosLease::new(ids.clone()).unwrap();assert_eq!(lease.id(),2);drop(lease);}assert_eq!(*ids.lock(),3u128);
 let leases:(Vec<_>)=(0..126).map(|_|G17PLogicalQosLease::new(ids.clone()).unwrap()).collect();assert!(G17PLogicalQosLease::new(ids.clone()).is_err());drop(leases);assert_eq!(G17PLogicalQosLease::new(ids).unwrap().id(),2);
}
#[test]fn allocation_failure_returns_reserved_qos_credit(){
 let ids=allocator();FAIL_ALLOC.with(|f|f.set(true));assert!(G17PLogicalQosLease::new(ids.clone()).is_err());assert_eq!(*ids.lock(),3u128);
}
#[test]fn failed_graph_retarget_keeps_old_owner_and_firmware_bytes(){
 let ids=allocator();let old=context(&ids,9,0x11110000);let next=context(&ids,10,0x22220000);let mut cl=compute();let mut r=render();cl.bind_logical_channel_control(&old).unwrap();r.retarget_command_context(&AsahiDevice,&mmu::Uat,&mmu::Vm{id:7},old.clone()).unwrap();
 cl.compute.queue_graph.fail=true;r.graph.fail=true;assert!(cl.bind_logical_channel_control(&next).is_err());assert!(r.retarget_command_context(&AsahiDevice,&mmu::Uat,&mmu::Vm{id:7},next).is_err());assert_eq!(cl.channel_control_gpu_va,old.channel_control_gpu_va());assert_eq!(r.descriptor_context_id,9);assert_eq!(r.logical_qos.as_ref().unwrap().id(),old.qos.id());
 assert_eq!(pid(&cl.compute.queue_graph.raw,0x48),old.owner_pid);assert_eq!(r.owner_pid,old.owner_pid);
 for at in [0x48,0x108]{assert_eq!(pid(&r.graph.raw,at),old.owner_pid);}
}
#[test]fn handoff_and_unpublished_rollback_keep_full_width_pid_across_engines(){
 let ids=allocator();let old=context(&ids,9,0x11110000);let next=context(&ids,10,0x22220000);let mut cl=compute();let mut r=render();
 for owner in [&old,&next,&old]{cl.bind_logical_channel_control(owner).unwrap();r.retarget_command_context(&AsahiDevice,&mmu::Uat,&mmu::Vm{id:7},owner.clone()).unwrap();
 assert_eq!(pid(&cl.compute.queue_graph.raw,0x48),owner.owner_pid);assert_eq!(r.owner_pid,owner.owner_pid);
 for at in [0x48,0x108]{assert_eq!(pid(&r.graph.raw,at),owner.owner_pid);}}
}
#[test]fn retarget_takes_the_new_contexts_pool_shared_across_its_pairs(){
 let ids=allocator();let a=context(&ids,9,0x11110000);let b=context(&ids,10,0x22220000);
 let(mut r0,mut r1)=(render_on(0),render_on(1));
 r0.retarget_command_context(&AsahiDevice,&mmu::Uat,&mmu::Vm{id:7},a.clone()).unwrap();
 r1.retarget_command_context(&AsahiDevice,&mmu::Uat,&mmu::Vm{id:7},a.clone()).unwrap();
 assert!(Arc::ptr_eq(&r0.render_flist,&r1.render_flist),"SHARED_FLIST: one pool per logical context");
 r0.retarget_command_context(&AsahiDevice,&mmu::Uat,&mmu::Vm{id:7},b.clone()).unwrap();
 assert!(!Arc::ptr_eq(&r0.render_flist,&r1.render_flist),"a retarget moves the graph to the new owner's pool");
 let released=Cell::new(0);assert!(a.render_flists_released(|_,_|{released.set(released.get()+1);true}));assert_eq!(released.get(),1);
}
#[test]fn private_pools_are_per_pair_when_sharing_is_off(){
 g17_host_path::MASK.with(|m|m.set(0));
 let ids=allocator();let a=context(&ids,9,0x11110000);
 let(mut r0,mut r1)=(render_on(0),render_on(1));
 r0.retarget_command_context(&AsahiDevice,&mmu::Uat,&mmu::Vm{id:7},a.clone()).unwrap();
 r1.retarget_command_context(&AsahiDevice,&mmu::Uat,&mmu::Vm{id:7},a.clone()).unwrap();
 assert!(!Arc::ptr_eq(&r0.render_flist,&r1.render_flist));
 r0.retarget_command_context(&AsahiDevice,&mmu::Uat,&mmu::Vm{id:7},a.clone()).unwrap();
 let released=Cell::new(0);assert!(a.render_flists_released(|_,_|{released.set(released.get()+1);true}));assert_eq!(released.get(),2);
}
#[test]fn logical_switches_do_not_rewrite_bootstrap_b2(){
 let ids=allocator();let mut q=compute();let mut m=Manager{resources:B2Resources{raw:vec![0;0x4000],writes:0},compute_b2_activation:g17_submission::G17PClB2ActivationState::Pending,compute_channel_control:Some(G17PClChannelControl{address:0x99990000,firmware_byte:Cell::new(0xff),published:Cell::new(false)})};
 for (id,address) in [(9,0x11110000),(10,0x22220000)]{q.bind_logical_channel_control(&context(&ids,id,address)).unwrap();m.publish_compute_b2_once(&mut q).unwrap();assert_eq!(word(&m.resources.raw,0x808),0x99990000);assert_eq!(q.channel_control_gpu_va,address);}
 assert_eq!(m.resources.writes,1);
}
#[test]fn encoded_qos_and_ta_parameter_buffer_ids_are_distinct(){
 let ids=allocator();let owner=context(&ids,9,0x11110000);let mut r=render();r.retarget_command_context(&AsahiDevice,&mmu::Uat,&mmu::Vm{id:7},owner.clone()).unwrap();
 let addresses=g17_submission::G17PColdOpeningOptionalAddresses{context_scratch:0x1000,firmware_scratch:0x2000,usc_priv_mem_flist:0x3000,usc_freelist_hardware_buffer_id:6,channel_control:owner.channel_control_gpu_va(),tiling_shared_object:0x4000,parameter_buffer_token:13,owner_pid:99,ta_hardware_buffer_id:r.ta_hardware_buffer_id};
 for (stage,qid,partner) in [(g17_submission::G17PColdOpeningStage::Tiling,0,1),(g17_submission::G17PColdOpeningStage::Fragment,1,0)]{
  let mut raw=[0;g17_submission::G17P_COLD_OPENING_OPTIONAL_SIZE];g17_submission::apply_g17p_cold_opening_optional(stage,addresses,qid,partner,r.qos_hardware_buffer_id().unwrap(),0,9,&mut raw).unwrap();
  assert_eq!(word(&raw,0x4a),owner.channel_control_gpu_va());assert_eq!(u16::from_le_bytes(raw[0x56..0x58].try_into().unwrap()),u16::from(owner.qos.id()));
  if qid==0{assert_eq!(u32::from_le_bytes(raw[0x7e..0x82].try_into().unwrap()),73);}else{assert_eq!(&raw[0x76..0x86],&[0xff;16]);}
 }
}
'''
native_b2 = "\n".join(item(submission, marker) for marker in [
    "pub(crate) struct G17PClB2PrimaryStateLayout {",
    "pub(crate) const G17P_CL_B2_PRIMARY_STATE_LAYOUT:",
    "pub(crate) fn apply_g17p_cl_b2_computed_state(",
    "pub(crate) fn apply_g17p_cold_opening_optional(",
])
# The constant declaration's terminating semicolon lies outside its braces.
native_b2 = native_b2.replace("region_2_write_extent: 0x18,\n    }", "region_2_write_extent: 0x18,\n    };")
constants = "\n".join(re.search(r"pub\(crate\) const " + name + r"\s*:[^;]+;", submission).group() for name in [
    "G17P_COLD_OPENING_OPTIONAL_SIZE", "G17P_NATIVE_RENDER_DOORBELL_PRIORITY", "G17P_NATIVE_RENDER_POLICY"])
addresses = item(submission, "pub(crate) struct G17PColdOpeningOptionalAddresses {")
native_b2 = r'''
mod g17_submission {
 use super::Result as HostResult;
 type Result<T,E> = std::result::Result<T,E>;
 #[derive(Debug)] pub enum SubmissionError{KickB2PrimaryStateAlreadyPublished,KickB2PrimaryStateBufferTooSmall,PartialOpeningGraphObjectSize}
 #[derive(Copy,Clone,PartialEq)] pub enum G17PClB2ActivationState{Pending,Published}
 #[derive(Copy,Clone,PartialEq)] pub enum G17PColdOpeningStage{Tiling,Fragment}
 fn put_u16_le(b:&mut[u8],at:usize,v:u16){b[at..at+2].copy_from_slice(&v.to_le_bytes());}
 fn put_u64_le(b:&mut[u8],at:usize,v:u64){b[at..at+8].copy_from_slice(&v.to_le_bytes());}
 fn put_u32_le(b:&mut[u8],at:usize,v:u32){b[at..at+4].copy_from_slice(&v.to_le_bytes());}
''' + constants + "\n#[derive(Copy,Clone)]\n" + addresses + "\n" + native_b2 + "\n}\n"
rust = native_b2 + rust.replace('#![allow(dead_code, unused_variables)]', '')
rust = r"""
use std::sync::atomic::{AtomicU32,Ordering};
macro_rules! pr_info { ($($arg:tt)*) => {{}}; }
mod module_parameters {
 pub struct Param; impl Param { pub fn value(&self)->&u32 { &0 } }
 #[allow(non_upper_case_globals)] pub static g17p_mcache_dsid_probe:Param=Param;
}
static G17P_MCACHE_PROBE_NEXT:AtomicU32=AtomicU32::new(96);
""" + item(resources, "fn g17p_mcache_probe_reserve(") + rust
rust += f'\n#[path = "{root / "g17_work_state.rs"}"] mod g17_work_state;\n'
with tempfile.TemporaryDirectory() as directory:
    path = Path(directory)
    test = path / "logical_owner.rs"
    test.write_text(with_g17_config(rust))
    binary = path / "logical_owner"
    compiled = subprocess.run(["rustc", "--edition=2021", "--test", "-Awarnings", str(test), "-o", str(binary)], capture_output=True, text=True)
    if compiled.returncode:
        raise SystemExit(compiled.stderr[:12000])
    subprocess.run([str(binary), "--skip", "g17_submission::"], check=True)
print("PASS production logical scheduler/QoS lifetime and bootstrap isolation; hardware validation required")
