#!/usr/bin/env python3
"""Compile production timeout readers against bounded DRAM mocks; no GPU claim."""
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
resources = (root / "g17_resources.rs").read_text()
manager = (root / "g17_manager.rs").read_text()


def block(source, marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


def constant(source, name):
    return re.search(r"(?:pub\(crate\) )?const " + name + r": .*?;", source).group()


# The report half lives in its own cold frame (stack depth).
diagnostic = block(manager, "    fn log_async_compute_timeout_report(") + block(manager, "    pub(crate) fn log_async_compute_timeout(")
assert "queue.completion_descriptor" in diagnostic
assert "registers" not in diagnostic and "fsleep" not in diagnostic
assert "snapshot_initial_channel_group" not in diagnostic  # fixed slot zero is wrong
assert "compute_b2_channel_control" in diagnostic
assert "qos_queue_record" in diagnostic
assert diagnostic.index("read_compute_ring_state") < diagnostic.index('"G17P async-timeout outer')

source = r'''
#![allow(dead_code)]
type Result<T=()> = std::result::Result<T,i32>;
const EINVAL:i32=22;const ERANGE:i32=34;const EOVERFLOW:i32=75;
mod mmu { pub const UAT_PGSZ:usize=0x4000; }
'''
submission_source = (root / "g17_submission.rs").read_text()
source += "mod g17_submission {" + constant(submission_source, "G17P_CL_KICK_ENTRY_SIZE") + "}\n"
compute = (root / "g17_compute.rs").read_text()
source += "mod g17_compute {\n" + "\n".join(constant(compute, name) for name in [
    "COMPUTE_REGISTER_COUNT", "COMPUTE_REGISTER_MAX", "COMPUTE_OPTIONAL", "COMPUTE_EVENT",
    "COMPUTE_ADD_KICKS",
    "COMPUTE_QUEUE_POINTERS", "COMPUTE_ITEM_RING"]) + "}\n"
source += "\n".join(constant(resources, name) for name in [
    "G17P_COMPUTE_DESCRIPTOR_SLOT_SIZE", "G17P_COMPUTE_STAMP_SUPPORT_OFFSET"])
source += r'''
''' + block(resources, "fn g17p_live_word_bounds(") + "\n" + block(resources, "fn g17p_live_read_u32(") + r'''
#[derive(Clone,PartialEq,Eq,Debug)]struct Bytes{base:u64,raw:Vec<u8>}
impl Bytes {
 fn iova(&self)->u64{self.base}
 fn with_bytes_mut<T>(&mut self,f:impl FnOnce(&mut[u8])->Result<T>)->Result<T>{f(&mut self.raw)}
 fn read_u32(&self,va:u64)->Result<u32>{let at=va.checked_sub(self.base).ok_or(ERANGE)? as usize;Ok(u32::from_le_bytes(self.raw.get(at..at+4).ok_or(ERANGE)?.try_into().unwrap()))}
 fn read_live_u32(&mut self,va:u64)->Result<u32>{let at=va.checked_sub(self.base).ok_or(EINVAL)? as usize;g17p_live_read_u32(&self.raw,at)}
 fn read_u64(&mut self,va:u64)->Result<u64>{let at=va.checked_sub(self.base).ok_or(ERANGE)? as usize;Ok(u64::from_le_bytes(self.raw.get(at..at+8).ok_or(ERANGE)?.try_into().unwrap()))}
}
struct Storage{descriptors:Bytes,descriptors_high:Bytes,support:Bytes,queue_graph:Bytes,entries:Bytes,ring_reclaimed:u32,ring_reclaimed_total:u64}
struct OwnerResources{partial_computed:Bytes}
struct Owner{resources:OwnerResources}
'''
for name in ["G17PComputeSubmissionSnapshot", "G17PComputeRingState"]:
    source += "#[derive(Debug,Copy,Clone)]\n" + block(resources, "pub(crate) struct " + name)
source += "impl Storage {\n" + "\n".join(block(resources, marker) for marker in [
    "    pub(crate) fn snapshot_compute_submission(",
    "    pub(crate) fn read_compute_stamp(",
    "    pub(crate) fn snapshot_compute_entry(",
    "    pub(crate) fn read_compute_ring_state("]) + "}\n"
source += "impl Owner {\n" + block(resources, "    pub(crate) fn compute_b2_channel_control(") + "}\n"
source += r'''
fn put32(b:&mut[u8],at:usize,v:u32){b[at..at+4].copy_from_slice(&v.to_le_bytes());}
fn put64(b:&mut[u8],at:usize,v:u64){b[at..at+8].copy_from_slice(&v.to_le_bytes());}
fn storage(qid:u32)->Storage {
 let base=0xffff_fc20_0000_0000+u64::from(qid)*0x100000;
 let b=|base,len|Bytes{base,raw:vec![0;len]};
 let mut s=Storage{descriptors:b(0x1000,0x8000),descriptors_high:b(base,0),support:b(base+0x10000,0x8000),queue_graph:b(base+0x20000,0x8000),entries:b(base+0x30000,0x400),ring_reclaimed:0,ring_reclaimed_total:0};
 for slot in 0..2 {
  let at=slot*G17P_COMPUTE_DESCRIPTOR_SLOT_SIZE;
  put32(&mut s.descriptors.raw,at,3);put32(&mut s.descriptors.raw,at+0x0c,qid+slot as u32);
  put32(&mut s.descriptors.raw,at+0xf54,qid);
  put32(&mut s.descriptors.raw,at+0x40,0x10201);
  put64(&mut s.descriptors.raw,at+0x44,((qid+slot as u32) as u64)<<8|slot as u64);
  put64(&mut s.descriptors.raw,at+0x10,s.support.base+0x100);
  put64(&mut s.descriptors.raw,at+0xf40,s.support.base+0x300);
  put32(&mut s.descriptors.raw,at+0xf50,(slot as u32+1)<<8);
 }
 put32(&mut s.support.raw,0x4004,2);put32(&mut s.support.raw,0x10c,1);
 put64(&mut s.queue_graph.raw,g17_compute::COMPUTE_OPTIONAL,15);
 put64(&mut s.queue_graph.raw,g17_compute::COMPUTE_EVENT,16);
 put64(&mut s.queue_graph.raw,g17_compute::COMPUTE_ADD_KICKS,14);
 s
}
#[test]fn selected_slot_and_qids_four_through_seven_are_read_only(){
 for qid in 4..=7 {
  let mut s=storage(qid);let before=(s.descriptors.clone(),s.support.clone(),s.queue_graph.clone());
  let r=s.snapshot_compute_submission(s.descriptors_high.base+0x4000).unwrap();
  assert_eq!(r.selector,3);assert_eq!(r.context,qid+1);assert_eq!(r.grid,qid);
  assert_eq!(r.work_key,Some(((qid+1) as u64)<<8|1));assert_eq!(r.tsq,[2,1,0]);
  assert_eq!(r.stamp_value,512);assert_eq!(r.scheduler_record,s.support.base+0x100);
  assert_eq!(r.stamp_pointer,s.support.base+0x300);
  assert_eq!([r.tag15[0],r.tag16[0],r.tag14[0]],[15,16,14]);
  assert_eq!(before,(s.descriptors,s.support,s.queue_graph));
 }
}
#[test]fn foreign_misaligned_out_of_range_and_short_objects_rejected(){
 let mut s=storage(7);let base=s.descriptors_high.base;
 for va in [base-1,base+1,base+0x8000,u64::MAX] {assert!(s.snapshot_compute_submission(va).is_err());}
 s.support.raw.truncate(4);assert!(s.snapshot_compute_submission(base).is_err());
 let mut s=storage(7);s.queue_graph.raw.truncate(4);assert!(s.snapshot_compute_submission(base).is_err());
}
#[test]fn consumption_is_not_a_completion_witness(){
 let mut s=storage(7);let base=s.descriptors_high.base;
 for consumer in [0,4] {
  put32(&mut s.queue_graph.raw,g17_compute::COMPUTE_QUEUE_POINTERS,consumer);
  put32(&mut s.queue_graph.raw,g17_compute::COMPUTE_QUEUE_POINTERS+0x40,4);
  let ring=s.read_compute_ring_state().unwrap();assert_eq!((ring.consumer,ring.producer),(consumer,4));
  let snapshot=s.snapshot_compute_submission(base).unwrap();assert_eq!(snapshot.tsq,[2,1,0]);
 }
}
#[test]fn sksm_entry_readback_is_bounded_and_does_not_mutate(){
 let mut s=storage(7);put64(&mut s.entries.raw,0x200,0x1234);
 let before=s.entries.clone();let words=s.snapshot_compute_entry(0x200).unwrap();
 assert_eq!(words[0],0x1234);assert_eq!(words[47],0);
 assert_eq!(s.entries,before);
 assert!(s.snapshot_compute_entry(0x300).is_err());
 assert!(s.snapshot_compute_entry(usize::MAX).is_err());
}
#[test]fn b2_channel_control_read_is_bounded_and_does_not_mutate(){
 let mut b=Bytes{base:0x1000,raw:vec![0;0x1000]};put64(&mut b.raw,0x808,0xdeadbeef);
 let before=b.clone();let mut o=Owner{resources:OwnerResources{partial_computed:b}};
 assert_eq!(o.compute_b2_channel_control().unwrap(),0xdeadbeef);
 assert_eq!(o.resources.partial_computed,before);
 let mut o=Owner{resources:OwnerResources{partial_computed:Bytes{base:u64::MAX-0x807,raw:vec![0;0x1000]}}};
 assert_eq!(o.compute_b2_channel_control(),Err(EOVERFLOW));
}
'''
with tempfile.TemporaryDirectory() as temp:
    path = Path(temp)
    (path / "test.rs").write_text(source)
    subprocess.run(["rustc", "--edition=2021", "--test", str(path / "test.rs"), "-o", str(path / "test")], check=True)
    subprocess.run([str(path / "test")], check=True)
print("PASS actual timeout readers: selected descriptor, QID 4..7, bounds, non-mutating reads; no hardware claim")
