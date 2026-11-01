#!/usr/bin/env python3
"""Run the production event codec, owner dispatch, and TVB commit with host storage."""
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
LIVE = (ROOT / 'g17_live_boot.rs').read_text()
DRIVER = (ROOT / 'driver.rs').read_text()
MANAGER = (ROOT / 'g17_manager.rs').read_text()

drain = LIVE[LIVE.index('fn drain_primary_firmware_event_batch('):]
drain = drain[:drain.index('fn complete_primary_real_recovery(')]
assert drain.index('G17PTvbGrowDisposition::Deferred') < drain.index('break;', drain.index('G17PTvbGrowDisposition::Deferred'))
service = LIVE[LIVE.index('    pub(crate) fn service_primary_firmware_events('):]
service = service[:service.index('    fn service_firmware_events_inline(')]
assert service.index('flush_pending_primary_device_control') < service.index('notifications_pending')
assert 'note_primary_device_control_backpressure' in service
assert 'if !self.primary_device_control_backpressured()' in service
inline = LIVE[LIVE.index('    fn service_firmware_events_inline('):]
inline = inline[:inline.index('    pub(crate) fn service_primary_recovery(')]
assert inline.index('primary_tvb_reply_pending') < inline.index('firmware_event_notifications')
assert 'pending_control && !self.flush_pending_primary_device_control()?' in inline
assert 'pending_release && !self.flush_pending_render_usc_freelist_release()?' in inline
worker = DRIVER[DRIVER.index('impl WorkItem<1>'):DRIVER.index('impl WorkItem<2>')]
assert worker.index('service_primary_firmware_events') < worker.index('fsleep(Delta::from_millis(1))')
requeue = DRIVER[DRIVER.index('fn requeue_g17p_firmware_event_worker_if_pending('):]
assert requeue.index('runtime.mutex().lock()') < requeue.index('event_worker_armed()') < requeue.index('enqueue')
for marker in ('publish_primary_device_control_record(',
               'publish_render_usc_freelist_request(',
               'publish_render_usc_freelist_release('):
    method = MANAGER[MANAGER.index('    pub(crate) fn ' + marker):]
    method = method[:method.index('\n    }')]
    assert ('pending_primary_control.is_some()' in method
            or 'primary_device_control_backpressured()' in method)

def item(file, marker):
    source = (ROOT / file).read_text()
    start = source.index(marker)
    end = source.index('{', start) + 1
    depth = 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    prefix = '#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n' if marker.startswith('struct G17PPm') else ''
    return prefix + source[start:end]

drm=(ROOT/'g17_drm.rs').read_text()
grow_worker=item('g17_drm.rs','    pub(crate) fn service_render_grows(')
assert grow_worker.index('drop(runtime);') < grow_worker.index('grow.grow(&drm)') < grow_worker.index('runtime.return_render_grow(') < grow_worker.index('drop(epoch);')
assert 'queue_render_tvb_grow(request)' in drain and 'service_render_tvb_grow(request)' not in drain
assert 'replay_prepared_render_grows' not in LIVE
assert 'disable_g17p_work::<4>(&self.drm)' in DRIVER
assert DRIVER.count('g17_tvb_grow_work <- new_work!') == 2
assert 'system_unbound().enqueue::<ARef<AsahiDevice>, 4>' in DRIVER

code = r'''
#![allow(dead_code, unused_variables)]
use std::cell::Cell as StdCell;
use std::rc::Rc;
#[derive(Clone)]struct Cell<T:Copy>(Rc<StdCell<T>>);
impl<T:Copy> Cell<T>{fn new(v:T)->Self{Self(Rc::new(StdCell::new(v)))}fn get(&self)->T{self.0.get()}fn set(&self,v:T){self.0.set(v)}}
type AsahiDevice=();type KBox<T>=Box<T>;
mod driver{pub fn queue_g17p_tvb_grow_worker(_:()) {}}
// v96 grow accounting (the real counters/budget; the mask stays 0 here, and
// the re-kick itself is exercised by test_g17_v96_grow_rekick.py).
mod g17_recovery_path{
 use std::sync::atomic::{AtomicU32,AtomicU64,Ordering};
 pub const GROW_REKICK:u32=1<<2;
 pub static MASK:AtomicU32=AtomicU32::new(0);
 pub fn enabled(bit:u32)->bool{MASK.load(Ordering::Relaxed)&bit!=0}
 pub static GROW_INGRESS:AtomicU64=AtomicU64::new(0);pub static GROW_DEFERRED:AtomicU64=AtomicU64::new(0);
 pub static GROW_PARKED_AT_RETURN:AtomicU64=AtomicU64::new(0);pub static GROW_REKICKS:AtomicU64=AtomicU64::new(0);
 pub static GROW_REPLIED:AtomicU64=AtomicU64::new(0);
 pub fn log_budget(counter:u64,first:u64,every:u64)->bool{counter<=first||counter%every==0}
 pub static WATCHED:AtomicU64=AtomicU64::new(0);
 pub fn watch_grow_reply(producer_after:u32,buffer_slot:u32){WATCHED.store(1<<63|u64::from(producer_after)<<32|u64::from(buffer_slot),Ordering::Relaxed);}
}
static G17P_GROW_TAKEN:std::sync::atomic::AtomicU64=std::sync::atomic::AtomicU64::new(0);

use std::sync::atomic::Ordering;
type Result<T=()> = std::result::Result<T, i32>;
const EINVAL:i32=22; const ENOENT:i32=2; const EIO:i32=5;
const EFAULT:i32=14; const EOVERFLOW:i32=75; const ERANGE:i32=34;
const EAGAIN:i32=11; const ENOMEM:i32=12; const EBUSY:i32=16;
const ETIMEDOUT:i32=110; const GFP_KERNEL:()=();
macro_rules! dev_info { ($($t:tt)*) => {()} }
macro_rules! dev_warn { ($($t:tt)*) => {()} }
struct KVec<T>(Vec<T>);
impl<T> KVec<T> {
 fn with_capacity(n:usize,_:())->Result<Self>{Ok(Self(Vec::with_capacity(n)))}
 fn push(&mut self,t:T,_:())->Result{self.0.push(t);Ok(())}
 fn remove(&mut self,i:usize)->Result<T>{Ok(self.0.remove(i))}
}
impl<T> std::ops::Deref for KVec<T>{type Target=Vec<T>;fn deref(&self)->&Vec<T>{&self.0}}
impl<T> std::ops::DerefMut for KVec<T>{fn deref_mut(&mut self)->&mut Vec<T>{&mut self.0}}
impl<T> IntoIterator for KVec<T>{type Item=T;type IntoIter=std::vec::IntoIter<T>;fn into_iter(self)->Self::IntoIter{self.0.into_iter()}}
mod mmu {
 use super::*;
 #[derive(Clone)]pub struct Vm {pub blocks:Cell<usize>,pub fail:Cell<bool>}
 impl Vm {
  pub fn render_pool_tvb_blocks(&self,_:u8,_:u64)->Option<usize>{Some(self.blocks.get())}
  pub fn append_render_pool_mappings(&self,_:u8,_:u64,before:usize,after:usize,aliases:usize)->Result{
   if self.fail.get(){return Err(ENOMEM)}
   assert_eq!(before,self.blocks.get());assert_eq!(aliases,after-before);
   self.blocks.set(after);Ok(())
  }
 }
}
mod g17_submission {
 pub const G17P_PBDESC_ENTRY_SIZE:usize=16;
 pub fn g17p_pbdesc_offset(id:u32)->usize{id as usize*16}
 #[derive(Default)]pub struct G17PPbDescRingState;
 pub fn encode_g17p_pbdesc(_:[u32;4],address:u64,pages:u32,_:G17PPbDescRingState)->[u32;4]{[address as u32,pages,0,0]}
}
''' + f'#[path="{ROOT}/g17_completion.rs"] mod g17_completion;\n' + r'''
mod g17_resources {
 use super::*;
 const G17P_PM_MAX_BLOCKS:usize=0xd1a;
 const G17P_PM_MAX_OWNED_BLOCKS:usize=G17P_PM_MAX_BLOCKS-1;
 const G17P_PM_MAX_BLOCKS_NOMEMLESS:usize=0x45e;
 const G17P_PM_PAGES_PER_BLOCK:usize=4;
 const G17P_RENDER_TVB_BLOCK_SIZE:usize=0x20000;
 const G17P_RENDER_TVB_BLOCK_STRIDE:usize=0x28000;
 const G17P_RENDER_USER_BASE:u64=0x10_0000_0000;
 const G17P_RENDER_USER_END:u64=G17P_RENDER_USER_BASE+0x1_0000_0000;
 const G17P_PM_PAGE_SIZE:usize=0x8000;
 const G17P_PM_SHARED_TOTAL_CURSOR:usize=0;
 const G17P_PM_SHARED_COMMITTED_CURSOR:usize=4;
 const G17P_PM_SHARED_READ_CURSOR:usize=8;
 fn put_u32(raw:&mut[u8],offset:usize,value:u32){raw[offset..offset+4].copy_from_slice(&value.to_le_bytes())}
 fn g17p_render_tvb_page_id(_:Pair,index:usize)->Option<u32>{Some(0x1000+index as u32*8)}
 #[derive(Copy,Clone)]struct Pair(u8);impl Pair{fn slot(self)->u8{self.0}}
 pub struct Object{pub raw:Vec<u8>}
 thread_local!{static MOVE_CONSUMER:StdCell<Option<(u32,u32)>>=const{StdCell::new(None)};}
 impl Object{fn with_bytes_mut<T>(&mut self,f:impl FnOnce(&mut[u8])->Result<T>)->Result<T>{
  if self.raw.is_empty(){return Err(EFAULT)}
  if self.raw.len()==0x30000{MOVE_CONSUMER.with(|pending|if let Some((left,next))=pending.get(){
   if left==1{put_u32(&mut self.raw,0x108,next);pending.set(None)}else{pending.set(Some((left-1,next)));}
  });}
  f(&mut self.raw)
 }}
 #[derive(Copy,Clone)]struct Layout{total_size:usize,shared_control:usize,block_page_bases:usize,page_list:usize,hwpb_state:usize}
 struct Pm{backing:Object,layout:Layout,hardware_buffer_id:u32,page_count:u32}
 impl Pm{fn page_list_low_va(&self)->u64{0x98760000}}
 fn block_address(index:usize)->u64{G17P_RENDER_USER_BASE+(0x1000+index as u64*8)*G17P_PM_PAGE_SIZE as u64}
 struct Growth{extension:usize,mappings:usize,addresses:KVec<u64>}
 struct Tvb{block_count:usize,logical_size:usize,extensions:KVec<usize>,block_addresses:KVec<u64>,fail:bool}
 impl Tvb {
  fn all_reachable_from(&self,_:&mmu::Vm)->bool{true}
  fn prepare_growth(&mut self,_:&AsahiDevice,_:&mmu::Vm,blocks:usize)->Result<Option<Growth>>{
   if self.fail{return Err(ENOMEM)}
   self.extensions.0.reserve(1);
   self.block_addresses.0.reserve(blocks-self.block_count);
   Ok(Some(Growth{extension:blocks-self.block_count,mappings:blocks-self.block_count,
    addresses:KVec((self.block_count..blocks).map(block_address).collect())}))
  }
 }
 struct ReachabilityCache{invalidations:usize}
 impl ReachabilityCache{fn invalidate(&mut self){self.invalidations+=1;}}
 pub struct G17PUserRenderStorage{render_tvb:Option<Tvb>,parameter_management:Option<Pm>,queue_pair:Pair,reachability_cache:ReachabilityCache}
 impl G17PUserRenderStorage {
  pub fn render_flist_id(&self)->u32{1}pub fn render_flist_pointer(&self)->u64{2}
  pub fn render_flist_operand_table(&self)->u64{3}pub fn render_flist_cookie(&self)->u64{4}pub fn render_flist_serial(&self)->u64{5}
  pub fn mark_render_flist_grown(&self){}

  fn render_pool_id(&self)->u64{1}
  pub fn tvb_capacity(&self)->usize{self.render_tvb.as_ref().unwrap().block_count}
  pub fn retained_hardware_buffer_id(&self)->Option<u32>{self.parameter_management.as_ref().map(|pm|pm.hardware_buffer_id)}
  pub fn new(id:u32,blocks:usize,slot:u8)->Self {
   let layout=Layout{total_size:0x30000,shared_control:0x100,block_page_bases:0x200,page_list:0x10000,hwpb_state:0};
   let mut raw=vec![0xa5;layout.total_size];
   for (offset,v) in [(0,blocks as u32),(4,blocks as u32),(8,7)]{put_u32(&mut raw,layout.shared_control+offset,v)}
   for i in 0..blocks{put_u32(&mut raw,layout.block_page_bases+i*8,0x1000+i as u32*8)}
   Self{render_tvb:Some(Tvb{block_count:blocks,logical_size:1,extensions:KVec(Vec::new()),block_addresses:KVec((0..blocks).map(block_address).collect()),fail:false}),
    parameter_management:Some(Pm{backing:Object{raw},layout,hardware_buffer_id:id,page_count:blocks as u32*4}),queue_pair:Pair(slot),reachability_cache:ReachabilityCache{invalidations:0}}
  }
 }
 struct Resources{pb_descriptor_table:Object}
 #[derive(Debug,Copy,Clone,PartialEq,Eq)]pub struct G17PDeviceControlPublication{pub producer_after:u32}
 type MappedObject=Object;
 pub struct G17ResourceOwner{dev:AsahiDevice,resources:Resources,pub replies:Vec<[u8;64]>,pub credit:bool}
 impl G17ResourceOwner {
  pub fn new()->Self{Self{dev:(),resources:Resources{pb_descriptor_table:Object{raw:vec![0x5a;0x1000]}},replies:vec![],credit:true}}
  pub fn dev(&self){}
  pub fn owned_dev(&self){}
  pub fn publish_primary_device_control_raw(&mut self,record:&[u8;64])->Result<G17PDeviceControlPublication>{
   if !self.credit{return Err(EAGAIN)} self.credit=false;
   self.replies.push(*record);Ok(G17PDeviceControlPublication{producer_after:self.replies.len() as u32})
  }
''' + '\n'.join(item('g17_resources.rs', m) for m in [
    '    pub(crate) fn grow_render_tvb_synchronously(',
    '    fn grow_render_tvb_inner(',
    '    pub(crate) fn prepare_host_tvb_grow(',
    '    pub(crate) fn commit_host_tvb_grow(',
    '    fn grow_render_tvb_owned(',
    '    pub(crate) fn grow_render_tvb_offlock(',
]) + '\n}\n' + '\n'.join(item('g17_resources.rs', m) for m in [
    'pub(crate) struct G17PHostTvbGrow {', 'impl G17PHostTvbGrow {',
    'fn g17p_compact_tvb_page_id(', 'fn g17p_tvb_grow_target(',
    'fn g17p_tvb_auto_grow_target(', 'struct G17PPmControlCursors {',
    'fn g17p_pm_control_cursors(', 'fn g17p_pm_growth_commit_matches(', 'struct G17PPmGrowthPlan {',
    'impl G17PPmGrowthPlan {', 'fn g17p_pm_growth_plan(',
]) + r'''
 #[test]fn live_commit_allows_only_bounded_consumer_progress_and_unchanged_host_producers(){
  let cap=G17P_PM_MAX_BLOCKS as u32;
  for read in [0,1,7,cap-2,cap-1]{for available in [0,1,8,cap-2]{
   let before=G17PPmControlCursors{read,total:(read+available)%cap,committed:(read+available)%cap};
   for advance in 0..cap{
    let after=G17PPmControlCursors{read:(read+advance)%cap,..before};
    assert_eq!(g17p_pm_growth_commit_matches(before,after,true),advance<=available);
    assert_eq!(g17p_pm_growth_commit_matches(before,after,false),advance==0);
   }
   assert!(!g17p_pm_growth_commit_matches(before,G17PPmControlCursors{total:(before.total+1)%cap,..before},true));
   assert!(!g17p_pm_growth_commit_matches(before,G17PPmControlCursors{committed:(before.committed+1)%cap,..before},true));
  }}
 }
 #[test]fn actual_live_growth_accepts_consumer_progress_during_allocation_without_rewriting_it(){
  for (old_read,committed,new_read) in [(7,32,8),(7,32,32),(G17P_PM_MAX_BLOCKS as u32-2,3,1)]{
   let mut storage=G17PUserRenderStorage::new(2,32,0);let vm=mmu::Vm{blocks:Cell::new(32),fail:Cell::new(false)};
   let pm=storage.parameter_management.as_mut().unwrap();put_u32(&mut pm.backing.raw,0x100,committed);put_u32(&mut pm.backing.raw,0x104,committed);put_u32(&mut pm.backing.raw,0x108,old_read);
   MOVE_CONSUMER.with(|pending|pending.set(Some((2,new_read))));
   assert_eq!(G17ResourceOwner::grow_render_tvb_offlock(&(),&mut storage,&vm),Ok(true));
   let pm=storage.parameter_management.as_ref().unwrap();let cursors=g17p_pm_control_cursors(&pm.backing.raw,0x100).unwrap();
   assert_eq!(cursors.read,new_read);assert_eq!(cursors.committed,(committed+10)%G17P_PM_MAX_BLOCKS as u32);assert_eq!(storage.tvb_capacity(),42);
  }
 }
 #[test]fn actual_live_growth_rejects_consumer_overrun_before_alias_commit(){
  let mut storage=G17PUserRenderStorage::new(2,32,0);let vm=mmu::Vm{blocks:Cell::new(32),fail:Cell::new(false)};
  MOVE_CONSUMER.with(|pending|pending.set(Some((2,33))));
  assert_eq!(G17ResourceOwner::grow_render_tvb_offlock(&(),&mut storage,&vm),Err(EAGAIN));assert_eq!(storage.tvb_capacity(),32);assert_eq!(vm.blocks.get(),32);
 }
 #[test]fn tvb_auto_growth_uses_measured_headroom_and_native_cap(){
  assert_eq!(g17p_tvb_auto_grow_target(238,60,G17P_PM_MAX_BLOCKS_NOMEMLESS),Some(179));
  assert_eq!(g17p_tvb_auto_grow_target(120,60,G17P_PM_MAX_BLOCKS_NOMEMLESS),None);
  assert_eq!(g17p_tvb_auto_grow_target(121,60,G17P_PM_MAX_BLOCKS_NOMEMLESS),Some(91));
  assert_eq!(g17p_tvb_auto_grow_target(usize::MAX,60,G17P_PM_MAX_BLOCKS_NOMEMLESS),Some(G17P_PM_MAX_BLOCKS_NOMEMLESS));
 }
 #[test]fn tvb_live_growth_preserves_all_firmware_state_except_new_blocks_and_host_cursors(){
  let mut owner=G17ResourceOwner::new();let mut storage=G17PUserRenderStorage::new(2,32,0);
  let vm=mmu::Vm{blocks:Cell::new(32),fail:Cell::new(false)};
  let before=storage.parameter_management.as_ref().unwrap().backing.raw.clone();
  let desc=owner.resources.pb_descriptor_table.raw.clone();
  assert!(owner.grow_render_tvb_synchronously(&mut storage,&vm).unwrap());
  assert_eq!(storage.tvb_capacity(),42);assert_eq!(vm.blocks.get(),42);
  assert_eq!(storage.render_tvb.as_ref().unwrap().extensions.0,vec![10]);
  let after=&storage.parameter_management.as_ref().unwrap().backing.raw;
  for i in 0..before.len(){
   if !(0x100..0x108).contains(&i)&&!(0x200+32*8..0x200+42*8).contains(&i){assert_eq!(after[i],before[i],"changed byte {i:#x}")}
  }
  assert_eq!(u32::from_le_bytes(after[0x100..0x104].try_into().unwrap()),42);
  assert_eq!(u32::from_le_bytes(after[0x104..0x108].try_into().unwrap()),42);
  for i in 32..42{assert_eq!(u32::from_le_bytes(after[0x200+i*8..0x204+i*8].try_into().unwrap()),0x1000+i as u32*8)}
  assert_eq!(owner.resources.pb_descriptor_table.raw,desc);
 }
 #[test]fn tvb_live_growth_does_not_acquire_a_descriptor_cpu_view(){
  let mut owner=G17ResourceOwner::new();let mut storage=G17PUserRenderStorage::new(2,32,0);
  owner.resources.pb_descriptor_table.raw.clear();
  let vm=mmu::Vm{blocks:Cell::new(32),fail:Cell::new(false)};
  assert_eq!(owner.grow_render_tvb_synchronously(&mut storage,&vm),Ok(true));
  assert_eq!(storage.tvb_capacity(),42);
 }

 #[test]fn host_grow_only_changes_its_shared_descriptor_at_commit(){
  let mut owner=G17ResourceOwner::new();let mut storage=G17PUserRenderStorage::new(2,32,0);
  let vm=mmu::Vm{blocks:Cell::new(32),fail:Cell::new(false)};
  let before=owner.resources.pb_descriptor_table.raw.clone();
  let mut grow=owner.prepare_host_tvb_grow(&storage,42).unwrap();
  grow.grow(&(),&mut storage,&vm).unwrap();
  assert_eq!(storage.tvb_capacity(),42);assert!(grow.updated);
  assert_eq!(owner.resources.pb_descriptor_table.raw,before);
  owner.commit_host_tvb_grow(&grow).unwrap();
  let after=&owner.resources.pb_descriptor_table.raw;
  assert_eq!(&after[..32],&before[..32]);assert_eq!(&after[48..],&before[48..]);
  assert_ne!(&after[32..48],&before[32..48]);
 }
 #[test]fn independent_host_growers_do_not_borrow_global_owner(){
  use std::sync::{Arc,Mutex,Barrier};
  let owner=Arc::new(Mutex::new(G17ResourceOwner::new()));
  let barrier=Arc::new(Barrier::new(3));let mut threads=vec![];
  for id in [2,3]{
   let mut storage=G17PUserRenderStorage::new(id,32,id as u8);
   let mut grow=owner.lock().unwrap().prepare_host_tvb_grow(&storage,42).unwrap();
   let barrier=barrier.clone();
   threads.push(std::thread::spawn(move||{
    let vm=mmu::Vm{blocks:Cell::new(32),fail:Cell::new(false)};
    barrier.wait();grow.grow(&(),&mut storage,&vm).unwrap();barrier.wait();
    (grow,storage)
   }));
  }
  let mut locked=owner.lock().unwrap();
  barrier.wait();barrier.wait(); // Actual grows finish while device owner is held elsewhere.
  for thread in threads.into_iter().rev(){let(grow,storage)=thread.join().unwrap();
   assert_eq!(storage.tvb_capacity(),42);locked.commit_host_tvb_grow(&grow).unwrap();}
  assert_eq!(&locked.resources.pb_descriptor_table.raw[32..48],&locked.resources.pb_descriptor_table.raw[48..64]);
 }
 #[test]fn failed_host_allocation_and_wrong_identity_leave_descriptor_unchanged(){
  for wrong_id in [false,true]{
   let mut owner=G17ResourceOwner::new();let mut storage=G17PUserRenderStorage::new(2,32,0);
   let vm=mmu::Vm{blocks:Cell::new(32),fail:Cell::new(false)};
   let before=owner.resources.pb_descriptor_table.raw.clone();
   let mut grow=owner.prepare_host_tvb_grow(&storage,42).unwrap();
   if wrong_id{storage.parameter_management.as_mut().unwrap().hardware_buffer_id=3;}
   else{storage.render_tvb.as_mut().unwrap().fail=true;}
   assert_eq!(grow.grow(&(),&mut storage,&vm),Err(if wrong_id{EINVAL}else{ENOMEM}));
   assert!(!grow.updated);owner.commit_host_tvb_grow(&grow).unwrap();
   assert_eq!(owner.resources.pb_descriptor_table.raw,before);assert_eq!(storage.tvb_capacity(),32);
  }
 }
 #[test]fn tvb_retired_growth_still_rebuilds_page_list_state_and_descriptor(){
  let mut owner=G17ResourceOwner::new();let mut storage=G17PUserRenderStorage::new(2,32,0);
  let vm=mmu::Vm{blocks:Cell::new(32),fail:Cell::new(false)};
  owner.grow_render_tvb_inner(&mut storage,&vm,42,false).unwrap();
  let pm=storage.parameter_management.as_ref().unwrap();
  let word=|offset:usize|u32::from_le_bytes(pm.backing.raw[offset..offset+4].try_into().unwrap());
  assert_eq!(word(0x34),168);assert_eq!(word(0x3c),42);assert_eq!(word(0x40),7);
  assert_eq!(word(pm.layout.page_list),0x1000+7*8);
  let desc=&owner.resources.pb_descriptor_table.raw[32..48];
  assert_eq!(desc.chunks_exact(4).map(|b|u32::from_le_bytes(b.try_into().unwrap())).collect::<Vec<_>>(),vec![0x98760000,168,0,0]);
 }
 #[test]fn tvb_growth_allocation_and_alias_failure_publish_nothing(){
  for fail_backing in [true,false]{
   let mut owner=G17ResourceOwner::new();let mut storage=G17PUserRenderStorage::new(2,32,0);
   storage.render_tvb.as_mut().unwrap().fail=fail_backing;
   let vm=mmu::Vm{blocks:Cell::new(32),fail:Cell::new(!fail_backing)};
   let before=storage.parameter_management.as_ref().unwrap().backing.raw.clone();
   assert_eq!(owner.grow_render_tvb_synchronously(&mut storage,&vm),Err(ENOMEM));
   assert_eq!(storage.tvb_capacity(),32);assert_eq!(vm.blocks.get(),32);
   assert_eq!(storage.parameter_management.as_ref().unwrap().backing.raw,before);
   assert!(storage.render_tvb.as_ref().unwrap().extensions.is_empty());
  }
 }
 #[test]fn tvb_growth_caps_final_increment_and_reports_no_growth_at_limit(){
  for remaining in 0..=12{
   let before=G17P_PM_MAX_OWNED_BLOCKS-remaining;
   assert_eq!(g17p_tvb_grow_target(before,G17P_PM_MAX_OWNED_BLOCKS),before+remaining.min(10));
  }
  assert_eq!(g17p_tvb_grow_target(usize::MAX,usize::MAX),usize::MAX);
  let max=G17P_PM_MAX_OWNED_BLOCKS;
  let mut owner=G17ResourceOwner::new();let mut storage=G17PUserRenderStorage::new(2,max-3,0);
  let vm=mmu::Vm{blocks:Cell::new(max-3),fail:Cell::new(false)};
  assert_eq!(owner.grow_render_tvb_synchronously(&mut storage,&vm),Ok(true));
  assert_eq!(storage.tvb_capacity(),max);
  assert_eq!(owner.grow_render_tvb_synchronously(&mut storage,&vm),Ok(false));
 }
}
enum G17PBootstrapUmaRequestState{Ready}
struct Uma{request:Option<()>,state:G17PBootstrapUmaRequestState}
impl Uma{fn matches_owner(&self,_:u32,_:u64,_:u64,_:u64)->bool{true}fn same_pool_object(&self,_:u64)->bool{true}}
struct G17PRenderRootOwner{context:u8,vm:mmu::Vm}
impl G17PRenderRootOwner{fn root_slot(&self)->Result<u8>{Ok(self.context)}fn vm(&self)->&mmu::Vm{&self.vm}}
struct Tracker{hardware_buffer_id:u32}
struct Parked{flist_index:usize,preparing:bool,tracker:Option<Tracker>,queued_tracker:Option<Tracker>,hardware_buffer_id:Option<u32>,storage:Option<Box<g17_resources::G17PUserRenderStorage>>,bind:Option<G17PRenderRootOwner>,hardware_buffer_published:bool}
struct RenderSlots{slots:[Option<Box<Parked>>;2],}
impl RenderSlots{
 fn get(&self,slot:u8)->&Parked{self.slots[usize::from(slot)].as_deref().unwrap()}
 fn get_mut(&mut self,slot:u8)->&mut Parked{self.slots[usize::from(slot)].as_deref_mut().unwrap()}
 fn parked_mut(&mut self,slot:u8)->impl Iterator<Item=&mut Parked>{let active=usize::from(slot);self.slots.iter_mut().enumerate().filter(move|(index,_)|*index!=active).filter_map(|(_,slot)|slot.as_deref_mut())}
}
mod g17_initdata{pub const CONTROL_RECORD_SIZE:usize=64;}
#[derive(Debug,Copy,Clone,PartialEq,Eq)]enum G17PTvbGrowDisposition{IgnoredStale,Published,Deferred,PreparationDeferred}
enum G17PPendingPrimaryControlFlush{Empty,Blocked,Published(g17_resources::G17PDeviceControlPublication)}
struct G17PPendingPrimaryControl{record:[u8;64],recovery_generation:u64,worker_attempts:u16}
const G17P_PRIMARY_CONTROL_RETRY_LIMIT:u16=1000;
struct Manager{active_render_grow:Option<g17_completion::G17PFirmwareTvbGrowEvent>,pending_render_grows:KVec<g17_completion::G17PFirmwareTvbGrowEvent>,resources:g17_resources::G17ResourceOwner,accepted_firmware_recovery_generation:u64,pending_primary_control:Option<G17PPendingPrimaryControl>,render_slots:RenderSlots,
 // Shared FList: the UMA lifecycle lives in the manager, indexed by each pair's pool.
 render_flist_owners:Vec<Option<Uma>>}
impl Manager{
 fn new()->Self{Self{active_render_grow:None,pending_render_grows:KVec::with_capacity(128,()).unwrap(),resources:g17_resources::G17ResourceOwner::new(),accepted_firmware_recovery_generation:9,
 pending_primary_control:None,render_slots:RenderSlots{slots:[Some(Box::new(Parked{flist_index:0,preparing:false,tracker:Some(Tracker{hardware_buffer_id:2}),queued_tracker:None,hardware_buffer_id:Some(2),storage:Some(Box::new(g17_resources::G17PUserRenderStorage::new(2,32,0))),bind:Some(G17PRenderRootOwner{context:10,vm:mmu::Vm{blocks:Cell::new(32),fail:Cell::new(false)}}),hardware_buffer_published:true})),Some(Box::new(Parked{flist_index:1,preparing:false,tracker:Some(Tracker{hardware_buffer_id:3}),queued_tracker:None,hardware_buffer_id:Some(3),storage:Some(Box::new(g17_resources::G17PUserRenderStorage::new(3,32,1))),bind:Some(G17PRenderRootOwner{context:11,vm:mmu::Vm{blocks:Cell::new(32),fail:Cell::new(false)}}),hardware_buffer_published:true}))]},render_flist_owners:vec![None,None]}}
 fn pending_render_usc_freelist_release(&self)->Option<()>{None}
''' + '\n'.join(item('g17_manager.rs',m) for m in ['    pub(crate) fn queue_render_tvb_grow(', '    pub(crate) fn take_render_grow(', '    pub(crate) fn return_render_grow(', '    fn publish_render_tvb_grow_reply(']) + item('g17_manager.rs', '    pub(crate) fn primary_device_control_backpressured(') + item('g17_manager.rs', '    pub(crate) fn flush_pending_primary_device_control(') + item('g17_manager.rs', '    pub(crate) fn note_primary_device_control_backpressure(') + item('g17_manager.rs', '    fn render_grow_owner_preparing(') + item('g17_manager.rs', '    pub(crate) fn replay_prepared_render_grows(') + item('g17_manager.rs', '    pub(crate) fn service_render_tvb_grow(') + item('g17_manager.rs', '    pub(crate) fn rekick_parked_render_grows(') + r'''
}''' + item('g17_manager.rs','pub(crate) struct G17PRenderGrow {') + item('g17_manager.rs','impl G17PRenderGrow {') + r'''
fn request()->g17_completion::G17PFirmwareTvbGrowEvent{g17_completion::G17PFirmwareTvbGrowEvent{vm_slot:10,buffer_slot:2,counter:0x12345678,subpipe:63,halt_count:9}}
#[test]fn detached_grow_keeps_event_queue_live_deduplicates_active_and_restores_exact_graph(){
 let mut m=Manager::new();let first=request();let second=g17_completion::G17PFirmwareTvbGrowEvent{buffer_slot:3,vm_slot:11,..first};
 m.queue_render_tvb_grow(first).unwrap();let mut work=m.take_render_grow().unwrap().unwrap();
 assert!(m.render_slots.get(0).preparing);assert!(m.render_slots.get(0).storage.is_none());
 m.queue_render_tvb_grow(first).unwrap();assert!(m.pending_render_grows.is_empty());
 m.queue_render_tvb_grow(second).unwrap();assert_eq!(m.pending_render_grows.len(),1);
 assert!(m.take_render_grow().unwrap().is_none());
 let result=work.grow(&());assert_eq!(result,Ok(true));
 assert_eq!(m.return_render_grow(work,result,true),Ok(G17PTvbGrowDisposition::Published));
 assert!(!m.render_slots.get(0).preparing);assert_eq!(m.render_slots.get(0).storage.as_ref().unwrap().tvb_capacity(),42);
 m.resources.credit=true;let mut work=m.take_render_grow().unwrap().unwrap();let result=work.grow(&());m.return_render_grow(work,result,true).unwrap();
 assert_eq!(m.resources.replies,vec![first.reply(true),second.reply(true)]);
}
#[test]fn detached_grow_stale_commit_keeps_graph_and_suppresses_reply(){
 for invalidate_epoch in [false,true]{let mut m=Manager::new();m.queue_render_tvb_grow(request()).unwrap();let mut work=m.take_render_grow().unwrap().unwrap();let result=work.grow(&());
 if !invalidate_epoch{m.accepted_firmware_recovery_generation+=1;}
 assert_eq!(m.return_render_grow(work,result,!invalidate_epoch),Ok(G17PTvbGrowDisposition::IgnoredStale));
 assert!(m.resources.replies.is_empty());assert!(!m.render_slots.get(0).preparing);assert!(m.render_slots.get(0).storage.is_some());assert!(m.active_render_grow.is_none());}
}
#[test]fn detached_grow_ring_full_preserves_reply_and_does_not_allocate_next_request(){
 let mut m=Manager::new();let first=request();m.queue_render_tvb_grow(first).unwrap();let mut work=m.take_render_grow().unwrap().unwrap();let result=work.grow(&());m.resources.credit=false;
 assert_eq!(m.return_render_grow(work,result,true),Ok(G17PTvbGrowDisposition::Deferred));
 let second=g17_completion::G17PFirmwareTvbGrowEvent{buffer_slot:3,vm_slot:11,..first};m.queue_render_tvb_grow(second).unwrap();assert!(m.take_render_grow().unwrap().is_none());
 assert_eq!(m.pending_primary_control.as_ref().unwrap().record,first.reply(true));assert_eq!(m.render_slots.get(1).storage.as_ref().unwrap().tvb_capacity(),32);
 m.resources.credit=true;m.flush_pending_primary_device_control().unwrap();assert_eq!(m.resources.replies,vec![first.reply(true)]);
 m.resources.credit=true;assert!(m.take_render_grow().unwrap().is_some());
}
#[test]fn detached_grow_failure_replies_false_and_returns_borrow(){
 let mut m=Manager::new();m.render_slots.get(0).bind.as_ref().unwrap().vm.fail.set(true);m.queue_render_tvb_grow(request()).unwrap();let mut work=m.take_render_grow().unwrap().unwrap();let result=work.grow(&());assert!(result.is_err());m.return_render_grow(work,result,true).unwrap();
 assert_eq!(m.resources.replies,vec![request().reply(false)]);assert!(!m.render_slots.get(0).preparing);assert!(m.active_render_grow.is_none());
}
#[test]fn tvb_raw_failure_event_decodes_and_replies_with_exact_native_bytes(){
 let mut raw=[0u8;72];for (i,w) in [0xa00000006u64,2,0,0,0,0,0,1,1].into_iter().enumerate(){raw[i*8..i*8+8].copy_from_slice(&w.to_le_bytes())}
 let r=g17_completion::G17PFirmwareTvbGrowEvent::decode(&raw).unwrap();
 assert_eq!((r.vm_slot,r.buffer_slot,r.counter,r.subpipe,r.halt_count),(10,2,0,0,0));
 for grew in [false,true]{let reply=r.reply(grew);let words:Vec<u64>=reply.chunks_exact(8).map(|b|u64::from_le_bytes(b.try_into().unwrap())).collect();assert_eq!(words,vec![8|((grew as u64)<<32),0xa00000002,0,0,0,0,0,0])}
}
#[test]fn tvb_packed_generation_and_nonzero_fields_survive_poisoned_tail(){
 let r=request();let mut raw=[0xa5;72];raw[..4].copy_from_slice(&6u32.to_le_bytes());
 for (off,v) in [(4,u32::MAX),(8,2),(12,r.counter),(16,r.subpipe)]{raw[off..off+4].copy_from_slice(&v.to_le_bytes())}
 let generation=0x8877665544332211u64;raw[20..28].copy_from_slice(&generation.to_le_bytes());
 let decoded=g17_completion::G17PFirmwareTvbGrowEvent::decode(&raw).unwrap();let reply=decoded.reply(true);
 assert_eq!(u64::from_le_bytes(reply[24..32].try_into().unwrap()),generation);
 assert_eq!(&reply[12..24],&[255,255,255,255,0x78,0x56,0x34,0x12,63,0,0,0]);
 assert!(reply[32..].iter().all(|v|*v==0));assert!(decoded.matches_owner(generation,2,55));
 for (off,value) in [(0,7u32),(4,0xfffffffe),(8,127),(16,64)]{let mut bad=raw;bad[off..off+4].copy_from_slice(&value.to_le_bytes());assert!(g17_completion::G17PFirmwareTvbGrowEvent::decode(&bad).is_err())}
 for size in [0,20,28,64,71]{assert!(g17_completion::G17PFirmwareTvbGrowEvent::decode(&raw[..size]).is_err())}
}
#[test]fn tvb_manager_routes_both_slots_and_preserves_request_identity(){
 for (id,context) in [(2,10),(3,11)]{let mut m=Manager::new();let r=g17_completion::G17PFirmwareTvbGrowEvent{buffer_slot:id,vm_slot:context,..request()};assert_eq!(m.service_render_tvb_grow(r),Ok(G17PTvbGrowDisposition::Published));assert_eq!(m.resources.replies,vec![r.reply(true)]);assert_eq!(m.render_slots.get(0).hardware_buffer_id,Some(2));assert_eq!(m.render_slots.get(0).storage.as_ref().unwrap().tvb_capacity(),if id==2{42}else{32});assert_eq!(m.render_slots.slots[1].as_ref().unwrap().storage.as_ref().unwrap().tvb_capacity(),if id==3{42}else{32})}
}
#[test]fn tvb_stale_generation_never_grows_or_publishes(){
 for halt in [0,8,10,u64::MAX]{let mut m=Manager::new();assert_eq!(m.service_render_tvb_grow(g17_completion::G17PFirmwareTvbGrowEvent{halt_count:halt,..request()}),Ok(G17PTvbGrowDisposition::IgnoredStale));assert!(m.resources.replies.is_empty());assert_eq!(m.render_slots.get(0).storage.as_ref().unwrap().tvb_capacity(),32)}
}
#[test]fn tvb_wrong_owner_and_allocation_failure_reply_false(){
 for failure in 0..4{let mut m=Manager::new();let mut r=request();match failure{0=>r.buffer_slot=126,1=>r.vm_slot=11,2=>{{let slot=m.render_slots.get_mut(0);slot.tracker=None;slot.hardware_buffer_published=false}},_=>m.render_slots.get(0).bind.as_ref().unwrap().vm.fail.set(true)};assert_eq!(m.service_render_tvb_grow(r),Ok(G17PTvbGrowDisposition::Published));assert_eq!(m.resources.replies,vec![r.reply(false)]);assert_eq!(m.render_slots.get(0).storage.as_ref().unwrap().tvb_capacity(),32)}
}
#[test]fn tvb_full_control_ring_defers_exact_encoded_reply_without_replaying_growth(){
 let mut m=Manager::new();let r=request();m.resources.credit=false;
 assert_eq!(m.service_render_tvb_grow(r),Ok(G17PTvbGrowDisposition::Deferred));
 assert!(m.primary_device_control_backpressured());assert!(m.resources.replies.is_empty());
 assert_eq!(m.render_slots.get(0).storage.as_ref().unwrap().tvb_capacity(),42);
 let next=g17_completion::G17PFirmwareTvbGrowEvent{counter:r.counter+1,..r};
 assert_eq!(m.service_render_tvb_grow(next),Err(EBUSY));
 assert_eq!(m.render_slots.get(0).storage.as_ref().unwrap().tvb_capacity(),42);
 for _ in 0..4{assert!(matches!(m.flush_pending_primary_device_control(),Ok(G17PPendingPrimaryControlFlush::Blocked)));m.note_primary_device_control_backpressure().unwrap();}
 assert_eq!(m.render_slots.get(0).storage.as_ref().unwrap().tvb_capacity(),42);
 m.resources.credit=true;
 assert!(matches!(m.flush_pending_primary_device_control(),Ok(G17PPendingPrimaryControlFlush::Published(_))));
 assert!(!m.primary_device_control_backpressured());assert_eq!(m.resources.replies,vec![r.reply(true)]);
 assert!(matches!(m.flush_pending_primary_device_control(),Ok(G17PPendingPrimaryControlFlush::Empty)));
 assert_eq!(m.resources.replies.len(),1);
}
#[test]fn tvb_deferred_reply_rejects_cross_recovery_replay(){
 let mut m=Manager::new();m.resources.credit=false;
 assert_eq!(m.service_render_tvb_grow(request()),Ok(G17PTvbGrowDisposition::Deferred));
 m.accepted_firmware_recovery_generation+=1;m.resources.credit=true;
 assert!(matches!(m.flush_pending_primary_device_control(),Err(EIO)));
 assert!(m.primary_device_control_backpressured());assert!(m.resources.replies.is_empty());
}
#[test]fn two_tvb_events_remain_ordered_across_one_deferred_reply(){
 let mut m=Manager::new();let first=request();m.resources.credit=false;
 assert_eq!(m.service_render_tvb_grow(first),Ok(G17PTvbGrowDisposition::Deferred));
 m.resources.credit=true;assert!(matches!(m.flush_pending_primary_device_control(),Ok(G17PPendingPrimaryControlFlush::Published(_))));
 let second=g17_completion::G17PFirmwareTvbGrowEvent{counter:first.counter+1,..first};m.resources.credit=true;
 assert_eq!(m.service_render_tvb_grow(second),Ok(G17PTvbGrowDisposition::Published));
 assert_eq!(m.resources.replies,vec![first.reply(true),second.reply(true)]);
 assert_eq!(m.render_slots.get(0).storage.as_ref().unwrap().tvb_capacity(),52);
}
#[test]fn deferred_reply_worker_budget_is_bounded(){
 let mut m=Manager::new();m.resources.credit=false;
 assert_eq!(m.service_render_tvb_grow(request()),Ok(G17PTvbGrowDisposition::Deferred));
 for _ in 0..G17P_PRIMARY_CONTROL_RETRY_LIMIT{assert_eq!(m.note_primary_device_control_backpressure(),Ok(()));}
 assert_eq!(m.note_primary_device_control_backpressure(),Err(ETIMEDOUT));
 assert!(m.primary_device_control_backpressured());
}

#[test]fn borrowed_graph_keeps_requests_while_other_pairs_progress(){
 let mut m=Manager::new();let r=request();
 m.render_slots.get_mut(0).preparing=true;
 let storage=m.render_slots.get_mut(0).storage.take().unwrap();
 assert_eq!(m.service_render_tvb_grow(r),Ok(G17PTvbGrowDisposition::PreparationDeferred));
 assert_eq!(m.service_render_tvb_grow(r),Ok(G17PTvbGrowDisposition::PreparationDeferred));
 assert_eq!(m.pending_render_grows.len(),1);assert!(m.resources.replies.is_empty());
 let other=g17_completion::G17PFirmwareTvbGrowEvent{vm_slot:11,buffer_slot:3,..r};
 assert_eq!(m.service_render_tvb_grow(other),Ok(G17PTvbGrowDisposition::Published));
 assert_eq!(m.resources.replies,vec![other.reply(true)]);
 let mut wakes=0;m.replay_prepared_render_grows(&mut||{wakes+=1;Ok(())}).unwrap();
 assert_eq!(wakes,0);assert_eq!(m.pending_render_grows.len(),1);
 m.render_slots.get_mut(0).storage=Some(storage);m.render_slots.get_mut(0).preparing=false;m.resources.credit=true;
 m.replay_prepared_render_grows(&mut||{wakes+=1;Ok(())}).unwrap();
 assert_eq!(wakes,1);assert!(m.pending_render_grows.is_empty());
 assert_eq!(m.resources.replies,vec![other.reply(true),r.reply(true)]);
 assert_eq!(m.render_slots.get(0).storage.as_ref().unwrap().tvb_capacity(),42);
}
#[test]fn deferred_preparation_retains_counter_and_subpipe_and_rejects_old_epoch(){
 let mut m=Manager::new();m.render_slots.get_mut(0).preparing=true;
 let first=request();let second=g17_completion::G17PFirmwareTvbGrowEvent{counter:77,subpipe:4,..first};
 for r in [first,second]{assert_eq!(m.service_render_tvb_grow(r),Ok(G17PTvbGrowDisposition::PreparationDeferred));}
 assert_eq!(&m.pending_render_grows[..],&[first,second]);
 m.accepted_firmware_recovery_generation+=1;
 m.replay_prepared_render_grows(&mut||panic!("no old reply")).unwrap();
 assert!(m.pending_render_grows.is_empty());assert!(m.resources.replies.is_empty());
 assert_eq!(m.render_slots.get(0).storage.as_ref().unwrap().tvb_capacity(),32);
}
#[test]fn prepared_replay_handles_control_credit_without_growing_twice(){
 let mut m=Manager::new();m.render_slots.get_mut(0).preparing=true;let r=request();
 m.service_render_tvb_grow(r).unwrap();m.render_slots.get_mut(0).preparing=false;m.resources.credit=false;
 m.replay_prepared_render_grows(&mut||panic!("no credit")).unwrap();
 assert!(m.pending_render_grows.is_empty());assert!(m.pending_primary_control.is_some());
 assert_eq!(m.render_slots.get(0).storage.as_ref().unwrap().tvb_capacity(),42);
 m.resources.credit=true;m.flush_pending_primary_device_control().unwrap();
 assert_eq!(m.resources.replies,vec![r.reply(true)]);
 assert_eq!(m.render_slots.get(0).storage.as_ref().unwrap().tvb_capacity(),42);
}
'''
with tempfile.TemporaryDirectory() as directory:
    source = Path(directory) / 'test.rs'
    binary = Path(directory) / 'test'
    source.write_text(code)
    subprocess.run(['rustc', '--edition=2021', '--test', str(source), '-o', str(binary)], check=True)
    subprocess.run([str(binary), '--test-threads=1'], check=True)
print('PASS production event-6 codec, owner dispatch, live TVB publication, and failure semantics')
