#!/usr/bin/env python3
"""Exercise the shared compute UMA/FList readers without modifying firmware state."""
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
resources = (root / "g17_resources.rs").read_text()
submission = (root / "g17_submission.rs").read_text()
manager = (root / "g17_manager.rs").read_text()


def block(source, marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


def constant(name):
    return re.search(r"pub\(crate\) const " + name + r": .*?;", submission).group()


diagnostic = block(manager, "    fn log_async_compute_timeout_report(") + block(manager, "    pub(crate) fn log_async_compute_timeout(")
assert "let pool_id = queue.storage.flist_id() as u8" in diagnostic
assert "snapshot_compute_uma(pool_id)" in diagnostic
assert "queue.storage.snapshot_compute_flist()" in diagnostic
assert "queue.storage.shared_support_gpu_va()" in diagnostic
assert "uma-slot={} " in diagnostic and "flist-index={} " in diagnostic
runtime_storage = block(resources, "pub(crate) struct G17PClRuntimeStorage {")
assert "shared_support: MappedObject" in runtime_storage
assert "support_state: MappedObject" in runtime_storage
assert "scheduler_state: MappedObject" in runtime_storage
assert "compute_shared_runtime" not in manager

source = r'''
#![allow(dead_code)]
type Result<T=()> = std::result::Result<T,i32>;
const EINVAL:i32=22;
#[derive(Clone,Debug,PartialEq,Eq)] struct Bytes(Vec<u8>);
impl Bytes {
 fn with_bytes_mut<T>(&mut self, f:impl FnOnce(&mut[u8])->Result<T>)->Result<T> {
  f(&mut self.0)
 }
}
struct Resources { uma_page_pool_descriptor_table:Bytes }
struct Owner { resources:Resources }
struct Queue { shared_support:Bytes, qid:u32 }
'''
source += "mod g17_submission {\n" + "\n".join(constant(name) for name in [
    "G17P_UMA_DESC_ENTRY_SIZE", "G17P_UMA_DESC_ENTRY_COUNT"])
source += block(submission, "pub(crate) const fn g17p_umadesc_offset(") + "}\n"
source += "impl Owner {" + block(resources, "    pub(crate) fn snapshot_compute_uma(") + "}\n"
source += "impl Queue {" + block(resources, "    pub(crate) fn snapshot_compute_flist(") + "}\n"
source += r'''
fn table()->Owner {
 let mut bytes=vec![0;256*32];
 for slot in 0..256 {
  for word in 0..4 {
   let offset=slot*32+word*8;
   bytes[offset..offset+8].copy_from_slice(&(0x1234000000000000u64 | (slot as u64)<<8 | word as u64).to_le_bytes());
  }
 }
 Owner {resources:Resources {uma_page_pool_descriptor_table:Bytes(bytes)}}
}
fn queue(id:u32)->Queue {
 let mut bytes=vec![0;112];
 for word in 0..28 {
  bytes[word*4..word*4+4].copy_from_slice(&(id<<16 | word as u32).to_le_bytes());
 }
 Queue {shared_support:Bytes(bytes),qid:id}
}
#[test] fn every_uma_slot_selects_its_owner_without_writing() {
 let mut owner=table();let before=owner.resources.uma_page_pool_descriptor_table.clone();
 for slot in 0..=255u8 {
  let words=owner.snapshot_compute_uma(slot).unwrap();
  for word in 0..4 {
   assert_eq!(words[word],0x1234000000000000 | u64::from(slot)<<8 | word as u64);
  }
 }
 assert_eq!(before,owner.resources.uma_page_pool_descriptor_table);
}
#[test] fn queue_local_progress_survives_more_than_ten_thousand_owners() {
 let run_table=[0x4000_0000u32,0x70];
 let mut queues:Vec<_>=(0..10_001).map(queue).collect();
 for (qid,queue) in queues.iter_mut().enumerate(){
  queue.shared_support.0[12*4..14*4].copy_from_slice(&[
   run_table[0].to_le_bytes(),run_table[1].to_le_bytes()].concat());
  queue.shared_support.0[21*4..22*4].copy_from_slice(&(qid as u32).to_le_bytes());
  let words=queue.snapshot_compute_flist().unwrap();
  assert_eq!([words[12],words[13]],run_table);
  assert_eq!(words[21],qid as u32);assert_eq!(queue.qid,qid as u32);
 }
}
#[test] fn short_objects_fail_without_panicking_or_writing() {
 let mut owner=table();owner.resources.uma_page_pool_descriptor_table.0.truncate(4*32-1);
 let before=owner.resources.uma_page_pool_descriptor_table.clone();
 assert!(owner.snapshot_compute_uma(2).is_ok());
 assert_eq!(owner.snapshot_compute_uma(3),Err(EINVAL));
 assert_eq!(owner.snapshot_compute_uma(255),Err(EINVAL));
 assert_eq!(before,owner.resources.uma_page_pool_descriptor_table);
 for size in [0,111] {
  let mut queue=queue(3);queue.shared_support.0.truncate(size);let before=queue.shared_support.clone();
  assert_eq!(queue.snapshot_compute_flist(),Err(EINVAL));assert_eq!(before,queue.shared_support);
 }
 let mut queue=queue(3);queue.shared_support.0.truncate(112);assert!(queue.snapshot_compute_flist().is_ok());
}
'''
with tempfile.TemporaryDirectory() as temp:
    path = Path(temp)
    (path / "test.rs").write_text(source)
    subprocess.run(["rustc", "--edition=2021", "--test", str(path / "test.rs"),
                    "-o", str(path / "test")], check=True)
    subprocess.run([str(path / "test")], check=True)
print("PASS FList snapshot readers: run-table fields, 10001 identities, bounds (not an ownership model)")
