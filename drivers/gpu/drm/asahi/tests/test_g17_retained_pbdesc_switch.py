#!/usr/bin/env python3
"""Retained owner switches execute production PBDesc paths without cursor reset."""
from pathlib import Path
import subprocess,tempfile,os
p=Path(__file__).resolve().parents[1]
def item(s,marker):
 a=s.index(marker);i=s.index('{',a)+1;depth=1
 while depth:
  depth+=(s[i]=='{')-(s[i]=='}');i+=1
 return s[a:i]
r=Path(os.environ.get('G17_PBDESC_RESOURCE_SOURCE',str(p/'g17_resources.rs'))).read_text();codec=(p/'g17_submission.rs').read_text()
functions='\n'.join(item(codec,x) for x in [
 'pub(crate) const fn g17p_pbdesc_segment(',
 'pub(crate) const fn encode_g17p_pbdesc(',
 'pub(crate) const fn decode_g17p_pbdesc(',
 'pub(crate) const fn g17p_pbdesc_offset('])
source=r'''
#![allow(dead_code,unused_variables)]
type Result<T=()>=std::result::Result<T,i32>; const EINVAL:i32=22; const EIO:i32=5;
macro_rules! dev_info {($($tt:tt)*)=>{()}}
mod util{pub fn g17p_verbose()->bool{false}}
mod g17_submission {
 pub(crate) const G17P_PBDESC_ENTRY_SIZE:usize=16;
 #[derive(Debug,Copy,Clone,Default,PartialEq,Eq)]
'''+item(codec,'pub(crate) struct G17PPbDescRingState {')+'\n'+functions+r'''
}
struct Buffer(Vec<u8>);impl Buffer{fn with_bytes_mut<R>(&mut self,f:impl FnOnce(&mut[u8])->Result<R>)->Result<R>{f(&mut self.0)}}
struct Resources{pb_descriptor_table:Buffer}
struct Manager{resources:Resources}
struct G17PUserRenderStorage{descriptor:Option<(u32,u64,u32)>}
impl G17PUserRenderStorage{fn parameter_buffer_descriptor(&self)->Result<Option<(u32,u64,u32)>>{Ok(self.descriptor)}}
impl Manager{
 fn stage_render_parameter_buffer_page_list(&mut self,_:&mut G17PUserRenderStorage)->Result{Ok(())}
'''+item(r,'    pub(crate) fn stage_render_parameter_buffer_descriptor(')+'\n'+item(r,'    pub(crate) fn stage_render_parameter_buffer_view(')+r'''
}
fn manager()->Manager{Manager{resources:Resources{pb_descriptor_table:Buffer(vec![0;64])}}}
fn put(m:&mut Manager,i:usize,w:[u32;4]){for (j,v) in w.into_iter().enumerate(){m.resources.pb_descriptor_table.0[i*16+j*4..i*16+j*4+4].copy_from_slice(&v.to_le_bytes());}}
fn get(m:&Manager,i:usize)->[u32;4]{std::array::from_fn(|j|u32::from_le_bytes(m.resources.pb_descriptor_table.0[i*16+j*4..i*16+j*4+4].try_into().unwrap()))}
#[test]fn alternating_retained_owners_preserve_live_cursors_flags_and_reserved_bits(){
 use g17_submission::*;let mut m=manager();
 let mut owners=[G17PUserRenderStorage{descriptor:Some((1,0x1000190000,4096))},G17PUserRenderStorage{descriptor:Some((2,0x1000290000,8192))}];
 for owner in &owners{m.stage_render_parameter_buffer_descriptor(owner.descriptor).unwrap();}
 for n in 0..4096u32{
  for owner in &mut owners{
   let (id,addr,count)=owner.descriptor.unwrap();
   let live=G17PPbDescRingState{counter_a:(0x3fffff-n)&0x3fffff,counter_b:n,flag_high:n&1,flag_low:(n>>1)&1};
   let words=encode_g17p_pbdesc([0xfffffff6,0x0fc00000,0xffc00000,0x7fc00000],addr,count,live);
   put(&mut m,id as usize,words);
   let all=m.resources.pb_descriptor_table.0.clone();
   m.stage_render_parameter_buffer_view(owner).unwrap();
   assert_eq!(m.resources.pb_descriptor_table.0,all,"owner switch rewrote firmware state");
   assert_eq!(decode_g17p_pbdesc(get(&m,id as usize)),live);
  }
 }
}
#[test]fn lost_descriptor_identity_is_rejected_without_repairing_or_resetting(){
 let mut m=manager();let mut owner=G17PUserRenderStorage{descriptor:Some((1,0x1000190000,4096))};
 m.stage_render_parameter_buffer_descriptor(owner.descriptor).unwrap();
 for wrong in [(1,0x1000290000,4096),(1,0x1000190000,2048)]{
  owner.descriptor=Some(wrong);let before=m.resources.pb_descriptor_table.0.clone();
  assert_eq!(m.stage_render_parameter_buffer_view(&mut owner),Err(EIO));assert_eq!(m.resources.pb_descriptor_table.0,before);
 }
 owner.descriptor=Some((4,0x1000190000,4096));assert_eq!(m.stage_render_parameter_buffer_view(&mut owner),Err(EINVAL));
}
#[test]fn live_growth_preserves_smaller_loaded_inventory_and_all_firmware_state(){
 use g17_submission::*;let mut m=manager();
 let mut owner=G17PUserRenderStorage{descriptor:Some((1,0x1000190000,128))};
 m.stage_render_parameter_buffer_descriptor(owner.descriptor).unwrap();
 let state=G17PPbDescRingState{counter_a:127,counter_b:3,flag_high:1,flag_low:1};
 let live=encode_g17p_pbdesc([0xfffffff6,0x0fc00000,0xffc00000,0x7fc00000],0x1000190000,128,state);
 put(&mut m,1,live);
 // Actual failing hardware sequence: live type6 growth32->42 blocks changes
 // owned capacity128->168 pages while retaining the loaded128-page image.
 for capacity in [168,208,448,1328]{
  owner.descriptor=Some((1,0x1000190000,capacity));
  let before=m.resources.pb_descriptor_table.0.clone();
  m.stage_render_parameter_buffer_view(&mut owner).unwrap();
  assert_eq!(m.resources.pb_descriptor_table.0,before);
  assert_eq!(get(&m,1),live);
 }
 // An impossible inventory larger than the owned backing is still rejected.
 put(&mut m,1,encode_g17p_pbdesc(live,0x1000190000,1329,state));
 let before=m.resources.pb_descriptor_table.0.clone();
 assert_eq!(m.stage_render_parameter_buffer_view(&mut owner),Err(EIO));
 assert_eq!(m.resources.pb_descriptor_table.0,before);
}
#[test]fn cold_new_owner_still_initializes_cursors_and_returns_rollback_image(){
 let mut m=manager();let previous=[0xffffffff;4];put(&mut m,1,previous);
 assert_eq!(m.stage_render_parameter_buffer_descriptor(Some((1,0x1000190000,4096))),Ok(Some((1,previous))));
 assert_eq!(g17_submission::decode_g17p_pbdesc(get(&m,1)),Default::default());
}
'''
source += '\n' + item(r, 'fn g17p_live_word_bounds(') + '\n' + item(r, 'fn g17p_live_read_u32(')
with tempfile.TemporaryDirectory() as d:
 f=Path(d)/'test.rs';f.write_text(source);exe=Path(d)/'test'
 subprocess.run(['rustc','--edition=2021','--test',str(f),'-o',str(exe)],check=True)
 subprocess.run([str(exe)],check=True)
