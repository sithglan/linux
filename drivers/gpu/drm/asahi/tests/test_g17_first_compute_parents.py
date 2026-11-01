#!/usr/bin/env python3
"""Encode the actual first-bind callsite with supplied producer dependencies."""
from pathlib import Path
import os
import subprocess
import tempfile
import test_g17_compute_scs_encoding as support

root = Path(__file__).resolve().parents[1]
manager = Path(os.environ.get('G17_FIRST_BIND_MANAGER_SOURCE', str(root / 'g17_manager.rs'))).read_text()
first = support.block(manager, '    fn submit_translated_first_bind_channel(')
start = first.index('        let prepared_entry = g17_submission::prepare_g17p_cl_kick_entry(')
end = first.index('.map_err(|_| EINVAL)?;', start) + len('.map_err(|_| EINVAL)?;')
call = first[start:end]
support.HARNESS = ''
source = support.rust_source() + r'''
use g17_submission::*;
type Result<T> = std::result::Result<T, ()>;
const EINVAL: () = ();
struct Queue { producer: G17PClKickProducerState, compute: Compute }
struct Compute;
impl Compute {fn queue_record_gpu_va(&self)->u64{0xffff_fc20_1234_0000}}
struct Command;
impl Command {fn scs_enabled(&self)->bool{false}}
struct Context<'a>{barriers:&'a[G17PClBarrierDependency],auxiliary:u64}
fn actual_first_entry(barriers:&[G17PClBarrierDependency])->Result<PreparedG17PClKickEntry>{
 let geometry=T8140_G17P_HAL200_SKSM_QUEUE_GEOMETRY;
 let queue=Queue{producer:geometry.initial_producer_state(13,2,G17PClQosConfig{
     queue_byte_32:0,class_40:0,word_48:0xffff}).unwrap(),compute:Compute};
 let command=Command;
 let context=Context{barriers,auxiliary:0x003f_ffff_ffff_ffff};
 let descriptor_high=0xffff_fc20_0010_0000;
 let command_state=prepare_g17p_compute_cl_command_state(0x70_0010_0000,0,0).unwrap();
''' + call + r'''
 Ok(prepared_entry)
}
fn word(bytes:&[u8],offset:usize)->u64{u64::from_le_bytes(bytes[offset..offset+8].try_into().unwrap())}
fn parent(qid:u8,stamp:u64)->G17PClBarrierDependency{
 let mut d=g17p_render_barrier_from_timestamp(stamp,qid).unwrap();d.native_timestamp=Some(stamp);d
}
fn main(){
 for parents in [vec![],vec![parent(15,1)],vec![parent(15,1),parent(17,1)],
                 vec![parent(15,0xff),parent(17,0x100),parent(19,0xffffffffff)]] {
  let entry=actual_first_entry(&parents).unwrap();
  let layout=G17P_CL_KICK_ENTRY_LAYOUT;
  for (i,p) in parents.iter().enumerate(){
   assert_eq!(word(&entry.entry,layout.dependencies+i*8),
       p.native_timestamp.unwrap() | ((p.queue_id as u64)<<40),
       "first-bind discarded or changed parent {}",i);
  }
  // Exact mask covers explicit parents and the ordinary own-queue predecessor.
  assert_eq!(word(&entry.entry,layout.dependency_control) as u32,
      (1u32<<(parents.len()+1))-1);
  assert_eq!(word(&entry.entry,layout.dependencies+parents.len()*8),13u64<<40);
  assert_eq!(entry.current_timestamp,1);
 }
 let too_many=vec![parent(15,1);G17P_CL_EXPLICIT_DEPENDENCIES_MAX+1];
 assert!(actual_first_entry(&too_many).is_err());
 println!("PASS actual first-bind encoder: independent, one/two/multiple parents, wrapped timestamps, capacity rejection");
}
'''
with tempfile.TemporaryDirectory() as directory:
    p=Path(directory);(p/'test.rs').write_text(source)
    subprocess.run(['rustc','--edition=2021',str(p/'test.rs'),'-o',str(p/'test')],check=True)
    subprocess.run([str(p/'test')],check=True)
