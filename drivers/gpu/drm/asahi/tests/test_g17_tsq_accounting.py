#!/usr/bin/env python3
"""Execute production shared TSQ accounting with fallible in-memory writes."""
from pathlib import Path
import subprocess
import tempfile
root=Path(__file__).resolve().parents[1]
s=(root/'g17_resources.rs').read_text()
a=s.index('    fn change_count(&mut self, word: u32, kicks: u32, add: bool)')
b=s.index('{',a)+1;depth=1
while depth:
    depth+=(s[b]=='{')-(s[b]=='}');b+=1
method=s[a:b]
rust=r'''
use std::sync::atomic::Ordering;
type Result<T=()>=std::result::Result<T,i32>;
const EINVAL:i32=22;const EOVERFLOW:i32=75;
mod mmu {pub const UAT_PGSZ:usize=0x4000;}
struct Storage {fail:bool,writes:Vec<(u64,u32)>}
impl Storage {
 fn iova(&self)->u64{0x10000}
 fn write_u32(&mut self,address:u64,count:u32)->Result {
  if self.fail{return Err(5)}self.writes.push((address,count));Ok(())
 }
}
struct State {words:[Option<u32>;63],counts:[u32;63],storage:Storage}
impl State {
 fn new()->Self{let mut s=Self{words:[None;63],counts:[0;63],storage:Storage{fail:false,writes:Vec::new()}};s.words[2]=Some(66);s}
'''+method+r'''
}
#[test]fn mixed_members_share_one_count_and_only_unpublished_work_is_cancelled(){
 let mut s=State::new();assert_eq!(s.change_count(66,2,true),Ok(2));
 assert_eq!(s.change_count(66,1,true),Ok(3));assert_eq!(s.change_count(66,1,false),Ok(2));
 assert_eq!(s.storage.writes,vec![(0x14008,2),(0x14008,3),(0x14008,2)]);
}
#[test]fn stale_generation_cannot_modify_new_owners_count(){
 let mut s=State::new();assert_eq!(s.change_count(2,1,true),Err(EINVAL));
 assert_eq!(s.change_count(u32::MAX,1,true),Err(EINVAL));assert!(s.storage.writes.is_empty());
}
#[test]fn overflow_underflow_and_mapping_failure_leave_host_accounting_unchanged(){
 let mut s=State::new();assert_eq!(s.change_count(66,1,false),Err(EOVERFLOW));
 s.counts[2]=u32::MAX;assert_eq!(s.change_count(66,1,true),Err(EOVERFLOW));
 s.counts[2]=4;s.storage.fail=true;assert_eq!(s.change_count(66,1,true),Err(5));
 assert_eq!(s.counts[2],4);assert!(s.storage.writes.is_empty());
}
#[test]fn concurrent_engines_do_not_lose_submitted_increments(){
 let s=std::sync::Mutex::new(State::new());
 std::thread::scope(|scope|{for _ in 0..8{let s=&s;scope.spawn(move||{
  for _ in 0..1000{s.lock().unwrap().change_count(66,1,true).unwrap();}
 });}});
 let s=s.lock().unwrap();assert_eq!(s.counts[2],8000);
 for (i,&(_,count)) in s.storage.writes.iter().enumerate(){assert_eq!(count,i as u32+1);}
}
'''
with tempfile.TemporaryDirectory() as d:
    p=Path(d)/'test.rs';p.write_text(rust);binary=Path(d)/'test'
    subprocess.run(['rustc','--edition=2021','--test',str(p),'-o',str(binary)],check=True)
    subprocess.run([str(binary)],check=True)
