#!/usr/bin/env python3
"""Compile the production-extracted timestamp reader and refresh assignments."""
from pathlib import Path
import subprocess, tempfile
ROOT=Path(__file__).resolve().parents[1]
s=(ROOT/'g17_resources.rs').read_text()
a=s.index('    pub(crate) fn render_pass_timestamps(')
b=s.index('    /// Read the latest qualifying firmware timestamp segments',a)
method=s[a:b]
m=(ROOT/'g17_manager.rs').read_text()
a=m.index('                state.tiling_timestamps = [pass[0], pass[0]];')
b=m.index('                // Bounded opt-in observation',a)
refresh=m[a:b]
commit=m[m.index('    pub(crate) fn commit_render_observation('):]
assert commit.index('let stamps = self.resources.render_completion_stamps(pair)?;') < commit.index('if !visible(fragment_stamp)') < commit.index('.render_pass_timestamps(ordinal)?;') < commit.index('retire_command_generation(ordinal)?;')
assert 'core::sync::atomic::fence(Ordering::Acquire)' in s[s.index('    pub(crate) fn render_completion_stamps('):s.index('    pub(crate) fn render_stamp_lanes(')]
head=r"""
const EINVAL:i32=22; const ERANGE:i32=34; const EOVERFLOW:i32=75;
type Result<T> = std::result::Result<T,i32>;
fn g17p_render_descriptor_slot(n:u64)->usize {(n%16) as usize}
struct Obj { words:Vec<u64>, len:usize }
impl Obj {fn with_bytes_mut<T>(&mut self,f:impl FnOnce(&mut[u8])->Result<T>)->Result<T>{
let raw=unsafe{std::slice::from_raw_parts_mut(self.words.as_mut_ptr().cast::<u8>(),self.len)}; f(raw)}}
struct Storage {command_generation_count:usize,render_command_ordinals:Vec<Option<u64>>,timestamps:Obj}
impl Storage {
"""
tail=r"""
}
#[derive(Clone,Copy)]struct State {tiling_timestamps:[u64;2],fragment_timestamps:[u64;2]}
struct Completion {execution:State,complete:bool}
fn refreshed(mut state:State,mut completion:Completion,pass:[u64;2])->(State,Completion){
REPLACE_REFRESH
(state,completion)}
fn main(){
 let mut tests=0;
 for ordinal in [0,1,15,16,3313,3937,4592,65536] {
  let mut st=Storage{command_generation_count:16,render_command_ordinals:vec![None;16],timestamps:Obj{words:vec![0;32],len:256}};
  let slot=(ordinal%16)as usize;st.render_command_ordinals[slot]=Some(ordinal);
  let stale=State{tiling_timestamps:[100,100],fragment_timestamps:[100,90]};
  for pair in [[101,155],[0,0],[1,0],[u64::MAX-4,u64::MAX-1]] {
   st.timestamps.words[slot*2]=pair[0];st.timestamps.words[slot*2+1]=pair[1];
   let before=st.timestamps.words.clone();
   let pass=st.render_pass_timestamps(ordinal).unwrap();assert_eq!(pass,pair);
   let (state,c)=refreshed(stale,Completion{execution:stale,complete:true},pass);
   assert_eq!(state.fragment_timestamps,pair);assert_eq!(c.execution.fragment_timestamps,pair);
   assert_eq!(c.execution.tiling_timestamps,[pair[0],pair[0]]);assert!(c.complete);
   assert_eq!(st.timestamps.words,before);tests+=1;
  }
  st.render_command_ordinals[slot]=Some(ordinal+16);assert_eq!(st.render_pass_timestamps(ordinal),Err(EINVAL));tests+=1;
  st.render_command_ordinals[slot]=Some(ordinal);st.timestamps.len=slot*16+15;
  assert_eq!(st.render_pass_timestamps(ordinal),Err(ERANGE));tests+=1;
 }
 println!("{} production reader/refresh/bounds/ownership cases PASS",tests);
}
""".replace('REPLACE_REFRESH',refresh)
with tempfile.TemporaryDirectory() as d:
 p=Path(d);(p/'test.rs').write_text(head+method+tail)
 subprocess.run(['rustc','--edition=2021',str(p/'test.rs'),'-o',str(p/'test')],check=True)
 subprocess.run([str(p/'test')],check=True)
 # Negative control: stale original completion must fail the same value checks.
 neg=tail.replace(refresh,'')
 (p/'test.rs').write_text(head+method+neg)
 subprocess.run(['rustc','--edition=2021','-Awarnings',str(p/'test.rs'),'-o',str(p/'negative')],check=True)
 r=subprocess.run([str(p/'negative')],capture_output=True)
 assert r.returncode != 0,'stale completion negative control unexpectedly passed'
 print('Unrefreshed completion negative control rejected; source order/acquire checks PASS')
