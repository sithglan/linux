#!/usr/bin/env python3
"""Execute production ConfigUpdate publication against independent legacy codecs.

Both engines use a separate pool in this candidate. Tests execute final fields,
FRG full/prefix staging, TA full/split publication and actual late preparation.
GEM/UAT owners are mocks; these tests do not establish hardware coherency or speed.
Legacy encoders and the pre-direct FRG method remain independent byte references.
The extracted kernel mapping-delta arm is unchanged; unrelated app arm is omitted.
"""
from pathlib import Path
import re
import subprocess
import tempfile
import test_g17_compute_scs_encoding as support

ROOT = Path(__file__).resolve().parents[1]
REFERENCE_METHOD = '    pub(crate) fn stage_fragment_prefix(&mut self, submission_ordinal: u64) -> Result {\n        let graph_va = self.graph.iova();\n        let optional_offset = g17p_render_optional_offset(false, submission_ordinal);\n        let items = [\n            self.descriptors\n                .iova()\n                .checked_add(g17p_render_3d_descriptor_offset(submission_ordinal) as u64)\n                .ok_or(EOVERFLOW)?,\n            graph_va + optional_offset as u64,\n        ];\n        let optional_addresses = self.optional_addresses(false);\n        let hardware_buffer_id = self.qos_hardware_buffer_id()?;\n        let flist_generation = self.flist_generation;\n        let plan = G17PRenderItemPlan::new(submission_ordinal, g17p_render_tag16_enabled())\n            .ok_or(EINVAL)?;\n        let scratch_handoff = self.scratch_handoff;\n        self.graph.with_bytes_mut(|raw| {\n            let mut group = [0u8; 16];\n            for (index, address) in items.iter().enumerate() {\n                put_u64(&mut group, index * 8, *address);\n            }\n            plan.copy_items(raw, Self::FRAGMENT_RING, 0, &group).ok_or(EINVAL)?;\n            g17_submission::apply_g17p_retained_optional_with_flist_generation_and_context_update(\n                g17_submission::G17PColdOpeningStage::Fragment,\n                optional_addresses,\n                submission_ordinal,\n                flist_generation,\n                g17p_render_generation_bias_enabled(),\n                self.queue_pair.fragment,\n                self.queue_pair.tiling,\n                hardware_buffer_id,\n                g17p_render_install(false),\n                self.descriptor_context_id,\n                self.context_update_pending,\n                &mut raw[optional_offset\n                    ..optional_offset + g17_submission::G17P_COLD_OPENING_OPTIONAL_SIZE],\n            )\n            .map_err(|_| EINVAL)?;\n            if let Some(seeds) = scratch_handoff {\n                g17_submission::apply_g17p_idle_scratch_seed(\n                    &mut raw[optional_offset..optional_offset + g17_submission::G17P_COLD_OPENING_OPTIONAL_SIZE],\n                    seeds[1],\n                ).map_err(|_| EINVAL)?;\n            }\n            if self.qos_update_pending { put_u32(raw, optional_offset + 0x52, 1); }\n            put_u32(raw, Self::FRAGMENT_POINTERS + 0x40, plan.prefix_end);\n            Ok(())\n        })\n    }'


def constant(source, name):
    match = re.search(r"^\s*(?:pub\(crate\) )?const " + name + r"\s*:[^;]+;", source, re.M)
    if not match:
        raise ValueError("missing production constant " + name)
    return match.group(0)


def rust_source():
    resource = (ROOT / "g17_resources.rs").read_text()
    render = (ROOT / "g17_render.rs").read_text()
    uat = (ROOT / "uat.rs").read_text()
    support.HARNESS = ""
    out = support.rust_source()
    out += "use uapi as g17_uapi;\nmod mmu {\n"
    out += constant(uat, "UAT_PGBIT") + "\n" + constant(uat, "UAT_PGSZ") + "\n}\n"
    out += "mod g17_render {\n"
    for name in ("TA_DESCRIPTOR_SIZE", "FRAGMENT_DESCRIPTOR_SIZE", "G17P_MCACHE_MAPPING_SIZE"):
        out += constant(render, name) + "\n"
    out += "}\n"
    for name in ("G17P_RENDER_DESCRIPTOR_SLOT_COUNT", "G17P_RENDER_TA_DESCRIPTOR_STRIDE",
                 "G17P_RENDER_3D_DESCRIPTOR_STRIDE", "G17P_RENDER_TA_DESCRIPTOR_ARRAY_SIZE",
                 "G17P_RENDER_3D_DESCRIPTOR_OFFSET", "G17P_RENDER_QUEUE_METADATA_BASE",
                 "G17P_USER_TILING_OPTIONAL", "G17P_USER_FRAGMENT_OPTIONAL",
                 "G17P_RENDER_OPTIONAL_STRIDE", "G17P_RENDER_EVENT_STRIDE", "G17P_USER_FRAGMENT_ENTRY_SIGNAL", "G17P_RETAINED_RENDER_ORDINALS",
                 "G17P_RENDER_QUEUE_GRAPH_SIZE", "G17P_RENDER_CONFIG_POOL_SIZE",
                 "G17P_RENDER_CONFIG_FRAGMENT_BASE", "G17P_USER_TILING_EVENT",
                 "G17P_USER_FRAGMENT_EVENT", "G17P_USER_TILING_ENTRY_SIGNAL",
                 "G17P_USER_TILING_POINTERS", "G17P_USER_TILING_RING",
                 "G17P_RENDER_POOL_A_SLOTS", "G17P_RENDER_SHARED_CONTROL_INNER", "G17P_RENDER_SUPPORT_SIZE", "G17P_RENDER_DESCRIPTOR_STORAGE_SIZE", "G17P_RENDER_3D_DESCRIPTOR_ARRAY_SIZE", "G17P_PAGE_SIZE", "G17P_BOOTSTRAP_CONTEXT_PEER_SIZE"):
        out += constant(resource, name) + "\n"
    for name in ("G17PRenderConfigUpdateHeader", "G17PRenderItemPlan", "G17PRenderConfigUpdateFields"):
        marker = ("pub(crate) " if name.endswith("Header") else "") + "struct " + name + " {"
        out += "#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n" + support.block(resource, marker) + "\n"
    for marker in ("impl G17PRenderItemPlan {", "fn write_g17p_render_config_update(", "fn prepare_g17p_render_config_update(", "fn store_g17p_render_config_update(", "fn g17p_render_config_offset(", "fn g17p_render_event_offset(", "fn g17p_render_ta_descriptor_offset(", "fn g17p_render_config_mapping_changed(", "fn g17p_render_config_pool_reachable(", "fn decode_g17p_render_config_update(",
                   "fn g17p_render_descriptor_slot(", "fn g17p_render_3d_descriptor_offset(",
                   "fn g17p_render_optional_offset(", "pub(crate) fn g17p_render_install(",
                   "pub(crate) fn g17p_render_generation_bias_enabled(",
                   "pub(crate) fn g17p_render_tag16_enabled("):
        out += support.block(resource, marker) + "\n"
    out += resource[resource.index("fn g17p_live_word_bounds("):resource.index("fn put_u32(")]
    out += MOCKS
    out += "impl Storage {\n"
    for name in ("FRAGMENT_POINTERS", "FRAGMENT_RING", "FRAGMENT_ENTRY_SIGNAL"):
        out += constant(resource, name) + "\n"
    for marker in ("    pub(crate) fn stage_fragment_prefix(", "    fn stage_fragment_config_group(",
                   "    pub(crate) fn stage_fragment(&", "    fn config_update_gpu_va(", "    fn final_config_update("):
        out += support.block(resource, marker) + "\n"
    out += REFERENCE_METHOD.replace("fn stage_fragment_prefix(", "fn reference_stage_fragment_prefix(", 1)
    out += "\n}\n"
    out += "type G17PRenderQueuePair = Pair;\n"
    for marker in ("struct G17PLateTilingBytes {", "impl G17PLateTilingBytes {",
                   "fn build_g17p_late_tiling_bytes(", "struct G17PLateTilingFields {",
                   "impl G17PLateTilingFields {", "fn build_g17p_late_tiling_fields("):
        out += support.block(resource, marker) + "\n"
    out += "#[derive(Debug,Copy,Clone)]\n" + support.block(resource,"struct G17PEventControlKick {") + "\n"
    out += "impl G17PEventControlKick {\n" + support.block(resource,"    fn prepare(support: &[u8], record_index: usize)") + "\n}\n"
    out += support.block(resource,"pub(crate) fn g17p_render_native_pool_a_first_record_enabled(") + "\n"
    out += support.block(resource,"pub(crate) struct G17PLateTilingPublication {") + "\n"
    out += support.block(resource,"impl G17PLateTilingPublication {") + "\n"
    out += LATE_MOCKS
    out += "impl LateStorage {\n"
    for marker in ("    fn config_update_gpu_va(", "    pub(crate) fn prepare_late_tiling_publication_with_stamp("):
        out += support.block(resource,marker) + "\n"
    out += "}\n"
    out += REACH_MOCKS
    # Compile the unchanged production kernel-domain branch. The unrelated app
    # mapping arm is omitted; all fields referenced by this branch are mocked.
    kernel_branch = support.block(resource,"    fn mapping_change_touches_graph(").split("        if intersects(self.render_state",1)[0]
    out += "impl ReachStorage {\n" + kernel_branch + "        false\n    }\n}\n"
    out += TESTS
    return out


MOCKS = r"""
type Result<T = ()> = std::result::Result<T, i32>;
const EINVAL: i32 = 22;
const EOVERFLOW: i32 = 75;
fn put_u16(raw: &mut [u8], offset: usize, value: u16) {
    raw[offset..offset+2].copy_from_slice(&value.to_le_bytes());
}
fn put_u32(raw: &mut [u8], offset: usize, value: u32) {
    raw[offset..offset+4].copy_from_slice(&value.to_le_bytes());
}
fn put_u64(raw: &mut [u8], offset: usize, value: u64) {
    raw[offset..offset+8].copy_from_slice(&value.to_le_bytes());
}
#[derive(Clone)]
struct MappedObject { bytes: Vec<u8>, va: u64, fail_map: bool, logical_size: usize }
impl MappedObject {
    fn iova(&self) -> u64 { self.va }
    fn with_bytes_mut<R>(&mut self, f: impl FnOnce(&mut [u8])->Result<R>)->Result<R> {
        if self.fail_map { return Err(12); }
        f(&mut self.bytes)
    }
}
#[derive(Clone, Copy)]
struct Pair { fragment: u16, tiling: u16 }
#[derive(Clone)]
struct Storage {
    graph: MappedObject, descriptors: MappedObject, config_updates: MappedObject,
    addresses: g17_submission::G17PColdOpeningOptionalAddresses,
    qos_id: u16, flist_generation: u64, queue_pair: Pair,
    descriptor_context_id: u16, context_update_pending: bool,
    qos_update_pending: bool, scratch_handoff: Option<[Option<u64>; 2]>,
}
impl Storage {
    fn optional_addresses(&self, tiling: bool)->g17_submission::G17PColdOpeningOptionalAddresses {
        let _ = tiling; self.addresses
    }
    fn qos_hardware_buffer_id(&self)->Result<u16> { Ok(self.qos_id) }
}
fn fixture()->Storage {
    Storage {
        graph: MappedObject {logical_size:G17P_RENDER_QUEUE_GRAPH_SIZE,bytes: vec![0xa5;G17P_RENDER_QUEUE_GRAPH_SIZE],va:0xffff_fc20_4000_0000,fail_map:false},
        config_updates: MappedObject {logical_size:G17P_RENDER_CONFIG_POOL_SIZE,bytes:vec![0xa5;G17P_RENDER_CONFIG_POOL_SIZE],va:0xffff_fc20_6000_0000,fail_map:false},
        descriptors: MappedObject {logical_size:0,bytes:vec![],va:0xffff_fc20_5000_0000,fail_map:false},
        addresses: g17_submission::G17PColdOpeningOptionalAddresses {
            context_scratch:0x70_0800_0000, firmware_scratch:0xffff_fc20_0800_0000,
            usc_priv_mem_flist:0xffff_fc20_0900_0000, usc_freelist_hardware_buffer_id:131,
            channel_control:0xffff_fc20_1000_0000, tiling_shared_object:0xffff_fc20_1100_0000,
            parameter_buffer_token:0xfeed_1234_5678, owner_pid:12345, ta_hardware_buffer_id:7,
        }, qos_id:19, flist_generation:0x1234_5678_9abc_def0,
        queue_pair:Pair {tiling:22,fragment:23}, descriptor_context_id:9,
        context_update_pending:false,qos_update_pending:false,scratch_handoff:None,
    }
}
"""

TESTS = r"""
fn test_typed_ta() {
    let mut count=0;
    for ordinal in [0,1,31,32,255,256,427,1280] {for qos in [false,true] {for context in [false,true] {
      for seed in [None,Some(None),Some(Some(0xa5_12345678))] {for tag16 in [false,true] {
        let s=fixture();let config_va=s.config_update_gpu_va(true,ordinal).unwrap();
        let n=build_g17p_late_tiling_fields(0xfffffc2000400000,s.graph.va,s.queue_pair,s.qos_id,
            s.addresses,s.descriptor_context_id,ordinal,s.flist_generation,ordinal+1,tag16,false,2,context,
            config_va,qos,seed).unwrap();
        let mut old=build_g17p_late_tiling_bytes(0xfffffc2000400000,s.graph.va,s.queue_pair,s.qos_id,
            s.addresses,s.descriptor_context_id,ordinal,s.flist_generation,ordinal+1,tag16,false,2,context).unwrap();
        if qos {put_u32(&mut old.optional,0x52,1);}
        if let Some(seed)=seed {g17_submission::apply_g17p_idle_scratch_seed(&mut old.optional,seed).unwrap();}
        let mut graph=s.graph.bytes.clone();let mut ref_graph=graph.clone();
        let mut pool=s.config_updates.bytes.clone();let original_pool=pool.clone();
        let mut support=vec![0u8;0x40000];let mut ref_support=support.clone();
        let offset=g17p_render_config_offset(true,ordinal);
        n.apply(&mut graph,&mut support,&mut pool[offset..offset+0xc0]);
        old.apply(&mut ref_graph,&mut ref_support);
        ref_graph[old.optional_offset..old.optional_offset+0xc0].fill(0xa5);
        let item1=G17P_USER_TILING_RING+((n.items.base+1)%n.items.capacity) as usize*8;
        put_u64(&mut ref_graph,item1,config_va);
        assert_eq!(graph,ref_graph);assert_eq!(support,ref_support);
        assert_eq!(&pool[offset..offset+0xc0],old.optional);
        assert_eq!(Some(n.config.header),decode_g17p_render_config_update(&old.optional));
        for i in 0..pool.len() {if !(offset..offset+0xc0).contains(&i) {assert_eq!(pool[i],original_pool[i]);}}
        let mut split=s.graph.bytes.clone();let mut split_support=vec![0u8;0x40000];let mut split_record=[0xa5;0xc0];
        n.apply_native_prefix(&mut split,&mut split_record);
        n.apply_native_kick(&mut split);n.apply_native_pool_transition(&mut split_support);
        assert_eq!(split,graph);assert_eq!(split_support,support);assert_eq!(split_record,old.optional);
        // Moving the retained owner preserves its separate backing and selected record.
        let mut owner=s.clone();owner.config_updates.bytes=pool;let retired=vec![owner];
        assert_eq!(&retired[0].config_updates.bytes[offset..offset+0xc0],old.optional);
        count+=1;
      }}}
    }}
    println!("PASS {count} production TA full/split/reference/pool-retention cases");
}
fn main() {
    let mut codec_cases=0;
    for stage in [g17_submission::G17PColdOpeningStage::Tiling,g17_submission::G17PColdOpeningStage::Fragment] {
      for ordinal in [0,1,255,256,65535,65536,u64::MAX] { for bias in [false,true] {
       for context in [false,true] { for qos in [false,true] {
        for seed in [None,Some(None),Some(Some(0xa5_1234_5678))] { for install in [0,1,65535] {
         let s=fixture(); let mut old=[0xa5;g17_submission::G17P_COLD_OPENING_OPTIONAL_SIZE];
         let mut backing=[0xa5;g17_submission::G17P_COLD_OPENING_OPTIONAL_SIZE+8];
         let offset=if backing.as_ptr() as usize & 7 == 0 {1} else {0};
         let new=&mut backing[offset..offset+g17_submission::G17P_COLD_OPENING_OPTIONAL_SIZE];
         g17_submission::apply_g17p_retained_optional_with_flist_generation_and_context_update(
           stage,s.addresses,ordinal,s.flist_generation,bias,127,126,31,install,65535,context,&mut old).unwrap();
         if let Some(seed)=seed {g17_submission::apply_g17p_idle_scratch_seed(&mut old,seed).unwrap();}
         if qos {put_u32(&mut old,0x52,1);}
         let header=write_g17p_render_config_update(stage,s.addresses,ordinal,s.flist_generation,
           bias,127,126,31,install,65535,context,qos,seed,new).unwrap();
         assert_eq!(*new,old,"codec ordinal={ordinal} stage={stage:?}");
         assert_eq!(Some(header),decode_g17p_render_config_update(&new));codec_cases+=1;
        } }
       } }
      } }
    }
    println!("PASS {codec_cases} final-field codec cases (TA/FRG, generation wrap, seeds, flags)");
    let seeds = [None, Some([None,None]), Some([Some(0x12_3456_789a),None]),
        Some([None,Some(0xa5_1234_5678)]), Some([Some(0x12_3456_789a),Some(0xa5_1234_5678)])];
    let ordinals = [0,1,31,32,426,427,853,1279,1280,204773];
    let mut cases=0;
    for ordinal in ordinals { for context_update in [false,true] {
      for qos_update in [false,true] { for seed in seeds { for owner in 0..2 {
        let mut new=fixture();
        new.context_update_pending=context_update; new.qos_update_pending=qos_update;
        new.scratch_handoff=seed;
        if owner==1 {
          new.flist_generation=0xfedc_ba98_7654_3210;
          new.addresses.usc_priv_mem_flist+=0x4000;
          new.addresses.usc_freelist_hardware_buffer_id=192;
          new.addresses.parameter_buffer_token=0x8765_4321;
          new.addresses.channel_control+=0x8000;
          new.descriptor_context_id=11; new.qos_id=31;
          new.queue_pair=Pair {tiling:126,fragment:127};
        }
        let before=new.graph.bytes.clone(); let mut old=new.clone();
        let header=new.stage_fragment_prefix(ordinal).unwrap();
        old.reference_stage_fragment_prefix(ordinal).unwrap();
        let old_offset=g17p_render_optional_offset(false,ordinal);
        let new_offset=g17p_render_config_offset(false,ordinal);
        let expected=old.graph.bytes[old_offset..old_offset+0xc0].to_vec();
        old.graph.bytes[old_offset..old_offset+0xc0].copy_from_slice(&before[old_offset..old_offset+0xc0]);
        let plan=G17PRenderItemPlan::new(ordinal,g17p_render_tag16_enabled()).unwrap();
        let ptr=Storage::FRAGMENT_RING+((plan.base+1)%plan.capacity) as usize*8;
        put_u64(&mut old.graph.bytes,ptr,new.config_updates.va+new_offset as u64);
        assert_eq!(&new.config_updates.bytes[new_offset..new_offset+0xc0],expected);
        for i in 0..new.config_updates.bytes.len() {
            if !(new_offset..new_offset+0xc0).contains(&i) {assert_eq!(new.config_updates.bytes[i],0xa5);}
        }
        for (i,(a,b)) in new.graph.bytes.iter().zip(&old.graph.bytes).enumerate() { assert_eq!(a,b,"graph offset={i:x} ordinal={ordinal} seed={seed:?}"); }
        let offset=g17p_render_config_offset(false,ordinal);
        let emitted=&new.config_updates.bytes[offset..offset+g17_submission::G17P_COLD_OPENING_OPTIONAL_SIZE];
        assert_eq!(Some(header),decode_g17p_render_config_update(emitted));
        assert_eq!(header.flist_generation,new.flist_generation);
        assert_eq!(header.flist_pointer,new.addresses.usc_priv_mem_flist);
        assert_eq!(header.flist_id,new.addresses.usc_freelist_hardware_buffer_id);
        assert_eq!(header.context_update,u16::from(context_update));
        if let Some(seed)=seed { assert_eq!(header.scratch_seed_valid,u32::from(seed[1].is_some()));
            assert_eq!(header.scratch_seed,seed[1].unwrap_or(0)); }
        let plan=G17PRenderItemPlan::new(ordinal,g17p_render_tag16_enabled()).unwrap();
        assert_eq!(u32::from_le_bytes(new.graph.bytes[Storage::FRAGMENT_POINTERS+0x40..Storage::FRAGMENT_POINTERS+0x44].try_into().unwrap()),plan.prefix_end);
        // Only the exact optional, two pointer slots and producer may change.
        for i in 0..before.len() {
          let relative=i.wrapping_sub(Storage::FRAGMENT_RING);
          let pointer=relative<plan.capacity as usize*8 &&
              [plan.base,(plan.base+1)%plan.capacity].contains(&((relative/8) as u32));
          if !pointer && !(Storage::FRAGMENT_POINTERS+0x40..Storage::FRAGMENT_POINTERS+0x44).contains(&i) {
              assert_eq!(before[i],new.graph.bytes[i],"unrelated byte {i:x} changed");
          }
        }
        cases+=1;
      } } }
    } }
    // Each failed call returns Err (hence no header) and no graph mutation.
    for length in [0,Storage::FRAGMENT_POINTERS+0x43,
        Storage::FRAGMENT_RING+0x500*8-1,G17P_USER_FRAGMENT_OPTIONAL+0x87] {
        let mut s=fixture();s.graph.bytes.truncate(length);let before=s.graph.bytes.clone();
        assert!(s.stage_fragment_prefix(0).is_err());assert_eq!(before,s.graph.bytes);
    }
    let mut s=fixture();s.graph.fail_map=true;let before=s.graph.bytes.clone();
    assert_eq!(s.stage_fragment_prefix(0),Err(12));assert_eq!(before,s.graph.bytes);
    let mut s=fixture();let before=s.graph.bytes.clone();
    assert!(s.stage_fragment_prefix(u64::MAX).is_err());assert_eq!(before,s.graph.bytes);
    let mut s=fixture();s.descriptors.va=u64::MAX;let before=s.graph.bytes.clone();
    assert_eq!(s.stage_fragment_prefix(0),Err(EOVERFLOW));assert_eq!(before,s.graph.bytes);
    for full in [false,true] {for pool_fails in [false,true] {
        let mut s=fixture();
        if pool_fails {s.config_updates.fail_map=true;} else {s.graph.fail_map=true;}
        let graph=s.graph.bytes.clone();let pool=s.config_updates.bytes.clone();
        assert!(s.stage_fragment_config_group(1,full).is_err());
        assert_eq!(s.graph.bytes,graph);assert_eq!(s.config_updates.bytes,pool);
    }}
    for len in [0,G17P_RENDER_CONFIG_FRAGMENT_BASE+0xbf] {
        let mut s=fixture();s.config_updates.bytes.truncate(len);
        let graph=s.graph.bytes.clone();let pool=s.config_updates.bytes.clone();
        assert!(s.stage_fragment_prefix(0).is_err());assert_eq!(s.graph.bytes,graph);assert_eq!(s.config_updates.bytes,pool);
    }
    for ordinal in [0,1,31,32,255,256,427,1280] {
        let mut s=fixture();s.stage_fragment(ordinal).unwrap();
        let plan=G17PRenderItemPlan::new(ordinal,g17p_render_tag16_enabled()).unwrap();
        assert_eq!(u32::from_le_bytes(s.graph.bytes[Storage::FRAGMENT_POINTERS+0x40..Storage::FRAGMENT_POINTERS+0x44].try_into().unwrap()),plan.final_end);
        let at=Storage::FRAGMENT_RING+((plan.base+1)%plan.capacity) as usize*8;
        assert_eq!(u64::from_le_bytes(s.graph.bytes[at..at+8].try_into().unwrap()),s.config_update_gpu_va(false,ordinal).unwrap());
        let mut expected=[0u8;g17_submission::G17P_COLD_OPENING_EVENT_SIZE];
        g17_submission::apply_g17p_retained_event(g17_submission::G17PColdOpeningStage::Fragment,ordinal,s.queue_pair.fragment,&mut expected).unwrap();
        let off=g17p_render_event_offset(false,ordinal);
        assert_eq!(&s.graph.bytes[off..off+expected.len()],expected);
    }
    println!("PASS {cases} relocated full-graph/pool equivalence cases plus full legacy group and failure-before-mutation");
    test_typed_ta();
    test_late_transaction();
    test_pool_reachability();
}
"""


LATE_MOCKS = r"""
struct MockGem { bytes:Vec<u8>, fail:bool }
struct MockMap(*mut u8);
impl MockMap { fn as_mut_ptr(&self)->*mut u8 {self.0} }
impl MockGem {
    fn vmap(&mut self)->Result<MockMap> {
        if self.fail {Err(12)} else {Ok(MockMap(self.bytes.as_mut_ptr()))}
    }
}
struct LateMapped { object:MockGem, logical_size:usize, va:u64 }
impl LateMapped {
    fn iova(&self)->u64 {self.va}
    fn from(m:MappedObject)->Self { Self {logical_size:m.logical_size,va:m.va,object:MockGem{bytes:m.bytes,fail:false}} }
}
struct LateStorage {
    graph:LateMapped, support:LateMapped, config_updates:LateMapped, descriptors:LateMapped,
    pool_a_record_index:u8, pool_a_submitted:u32, queue_pair:Pair, qos_id:u16,
    addresses:g17_submission::G17PColdOpeningOptionalAddresses, descriptor_context_id:u16,
    flist_generation:u64, context_update_pending:bool, qos_update_pending:bool,
    scratch_handoff:Option<[Option<u64>;2]>,
}
impl LateStorage {
    fn optional_addresses(&self,_tiling:bool)->g17_submission::G17PColdOpeningOptionalAddresses {self.addresses}
    fn qos_hardware_buffer_id(&self)->Result<u16> {Ok(self.qos_id)}
}
fn late_fixture()->LateStorage {
    let s=fixture();
    LateStorage {
        graph:LateMapped::from(s.graph),config_updates:LateMapped::from(s.config_updates),
        descriptors:LateMapped::from(s.descriptors),
        support:LateMapped{object:MockGem{bytes:vec![0;G17P_RENDER_SUPPORT_SIZE],fail:false},logical_size:G17P_RENDER_SUPPORT_SIZE,va:0xfffffc2070000000},
        pool_a_record_index:1,pool_a_submitted:123,queue_pair:s.queue_pair,qos_id:s.qos_id,
        addresses:s.addresses,descriptor_context_id:s.descriptor_context_id,flist_generation:s.flist_generation,
        context_update_pending:true,qos_update_pending:true,scratch_handoff:Some([Some(0x123),None]),
    }
}
fn test_late_transaction() {
    for ordinal in [0,1,31,32,427,1280] {
        let mut s=late_fixture();let pool_before=s.config_updates.object.bytes.clone();let graph_before=s.graph.object.bytes.clone();
        let canceled=s.prepare_late_tiling_publication_with_stamp(ordinal,ordinal+1).unwrap();
        drop(canceled);
        assert_eq!(s.config_updates.object.bytes,pool_before);assert_eq!(s.graph.object.bytes,graph_before);
        let publication=s.prepare_late_tiling_publication_with_stamp(ordinal,ordinal+1).unwrap();
        let expected=publication.config_update_header().unwrap();
        // Moving the graph owner preserves the heap allocations underlying all
        // three publication pointers, just as graph retention moves its objects.
        let mut retained=vec![s];
        publication.publish_native_prefix();publication.publish_native_kick();publication.publish_native_pool_transition();
        let s=&mut retained[0];let offset=g17p_render_config_offset(true,ordinal);
        assert_eq!(Some(expected),decode_g17p_render_config_update(&s.config_updates.object.bytes[offset..offset+0xc0]));
        let plan=G17PRenderItemPlan::new(ordinal,g17p_render_tag16_enabled()).unwrap();
        let item=G17P_USER_TILING_RING+((plan.base+1)%plan.capacity) as usize*8;
        assert_eq!(u64::from_le_bytes(s.graph.object.bytes[item..item+8].try_into().unwrap()),s.config_update_gpu_va(true,ordinal).unwrap());
    }
    for failure in 0..7 {
        let mut s=late_fixture();
        match failure {0=>s.config_updates.object.fail=true,1=>s.graph.object.fail=true,2=>s.support.object.fail=true,
            3=>s.config_updates.logical_size=0,4=>s.graph.logical_size-=1,5=>s.support.logical_size=1,_=>s.pool_a_record_index=35};
        let graph=s.graph.object.bytes.clone();let pool=s.config_updates.object.bytes.clone();let support=s.support.object.bytes.clone();
        assert!(s.prepare_late_tiling_publication_with_stamp(1,2).is_err());
        assert_eq!(s.graph.object.bytes,graph);assert_eq!(s.config_updates.object.bytes,pool);assert_eq!(s.support.object.bytes,support);
        assert_eq!(s.pool_a_submitted,123);
    }
    let base=0xfffffc2060000000;
    for at in [base,base+0x4000,base+0x7fff] {assert!(g17p_render_config_mapping_changed(base,0x8000,at..at+1));}
    assert!(!g17p_render_config_mapping_changed(base,0x8000,base-1..base));
    assert!(!g17p_render_config_mapping_changed(base,0x8000,base+0x8000..base+0x8001));
    assert!(g17p_render_config_mapping_changed(u64::MAX-1,8,0..1));
    println!("PASS actual late-TA preparation/publication/cancellation/moved-owner + seven failure gates + mapping-delta boundaries");
}
"""

REACH_MOCKS = r"""
use core::ops::Range;
#[derive(Clone,Copy)]
struct ReachObject {va:u64,logical_size:usize}
impl ReachObject {fn iova(&self)->u64 {self.va}}
struct ReachBacking {ta_context:ReachObject,fragment_context:ReachObject}
struct ReachStorage {
    descriptors:ReachObject,graph:ReachObject,config_updates:ReachObject,support:ReachObject,
    timestamps:ReachObject,fragment_status:ReachObject,sksm_backing:Option<ReachBacking>,
}
fn test_pool_reachability() {
    let old=ReachObject{va:0x100000,logical_size:0x4000};
    let pool=ReachObject{va:0xfffffc2060000000,logical_size:G17P_RENDER_CONFIG_POOL_SIZE};
    let s=ReachStorage{descriptors:old,graph:old,config_updates:pool,support:old,timestamps:old,
        fragment_status:old,sksm_backing:Some(ReachBacking{ta_context:old,fragment_context:old})};
    for at in [pool.va,pool.va+0x4000,pool.va+0x7fff] {
        assert!(s.mapping_change_touches_graph(at..at+1,true));
        assert!(!s.mapping_change_touches_graph(at..at+1,false));
    }
    assert!(!s.mapping_change_touches_graph(pool.va-1..pool.va,true));
    assert!(!s.mapping_change_touches_graph(pool.va+0x8000..pool.va+0x8001,true));
    let mut reads=0;
    assert!(g17p_render_config_pool_reachable(pool.va,pool.logical_size,false,|a,z|{reads+=1;a==pool.va&&z==0x8000}));
    assert_eq!(reads,1);
    assert!(!g17p_render_config_pool_reachable(pool.va,pool.logical_size,false,|_,_|false));
    assert!(!g17p_render_config_pool_reachable(pool.va,pool.logical_size,true,|_,_|panic!("WC must be rejected before mapping proof")));
    assert!(!g17p_render_config_pool_reachable(pool.va,pool.logical_size-1,false,|_,_|panic!("wrong extent")));
    println!("PASS production mapping_change_touches_graph kernel arm + exact pool-only reachability failure/WC/extent checks");
}
"""

def main():
    with tempfile.TemporaryDirectory(prefix="g17-fragment-config-") as directory:
        p=Path(directory); (p/"test.rs").write_text(rust_source())
        subprocess.run(["rustc","--edition=2021",str(p/"test.rs"),"-o",str(p/"test")],check=True)
        subprocess.run([str(p/"test")],check=True)

if __name__ == "__main__":
    main()
