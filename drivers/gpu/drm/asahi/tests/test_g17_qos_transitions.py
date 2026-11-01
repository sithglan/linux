#!/usr/bin/env python3
"""Run production QoS accounting/rollback and CL ConfigUpdate encoders.

Only memory mapping and the architecture clock are mocked. This does not
model firmware execution or establish that the mixed hardware fault is fixed.
"""
from pathlib import Path
import json
import re
import subprocess
import tempfile
from g17_config_test_support import with_g17_config

root = Path(__file__).resolve().parents[1]
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


publish = item(resources, "    fn publish_qos_queue_record(")
publish, n = re.subn(r"        let submit_time: u64;.*?        let mut published",
                    "        let submit_time = CLOCK.with(|c| { let n = c.get()+1; c.set(n); n });\n        let mut published",
                    publish, flags=re.S)
assert n == 1
methods = publish + item(resources, "    pub(crate) fn cancel_qos_publication(")
methods += item(resources, "    pub(crate) fn complete_qos_queue_records(")
first = item(manager, "    fn submit_translated_first_bind_channel(")
repeat = item(manager, "    fn submit_translated_repeat_channel(")
for body in [first, repeat]:
    assert body.index("publish_compute_channel(") < body.index("queue.published_config =")
    assert "cancel_qos_publication(qos_publication)" in body
assert "config_identity.updates_from(queue.published_config)" in repeat
poll = item(manager, "    pub(crate) fn poll_staged_independent_compute_completion(")
assert poll.index("read_compute_stamp()?") < poll.index("complete_qos_queue_records(")
assert poll.index("complete_qos_queue_records(") < poll.index("queue.completion_descriptor = 0")
reset = item(resources, "    pub(crate) fn reset_session_after_processor_stop(")
for field in ["qos_in_flight", "qos_dm_in_flight", "qos_dm_by_qid"]:
    assert f"self.{field}.fill(0)" in reset

code = r'''
#![allow(dead_code)]
use std::cell::Cell;
type Result<T=()> = std::result::Result<T, i32>;
const EINVAL:i32=22;const EIO:i32=5;const EBUSY:i32=16;const EOVERFLOW:i32=75;
const G17P_QOS_QUEUE_COUNT:usize=128;
thread_local!{static CLOCK:Cell<u64>=const{Cell::new(100)};}
struct Bytes { raw:Vec<u8>, fail:bool }
impl Bytes { fn with_bytes_mut<T>(&mut self,f:impl FnOnce(&mut[u8])->Result<T>)->Result<T>{if self.fail {Err(EIO)}else{f(&mut self.raw)}} }
struct Resources{qos_resource:Bytes}
struct Owner{resources:Resources,qos_in_flight:[u32;128],qos_dm_in_flight:[u64;4],qos_dm_by_qid:[u8;128]}
''' + "#[derive(Debug,Copy,Clone)]\n" + item(resources, "pub(crate) struct G17PQosPublication {")
code += resources[resources.index("fn g17p_live_word_bounds("):resources.index("fn put_u32(")]
code += "\nimpl Owner {\n" + methods + r'''
fn new()->Self{Self{resources:Resources{qos_resource:Bytes{raw:vec![0;0xe40],fail:false}},qos_in_flight:[0;128],qos_dm_in_flight:[0;4],qos_dm_by_qid:[0;128]}}
fn submit(&mut self,qid:u8,hbid:u8,dm:u8)->G17PQosPublication{self.publish_qos_queue_record(qid,hbid,15,0x100000+hbid as u64*0x4000,dm,2000).unwrap()}
fn bytes(&self)->&[u8]{&self.resources.qos_resource.raw}
fn word(&self,at:usize)->u64{u64::from_le_bytes(self.bytes()[at..at+8].try_into().unwrap())}
fn count(&self,at:usize)->u32{u32::from_le_bytes(self.bytes()[at..at+4].try_into().unwrap())}
}
'''
code += '\n#[path = ' + json.dumps(str(root / "g17_compute.rs")) + '] mod g17_compute;\n'
code = with_g17_config(code)
code += r'''
use g17_compute::{ComputeConfigIdentity as Identity,ComputeConfigUpdates as Updates};
fn identity(page:u64,id:u8,context:u16)->Identity{Identity{channel_control:page,qos_id:id,context_id:Some(context)}}
#[test]fn unchanged_repeat_keeps_context_and_qos_caches_after_idle(){
 let mut o=Owner::new();let current=identity(0x100000,4,7);
 assert_eq!(current.updates_from(None),Updates{qos:true,context:true});
 o.submit(4,4,2);o.complete_qos_queue_records(&[(4,4,2)]).unwrap();
 assert_eq!(&o.bytes()[32..34],&[255,0]);o.submit(4,4,2);
 assert_eq!(&o.bytes()[32..34],&[4,15]);
 assert_eq!(current.updates_from(Some(current)),Updates{qos:false,context:false});
}
#[test]fn logical_rebind_reasserts_flags_even_if_numeric_ids_reused(){
 let old=identity(0x100000,4,7);let new=identity(0x200000,4,7);
 assert_eq!(new.updates_from(Some(old)),Updates{qos:true,context:true});
 assert_eq!(identity(old.channel_control,5,7).updates_from(Some(old)),Updates{qos:true,context:false});
 assert_eq!(identity(old.channel_control,4,8).updates_from(Some(old)),Updates{qos:false,context:true});
 // Preparing a new identity, then rolling it back, leaves publication cache old.
 let _prepared=new.updates_from(Some(old));
 assert_eq!(old.updates_from(Some(old)),Updates{qos:false,context:false});
}
#[test]fn encoder_separates_install_qos_and_context_flags(){
 use g17_compute::*;
 for install in [false,true]{for qos in [false,true]{for context in [false,true]{
 let mut raw=[0;COMPUTE_QUEUE_GRAPH_SIZE];
 encode_compute_config_update(&mut raw,ComputeSksmEntryAliases::new(0x7004000000,0xfffffc2000400000,0x20000,0x4000).unwrap(),0x10000,0x20000,4,3,install,Updates{qos,context},Some(7)).unwrap();
 for (at,value) in [(0x1a,install),(0x52,qos),(0x62,context)]{assert_eq!(u32::from_le_bytes(raw[COMPUTE_OPTIONAL+at..COMPUTE_OPTIONAL+at+4].try_into().unwrap()),u32::from(value));}
 }}}
}
#[test]fn shared_owner_engines_retire_independently_and_preserve_monotonic_totals(){
 let mut o=Owner::new();for(q,dm)in[(0,0),(1,1),(4,2)]{o.submit(q,4,dm);}
 let before=o.bytes().to_vec();o.complete_qos_queue_records(&[(0,4,0),(1,4,1)]).unwrap();
 assert_eq!(&o.bytes()[32..34],&[4,15]);assert_eq!(o.qos_dm_in_flight,[0,0,1,0]);
 for i in 0..before.len(){if ![0,1,8,9].contains(&i){assert_eq!(o.bytes()[i],before[i],"offset {i:#x}");}}
 o.complete_qos_queue_records(&[(4,4,2)]).unwrap();assert_eq!(o.count(0x610),3);assert_eq!(o.qos_dm_in_flight,[0;4]);
}
#[test]fn retained_config_update_uses_full_width_logical_owner_pid(){
 use g17_compute::*;
 let mut raw=[0;COMPUTE_QUEUE_GRAPH_SIZE];
 for pid in [567u32,570,0x12345678]{raw[0x48..0x4c].copy_from_slice(&pid.to_le_bytes());
 encode_compute_config_update(&mut raw,ComputeSksmEntryAliases::new(0x7004000000,0xfffffc2000400000,0x20000,0x4000).unwrap(),0x10000,0x20000,4,3,false,Updates{qos:false,context:false},Some(7)).unwrap();
 assert_eq!(&raw[COMPUTE_OPTIONAL+0x5a..COMPUTE_OPTIONAL+0x5e],&pid.to_le_bytes());
 assert_eq!(&raw[0x48..0x4c],&pid.to_le_bytes());}
}
#[test]fn dm_busy_period_start_is_not_overwritten_by_concurrent_queues(){
 let mut o=Owner::new();o.submit(4,4,2);let time=o.word(0xe18);
 o.submit(5,5,2);assert_eq!(o.word(0xe18),time);assert_eq!(o.word(0xe00),1);
 o.complete_qos_queue_records(&[(4,4,2)]).unwrap();o.submit(6,6,2);assert_eq!(o.word(0xe18),time);
 o.complete_qos_queue_records(&[(5,5,2),(6,6,2)]).unwrap();o.submit(4,4,2);
 assert!(o.word(0xe18)>time);assert_eq!(o.word(0xe00),2);assert_eq!(o.word(0xe30),4);
}
#[test]fn multiple_outstanding_kicks_keep_membership_until_last_completion(){
 let mut o=Owner::new();o.submit(4,4,2);o.submit(4,4,2);
 o.complete_qos_queue_records(&[(4,4,2)]).unwrap();assert_eq!(&o.bytes()[32..34],&[4,15]);
 o.complete_qos_queue_records(&[(4,4,2)]).unwrap();assert_eq!(&o.bytes()[32..34],&[255,0]);
}
#[test]fn duplicate_stale_owner_and_wrong_engine_retirements_are_atomic(){
 let mut o=Owner::new();o.submit(0,4,0);o.submit(1,4,1);let old=o.bytes().to_vec();
 for owners in [vec![(0,4,0),(1,5,1)],vec![(0,4,0),(0,4,0)],vec![(0,4,1)],vec![(128,4,0)]]{
 assert!(o.complete_qos_queue_records(&owners).is_err());assert_eq!(o.bytes(),old);assert_eq!(o.qos_dm_in_flight,[1,1,0,0]);}
 o.complete_qos_queue_records(&[(0,4,0)]).unwrap();let old=o.bytes().to_vec();
 assert!(o.complete_qos_queue_records(&[(0,4,0)]).is_err());assert_eq!(o.bytes(),old);
}
#[test]fn unpublished_pair_cancellation_retires_rows_without_rewinding_shared_totals(){
 let mut o=Owner::new();let a=o.submit(1,4,1);let b=o.submit(0,4,0);let old=o.bytes().to_vec();
 o.cancel_qos_publication(b).unwrap();o.cancel_qos_publication(a).unwrap();
 for i in 0..old.len(){if ![0,1,8,9].contains(&i){assert_eq!(o.bytes()[i],old[i]);}}
 assert_eq!(&o.bytes()[0..2],&[255,0]);assert_eq!(&o.bytes()[8..10],&[255,0]);
 assert_eq!(o.qos_dm_in_flight,[0;4]);assert!(o.qos_in_flight.iter().all(|x|*x==0));
 assert!(o.cancel_qos_publication(a).is_err());
}
#[test]fn stale_undo_cannot_erase_newer_work(){
 let mut o=Owner::new();let a=o.submit(4,4,2);let b=o.submit(5,5,2);let before=o.bytes().to_vec();
 assert!(o.cancel_qos_publication(a).is_err());assert_eq!(o.bytes(),before);
 o.cancel_qos_publication(b).unwrap();assert!(o.cancel_qos_publication(a).is_err());
 o.complete_qos_queue_records(&[(4,4,2)]).unwrap();assert_eq!(o.qos_dm_in_flight,[0;4]);
}
#[test]fn mapping_and_bounds_failures_consume_no_accounting(){
 let mut o=Owner::new();o.resources.qos_resource.fail=true;
 assert!(o.publish_qos_queue_record(4,4,15,0x10000,2,2000).is_err());assert_eq!(o.qos_dm_in_flight,[0;4]);
 o.resources.qos_resource.fail=false;o.resources.qos_resource.raw.truncate(0xd00);let before=o.bytes().to_vec();
 assert!(o.publish_qos_queue_record(4,4,15,0x10000,2,2000).is_err());assert_eq!(o.bytes(),before);
 assert!(o.publish_qos_queue_record(4,4,15,0x10000,4,2000).is_err());assert_eq!(o.bytes(),before);
}
#[test]fn sustained_churn_has_no_active_accounting_debt(){
 let mut o=Owner::new();for n in 0..4096{let h=(n%126+2)as u8;for(q,dm)in[(0,0),(1,1),(4,2)]{o.submit(q,h,dm);}
 o.complete_qos_queue_records(&[(4,h,2),(0,h,0),(1,h,1)]).unwrap();assert_eq!(o.qos_dm_in_flight,[0;4]);}
 assert_eq!(o.count(0x410),4096);assert_eq!(o.word(0xe30),4096);assert_eq!(o.word(0xe00),3*4096);
}
'''
with tempfile.TemporaryDirectory(prefix="g17-qos-transitions-") as tmp:
    source, binary = Path(tmp) / "test.rs", Path(tmp) / "test"
    source.write_text(code)
    built = subprocess.run(["rustc", "--edition=2021", "--test", str(source), "-o", str(binary)], capture_output=True, text=True)
    if built.returncode:
        raise SystemExit(built.stderr[:12000])
    subprocess.run([str(binary)], check=True)
print("PASS production QoS transition/accounting tests; hardware validation required")
