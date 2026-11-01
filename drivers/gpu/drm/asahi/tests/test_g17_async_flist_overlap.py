#!/usr/bin/env python3
"""Actual FList publisher/matcher: one grow plus many independent command refs."""
from pathlib import Path
import subprocess,tempfile
p=Path(__file__).resolve().parent
fixture=p/'test_g17_retained_flist.py'
env={'__file__':str(fixture)}
exec(fixture.read_text().split('body = ')[0],env)
block=env['block'];manager=env['manager'];runtime=env['runtime']
body='#![allow(dead_code)]\nmod g17_submission {'+env['submission_part']+'}\nmod g17_initdata {'+env['initdata_part']+'}\n'+env['lease_part']+r'''
use std::cell::Cell;
type Result<T=()> = std::result::Result<T,i32>;
const EINVAL:i32=22;const EIO:i32=5;const EAGAIN:i32=11;const EOVERFLOW:i32=75;
mod g17_resources{pub struct G17PDeviceControlPublication;pub struct G17PRenderQueuePair;impl G17PRenderQueuePair{pub const SLOT_COUNT:u8=64;}}
struct Storage{grown:Cell<bool>}impl Storage{fn mark_render_flist_grown(&self){self.grown.set(true)}}
struct Slot{storage:Option<Box<Storage>>,preparing:bool,bootstrap_uma:Option<G17PBootstrapUmaLifecycle>,prepared_render_flist:Option<G17PBootstrapUmaLifecycle>,next_render_flist_sequence:u64}
struct Slots(Vec<Slot>);impl Slots{fn get(&self,s:u8)->&Slot{&self.0[s as usize]}fn get_mut(&mut self,s:u8)->&mut Slot{&mut self.0[s as usize]}fn allocated(&self,s:u8)->Result<&Slot>{self.0.get(s as usize).ok_or(EINVAL)}}
struct Resources{records:Vec<Vec<u8>>,full:bool}impl Resources{fn publish_primary_device_control_raw(&mut self,r:&[u8])->Result<g17_resources::G17PDeviceControlPublication>{if self.full{return Err(EAGAIN)}self.records.push(r.to_vec());Ok(g17_resources::G17PDeviceControlPublication)}}
struct Manager{render_slots:Slots,resources:Resources}
impl Manager{fn primary_device_control_backpressured(&self)->bool{self.resources.full}
'''+block(manager,r'^    pub\(crate\) fn publish_render_usc_freelist_request\(')+block(manager,r'^    pub\(crate\) fn match_render_usc_freelist_completion\(')+r'''
}
fn manager()->Manager{Manager{render_slots:Slots((0..2).map(|_|Slot{storage:Some(Box::new(Storage{grown:Cell::new(false)})),preparing:false,bootstrap_uma:None,prepared_render_flist:None,next_render_flist_sequence:0}).collect()),resources:Resources{records:vec![],full:false}}}
fn prepare(m:&mut Manager,slot:u8)->Result{
 let s=m.render_slots.get_mut(slot);
 s.prepared_render_flist=Some(G17PBootstrapUmaLifecycle::acquire_owner(s.bootstrap_uma.as_ref(),s.next_render_flist_sequence,129+u32::from(slot),0x1000+u64::from(slot)*0x100,0x2000+u64::from(slot)*0x100,0x3000+u64::from(slot)*0x100,true).map_err(|_|EIO)?);Ok(())
}
#[test]fn one_grow_can_coexist_with_every_queued_command_and_retires_exactly_once(){
 for commands in 1..=64 {
  let mut m=manager();let mut request=None;
  for _ in 0..commands {
   prepare(&mut m,0).unwrap();
   let published=m.publish_render_usc_freelist_request(0).unwrap();
   assert_eq!(published.is_some(),request.is_none());
   let l=m.render_slots.get(0).bootstrap_uma.unwrap();request=l.request;
   assert!(l.request_published);assert_eq!(l.state,G17PBootstrapUmaRequestState::Submitted);
  }
  assert_eq!(m.resources.records.len(),1);
  let l=m.render_slots.get(0).bootstrap_uma.unwrap();assert_eq!(l.hardware_buffer_references,commands*2+1);assert_eq!(l.pending_release_count,commands*2);
  // Firmware completion can arrive while this exact graph is borrowed.
  let graph=m.render_slots.get_mut(0).storage.take().unwrap();m.render_slots.get_mut(0).preparing=true;
  m.match_render_usc_freelist_completion(request.unwrap()).unwrap();
  assert!(!graph.grown.get()); // no touching memory held by preparer
  assert_eq!(m.match_render_usc_freelist_completion(request.unwrap()),Err(EIO));
  let l=m.render_slots.get_mut(0).bootstrap_uma.as_mut().unwrap();assert_eq!(l.hardware_buffer_references,commands*2);
  for n in 0..commands {let release=l.retire_render_references().unwrap();assert_eq!(release.is_some(),n+1==commands);}
  assert_eq!(l.hardware_buffer_references,0);assert_eq!(l.pending_release_count,0);
 }
}
#[test]fn failed_ring_commit_does_not_announce_or_consume_refs_and_pending_reacquire_needs_no_ring_space(){
 let mut m=manager();prepare(&mut m,0).unwrap();let before=m.render_slots.get(0).prepared_render_flist;
 m.resources.full=true;assert!(matches!(m.publish_render_usc_freelist_request(0),Err(EAGAIN)));
 assert_eq!(m.render_slots.get(0).prepared_render_flist,before);assert!(m.render_slots.get(0).bootstrap_uma.is_none());
 m.resources.full=false;assert!(m.publish_render_usc_freelist_request(0).unwrap().is_some());
 m.resources.full=true;prepare(&mut m,0).unwrap();assert!(m.publish_render_usc_freelist_request(0).unwrap().is_none());
 assert_eq!(m.resources.records.len(),1);assert_eq!(m.render_slots.get(0).bootstrap_uma.unwrap().hardware_buffer_references,5);
}
#[test]fn completion_identity_and_foreign_owner_fail_without_changing_live_lease(){
 let mut m=manager();for slot in 0..2{prepare(&mut m,slot).unwrap();m.publish_render_usc_freelist_request(slot).unwrap();}
 let before=m.render_slots.get(0).bootstrap_uma.unwrap();let mut wrong=before.request.unwrap();wrong.cookie+=1;
 assert_eq!(m.match_render_usc_freelist_completion(wrong),Err(EIO));assert_eq!(m.render_slots.get(0).bootstrap_uma,Some(before));
 assert!(G17PBootstrapUmaLifecycle::acquire_owner(Some(&before),before.sequence+1,129,before.shared_control+1,before.operand_table,before.cookie,true).is_err());
 m.match_render_usc_freelist_completion(before.request.unwrap()).unwrap();assert!(m.render_slots.get(0).storage.as_ref().unwrap().grown.get());
 assert_eq!(m.render_slots.get(1).bootstrap_uma.unwrap().state,G17PBootstrapUmaRequestState::Submitted);
}
'''
with tempfile.TemporaryDirectory() as temp:
 src=Path(temp)/'test.rs';src.write_text(body);exe=Path(temp)/'test'
 subprocess.run(['rustc','--edition=2021','--test',str(src),'-o',str(exe)],check=True)
 subprocess.run([str(exe)],check=True)
request=block(runtime,r'^    fn publish_render_usc_freelist_request\(')
a=request.index('.send_message(')
assert 'fsleep(' not in request[a:]
assert 'control_counters' not in request and 'render_usc_freelist_grow_matched' not in request
returned=block(manager,r'^    pub\(crate\) fn return_render_restage\(')
assert returned.index('owner.matches_owner(')<returned.index('storage.mark_render_flist_grown()')<returned.index('slot.storage = Some(storage)')
assert 'G17PBootstrapUmaRequestState::Submitted' not in block(manager,r'^    pub\(crate\) fn take_render_restage\(')
assert 'G17PBootstrapUmaRequestState::Submitted' in block(manager,r'^    pub\(crate\) fn take_render_observation\(')
print('PASS actual FList publisher/matcher: 1..64 commands share one async grow; exact refs, ring retry, borrowed graph, foreign/duplicate completion, no post-announce wait')
