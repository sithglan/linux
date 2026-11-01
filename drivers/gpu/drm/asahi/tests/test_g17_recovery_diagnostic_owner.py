#!/usr/bin/env python3
"""Recovery diagnostics route by explicit physical pair and cover live peers."""
from pathlib import Path
import subprocess
import tempfile

s=(Path(__file__).resolve().parents[1]/'g17_manager.rs').read_text()
start=s.index('    pub(crate) fn log_render_recovery_pre_ack(')
end=s.index('{',start)+1
depth=1
while depth:
    depth+=(s[end]=='{')-(s[end]=='}');end+=1
method=s[start:end]
pick_start=s.index('                    let exact = fault_target.and_then')
pick_end=s.index('                    if let Some(tracker) = exact.or',pick_start)
pick=s[pick_start:pick_end]
program=r'''
macro_rules! dev_info {($($t:tt)*)=>{()}}
mod g17_resources {pub struct G17PRenderQueuePair;impl G17PRenderQueuePair{pub const SLOT_COUNT:u8=64;}}
mod module_parameters {
 pub static FLAGS:std::sync::atomic::AtomicU32=std::sync::atomic::AtomicU32::new(0);
 pub struct Param;impl Param {pub fn value(&self)->&'static u32{if FLAGS.load(std::sync::atomic::Ordering::SeqCst)==4{&4}else{&0}}}
 #[allow(non_upper_case_globals)]pub static g17p_fault_report:Param=Param;
}
struct Pair {storage:Option<()>,tracker:Option<()>,queued_tracker:Option<()>}
struct Slots {pairs:[Option<Pair>;64]}
impl Slots{fn allocated(&self,slot:u8)->Result<&Pair,i32>{self.pairs.get(slot as usize).and_then(Option::as_ref).ok_or(19)}}
struct Manager {render_slots:Slots,blamed:Option<u8>,captured:Vec<u8>}
impl Manager {
 fn new(blamed:Option<u8>)->Self{
  module_parameters::FLAGS.store(0,std::sync::atomic::Ordering::SeqCst);
  let mut pairs=[const{None};64];for slot in [5,6] {pairs[slot]=Some(Pair{storage:Some(()),tracker:Some(()),queued_tracker:None});}
  Self{render_slots:Slots{pairs},blamed,captured:Vec::new()}
 }
 fn pending_primary_recovery(&self)->Option<(u64,u32,Option<u8>)>{Some((0,1,self.blamed))}
 fn render_slot_for_qid(&self,qid:u8)->Option<u8>{match qid{16|17=>Some(5),14|15=>Some(6),_=>None}}
 fn log_selected_render_recovery_pre_ack(&mut self,slot:u8,_:&'static str,_:Option<u8>){self.captured.push(slot)}
'''+method+r'''
}
#[test]fn blamed_physical_pair_is_inspected_without_a_publisher_cursor(){
 let mut m=Manager::new(Some(17));m.log_render_recovery_pre_ack("halted");assert_eq!(m.captured,[5]);
}
#[test]fn unrelated_compute_fault_does_not_invent_a_render_owner(){
 for blamed in [None,Some(13)]{let mut m=Manager::new(blamed);m.log_render_recovery_pre_ack("halted");assert!(m.captured.is_empty());}
}
#[test]fn unallocated_blamed_pair_is_not_created_or_substituted(){
 let mut m=Manager::new(Some(17));m.render_slots.pairs[5]=None;m.log_render_recovery_pre_ack("halted");assert!(m.captured.is_empty());assert!(m.render_slots.pairs[5].is_none());
}
#[test]fn detailed_recovery_covers_each_live_peer_once(){
 let mut m=Manager::new(Some(17));module_parameters::FLAGS.store(4,std::sync::atomic::Ordering::SeqCst);
 m.log_render_recovery_pre_ack("halted");assert_eq!(m.captured,[5,6]);
}
'''
program += r'''
struct Tracker {ordinal:u64,native_timestamps:Option<[u64;2]>}
struct Slot {tracker:Option<Tracker>,queued_tracker:Vec<Tracker>}
fn pick(slot:&Slot,fault_target:Option<(usize,u64)>)->(Option<u64>,bool){
''' + pick + r'''
 (exact.or(slot.tracker.as_ref()).map(|tracker|tracker.ordinal),exact.is_some())
}
#[test]fn hardware_frontier_can_be_ahead_of_oldest_host_tracker(){
 let slot=Slot{tracker:Some(Tracker{ordinal:1012,native_timestamps:Some([1013,1013])}),
 queued_tracker:(1013..1020).map(|ordinal|Tracker{ordinal,native_timestamps:Some([ordinal+1,ordinal+1])}).collect()};
 assert_eq!(pick(&slot,Some((1,1016))),(Some(1015),true));
 assert_eq!(pick(&slot,Some((0,1017))),(Some(1016),true));
}
#[test]fn missing_frontier_is_explicitly_labelled_as_host_fallback(){
 let slot=Slot{tracker:Some(Tracker{ordinal:9,native_timestamps:Some([10,10])}),queued_tracker:Vec::new()};
 assert_eq!(pick(&slot,Some((1,15))),(Some(9),false));
 assert_eq!(pick(&slot,None),(Some(9),false));
}
#[test]fn native_timestamp_match_keeps_wrap_generation(){
 let slot=Slot{tracker:Some(Tracker{ordinal:0,native_timestamps:Some([1,1])}),queued_tracker:vec![Tracker{ordinal:256,native_timestamps:Some([257,257])}]};
 assert_eq!(pick(&slot,Some((1,257))),(Some(256),true));
}
'''
with tempfile.TemporaryDirectory() as d:
    path=Path(d)/'owner.rs';exe=Path(d)/'owner'
    path.write_text(program)
    subprocess.run(['rustc','--edition=2021','--test',str(path),'-o',str(exe)],check=True)
    subprocess.run([str(exe),"--test-threads=1"],check=True)
