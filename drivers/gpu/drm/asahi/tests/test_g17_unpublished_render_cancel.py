#!/usr/bin/env python3
"""Run exact unpublished-generation rollback against retained live generations."""
from pathlib import Path
import tempfile, subprocess
r=Path(__file__).resolve().parents[1];s=(r/'g17_resources.rs').read_text()
def item(key):
 a=s.index(key);b=s.index('{',a)+1;n=1
 while n:n+=(s[b]=='{')-(s[b]=='}');b+=1
 return s[a:b]
code=r'''
#![allow(dead_code)]
type Result<T=()> = std::result::Result<T,i32>;
const EIO:i32=5;const G17P_RENDER_RT_EVENT_RECORD_COUNT:usize=4;
struct Ownership{ordinals:[Option<u64>;2]}
impl Ownership{fn release(&mut self,ordinal:u64)->bool{if let Some(slot)=self.ordinals.iter_mut().find(|slot|**slot==Some(ordinal)){*slot=None;true}else{false}}}
struct Pm{scene_ownership:Ownership}
struct Retired{owners:[Option<u64>;2]}
struct KVec<T>(Vec<T>);
impl<T>KVec<T>{fn remove(&mut self,i:usize)->Result<T>{Ok(self.0.remove(i))}}
impl<T>std::ops::Deref for KVec<T>{type Target=Vec<T>;fn deref(&self)->&Vec<T>{&self.0}}
impl<T>std::ops::DerefMut for KVec<T>{fn deref_mut(&mut self)->&mut Vec<T>{&mut self.0}}
struct Target{rt_slot_ordinals:[Option<u64>;4],retired_backings:KVec<Retired>}
struct Storage{command_generation_count:usize,render_command_ordinals:[Option<u64>;2],parameter_management:Option<Pm>,render_target_memory:Target}
impl Storage{
 fn new()->Self{Self{command_generation_count:2,render_command_ordinals:[Some(4),Some(5)],parameter_management:Some(Pm{scene_ownership:Ownership{ordinals:[Some(4),Some(5)]}}),render_target_memory:Target{rt_slot_ordinals:[Some(4),Some(5),None,None],retired_backings:KVec(vec![Retired{owners:[Some(4),Some(5)]},Retired{owners:[None,Some(5)]}])}}}
''' + item('    pub(crate) fn retire_command_generation(') + item('    pub(crate) fn cancel_unpublished_command(') + r'''
}
#[test]fn committed_unpublished_generation_releases_only_itself(){let mut s=Storage::new();s.cancel_unpublished_command(5).unwrap();assert_eq!(s.render_command_ordinals,[Some(4),None]);assert_eq!(s.parameter_management.as_ref().unwrap().scene_ownership.ordinals,[Some(4),None]);assert_eq!(s.render_target_memory.rt_slot_ordinals,[Some(4),None,None,None]);assert_eq!(s.render_target_memory.retired_backings.len(),1);assert_eq!(s.render_target_memory.retired_backings[0].owners,[Some(4),None]);}
#[test]fn partial_preparation_only_retracts_exact_rt_reservation(){let mut s=Storage::new();s.render_target_memory.rt_slot_ordinals[3]=Some(7);s.cancel_unpublished_command(7).unwrap();assert_eq!(s.render_command_ordinals,[Some(4),Some(5)]);assert_eq!(s.parameter_management.as_ref().unwrap().scene_ownership.ordinals,[Some(4),Some(5)]);assert_eq!(s.render_target_memory.rt_slot_ordinals,[Some(4),Some(5),None,None]);assert_eq!(s.render_target_memory.retired_backings.len(),2);}
#[test]fn warm_constructor_generation_remains_seeded(){let mut s=Storage::new();s.render_command_ordinals[0]=Some(0);s.cancel_unpublished_command(0).unwrap();assert_eq!(s.render_command_ordinals,[Some(0),Some(5)]);}
#[test]fn repeated_cancellation_does_not_release_live_neighbor(){let mut s=Storage::new();s.cancel_unpublished_command(5).unwrap();s.cancel_unpublished_command(5).unwrap();assert_eq!(s.render_command_ordinals,[Some(4),None]);assert_eq!(s.parameter_management.as_ref().unwrap().scene_ownership.ordinals,[Some(4),None]);}
'''
with tempfile.TemporaryDirectory() as d:
 p=Path(d)/'cancel.rs';p.write_text(code);b=Path(d)/'cancel'
 subprocess.run(['rustc','--edition=2021','--test',str(p),'-o',str(b)],check=True)
 subprocess.run([str(b)],check=True)
print('PASS actual unpublished Scene/RT rollback without releasing preceding GPU owners')
