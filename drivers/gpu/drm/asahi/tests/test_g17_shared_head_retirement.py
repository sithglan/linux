#!/usr/bin/env python3
"""Exercise production pair retirement independently of owner-wide TSQ occupancy."""
from pathlib import Path
import subprocess
import tempfile
root = Path(__file__).resolve().parents[1]
source = (root / 'g17_manager.rs').read_text()
def item(marker):
    start = source.index(marker)
    end = source.index('{', start) + 1
    depth = 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[start:end]
rust = r'''
#![allow(dead_code)]
type Result<T> = std::result::Result<T,i32>;
const EBUSY:i32=16;
#[derive(Clone,Copy)] struct Queue(bool); impl Queue {fn idle(&self)->bool{self.0}}
#[derive(Clone,Copy)] struct Snapshot{job_list_empty:bool,tiling:Queue,fragment:Queue}
struct Storage{context:u16,shared:bool,result:Result<Snapshot>}
impl Storage{fn descriptor_context_id(&self)->u16{self.context}fn has_shared_tsq(&self)->bool{self.shared}fn snapshot(&mut self,_:u64)->Result<Snapshot>{self.result}}
#[derive(PartialEq)] enum G17PBootstrapUmaRequestState{Completed,Pending}
struct G17PBootstrapUmaLifecycle;
// Shared FList: per-pair FList leases and the last retired FList ordinal.
#[derive(Default)] struct Leases(bool); impl Leases {fn is_some(&self)->bool{self.0} fn is_none(&self)->bool{!self.0}}
struct Bind(u8);impl Bind{fn root_slot(&self)->Result<u8>{Ok(self.0)}}
struct G17PRenderSlotState{preparing:bool,bind:Option<Bind>,storage:Option<Box<Storage>>,submission_ordinal:u64,tracker:Option<()>,queued_tracker:Option<()>,hardware_buffer_id:Option<()>,hardware_buffer_published:bool,prepared_render_flist:Option<()>,flist_leases:Leases,flist_retired_ordinal:Option<u64>}
struct Manager;
impl Manager {
''' + item('    fn render_slot_idle_for_context(') + item('    fn render_slot_graph_never_kicked(') + r'''
}
fn state(shared:bool)->G17PRenderSlotState{G17PRenderSlotState{preparing:false,bind:Some(Bind(7)),storage:Some(Box::new(Storage{context:7,shared,result:Ok(Snapshot{job_list_empty:false,tiling:Queue(true),fragment:Queue(true)})})),submission_ordinal:1,tracker:None,queued_tracker:None,hardware_buffer_id:None,hardware_buffer_published:false,prepared_render_flist:None,flist_leases:Leases(false),flist_retired_ordinal:None}}
#[test]fn shared_head_other_members_do_not_serialize_retired_pair(){assert_eq!(Manager::render_slot_idle_for_context(&mut state(true),7,&[]),Ok(true));}
#[test]fn private_head_still_requires_empty_list(){assert_eq!(Manager::render_slot_idle_for_context(&mut state(false),7,&[]),Ok(false));}
#[test]fn each_live_host_owner_blocks_retirement(){for n in 0..7{let mut s=state(true);match n{0=>s.tracker=Some(()),1=>s.queued_tracker=Some(()),2=>s.hardware_buffer_id=Some(()),3=>s.hardware_buffer_published=true,4=>s.flist_leases=Leases(true),5=>s.flist_retired_ordinal=Some(1),_=>s.prepared_render_flist=Some(())};assert_eq!(Manager::render_slot_idle_for_context(&mut s,7,&[]),Ok(false));}}
#[test]fn either_busy_physical_queue_blocks_retirement(){for ta in [false,true]{let mut s=state(true);s.storage.as_mut().unwrap().result=Ok(Snapshot{job_list_empty:false,tiling:Queue(ta),fragment:Queue(!ta)});assert_eq!(Manager::render_slot_idle_for_context(&mut s,7,&[]),Ok(false));}}
#[test]fn unreadable_firmware_is_never_assumed_retired(){let mut s=state(true);s.storage.as_mut().unwrap().result=Err(EBUSY);assert_eq!(Manager::render_slot_idle_for_context(&mut s,7,&[]),Ok(false));s.storage.as_mut().unwrap().result=Err(5);assert_eq!(Manager::render_slot_idle_for_context(&mut s,7,&[]),Err(5));}
#[test]fn unpublished_graph_and_unrelated_context_do_not_block(){let mut s=state(true);s.submission_ordinal=0;assert_eq!(Manager::render_slot_idle_for_context(&mut s,7,&[]),Ok(true));s.tracker=Some(());assert_eq!(Manager::render_slot_idle_for_context(&mut s,8,&[]),Ok(true));assert_eq!(Manager::render_slot_idle_for_context(&mut s,7,&[]),Ok(false));}
#[test]fn preparing_owner_blocks_only_its_context(){let mut s=state(true);s.preparing=true;let storage=s.storage.take().unwrap();assert_eq!(Manager::render_slot_idle_for_context(&mut s,7,&[]),Ok(false));assert_eq!(Manager::render_slot_idle_for_context(&mut s,8,&[]),Ok(true));assert!(!Manager::render_slot_graph_never_kicked(&s));s.storage=Some(storage);s.preparing=false;assert_eq!(Manager::render_slot_idle_for_context(&mut s,7,&[]),Ok(true));}
'''
with tempfile.TemporaryDirectory() as d:
    src=Path(d)/'retirement.rs';src.write_text(rust)
    binary=Path(d)/'retirement'
    subprocess.run(['rustc','--edition=2021','--test',str(src),'-o',str(binary)],check=True)
    subprocess.run([str(binary)],check=True)
print('PASS production pair retirement gates; snapshots are fixtures, not firmware simulation')
