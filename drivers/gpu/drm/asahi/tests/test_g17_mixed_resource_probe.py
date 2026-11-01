#!/usr/bin/env python3
"""Compile the actual read-only two-slot census using DRAM observation mocks."""
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def item(source, marker):
    start = source.index(marker)
    end = source.index('{', start) + 1
    depth = 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[start:end]


source = (ROOT / 'g17_manager.rs').read_text()
assert 'default: 0' in item((ROOT / 'asahi.rs').read_text(), 'g17p_mixed_resource_probe: u32')
submit = item(source, 'pub(crate) fn submit_translated_compute(')
timeout = item(source, 'fn log_async_compute_timeout_report(') + item(source, 'pub(crate) fn log_async_compute_timeout(')
assert 'self.log_mixed_resource_census(queue, false);' in submit
assert 'self.log_mixed_resource_census(queue, true);' in timeout
census = item(source, 'fn log_mixed_resource_census(')
assert 'select_render_slot(' not in census
assert 'registers' not in census and 'retire_' not in census

preamble = r'''
#![allow(dead_code)]
use std::sync::{Mutex,atomic::{AtomicU32,Ordering}};
type Result<T> = std::result::Result<T,i32>;
static LOG:Mutex<Vec<String>>=Mutex::new(Vec::new());
macro_rules! pr_info {($($arg:tt)*)=>{LOG.lock().unwrap().push(format!($($arg)*));};}
mod kernel {pub mod time {
 pub struct Monotonic;
 pub struct Instant<T>(std::time::Instant,std::marker::PhantomData<T>);
 impl<T> Instant<T>{pub fn now()->Self{Self(std::time::Instant::now(),std::marker::PhantomData)}pub fn elapsed(&self)->std::time::Duration{self.0.elapsed()}}
}}
mod module_parameters {
 pub struct Param(pub super::AtomicU32);impl Param{pub fn value(&self)->Box<u32>{Box::new(self.0.load(super::Ordering::SeqCst))}}
 #[allow(non_upper_case_globals)]pub static g17p_mixed_resource_probe:Param=Param(super::AtomicU32::new(0));
}
#[derive(Debug,Clone,Copy,PartialEq)]struct RenderState {
 tiling:[u32;3],fragment:[u32;3],job_list:[u64;2],job_list_empty:bool,
 tiling_timestamps:[u64;2],fragment_timestamps:[u64;2],
}
mod g17_resources {
 pub struct G17PUserRenderStorage {pub reads:u32,pub sample:super::Result<super::RenderState>}
 impl G17PUserRenderStorage {
  pub fn descriptor_context_id(&self)->u16{7}
  pub fn retained_hardware_buffer_id(&self)->Option<u32>{Some(8)}
  pub fn render_pool_id(&self)->u64{9}
  pub fn parameter_manager_generation(&self)->Option<u64>{Some(10)}
  pub fn tvb_capacity(&self)->usize{32}
  pub fn context_update_pending(&self)->bool{false}
  pub fn snapshot(&mut self,_ordinal:u64)->super::Result<super::RenderState>{self.reads+=1;self.sample}
 }
}
fn storage()->g17_resources::G17PUserRenderStorage {
 g17_resources::G17PUserRenderStorage{reads:0,sample:Ok(RenderState{
  tiling:[3,3,3],fragment:[3,3,3],job_list:[0x123,0x456],job_list_empty:false,
  tiling_timestamps:[22,22],fragment_timestamps:[22,0],
 })}
}
struct Bind(u64);impl Bind{fn vm_id(&self)->u64{self.0}}
struct Slot {
 retired_handoff_graph:Option<()>,submission_ordinal:u64,tracker:Option<()>,hardware_buffer_id:Option<u32>,
 hardware_buffer_published:bool,bind:Option<Bind>,storage:Option<Box<g17_resources::G17PUserRenderStorage>>,
 flist_index:usize,
}
struct RenderSlots{slots:[Option<Slot>;2],active_slot:u8}
impl RenderSlots {
 fn get(&self,slot:u8)->&Slot{self.slots[usize::from(slot)].as_ref().unwrap()}
 fn get_mut(&mut self,slot:u8)->&mut Slot{self.slots[usize::from(slot)].as_mut().unwrap()}
 fn active(&self)->&Slot{self.slots[usize::from(self.active_slot)].as_ref().unwrap()}
 fn active_mut(&mut self)->&mut Slot{self.slots[usize::from(self.active_slot)].as_mut().unwrap()}
 fn iter_mut(&mut self)->impl Iterator<Item=(u8,Option<&mut Slot>)>{self.slots.iter_mut().enumerate().map(|(i,s)|(i as u8,s.as_mut()))}
}
struct HardwareIds{free:[u64;2],reserved:[u64;2]}
#[derive(Default)]struct Resources{uma_reads:u32,qos_reads:u32,growth_reads:u32}
impl Resources {
 fn snapshot_compute_uma(&mut self,slot:u8)->Result<[u64;4]>{self.uma_reads+=1;Ok([u64::from(slot);4])}
 fn snapshot_compute_qos_owner(&mut self,_qid:u8,_hbid:u8)->Result<[u64;12]>{self.qos_reads+=1;Ok([0;12])}
 fn log_render_uma_growth(&mut self,_checkpoint:&str){self.growth_reads+=1;}
}
#[derive(Default)]struct ComputeStorage{reads:u32}
impl ComputeStorage{
 fn flist_id(&self)->u16{15}
 fn snapshot_compute_flist(&mut self)->Result<[u32;28]>{self.reads+=1;Ok([0;28])}
}
struct G17PSksmQueue{queue_id:u8,hardware_buffer_id:u8,submission_ordinal:u32,storage:ComputeStorage}
impl G17PSksmQueue{fn qos_hardware_buffer_id(&self)->u8{self.hardware_buffer_id}fn new()->Self{Self{queue_id:14,hardware_buffer_id:12,submission_ordinal:16,storage:ComputeStorage::default()}}}
struct Teardown{graphs:Vec<()>}
// Shared FList: the UMA lifecycle lives in the manager, indexed by each pair's pool.
struct Manager {
 render_slots:RenderSlots,render_hardware_buffer_ids:HardwareIds,logical_queue_teardowns:Vec<Teardown>,
 resources:Resources,render_flist_owners:Vec<Option<u32>>,
}
impl Manager {
 fn new(active:u8)->Self {
  let mut slots=[None,None];slots[usize::from(1-active)]=Some(Slot{
   retired_handoff_graph:Some(()),submission_ordinal:3,tracker:None,hardware_buffer_id:None,
   hardware_buffer_published:false,bind:Some(Bind(43)),storage:Some(Box::new(storage())),flist_index:usize::from(1-active),
  });
  slots[usize::from(active)]=Some(Slot{
   retired_handoff_graph:None,submission_ordinal:4,tracker:Some(()),hardware_buffer_id:Some(8),
   hardware_buffer_published:true,bind:Some(Bind(42)),storage:Some(Box::new(storage())),flist_index:usize::from(active),
  });
  Self{render_slots:RenderSlots{slots,active_slot:active},render_hardware_buffer_ids:HardwareIds{free:[1,2],reserved:[3,4]},
   logical_queue_teardowns:vec![Teardown{graphs:vec![()]}],resources:Resources::default(),
   render_flist_owners:{let mut o=vec![None,None];o[usize::from(active)]=Some(4);o}}
 }
}
fn mode(v:u32){module_parameters::g17p_mixed_resource_probe.0.store(v,Ordering::SeqCst);LOG.lock().unwrap().clear();}
'''
production = item(source, 'fn g17p_mixed_resource_sample(')
production += 'impl Manager {\n' + item(source, 'fn log_mixed_render_owner(') + census + '\n}\n'
tests = r'''
#[test]fn default_and_unknown_modes_do_no_reads_or_logs() {
 for value in [0,3,u32::MAX] {mode(value);let mut m=Manager::new(0);let mut q=G17PSksmQueue::new();
  m.log_mixed_resource_census(&mut q,false);m.log_mixed_resource_census(&mut q,true);
  assert_eq!(m.resources.uma_reads,0);assert_eq!(q.storage.reads,0);assert_eq!(m.render_slots.active().storage.as_ref().unwrap().reads,0);
  assert!(LOG.lock().unwrap().is_empty());
 }
}
#[test]fn fault_only_mode_has_no_prepublication_sampling() {
 mode(1);let mut m=Manager::new(0);let mut q=G17PSksmQueue::new();
 m.log_mixed_resource_census(&mut q,false);assert!(LOG.lock().unwrap().is_empty());
 m.log_mixed_resource_census(&mut q,true);assert_eq!(m.resources.uma_reads,2);assert_eq!(q.storage.reads,1);
 assert!(LOG.lock().unwrap().iter().all(|s|s.contains("compute-fault")));
}
#[test]fn periodic_policy_is_explicit_and_always_allows_faults() {
 for ordinal in 0..1024 {assert_eq!(g17p_mixed_resource_sample(2,ordinal,false),ordinal<=1||ordinal%16==0);assert!(g17p_mixed_resource_sample(2,ordinal,true));}
 mode(2);let mut m=Manager::new(0);let mut q=G17PSksmQueue::new();q.submission_ordinal=3;
 m.log_mixed_resource_census(&mut q,false);assert_eq!(m.resources.uma_reads,0);
}
#[test]fn both_slots_are_observed_without_changing_selection_or_ownership() {
 for active in [0,1] {mode(2);let mut m=Manager::new(active);let mut q=G17PSksmQueue::new();
  m.log_mixed_resource_census(&mut q,false);
  assert_eq!(m.render_slots.active_slot,active);
  assert_eq!(m.render_hardware_buffer_ids.free,[1,2]);assert_eq!(m.render_hardware_buffer_ids.reserved,[3,4]);
  assert_eq!(m.render_slots.slots[usize::from(active)].as_ref().unwrap().storage.as_ref().unwrap().reads,1);
  assert_eq!(m.render_slots.slots[usize::from(1-active)].as_ref().unwrap().storage.as_ref().unwrap().reads,1);
  let log=LOG.lock().unwrap();let rows:Vec<_>=log.iter().filter(|s|s.contains("mixed-resource render[")).collect();
  assert_eq!(rows.len(),2);assert_eq!(rows.iter().filter(|s|s.contains("slot=0")).count(),1);
  assert_eq!(rows.iter().filter(|s|s.contains("slot=1")).count(),1);
  assert!(rows.iter().any(|s|s.contains("(3, false, None, false, Some(43))")));
  assert!(rows.iter().any(|s|s.contains("(4, true, Some(8), true, Some(42))")));
  assert!(log[0].contains("teardowns=1 displaced-graphs=1"),"{}",log[0]);
 }
}
#[test]fn absent_storage_and_failed_snapshot_remain_explicit_without_failing_work() {
 mode(2);let mut m=Manager::new(0);m.render_slots.active_mut().storage.as_mut().unwrap().sample=Err(-5);m.render_slots.slots[1]=None;
 m.log_mixed_resource_census(&mut G17PSksmQueue::new(),false);
 assert_eq!(m.render_slots.active().storage.as_ref().unwrap().sample,Err(-5));
 let log=LOG.lock().unwrap();assert!(log.iter().any(|s|s.contains("Err(")));
 assert!(log.iter().any(|s|s.contains("slot=1")&&s.ends_with("=None\n")));
 assert_eq!(m.resources.growth_reads,1);
}
#[test]fn equal_counters_with_linked_job_and_incomplete_pass_stamps_are_reported_raw() {
 mode(2);let mut m=Manager::new(0);let before=m.render_slots.active().storage.as_ref().unwrap().sample;
 m.log_mixed_resource_census(&mut G17PSksmQueue::new(),false);
 assert_eq!(m.render_slots.active().storage.as_ref().unwrap().sample,before);
 let log=LOG.lock().unwrap();assert!(log.iter().any(|s|s.contains("job_list_empty: false")&&s.contains("fragment_timestamps: [16, 0]")));
 assert!(log.iter().all(|s|!s.contains("TA-active")&&!s.contains("3D-active")));
}
'''

with tempfile.TemporaryDirectory() as directory:
    source_file = Path(directory) / 'census.rs'
    binary = Path(directory) / 'census'
    source_file.write_text(preamble + production + tests)
    subprocess.run(['rustc', '--edition=2021', '--test', str(source_file), '-o', str(binary)], check=True)
    subprocess.run([str(binary), '--test-threads=1', '--nocapture'], check=True, timeout=20)
