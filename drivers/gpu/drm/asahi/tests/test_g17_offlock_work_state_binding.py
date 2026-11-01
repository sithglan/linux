#!/usr/bin/env python3
"""Check actual binding bytes with one TSQ assignment and detached preparation."""
from pathlib import Path
import json,subprocess,tempfile
r=Path(__file__).resolve().parents[1];s=(r/'g17_resources.rs').read_text();m=(r/'g17_manager.rs').read_text()
def item(s,key):
 a=s.index(key);b=s.index('{',a)+1;n=1
 while n:n+=(s[b]=='{')-(s[b]=='}');b+=1
 return s[a:b]
bind=item(s,'    pub(crate) fn bind_shared_work_state(')
assert bind.count('render_tsq_slot(')==1
prepare=item(m,'impl G17PRenderRestage {')
assert prepare.index('result?;')<prepare.index('bind_shared_work_state(')<prepare.index('descriptor_context_ids()')<prepare.index('self.reservation.prepared_sksm = Some(')
commit=item(m,'    fn begin_translated_render_with_hardware_buffer(')
assert 'if !restaged {\n            self.render_slots.get_mut(pair_slot).storage' in commit
validate=item(m,'    fn validate_render_queue_consistency(')
assert 'Some(contexts) => contexts' in validate and 'None => storage.descriptor_context_ids()?' in validate
program=r'''
#![allow(dead_code)]
use std::cell::Cell;use std::sync::atomic::{fence,Ordering};
type Result<T=()> = std::result::Result<T,i32>;const EINVAL:i32=22;const ERANGE:i32=34;const EOVERFLOW:i32=75;
'''+f'#[path={json.dumps(str(r/"g17_work_state.rs"))}] mod g17_work_state;\n'+r'''
mod g17_render{pub const TA_DESCRIPTOR_SIZE:usize=0xc00;}
const G17P_RENDER_3D_DESCRIPTOR_STRIDE:usize=0x2300;
fn g17p_render_ta_descriptor_offset(o:u64)->usize{assert_eq!(o,7);0}
fn g17p_render_3d_descriptor_offset(o:u64)->usize{assert_eq!(o,7);0x1000}
struct Pair;impl Pair{fn slot(&self)->u8{3}}
struct Bytes(Vec<u8>);impl Bytes{fn with_bytes_mut<T>(&mut self,f:impl FnOnce(&mut[u8])->Result<T>)->Result<T>{f(&mut self.0)}}
struct G17PWorkStateLease{calls:Cell<usize>,slot:u32,node:u64}
impl G17PWorkStateLease{fn render_tsq_slot(&self,p:u8)->Result<g17_work_state::TsqSlot>{assert_eq!(p,3);self.calls.set(self.calls.get()+1);g17_work_state::TsqSlot::new(self.slot).ok_or(EINVAL)}fn tsq_gpu_va(&self)->Result<u64>{Ok(self.node)}}
struct Storage{descriptor_ordinal:u64,queue_pair:Pair,graph:Bytes,descriptors:Bytes}
impl Storage{fn queue_record_offset(&self,t:bool)->usize{if t{0x100}else{0}}
'''+bind+r'''}
#[test]fn writes_identical_ta_fragment_and_queue_slots_with_one_assignment(){
 for slot in 1..4{
 let owner=G17PWorkStateLease{calls:Cell::new(0),slot,node:0x12000};
 let mut s=Storage{descriptor_ordinal:7,queue_pair:Pair,graph:Bytes(vec![0xa5;0x200]),descriptors:Bytes(vec![0xa5;0x3300])};
 let mut expected=s.descriptors.0.clone();
 let (ta,frag)=expected.split_at_mut(0x1000);
 assert!(g17_work_state::TsqSlot::new(slot).unwrap().bind_render(&mut ta[..0xc00],frag,owner.node));
 s.bind_shared_work_state(&owner).unwrap();assert_eq!(s.descriptors.0,expected);assert_eq!(owner.calls.get(),1);
 for offset in [0x38,0x138]{assert_eq!(&s.graph.0[offset..offset+4],&slot.to_le_bytes());}
 }
}
#[test]fn errors_cannot_publish_and_keep_single_assignment(){
 let owner=G17PWorkStateLease{calls:Cell::new(0),slot:1,node:0};
 let mut s=Storage{descriptor_ordinal:7,queue_pair:Pair,graph:Bytes(vec![0;0x200]),descriptors:Bytes(vec![0;0x3300])};
 assert_eq!(s.bind_shared_work_state(&owner),Err(EINVAL));assert_eq!(owner.calls.get(),1);
 s.descriptors.0.truncate(0x1000);assert_eq!(s.bind_shared_work_state(&owner),Err(ERANGE));
}
'''
with tempfile.TemporaryDirectory() as tmp:
 p=Path(tmp)/'test.rs';p.write_text(program);exe=Path(tmp)/'test'
 subprocess.run(['rustc','--edition=2021','--test',str(p),'-o',str(exe)],check=True)
 subprocess.run([str(exe)],check=True)
print('PASS exact descriptor binding with one TSQ lookup; detached binding/readback precede commit; cold validation fallback retained')
