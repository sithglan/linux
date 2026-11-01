#!/usr/bin/env python3
"""Exercise production PM reservations with independently finishing builders."""
from pathlib import Path
import subprocess,tempfile
ROOT=Path(__file__).resolve().parents[1]
manager=(ROOT/'g17_manager.rs').read_text();resources=(ROOT/'g17_resources.rs').read_text()
def item(s,marker):
 a=s.index(marker);b=s.index('{',a)+1;depth=1
 while depth:depth+=(s[b]=='{')-(s[b]=='}');b+=1
 return s[a:b]
reserve=item(manager,'    fn prepare_render_pm_manager_generation(')
commit=item(manager,'    fn commit_render_pm_manager_generation(')
plan=item(resources,'pub(crate) fn g17p_pm_manager_generation_plan(')
assert 'put_u64(raw, selected_scene + 0x4c, manager_generation)' in resources
# The cold no-storage branch is an out-of-line helper (kernel stack depth);
# splice its body back at the call site so ordering invariants still read in order.
for name in ['stage_compute_bootstrap_render','warm_render_pair_storage_inner','begin_translated_render_with_hardware_buffer']:
 body=item(manager,'fn '+name+'(')
 body=body.replace('self.install_cold_render_storage(', item(manager,'    fn install_cold_render_storage(')+'(', 1)
 assert body.index('prepare_render_pm_manager_generation()?') < body.index('.new_user_render_storage(')
 if name == 'warm_render_pair_storage_inner':
  assert body.index('.new_user_render_storage(') < body.index('finish_warm_render_pair_storage(')
  assert 'commit_render_pm_manager_generation(pair_slot, pm_generation)' in item(manager,'fn finish_warm_render_pair_storage(')
 else:
  assert body.index('.new_user_render_storage(') < body.index('commit_render_pm_manager_generation(pair_slot, pm_generation)')
rust=r'''
#![allow(dead_code)]
type Result<T=()> = std::result::Result<T,i32>;
const EINVAL:i32=22;const EOVERFLOW:i32=75;const ENODEV:i32=19;const EIO:i32=5;
mod g17_resources {use super::*;pub fn g17p_render_tvb_mode()->u32{2}
'''+plan+r'''
}
struct Storage(Option<u64>);impl Storage{fn parameter_manager_generation(&self)->Option<u64>{self.0}}
struct Pair{storage:Option<Box<Storage>>}
struct Slots([Pair;3]);impl Slots{fn get(&self,slot:u8)->&Pair{&self.0[slot as usize]}}
struct Manager{next_render_pm_manager_generation:u64,render_slots:Slots}
impl Manager{
 fn new()->Self{Self{next_render_pm_manager_generation:9,render_slots:Slots([Pair{storage:None},Pair{storage:None},Pair{storage:None}])}}
'''+reserve+'\n'+commit+r'''
}
#[test]fn concurrent_builders_finish_in_reverse_order_with_distinct_identities(){
 let mut m=Manager::new();let a=m.prepare_render_pm_manager_generation().unwrap();let b=m.prepare_render_pm_manager_generation().unwrap();
 assert_eq!(a,Some(9));assert_eq!(b,Some(10));
 m.render_slots.0[1].storage=Some(Box::new(Storage(b)));m.commit_render_pm_manager_generation(1,b).unwrap();
 m.render_slots.0[0].storage=Some(Box::new(Storage(a)));m.commit_render_pm_manager_generation(0,a).unwrap();
 assert_eq!(m.next_render_pm_manager_generation,11);
}
#[test]fn failed_builder_does_not_reuse_a_reserved_identity(){
 let mut m=Manager::new();assert_eq!(m.prepare_render_pm_manager_generation(),Ok(Some(9)));
 assert_eq!(m.prepare_render_pm_manager_generation(),Ok(Some(10)));
}
#[test]fn wrong_pair_generation_cannot_commit(){
 let mut m=Manager::new();let a=m.prepare_render_pm_manager_generation().unwrap();let b=m.prepare_render_pm_manager_generation().unwrap();
 m.render_slots.0[1].storage=Some(Box::new(Storage(b)));assert_eq!(m.commit_render_pm_manager_generation(1,a),Err(EIO));
 assert_eq!(m.next_render_pm_manager_generation,11);
 assert_eq!(m.commit_render_pm_manager_generation(0,a),Err(ENODEV));
}
#[test]fn overflow_does_not_wrap_or_advance_the_allocator(){
 let mut m=Manager::new();m.next_render_pm_manager_generation=u64::MAX;
 assert_eq!(m.prepare_render_pm_manager_generation(),Err(EOVERFLOW));assert_eq!(m.next_render_pm_manager_generation,u64::MAX);
 m.next_render_pm_manager_generation=0;assert_eq!(m.prepare_render_pm_manager_generation(),Err(EINVAL));
}
'''
with tempfile.TemporaryDirectory() as d:
 p=Path(d)/'pm.rs';p.write_text(rust);binary=Path(d)/'pm'
 subprocess.run(['rustc','--edition=2021','--test',str(p),'-o',str(binary)],check=True)
 subprocess.run([str(binary)],check=True)
