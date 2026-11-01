#!/usr/bin/env python3
"""Exercise the actual page-list diagnostic method and shared-word helpers."""
from pathlib import Path
import subprocess, tempfile, os
root=Path(__file__).resolve().parents[1]
r=Path(os.environ.get('G17_PB_DEBUG_SOURCE',str(root/'g17_resources.rs'))).read_text()
def item(marker):
 a=r.index(marker);i=r.index('{',a)+1;depth=1
 while depth:
  depth+=(r[i]=='{')-(r[i]=='}');i+=1
 return r[a:i]
s=r'''
#![allow(dead_code,unused_variables)]
use std::sync::atomic::{AtomicBool,Ordering};
type Result<T=()>=std::result::Result<T,i32>;
const EINVAL:i32=22; const ERANGE:i32=34;
static VERBOSE:AtomicBool=AtomicBool::new(false);
mod util {pub fn g17p_verbose()->bool{super::VERBOSE.load(super::Ordering::Relaxed)}}
macro_rules! dev_info {($dev:expr,$($arg:tt)*)=>{let _=format_args!($($arg)*);}}
struct Buffer { words:Vec<u32>, logical_size:usize, maps:usize }
impl Buffer {fn with_bytes_mut<R>(&mut self,f:impl FnOnce(&mut[u8])->Result<R>)->Result<R>{
 self.maps+=1;
 let bytes=unsafe{std::slice::from_raw_parts_mut(self.words.as_mut_ptr().cast(),self.logical_size)};
 f(bytes)
}}
struct Layout {page_list:usize}
struct Pm {layout:Layout,page_list_vas:[u64;2],backing:Buffer}
struct G17PUserRenderStorage {parameter_management:Option<Pm>}
struct Manager;
impl Manager {
''' + item('    pub(crate) fn stage_render_parameter_buffer_page_list(')+r'''
}
''' + item('fn g17p_live_word_bounds(')+'\n'+item('fn g17p_live_read_u32(')+r'''
fn storage()->G17PUserRenderStorage{G17PUserRenderStorage{parameter_management:Some(Pm{
 layout:Layout{page_list:16},page_list_vas:[0x1000,0x2000],
 backing:Buffer{words:(0..16).collect(),logical_size:64,maps:0}})}}
#[test]fn disabled_debug_never_maps_or_reads_backing(){
 VERBOSE.store(false,Ordering::Relaxed);let mut s=storage();let mut m=Manager;
 for _ in 0..1000 {m.stage_render_parameter_buffer_page_list(&mut s).unwrap();}
 assert_eq!(s.parameter_management.unwrap().backing.maps,0);
}
#[test]fn enabled_debug_reads_valid_words_without_mutation(){
 VERBOSE.store(true,Ordering::Relaxed);let mut s=storage();let before=s.parameter_management.as_ref().unwrap().backing.words.clone();
 Manager.stage_render_parameter_buffer_page_list(&mut s).unwrap();
 let b=&s.parameter_management.as_ref().unwrap().backing;
 assert_eq!(b.maps,1);assert_eq!(b.words,before);VERBOSE.store(false,Ordering::Relaxed);
}
#[test]fn invalid_extent_is_rejected_with_debug_on_or_off(){
 for enabled in [false,true]{
  VERBOSE.store(enabled,Ordering::Relaxed);
  for offset in [33,usize::MAX-15,usize::MAX]{let mut s=storage();s.parameter_management.as_mut().unwrap().layout.page_list=offset;
   assert_eq!(Manager.stage_render_parameter_buffer_page_list(&mut s),Err(ERANGE));
   assert_eq!(s.parameter_management.unwrap().backing.maps,0);
  }
  assert_eq!(Manager.stage_render_parameter_buffer_page_list(&mut G17PUserRenderStorage{parameter_management:None}),Err(EINVAL));
 }
 VERBOSE.store(false,Ordering::Relaxed);
}
#[test]fn live_word_read_is_exact_and_bounds_checked(){
 let words=[0x12345678u32,0xaabbccdd];let bytes=unsafe{std::slice::from_raw_parts(words.as_ptr().cast(),8)};
 assert_eq!(g17p_live_read_u32(bytes,0),Ok(u32::from_le(words[0])));
 assert_eq!(g17p_live_read_u32(bytes,4),Ok(u32::from_le(words[1])));
 for offset in [1,5,8,usize::MAX]{assert_eq!(g17p_live_read_u32(bytes,offset),Err(EINVAL));}
}
'''
with tempfile.TemporaryDirectory() as d:
 f=Path(d)/'test.rs';f.write_text(s);exe=Path(d)/'test'
 subprocess.run(['rustc','--edition=2021','--test',str(f),'-o',str(exe)],check=True)
 subprocess.run([str(exe),'--test-threads=1'],check=True)
