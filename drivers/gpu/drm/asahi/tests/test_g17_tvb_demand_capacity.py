#!/usr/bin/env python3
"""Execute the production restage capacity guard in static and dynamic modes."""
import os
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
source = Path(os.environ.get('G17_SCENE_RESOURCE_SOURCE', str(root / 'g17_resources.rs'))).read_text()
diagnostic = source.index('"G17P render restage: parameter-buffer pool too small;')
start = source.rindex('        if g17p_render_tvb_enabled() {', 0, diagnostic)
end = source.index('{', start) + 1
depth = 1
while depth:
    depth += (source[end] == '{') - (source[end] == '}')
    end += 1
guard = source[start:end]
body = r'''
#![allow(dead_code,unused_variables)]
type Result<T=()> = std::result::Result<T, i32>;
const EIO:i32=5; const ERANGE:i32=34;
macro_rules! dev_err {($($tt:tt)*)=>{()}}
thread_local! { static MODE: std::cell::Cell<u32> = const { std::cell::Cell::new(2) }; }
fn g17p_render_tvb_mode()->u32 { MODE.with(|v|v.get()) }
fn g17p_render_tvb_enabled()->bool { g17p_render_tvb_mode()!=0 }
struct Tvb {logical_size:usize,block_count:usize}
struct Layout {tvb_size:usize,tvb_blocks:usize}
struct Storage {render_tvb:Option<Tvb>}
impl Storage {
 fn check(&mut self,render_layout:&Layout)->Result {
''' + guard + r'''
  Ok(())
 }
}
fn size(blocks:usize)->usize {(blocks-1)*0x28000+0x20000}
#[test]fn dynamic_demand_growth_does_not_reject_larger_target_or_mutate_inventory(){
 for mode in [2,3] {
  MODE.with(|v|v.set(mode));
  let mut storage=Storage{render_tvb:Some(Tvb{logical_size:size(32),block_count:32})};
  for wanted in [32,40,64,256,32,40] {
   assert_eq!(storage.check(&Layout{tvb_size:size(wanted),tvb_blocks:wanted}),Ok(()));
   assert_eq!(storage.render_tvb.as_ref().unwrap().block_count,32);
   assert_eq!(storage.render_tvb.as_ref().unwrap().logical_size,size(32));
  }
 }
}
#[test]fn static_pool_still_rejects_either_insufficient_bound(){
 MODE.with(|v|v.set(1));
 let mut storage=Storage{render_tvb:Some(Tvb{logical_size:size(32),block_count:32})};
 assert_eq!(storage.check(&Layout{tvb_size:size(32),tvb_blocks:32}),Ok(()));
 for layout in [Layout{tvb_size:size(40),tvb_blocks:32},Layout{tvb_size:size(32),tvb_blocks:40}] {
  assert_eq!(storage.check(&layout),Err(ERANGE));
 }
}
#[test]fn dynamic_mode_requires_an_existing_pool(){
 MODE.with(|v|v.set(2));
 assert_eq!(Storage{render_tvb:None}.check(&Layout{tvb_size:size(40),tvb_blocks:40}),Err(EIO));
}
'''
with tempfile.TemporaryDirectory() as directory:
    path=Path(directory)
    (path/'test.rs').write_text(body)
    subprocess.run(['rustc','--edition=2021','--test',str(path/'test.rs'),'-o',str(path/'test')],check=True)
    subprocess.run([str(path/'test')],check=True)
