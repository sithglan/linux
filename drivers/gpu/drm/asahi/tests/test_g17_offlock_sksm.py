#!/usr/bin/env python3
"""Compare the extracted entry writer to the real firmware codec."""
from pathlib import Path
import subprocess,tempfile,json
r=Path(__file__).resolve().parents[1];m=(r/'g17_manager.rs').read_text();l=(r/'g17_live_boot.rs').read_text()
def item(s,key):
 a=s.index(key);b=s.index('{',a)+1;n=1
 while n:n+=(s[b]=='{')-(s[b]=='}');b+=1
 return s[a:b]
prepare=item(m,'impl G17PRenderRestage {')
assert prepare.index('restage_command(')<prepare.index('prepare_render_sksm_entry(')<prepare.index('self.reservation.prepared_sksm = Some(')
assert prepare.count('prepare_render_sksm_entry(')==2
stage=item(m,'    fn stage_render_groups_with_sksm(')
assert stage.index('prepared.before != [queues.tiling.producer, queues.fragment.producer]')<stage.index('.stage_fragment_completion_scratch(')
commit=item(m,'    fn publish_render_sksm_group(')
assert commit.index('if before != queue.producer { return Err(EIO); }') < commit.index('queue.producer = after;')
assert 'prepare_render_sksm_entry(storage' in commit
assert 'dependencies.entries[..self.dependencies.count]' in prepare
assert 'let dependencies = self.render_submission_dependencies(completion, command)?' in item(l,'    pub(crate) fn take_render_restage(')
codec=(r/'g17_submission.rs').read_text().split('#[cfg(test)]\nmod tests {',1)[0]
a=codec.index('#[cfg(not(test))]\nuse crate::g17_manager::G17SksmScratchGeometry;')
b=codec.index('/// The submission producer subset',a)
codec=codec[:a]+'use crate::G17SksmScratchGeometry;\n\n'+codec[b:]
geometry='#[derive(Debug,Clone,Copy,PartialEq,Eq)]\n'+item(m,'pub(crate) struct G17SksmScratchGeometry {')
geometry += item(m,'pub(crate) struct G17SksmScratchOffsets {') + item(m,'impl G17SksmScratchGeometry {') + 'enum ConstructionError { SksmScratchOffsetOverflow }'
program=r'''
#![allow(dead_code,unused_variables)]
use std::sync::atomic::{fence,Ordering};
type Result<T=()> = std::result::Result<T,i32>;const EINVAL:i32=22;const EIO:i32=5;const ENODEV:i32=19;
macro_rules! pr_info {($($args:tt)*)=>{()};}
macro_rules! dev_info {($($args:tt)*)=>{()};}
struct AsahiDevice;
struct G17PManagerConstruction{render_slots:Slots}
struct Slot{storage:Option<Box<g17_resources::G17PUserRenderStorage>>,sksm:Option<Queues>,submission_ordinal:u64,auxiliary_timestamp:u64}
struct Queue{tiling:bool,producer:g17_submission::G17PClKickProducerState}
struct Queues{tiling:Queue,fragment:Queue,scratch:G17SksmScratchGeometry,uat_owner:u64}
struct Slots(Slot);impl Slots{fn get(&self,_:u8)->&Slot{&self.0}fn get_mut(&mut self,_:u8)->&mut Slot{&mut self.0}}
struct Uat;impl Uat{fn ttb_base(&self)->u64{1}}
impl G17PManagerConstruction{fn sksm_scratch(&self)->G17SksmScratchGeometry{G17SksmScratchGeometry{write0_base:0,write1_base:0,stride:0}}fn uat(&self)->Uat{Uat}}
impl G17PManagerConstruction{fn render_stage_fail(_: &str,e:i32)->i32{e}}
mod util{pub fn g17p_verbose()->bool{false}}
mod g17_resources{
 use super::*;
 pub struct G17PUserRenderStorage{pub writes:Vec<(bool,usize,usize,Vec<u8>)>,pub fail:bool}
 impl G17PUserRenderStorage{
  pub fn descriptor_high_va(&self,t:bool)->Result<u64>{Ok(if t{0xffffa00000100000}else{0xffffa00000200000})}
  pub fn descriptor_low_va(&self,t:bool)->Result<u64>{Ok(if t{0x100000}else{0x200000})}
  pub fn work_item_gpu_vas(&self,_:bool)->Result<[u64;3]>{Ok([1,2,3])}
  pub fn tiling_mcache(&self)->Option<g17_submission::G17PClMcacheAperture>{None}
  pub fn fragment_mcache(&self)->Option<g17_submission::G17PClMcacheAperture>{None}
  pub fn write_sksm_entry(&mut self,t:bool,o:usize,z:usize,b:&[u8])->Result{if self.fail{return Err(EIO)}self.writes.push((t,o,z,b.to_vec()));Ok(())}
  pub fn sksm_entry_low_va(&self,_:bool)->u64{0}
 }
}
''' + geometry+'\nmod g17_submission {\n'+codec+'\n}\n'+f'#[path={json.dumps(str(r/"g17_config.rs"))}] mod g17_config;\n' + item(m,'fn prepare_render_sksm_entry(') + r'''
#[test]fn prepared_entry_matches_codec_and_keeps_input_frontier_unchanged(){
 for tiling in [false,true]{for stamp in [0,1,255,256,65535]{
  let mut storage=g17_resources::G17PUserRenderStorage{writes:vec![],fail:false};
  let mut producer=g17_submission::T8140_G17P_HAL200_SKSM_QUEUE_GEOMETRY.initial_producer_state(if tiling{8}else{9},if tiling{0}else{1},g17_submission::G17PClQosConfig{queue_byte_32:5,class_40:2,word_48:0}).unwrap();
  producer.current_timestamp=stamp;producer.base_timestamp=stamp;producer.previous_valid=true;producer.previous_timestamp=stamp.wrapping_sub(1)&g17_submission::KICK_TS_MASK;
  let before=producer;let barriers=[g17_submission::g17p_render_barrier_from_timestamp(37,12).unwrap()];
  let after=prepare_render_sksm_entry(&mut storage,&AsahiDevice,tiling,producer,7,99,0x300000,&barriers).unwrap();
  assert_eq!(producer,before);assert_eq!(storage.writes.len(),1);
  let state=g17_submission::prepare_g17p_render_cl_command_state(storage.descriptor_low_va(tiling).unwrap(),tiling).unwrap();
  let expected=g17_submission::prepare_g17p_cl_kick_entry(producer,g17_submission::G17PClKickEntryOperands{
   descriptor_flag_4c:!tiling&&(g17_config::RENDER_HEADER_FLAGS&1!=0),descriptor_flag_5c8:g17_config::RENDER_HEADER_FLAGS&2!=0,
   converted_command_timestamp:stamp,barriers:&barriers,mcache:None,payload:[storage.descriptor_high_va(tiling).unwrap(),0x300000],event_mask:state.event_mask,rce_kind:0,rce_bindings:state.rce_bindings,auxiliary:99,
  }).unwrap();
  assert_eq!(storage.writes[0].1,expected.entry_offset as usize);assert_eq!(storage.writes[0].2,expected.zero_length as usize);assert_eq!(storage.writes[0].3,expected.entry);
  let mut expected_after=expected.producer_after;expected_after.last_add_kicks_timestamp=expected.current_timestamp;assert_eq!(after,expected_after);
 }}
}
#[test]fn write_failure_never_advances_input_producer(){let mut storage=g17_resources::G17PUserRenderStorage{writes:vec![],fail:true};let producer=g17_submission::T8140_G17P_HAL200_SKSM_QUEUE_GEOMETRY.initial_producer_state(9,1,g17_submission::G17PClQosConfig{queue_byte_32:5,class_40:2,word_48:0}).unwrap();let before=producer;assert_eq!(prepare_render_sksm_entry(&mut storage,&AsahiDevice,false,producer,0,99,0x300000,&[]),Err(EIO));assert_eq!(producer,before);assert!(storage.writes.is_empty());}
'''
program += '\nimpl G17PManagerConstruction {\n' + commit + '\n}\n' + r'''
#[test]fn actual_commit_rejects_stale_frontier_and_never_reencodes_prepared_entry(){
 let before=g17_submission::T8140_G17P_HAL200_SKSM_QUEUE_GEOMETRY.initial_producer_state(9,1,g17_submission::G17PClQosConfig{queue_byte_32:5,class_40:2,word_48:0}).unwrap();
 let mut storage=g17_resources::G17PUserRenderStorage{writes:vec![],fail:false};
 let after=prepare_render_sksm_entry(&mut storage,&AsahiDevice,false,before,7,99,0x300000,&[]).unwrap();
 let mut m=G17PManagerConstruction{render_slots:Slots(Slot{storage:Some(Box::new(storage)),sksm:Some(Queues{scratch:G17SksmScratchGeometry{write0_base:0,write1_base:0,stride:0},uat_owner:1,tiling:Queue{tiling:true,producer:before},fragment:Queue{tiling:false,producer:before}}),submission_ordinal:7,auxiliary_timestamp:99})};
 let mut wrong=before;wrong.current_timestamp+=1;
 assert_eq!(m.publish_render_sksm_group(0,&AsahiDevice,false,0x300000,&[],Some((wrong,after))),Err(EIO));
 assert_eq!(m.render_slots.0.sksm.as_ref().unwrap().fragment.producer,before);
 assert_eq!(m.render_slots.0.storage.as_ref().unwrap().writes.len(),1);
 assert_eq!(m.publish_render_sksm_group(0,&AsahiDevice,false,0x300000,&[],Some((before,after))),Ok(before.current_timestamp));
 assert_eq!(m.render_slots.0.sksm.as_ref().unwrap().fragment.producer,after);
 assert_eq!(m.render_slots.0.storage.as_ref().unwrap().writes.len(),1);
}
'''
with tempfile.TemporaryDirectory() as tmp:
 p=Path(tmp)/'sksm.rs';p.write_text(program);exe=Path(tmp)/'sksm'
 subprocess.run(['rustc','--edition=2021','--test',str(p),'-o',str(exe)],check=True)
 subprocess.run([str(exe),'prepared_entry_matches_codec'],check=True)
 subprocess.run([str(exe),'write_failure_never_advances'],check=True)
 subprocess.run([str(exe),'actual_commit_rejects'],check=True)
print('PASS production SKSM entry writer versus actual codec: both engines, timestamp wrap, explicit dependencies, write-failure frontier retention')
