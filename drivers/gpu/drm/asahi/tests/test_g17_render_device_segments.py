#!/usr/bin/env python3
"""Exercise actual device-segment reader with exact generation and fallible backing."""
from pathlib import Path
import tempfile, subprocess
from test_g17_compute_scs_encoding import block
root=Path(__file__).resolve().parents[1]
s=(root/'g17_resources.rs').read_text()
names=['G17P_RENDER_DESCRIPTOR_SLOT_COUNT','G17P_RENDER_TA_DESCRIPTOR_STRIDE','G17P_RENDER_3D_DESCRIPTOR_STRIDE','G17P_RENDER_TA_DESCRIPTOR_ARRAY_SIZE','G17P_RENDER_3D_DESCRIPTOR_OFFSET','G17P_RENDER_3D_DESCRIPTOR_ARRAY_SIZE','G17P_RENDER_DESCRIPTOR_STORAGE_SIZE']
constants=[]
for name in names:
    start=s.index('const '+name+':');end=s.index(';',start)+1
    constants.append(s[start:end])
code=r'''
#![allow(dead_code)]
type Result<T=()> = std::result::Result<T,i32>;
const EINVAL:i32=22;const ERANGE:i32=34;
mod mmu {pub const UAT_PGSZ:usize=0x4000;}
mod g17_render {pub const TA_DESCRIPTOR_SIZE:usize=0x9c0;pub const FRAGMENT_DESCRIPTOR_SIZE:usize=0x2240;pub const G17P_MCACHE_MAPPING_SIZE:usize=16;}
mod g17_uapi {pub const UAPI_MAX_ATTACHMENTS:usize=16;}
struct Object {data:Vec<u8>,fail:bool,calls:usize}
impl Object {fn iova(&self)->u64{0xfffffc2000000000}fn with_bytes_mut<T>(&mut self,f:impl FnOnce(&mut [u8])->Result<T>)->Result<T>{self.calls+=1;if self.fail{Err(5)}else{f(&mut self.data)}}}
struct Storage {command_generation_count:usize,render_command_ordinals:[Option<u64>;16],descriptors:Object}
'''+'\n'.join(constants)+'\n'+'\n'.join(block(s,'fn '+name) for name in ['g17p_render_descriptor_slot','g17p_render_ta_descriptor_offset','g17p_render_3d_descriptor_offset','g17p_render_device_bytes'])+'\nimpl Storage {\n'+block(s,'    pub(crate) fn render_device_segments(')+r'''
}
fn main(){
 for ordinal in (0..128u64).chain([1<<32,u64::MAX-1]) {
  let mut x=Storage{command_generation_count:16,render_command_ordinals:[None;16],descriptors:Object{data:vec![0xa5;G17P_RENDER_DESCRIPTOR_STORAGE_SIZE],fail:false,calls:0}};
  x.render_command_ordinals[ordinal as usize%16]=Some(ordinal);
  let ta=g17p_render_ta_descriptor_offset(ordinal);let fr=g17p_render_3d_descriptor_offset(ordinal);
  // Firmware-written adjusted values must be preserved exactly, including
  // zero and reversed pairs; the observation must not invent availability.
  for pairs in [[17,29,31,67],[0,0,0,0],[99,1,800,700],[u64::MAX-7,u64::MAX-1,100,200]] {
   for (at,value) in [(ta+0x956,pairs[0]),(ta+0x95e,pairs[1]),(fr+0x21f0,pairs[2]),(fr+0x21f8,pairs[3])] {x.descriptors.data[at..at+8].copy_from_slice(&value.to_le_bytes());}
   let before=x.descriptors.data.clone();let owners=x.render_command_ordinals;
   assert_eq!(x.render_device_segments(ordinal).unwrap(),[x.descriptors.iova()+ta as u64,x.descriptors.iova()+fr as u64,pairs[0],pairs[1],pairs[2],pairs[3]]);
   assert_eq!(x.descriptors.data,before);assert_eq!(x.render_command_ordinals,owners);
  }
  let calls=x.descriptors.calls;assert_eq!(x.render_device_segments(ordinal.wrapping_add(16)),Err(EINVAL));assert_eq!(x.descriptors.calls,calls);
  x.descriptors.fail=true;assert_eq!(x.render_device_segments(ordinal),Err(5));x.descriptors.fail=false;
  x.descriptors.data.truncate(fr+0x21ff);assert_eq!(x.render_device_segments(ordinal),Err(ERANGE));
  x.command_generation_count=0;let calls=x.descriptors.calls;assert_eq!(x.render_device_segments(ordinal),Err(EINVAL));assert_eq!(calls,x.descriptors.calls);
 }
 println!("PASS actual segment reader: 520 values/ordinal cases, exact identity, wrap, missing map, short backing and unchanged owner/data");
}
'''
with tempfile.TemporaryDirectory() as d:
    p=Path(d)/'segments.rs';p.write_text(code);binary=Path(d)/'segments'
    subprocess.run(['rustc','--edition=2021',str(p),'-o',str(binary)],check=True)
    subprocess.run([str(binary)],check=True)
