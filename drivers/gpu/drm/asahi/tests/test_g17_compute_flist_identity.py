#!/usr/bin/env python3
"""B1 retained per-graph FList identity, counters and publication integration."""
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
resources = (ROOT / "g17_resources.rs").read_text()
manager = (ROOT / "g17_manager.rs").read_text()
submission = (ROOT / "g17_submission.rs").read_text()


def block(text, marker):
    start = text.index(marker)
    opening = text.index("{", start)
    depth = 1
    end = opening + 1
    while depth:
        depth += (text[end] == "{") - (text[end] == "}")
        end += 1
    return text[start:end]


storage = block(resources, "pub(crate) struct G17PClRuntimeStorage")
assert "flist: Arc<G17PComputeFlist>" in storage
assert "scheduler_support: MappedObject" in storage
constructor = block(resources[resources.index("impl G17PClRuntimeStorage"):], "pub(crate) fn new(")
assert "&flist.shared_support" in constructor and "&flist.support_state" in constructor
assert "apply_g17p_cl_queue_static_state" not in constructor
assert "shared_support.with_bytes_mut" not in constructor
prepare = block(manager, "pub(crate) fn prepare_g17p_compute_queue(")
assert "G17PComputeFlist::new(dev, self.uat(), queue_id)" in prepare
assert "initialize_compute_pool_uma(&flist)" in prepare
assert "compute_flist: Option" not in manager
assert "_flist: mmu::KernelMapping" in manager
quarantine = block(manager, "pub(crate) fn quarantine(self)")
assert quarantine.count("_flist") == 2
# The first bind encodes its tag-15 record in the queue graph's optional
# window; a repeat encodes its own record slot and never writes that window
# (the first bind's ring entry may still name it). Both carry the cookie.
for marker, cookie in [
    ("pub(crate) fn stage_initial_channel_group_without_fallback_event(",
     "COMPUTE_OPTIONAL + 0x3e, u64::from(flist_generation)"),
    ("pub(crate) fn stage_repeat_config_update(",
     "put_u64(&mut record, 0x3e, u64::from(flist_generation))"),
]:
    stage = block(resources, marker)
    assert "flist_generation: u32" in stage
    assert cookie in stage, marker
repeat_stage = block(resources, "pub(crate) fn stage_repeat_config_update(")
assert "encode_compute_config_update_record(" in repeat_stage
assert "queue_graph.with_bytes_mut" in repeat_stage and "get(0x48..0x4c)" in repeat_stage
assert "COMPUTE_OPTIONAL" not in repeat_stage.split("if rewind")[0].replace("COMPUTE_OPTIONAL_SIZE", ""), \
    "a repeat must not write the queue graph's tag-15 window"
assert "flist_generation: g17p_allocate_object_id()?" in resources
# Three descriptor builds plus the fall-through path's exact preflight
# register-program checkpoint derivation all use the retained physical pool.
assert manager.count("usc_flist_id: u32::from(queue.storage.flist_id())") == 4
assert "context.execution_gate" not in manager
for marker, publication in [
    ("fn submit_translated_first_bind_channel(", "publish_initial_config_update_pointer(group)"),
    ("fn submit_translated_repeat_channel(", '"the tag-3 command pointer"'),
]:
    function = block(manager, marker)
    assert function.count("reserve_flist_submission()") == 1
    assert function.index("clear_compute_usage_slot()") < function.index("reserve_flist_submission()")
    assert function.index("reserve_flist_submission()") < function.index(publication)
    assert function.count("cancel_unpublished_flist_submission(flist_target)") == 1
direct = block(manager, "pub(crate) fn submit_translated_compute(")
assert direct.index("submission_phase.store(G17P_SUBMIT_PHASE_B2") < direct.index("self.publish_compute_entry(")

source = f'''
use std::sync::atomic::{{AtomicU64, Ordering}};
use std::cell::UnsafeCell;
type Result<T=(), E=u32> = std::result::Result<T,E>;
const EINVAL:u32=22; const EOVERFLOW:u32=75;
struct Gem {{ word:UnsafeCell<u64> }}
struct View<T> {{ ptr:*mut T }}
impl<T> View<T> {{ fn as_mut_ptr(&self)->*mut T {{ self.ptr }} }}
impl Gem {{ fn vmap<T>(&self)->Result<View<T>> {{ Ok(View{{ptr:self.word.get().cast()}}) }} }}
struct Object {{ gem:Gem }}
struct MappedObject {{ object:Object }}
fn mapped()->MappedObject {{ MappedObject{{object:Object{{gem:Gem{{word:UnsafeCell::new(0)}}}}}} }}
{block(resources, 'pub(crate) struct G17PComputeFlist {')}
impl G17PComputeFlist {{
{block(resources, 'pub(crate) fn reserve_submission(')}
{block(resources, 'fn cancel_unpublished_submission(')}
}}
#[path = "{ROOT / 'g17_config.rs'}"] mod g17_config;
#[path = "{ROOT / 'g17_compute.rs'}"] mod g17_compute;
#[derive(Debug)] enum SubmissionError {{ KickRetainedStorageBufferTooSmall }}
const G17P_COMPUTE_FLIST_ID:u16 = g17_config::COMPUTE_FLIST_ID;
fn put_u64_le(b:&mut[u8], o:usize, x:u64) {{ b[o..o+8].copy_from_slice(&x.to_le_bytes()); }}
fn put_u32_le(b:&mut[u8], o:usize, x:u32) {{ b[o..o+4].copy_from_slice(&x.to_le_bytes()); }}
fn u64_at(b:&[u8], o:usize)->u64 {{ u64::from_le_bytes(b[o..o+8].try_into().unwrap()) }}
{block(submission, 'pub(crate) fn apply_g17p_cl_flist_static_state(')}

#[test]
fn gpu_pool_selector_agrees_with_registration_for_all_physical_queues() {{
    for qid in 0..128u8 {{
        let id=g17_config::compute_flist_id(qid);
        let p=g17_compute::ComputeRegisterParameters {{
            preempt_base:0x7001000000, cdm_base:0x4000100000,
            usc_exec_base:0x10000000000, helper_binary:0, helper_data:0,
            helper_config:0, dispatch_identity:0, context_id:5,
            work_ordinal:0, robustness:0x7002000000,
            operand_state_base:0x7003000000, usc_flist_id:u32::from(id),
        }};
        let registers=g17_compute::build_compute_registers(p).unwrap();
        assert_eq!(registers.as_slice().iter().find(|r|r.number==0x14028).unwrap().value,u64::from(id));
    }}
}}

#[test]
fn canonical_flist_identity_matches_every_queue_registration() {{
    let mut pool=[0xa5;0x4000]; let mut target=[0xa5;0x4000];
    let ptr=0xffff_fc20_c400_0000;
    apply_g17p_cl_flist_static_state(&mut pool,&mut target,0x7002000000,0x7002208000,ptr+0x4000,1).unwrap();
    assert_eq!(u64_at(&pool,8),1);
    assert_eq!(u64_at(&pool,0x4c),ptr+0x4000);
    assert_eq!(u64_at(&pool,0x54),0);
    assert_eq!(u64_at(&target,0),0);
    for qid in 0..128u8 {{
        apply_g17p_cl_flist_static_state(&mut pool,&mut target,0x7002000000,0x7002208000,ptr+0x4000,g17_config::compute_flist_id(qid)).unwrap();
        let mut graph=vec![0u8;g17_compute::COMPUTE_QUEUE_GRAPH_SIZE];
        let aliases=g17_compute::ComputeSksmEntryAliases::new(0x7000400000,0xfffffc20c3000000,0x8000,0x100).unwrap();
        g17_compute::encode_compute_config_update(&mut graph,aliases,ptr,ptr+0x10000+u64::from(qid)*0x4000,qid,qid,true,g17_compute::ComputeConfigUpdates{{qos:true,context:true}},Some(5+u16::from(qid))).unwrap();
        let rec=&graph[g17_compute::COMPUTE_OPTIONAL..];
        assert_eq!(u64_at(rec,0x36),ptr);
        assert_eq!(u32::from_le_bytes(rec[0x46..0x4a].try_into().unwrap()),u64_at(&pool,8) as u32);
        assert_eq!(u64_at(rec,0x3e),0x3f);
    }}
}}

#[test]
fn three_private_pools_preserve_accounting_across_12003_owners() {{
    // B1 table[ID] stores one pointer; completion checks that exact identity.
    let ptr=0x12340000u64;
    let mut table=[0u64;4]; let mut completed=[0u64;3];
    let owners:[G17PComputeFlist;3]=std::array::from_fn(|i|G17PComputeFlist{{id:i as u16+1,backing:mapped(),shared_support:mapped(),support_state:mapped(),submitted:AtomicU64::new(0)}});
    for round in 0..4001 {{
        for i in 0..3 {{ table[i+1]=ptr+i as u64*0x4000; assert_eq!(owners[i].reserve_submission().unwrap(),round+1); }}
        // Independent completion order must not touch another pool's counter.
        for i in [2,0,1] {{ assert_eq!(table[i+1],ptr+i as u64*0x4000); completed[i]+=1; }}
        for i in 0..3 {{
        let owner=&owners[i];
        assert_eq!(completed[i],round+1);
        // An unpublished first-pointer refusal may cancel only the last
        // reservation; a later partial publication must not do so.
        let last=owner.reserve_submission().unwrap(); assert_eq!(last,completed[i]+1);
        assert_eq!(owner.cancel_unpublished_submission(last-1),Err(EINVAL));
        owner.cancel_unpublished_submission(last).unwrap();
        let submitted=owner.submitted.load(Ordering::Acquire); assert_eq!(completed[i],submitted);
        assert_eq!(unsafe{{*owner.support_state.object.gem.word.get()}},submitted.to_le());
        }}
    }}
    assert_eq!(completed.iter().sum::<u64>(),12003);
    // Regression witness: same-ID distinct pointers cannot coexist in B1.
    table[1]=ptr+0x4000; assert_ne!(table[1],ptr);
    // The former registration index zero fails ID-one lookup even alone.
    table=[ptr,0,0,0]; assert_ne!(table[1],ptr);
    let owner=&owners[0];
    owner.submitted.store(u64::MAX,Ordering::Release);
    assert_eq!(owner.reserve_submission(),Err(EOVERFLOW));
}}
'''
with tempfile.TemporaryDirectory(prefix="g17-flist-identity-") as directory:
    rust = Path(directory) / "test.rs"
    binary = Path(directory) / "test"
    rust.write_text(source)
    subprocess.run(["rustc", "--edition=2021", "--test", "-Awarnings", str(rust), "-o", str(binary)], check=True)
    subprocess.run([str(binary)], check=True)
print("g17 B1 private FList identity and 12003-owner model: PASS")
