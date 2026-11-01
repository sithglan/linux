#!/usr/bin/env python3
"""Exercise the production one-pass G17P completion batch index."""
from pathlib import Path
import subprocess, tempfile

p = Path(__file__).resolve().parents[1]
resources = (p / "g17_resources.rs").read_text()
completion = (p / "g17_completion.rs").read_text()


def item(source, marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


batch_start = resources.index(
    "#[derive(Debug, Copy, Clone, PartialEq, Eq)]",
    resources.index("const G17P_COMPUTE_COMPLETION_QID_COUNT"),
)
batch_impl_start = resources.index("impl G17PComputeCompletionBatch {")
batch = resources[batch_start:batch_impl_start] + item(
    resources, "impl G17PComputeCompletionBatch {"
)
scan = item(resources, "    pub(crate) fn scan_compute_completion_batch(")
qid = item(completion, "pub(crate) const fn g17p_ksm_completion_queue_id(")
kick_ts = item(completion, "pub(crate) const fn g17p_ksm_completion_timestamp(")
full_decode = item(completion, "pub(crate) fn decode_ksm_completion_entry(")
assert "g17p_ksm_completion_timestamp(control)" in full_decode
assert "Ordering::Acquire" in scan
assert ".scan_compute_completion_batch(&mut self.compute_completion_batch)" in (
    p / "g17_manager.rs"
).read_text()

source = r'''
#![allow(dead_code)]
use std::cell::Cell;
type Error=i32;type Result<T=()>=std::result::Result<T,Error>;
const EINVAL:i32=22;const EBUSY:i32=16;const ERANGE:i32=34;const EIO:i32=5;const EOVERFLOW:i32=75;const ENOSPC:i32=28;
const GFP_KERNEL:u32=0;
mod g17_queue_limits{pub const MAX_QUEUES:u32=58;pub const MAX_COMPUTE_IN_FLIGHT_PER_QUEUE:u32=16;}
/// Host stand-in for the kernel KVec: fixed capacity, never grows here.
struct KVec<T>(Vec<T>);
impl<T> KVec<T>{
 fn with_capacity(n:usize,_flags:u32)->Result<Self>{Ok(Self(Vec::with_capacity(n)))}
 fn len(&self)->usize{self.0.len()} fn capacity(&self)->usize{self.0.capacity()} fn clear(&mut self){self.0.clear()}
 fn push_within_capacity(&mut self,v:T)->std::result::Result<(),T>{if self.0.len()<self.0.capacity(){self.0.push(v);Ok(())}else{Err(v)}}
}
impl<T> std::ops::Deref for KVec<T>{type Target=[T];fn deref(&self)->&[T]{&self.0}}
impl<T> std::ops::DerefMut for KVec<T>{fn deref_mut(&mut self)->&mut[T]{&mut self.0}}
type KVVec<T>=KVec<T>;
const G17P_COMPUTE_COMPLETION_RECORD_SIZE:usize=0x40;
const G17P_COMPUTE_COMPLETION_TIMESTAMP_MASK:u64=0x003f_ffff_ffff_ffff;
const G17P_COMPUTE_COMPLETION_DESCRIPTOR_MASK:u64=!1u64;
const G17P_COMPUTE_COMPLETION_QID_COUNT:usize=128;
mod g17_completion{const KSM_QID_MASK:u8=0x7f;const KSM_TIMESTAMP_MASK:u64=0x00ff_ffff_ffff;
'''+qid+"\n"+kick_ts+r'''
}
'''+batch+r'''
mod g17_initdata{pub const G17P_KSM_COMPLETION_CAPACITY:u32=0x800;}
thread_local!{static BORROWS:Cell<usize>=const{Cell::new(0)};}
struct Object{logical_size:usize,bytes:Vec<u8>}
impl Object{fn with_bytes_mut<T>(&mut self,f:impl FnOnce(&mut[u8])->Result<T>)->Result<T>{BORROWS.with(|n|n.set(n.get()+1));f(&mut self.bytes)}}
struct Objects{completion_ordinal_0:Object}
struct Owner{resources:Objects}
impl Owner{
'''+scan+r'''
}
fn owner()->Owner{Owner{resources:Objects{completion_ordinal_0:Object{logical_size:0x800*0x40,bytes:vec![0;0x800*0x40]}}}}
fn put(raw:&mut[u8],slot:usize,at:usize,value:u64){let offset=slot*0x40+at;raw[offset..offset+8].copy_from_slice(&value.to_le_bytes());}
/// KSM control word: stamp index in bits 48..55, stamp in 16..47, QID at 9.
fn control(qid:u8,ts:u64)->u64{((ts&0xff)<<48)|(((ts>>8)&0xffff_ffff)<<16)|(u64::from(qid)<<9)|0x10}
fn record(raw:&mut[u8],slot:usize,qid:u8,ts:u64,descriptor:u64,a_start:u64,b_start:u64,b_end:u64){put(raw,slot,0,control(qid,ts));put(raw,slot,0x10,a_start);put(raw,slot,0x20,b_start);put(raw,slot,0x28,b_end);put(raw,slot,0x30,descriptor|1);put(raw,slot,0x38,0x7000_0000+u64::from(qid));}
#[test]fn measured_control_words_decode_to_their_kicks(){
 // Neo 2026-09-26 19:56: qid 13 slots 9..11 and a barrier-waiting record (bit 57).
 for (word,ts) in [(0x25000000361a33u64,0x3625u64),(0x26000000361a33,0x3626),(0x27000000361a33,0x3627),(0x21c000000361a33,0x361c)] {
  assert_eq!(g17_completion::g17p_ksm_completion_queue_id(word),13);assert_eq!(g17_completion::g17p_ksm_completion_timestamp(word),ts);
 }
 assert_eq!(g17_completion::g17p_ksm_completion_timestamp(control(13,0x12_3456_789a)),0x12_3456_789a);
}
#[test]fn fifty_eight_coalesced_qids_share_one_full_ring_borrow(){
 let mut owner=owner();let mut batch=G17PComputeCompletionBatch::new().unwrap();
 for qid in 4u8..62{let descriptor=u64::from(qid)<<12;let ts=0x100+u64::from(qid);batch.request(qid,descriptor,ts).unwrap();record(&mut owner.resources.completion_ordinal_0.bytes,usize::from(qid),qid,ts,descriptor,u64::from(qid),u64::from(qid)+1,u64::from(qid)+200);}
 BORROWS.with(|n|n.set(0));owner.scan_compute_completion_batch(&mut batch).unwrap();assert_eq!(BORROWS.with(Cell::get),1);
 for qid in 4u8..62{let descriptor=u64::from(qid)<<12;let found=batch.take(qid,descriptor,0x100+u64::from(qid)).unwrap().unwrap();assert_eq!(found.timestamps,[u64::from(qid),u64::from(qid)+200]);assert_eq!(found.interval,Some([u64::from(qid)+1,u64::from(qid)+200]));}
}
#[test]fn newest_duplicate_of_the_exact_kick_wins_anywhere_in_the_ring(){
 let mut owner=owner();let mut batch=G17PComputeCompletionBatch::new().unwrap();let descriptor=0x7f000;
 batch.request(127,descriptor,0x55).unwrap();record(&mut owner.resources.completion_ordinal_0.bytes,2047,127,0x55,descriptor,10,20,200);record(&mut owner.resources.completion_ordinal_0.bytes,0,127,0x55,descriptor,11,21,201);record(&mut owner.resources.completion_ordinal_0.bytes,1,127,0x55,0xdead000,99,100,999);
 owner.scan_compute_completion_batch(&mut batch).unwrap();let found=batch.take(127,descriptor,0x55).unwrap().unwrap();assert_eq!(found.timestamps,[11,201]);assert_eq!(found.interval,Some([21,201]));
}
#[test]fn older_lap_of_the_same_descriptor_foreign_and_tag_only_records_never_match(){
 let mut owner=owner();let mut batch=G17PComputeCompletionBatch::new().unwrap();let descriptor=0x41000;let tags=0x3ffu64<<54;
 batch.request(65,descriptor,0x1_2345).unwrap();
 // Same descriptor slot 256 kicks earlier, even with a larger end: not this kick.
 record(&mut owner.resources.completion_ordinal_0.bytes,0,65,0x1_2245,descriptor,1,2,9999);
 record(&mut owner.resources.completion_ordinal_0.bytes,1,65,0x1_2345,0x99000,9,10,900);
 record(&mut owner.resources.completion_ordinal_0.bytes,2,65,0x1_2345,descriptor,tags,tags,tags);
 record(&mut owner.resources.completion_ordinal_0.bytes,3,66,0x1_2345,descriptor,5,6,7);
 owner.scan_compute_completion_batch(&mut batch).unwrap();assert_eq!(batch.take(65,descriptor,0x1_2345).unwrap(),None);
 batch.clear();batch.request(65,descriptor,0x1_2345).unwrap();
 record(&mut owner.resources.completion_ordinal_0.bytes,4,65,0x1_2345,descriptor,tags|12,tags|30,tags|140);
 owner.scan_compute_completion_batch(&mut batch).unwrap();let found=batch.take(65,descriptor,0x1_2345).unwrap().unwrap();assert_eq!(found.timestamps,[12,140]);assert_eq!(found.interval,Some([30,140]));
}
#[test]fn publication_after_snapshot_is_seen_only_by_a_later_scan(){
 let mut owner=owner();let mut batch=G17PComputeCompletionBatch::new().unwrap();let descriptor=0x4000;
 batch.request(4,descriptor,1).unwrap();owner.scan_compute_completion_batch(&mut batch).unwrap();record(&mut owner.resources.completion_ordinal_0.bytes,7,4,1,descriptor,2,3,8);assert_eq!(batch.take(4,descriptor,1).unwrap(),None);
 batch.clear();batch.request(4,descriptor,1).unwrap();owner.scan_compute_completion_batch(&mut batch).unwrap();assert_eq!(batch.take(4,descriptor,1).unwrap().unwrap().timestamps,[2,8]);
}
#[test]fn invalid_split_and_unknown_requests_fail_closed(){
 let mut batch=G17PComputeCompletionBatch::new().unwrap();assert_eq!(batch.request(4,0,1),Err(EINVAL));assert_eq!(batch.request(4,0x1000,0),Err(EINVAL));assert_eq!(batch.request(200,0x1000,1),Err(EINVAL));
 batch.request(4,0x1000,7).unwrap();batch.request(5,0x2000,7).unwrap();
 // A QID's requests must be contiguous: returning to QID 4 after QID 5 is a host bug.
 assert_eq!(batch.request(4,0x3000,8),Err(EBUSY));
 assert_eq!(batch.take(4,0x2000,7),Err(EIO),"descriptor never requested for this QID");
 assert_eq!(batch.take(4,0x1000,8),Err(EIO),"kick never requested for this QID");
 assert_eq!(batch.take(4,0x1000,7),Ok(None));
}
#[test]fn per_queue_depth_is_bounded(){
 let mut batch=G17PComputeCompletionBatch::new().unwrap();
 for k in 0..16u64{batch.request(9,0x1000*(k+1),k+1).unwrap();}
 assert_eq!(batch.request(9,0x100000,17),Err(ENOSPC));
}
#[test]fn one_scan_captures_every_in_flight_kick_of_a_queue(){
 // Four kicks of QID 6 in flight on distinct descriptor slots, reported in
 // one batch, in ring slots that are not in time order.
 let mut owner=owner();let mut batch=G17PComputeCompletionBatch::new().unwrap();
 let d=[0x6000u64,0x6400,0x6800,0x6c00];let slots=[12usize,9,700,0];
 for (k,desc) in d.iter().enumerate(){batch.request(6,*desc,0x3628+k as u64).unwrap();record(&mut owner.resources.completion_ordinal_0.bytes,slots[k],6,0x3628+k as u64,*desc,110+10*k as u64,112+10*k as u64,118+10*k as u64);}
 BORROWS.with(|n|n.set(0));owner.scan_compute_completion_batch(&mut batch).unwrap();assert_eq!(BORROWS.with(Cell::get),1);
 for (k,desc) in d.iter().enumerate(){let found=batch.take(6,*desc,0x3628+k as u64).unwrap().unwrap();assert_eq!(found.timestamps,[110+10*k as u64,118+10*k as u64]);assert_eq!(found.pairs[2],112+10*k as u64);}
}
#[test]fn a_missing_older_record_does_not_hide_a_present_newer_one(){
 let mut owner=owner();let mut batch=G17PComputeCompletionBatch::new().unwrap();
 batch.request(7,0x7000,0x40).unwrap();batch.request(7,0x7400,0x41).unwrap();
 record(&mut owner.resources.completion_ordinal_0.bytes,3,7,0x41,0x7400,60,61,70);
 owner.scan_compute_completion_batch(&mut batch).unwrap();
 assert_eq!(batch.take(7,0x7000,0x40).unwrap(),None);
 assert_eq!(batch.take(7,0x7400,0x41).unwrap().unwrap().timestamps,[60,70]);
}
'''

with tempfile.TemporaryDirectory() as tmp:
    src = Path(tmp) / "test.rs"
    exe = Path(tmp) / "test"
    src.write_text(source)
    subprocess.run(
        ["rustc", "--edition=2021", "--test", str(src), "-o", str(exe)],
        check=True,
    )
    subprocess.run([str(exe), "--test-threads=1"], check=True)

print("PASS one-pass G17P completion batch: exact (QID, kick timestamp, descriptor) identity, 58 QIDs, several in-flight kicks per QID, out-of-order slots, acquire snapshot")
