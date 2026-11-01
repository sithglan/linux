#!/usr/bin/env python3
"""Execute actual admission reads/anchor selection, with poison unread surfaces."""
from pathlib import Path
import subprocess,tempfile
root=Path(__file__).resolve().parents[1]
r=(root/'g17_resources.rs').read_text();m=(root/'g17_manager.rs').read_text()
def item(s,key):
 a=s.index(key);b=s.index('{',a)+1;n=1
 while n:n+=(s[b]=='{')-(s[b]=='}');b+=1
 return s[a:b]
read=item(r,'    pub(crate) fn admission_queue_indices(')
anchor=item(m,'    fn render_admission_indices(')
assert all(x not in read for x in ['tsq','timestamps','JOB_LIST'])
stage=item(m,'    fn stage_render_groups_with_sksm(')
begin=item(m,'    fn begin_translated_render_with_hardware_buffer(')
assert '.snapshot(' not in stage and '.snapshot(' not in begin
# Retirement still requires the complete observation, never the admission anchor.
assert '.snapshot(self.ordinal)' in item(m,'impl G17PRenderObservation {')
for mode in [0,2]:
 program=r'''
#![allow(dead_code)]
type Result<T=()> = std::result::Result<T,i32>;const EINVAL:i32=22;const EIO:i32=5;const ENODEV:i32=19;
mod g17_completion{#[derive(Debug,Clone,Copy,PartialEq,Eq)]pub struct QueueIndices{pub done:u32,pub read:u32,pub write:u32}}
mod g17_config{pub const RENDER_NATIVE_DOORBELLS:u32=MODE;}
fn g17p_native_fragment_first_mode(m:u32)->bool{m==2||m==3}
mod g17_resources{use super::*;
'''+'#[derive(Debug,Clone,Copy)]\n'+item(r,'pub(crate) struct G17PRenderQueueIndices {')+r'''
 pub struct Graph{pub raw:Vec<u8>,pub reads:usize,pub fail:bool}
 impl Graph{fn with_bytes_mut<T>(&mut self,f:impl FnOnce(&mut[u8])->Result<T>)->Result<T>{self.reads+=1;if self.fail{return Err(EIO)}f(&mut self.raw)}}
 pub struct Storage{pub graph:Graph}
 impl Storage{const TILING_POINTERS:usize=0;const FRAGMENT_POINTERS:usize=0x80;
'''+read+r'''}
 pub struct Window(u32);impl Window{pub fn target(&self)->u32{self.0}}
 pub fn g17p_render_completion_window(o:u64)->Result<Window>{if o==u64::MAX{return Err(EINVAL)}Ok(Window(((o+1)*4%1024)as u32))}
}
struct Tracker{ordinal:u64,tiling_prefix:u32,fragment_prefix:u32}
struct Slot{storage:Option<Box<g17_resources::Storage>>,tracker:Option<Tracker>}
struct Slots(Slot);impl Slots{fn get_mut(&mut self,s:u8)->&mut Slot{assert_eq!(s,3);&mut self.0}}
struct Manager{render_slots:Slots}
impl Manager{
'''+anchor+r'''}
fn manager()->Manager{
 let mut raw=vec![0xff;0xc4];
 for (at,value) in [(0,12u32),(0x30,16),(0x40,20),(0x80,24),(0xb0,28),(0xc0,32)]{raw[at..at+4].copy_from_slice(&value.to_le_bytes());}
 Manager{render_slots:Slots(Slot{storage:Some(Box::new(g17_resources::Storage{graph:g17_resources::Graph{raw,reads:0,fail:false}})),tracker:None})}
}
#[test]fn cold_reads_only_six_cursors_once_and_propagates_failure(){
 let mut m=manager();let s=m.render_admission_indices(3).unwrap();
 assert_eq!(s.tiling,g17_completion::QueueIndices{done:12,read:16,write:20});
 assert_eq!(s.fragment,g17_completion::QueueIndices{done:24,read:28,write:32});
 let g=&mut m.render_slots.0.storage.as_mut().unwrap().graph;assert_eq!(g.reads,1);g.raw.truncate(0xc3);
 assert!(matches!(m.render_admission_indices(3),Err(EINVAL)));
 m.render_slots.0.storage.as_mut().unwrap().graph.fail=true;assert!(matches!(m.render_admission_indices(3),Err(EIO)));
 m.render_slots.0.storage=None;assert!(matches!(m.render_admission_indices(3),Err(ENODEV)));
}
#[test]fn overlapping_anchor_survives_wrap_without_touching_graph(){
 for ordinal in [0,31,254,255,256,511,65535]{
  let mut m=manager();let target=g17_resources::g17p_render_completion_window(ordinal).unwrap().target();
  m.render_slots.0.tracker=Some(Tracker{ordinal,tiling_prefix:target,fragment_prefix:target});
  m.render_slots.0.storage.as_mut().unwrap().graph.fail=true;
  if g17_config::RENDER_NATIVE_DOORBELLS==0{
   let s=m.render_admission_indices(3).unwrap();assert_eq!(s.tiling,s.fragment);assert_eq!(s.tiling.done,target);
   assert_eq!(m.render_slots.0.storage.as_ref().unwrap().graph.reads,0);
   m.render_slots.0.tracker.as_mut().unwrap().fragment_prefix=target.wrapping_add(1);
   assert!(matches!(m.render_admission_indices(3),Err(EIO)));
  }else{assert!(matches!(m.render_admission_indices(3),Err(EIO)));assert_eq!(m.render_slots.0.storage.as_ref().unwrap().graph.reads,1);}
 }
}
'''
 program=program.replace('=MODE','= '+str(mode))
 with tempfile.TemporaryDirectory() as tmp:
  src=Path(tmp)/'test.rs';src.write_text(program);exe=Path(tmp)/'test'
  subprocess.run(['rustc','--edition=2021','--test',str(src),'-o',str(exe)],check=True)
  subprocess.run([str(exe)],check=True)
print('PASS actual admission cursor reader + queued predecessor: ring wrap, poisoned unused graph, errors, diagnostic fallback; no TSQ/timestamp read in admission')
