#!/usr/bin/env python3
"""Execute the production UMA save path with firmware-updated pool counters."""
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
codec = (root / "g17_submission.rs").read_text()
resources = (root / "g17_resources.rs").read_text()
manager = (root / "g17_manager.rs").read_text()


def item(source, marker):
    start = source.index(marker)
    end = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


park = item(manager, "    fn park_render_sksm_backing_for_graph_rebuild(")
assert park.index("save_user_render_uma") < park.index("take_sksm_backing")
source = r'''
#![allow(dead_code)]
use std::cell::Cell;
type Result<T=()> = std::result::Result<T, i32>;
const EIO:i32=5;
mod g17_submission {
#[derive(Debug,Copy,Clone,PartialEq,Eq)]
''' + item(codec, "pub(crate) struct G17PUmaPagePoolState {") + "\n" + "\n".join(
    item(codec, marker) for marker in [
        "pub(crate) const fn encode_g17p_umadesc(",
        "pub(crate) const fn decode_g17p_umadesc(",
    ]) + r'''
}
use g17_submission::*;
struct Pool {state:Cell<G17PUmaPagePoolState>}
impl Pool {
 fn id(&self)->u16{129}
 fn uma_state(&self)->Result<G17PUmaPagePoolState>{Ok(self.state.get())}
 fn store_uma_state(&self,state:G17PUmaPagePoolState)->Result{self.state.set(state);Ok(())}
}
struct G17PUserRenderStorage{render_flist:Pool}
struct Resources{descriptor:[u64;4]}
impl Resources {
 fn snapshot_compute_uma(&mut self,id:u8)->Result<[u64;4]>{assert_eq!(id,129);Ok(self.descriptor)}
''' + item(resources, "    pub(crate) fn save_user_render_uma(") + r'''
}
fn cold()->G17PUmaPagePoolState {
 G17PUmaPagePoolState{page_list:0x700c310000,unit_pages:0x40000,counter_a:0,counter_b:0x1100,empty:0,total_pages:0x1100}
}
#[test]fn save_and_reload_preserves_captured_firmware_growth_and_cursors(){
 let pool=G17PUserRenderStorage{render_flist:Pool{state:Cell::new(cold())}};
 let mut resources=Resources{descriptor:[0x08000000e0186200,0x3bf00000ff00,0x1600,0xfeedface]};
 let live=resources.descriptor;
 resources.save_user_render_uma(&pool).unwrap();
 assert_eq!(pool.render_flist.state.get().total_pages,0x1600);
 assert_eq!(pool.render_flist.state.get().counter_a,0x7f8);
 assert_eq!(pool.render_flist.state.get().counter_b,0x1df8);
 let restored=encode_g17p_umadesc([0,0,0,0x12345678],pool.render_flist.state.get());
 assert_eq!(&restored[..3],&live[..3]);
 assert_eq!(restored[3],0x12345678);
 assert_eq!(resources.descriptor,live);
 assert_ne!(&encode_g17p_umadesc([0;4],cold())[..3],&live[..3]);
}
#[test]fn reject_a_different_pool_without_changing_either_owner(){
 let pool=G17PUserRenderStorage{render_flist:Pool{state:Cell::new(cold())}};
 let mut resources=Resources{descriptor:encode_g17p_umadesc([0;4],cold())};
 resources.descriptor[0]^=0x80;
 let before=resources.descriptor;
 assert_eq!(resources.save_user_render_uma(&pool),Err(EIO));
 assert_eq!(pool.render_flist.state.get(),cold());
 assert_eq!(resources.descriptor,before);
}
#[test]fn counters_survive_width_boundaries_and_repeated_handoffs(){
 let pool=G17PUserRenderStorage{render_flist:Pool{state:Cell::new(cold())}};
 for count in [0,1,0xffff,0x10000,0xffffff,0x1000000,0xfffffff] {
  for empty in 0..8 {
   let state=G17PUmaPagePoolState{counter_a:count,counter_b:0xfffffff-count,empty,total_pages:0x1600,..cold()};
   let mut resources=Resources{descriptor:encode_g17p_umadesc([0,0,0,u64::MAX],state)};
   resources.save_user_render_uma(&pool).unwrap();
   assert_eq!(pool.render_flist.state.get(),state);
  }
 }
}
'''
with tempfile.TemporaryDirectory() as directory:
    path = Path(directory)
    (path / "test.rs").write_text(source)
    subprocess.run(["rustc", "--edition=2021", "--test", str(path / "test.rs"),
                    "-o", str(path / "test")], check=True)
    subprocess.run([str(path / "test")], check=True)
