#!/usr/bin/env python3
"""Actual retained encoding block + direct slot views versus saved staging path.

Compiles production render builders, retained/common tail and MCache encoder.
The saved reference encodes to the old staging image and copies active extents;
the candidate encodes into actual selected generation views. Runtime/GEM and
firmware coherency are not emulated. Existing cancellation methods run separately.
"""
from pathlib import Path
import json, re, subprocess, tempfile
from test_g17_compute_scs_encoding import block
ROOT = Path(__file__).resolve().parents[1]
REFERENCE_ENCODING = '            if warm_first_kick {\n                // The constructor\'s shape for a pair\'s first kick.\n                g17_render::build_cold_opening_ta_descriptor(\n                    &translated.parameters,\n                    tiling_objects,\n                    u32::from(descriptor_context_id),\n                    command_buffer_state_words[0],\n                    translated.parameters.ta_hardware_buffer_id,\n                    tiling,\n                )\n                .map_err(|_| EINVAL)?;\n            } else {\n                g17_render::build_ta_descriptor(\n                    &translated.parameters,\n                    tiling_objects,\n                    g17_render::RenderDescriptorMetadata {\n                        context_id: u32::from(descriptor_context_id),\n                        submission_ordinal,\n                        command_buffer_state_word: command_buffer_state_words[0],\n                        ta_hardware_buffer_id: translated.parameters.ta_hardware_buffer_id,\n                        submit_sequence: submission_ordinal as u64 * 2 + 1,\n                    },\n                    tiling,\n                )\n                .map_err(|_| EINVAL)?;\n            }\n            g17_render::apply_g17p_retained_descriptor_fields(\n                g17_render::RenderDescriptorKind::Tiling,\n                descriptor_ta_rce_va,\n                self.render_flist.control_shared_va(),\n                self.queue_pair.tiling as u8,\n                self.queue_pair.fragment as u8,\n                submission_ordinal,\n                warm_first_kick,\n                g17p_render_native_pm_bytes(),\n                tiling,\n            )\n            .map_err(|_| EINVAL)?;\n            if warm_first_kick {\n                g17_render::build_cold_opening_fragment_descriptor(\n                    &translated.parameters,\n                    objects,\n                    u32::from(descriptor_context_id),\n                    command_buffer_state_words[1],\n                    fragment,\n                )\n                .map_err(|_| EINVAL)?;\n            } else {\n                g17_render::build_fragment_descriptor(\n                    &translated.parameters,\n                    objects,\n                    g17_render::RenderDescriptorMetadata {\n                        context_id: u32::from(descriptor_context_id),\n                        submission_ordinal,\n                        command_buffer_state_word: command_buffer_state_words[1],\n                        ta_hardware_buffer_id: 0,\n                        submit_sequence: submission_ordinal as u64 * 2,\n                    },\n                    fragment,\n                )\n                .map_err(|_| EINVAL)?;\n            }\n            g17_render::apply_g17p_retained_descriptor_fields(\n                g17_render::RenderDescriptorKind::Fragment,\n                descriptor_3d_rce_va,\n                self.render_flist.control_shared_va(),\n                self.queue_pair.fragment as u8,\n                self.queue_pair.fragment as u8,\n                submission_ordinal,\n                warm_first_kick,\n                g17p_render_native_pm_bytes(),\n                fragment,\n            )\n            .map_err(|_| EINVAL)?;\n            // The job descriptor carries the RCE arrays firmware executes\n            // (descriptor-local RegisterArrays at +0xa0/+0x7c0/+0xee0/+0x1600,\n            // see the SKSM mode-3 witness and audit_fragment_rce_mapping,\n            // which compares the descriptor, not this buffer). fragment_rce is\n            // a diagnostic mirror staged once at construction; refreshing it\n            // cost a 16 KiB write-combined clear plus copy on every render.\n            // Keep it current only for the descriptor-dump diagnostic.\n            if *crate::module_parameters::g17p_render_dump_descriptor.value() != 0 {\n                fragment_rce.with_bytes_mut(|rce| {\n                    g17_render::stage_g17p_fragment_rce_programs(\n                        fragment,\n                        fragment_rce_va,\n                        rce,\n                    )\n                    .map_err(|_| EINVAL)\n                })?;\n                fragment_rce.with_bytes_mut(|rce| {\n                    let dump_size = (4 * g17_render::G17P_FRAGMENT_RCE_PROGRAM_STRIDE)\n                        as usize;\n                    for (index, chunk) in rce[..dump_size].chunks(16).enumerate() {\n                        dev_info!(\n                            dev.as_ref(),\n                            "G17P 3d-rce restage +{:#05x} {:02x?}\\n",\n                            index * 16,\n                            chunk,\n                        );\n                    }\n                    Ok(())\n                })?;\n            }\n            apply_g17p_generic_descriptor_tail(\n                tiling,\n                fragment,\n                self.queue_pair.tiling,\n                self.queue_pair.fragment,\n                render_state_va + G17P_RENDER_STATE_TA_STATUS as u64,\n                translated.parameters.fragment_status,\n                self.fragment_status.iova(),\n            )\n            .ok_or(EINVAL)?;\n            // The physical queue timestamp is staged after this fresh image\n            // replaces the retained one and before its command pointer is\n            // published. Building the descriptor must not approximate it from\n            // the userspace submission ordinal.\n            put_u64(fragment, 0x2160, 0);\n            apply_g17p_fragment_mcache(\n                &translated.fragment_attachments,\n                translated.parameters.aux_fb,\n                G17P_RENDER_STATE_AUX_FB_SIZE as u64,\n                descriptor_3d_rce_va,\n                g17_render::G17P_FRAGMENT_MCACHE_OFFSET,\n                render_root_slot,\n                mcache_hwsid,\n                fragment,\n            )?'

def constant(s, name):
    return re.search(r'^\s*(?:pub\(crate\) )?const ' + name + r'\s*:[^;]+;', s, re.M).group(0)

def region(s):
    start = s.index('        let fragment_mcache = {', s.index('    pub(crate) fn restage_command('))
    start = s.index('            if warm_first_kick {', start)
    end = s.index('\n        };', start)
    return s[start:end]

def source():
    r=(ROOT/'g17_resources.rs').read_text(); old=None
    u=(ROOT/'g17_uapi.rs').read_text(); uart=(ROOT/'uat.rs').read_text()
    out='#![allow(dead_code,unused_variables)]\n'
    for name in ('g17_config','g17_render'):
        out+=f'#[path={json.dumps(str(ROOT/(name+".rs")))}] mod {name};\n'
    out+=r'''
use std::sync::atomic::Ordering;
type Result<T=()> = std::result::Result<T,i32>;
const EINVAL:i32=22;const EOVERFLOW:i32=75;
mod util {pub fn g17p_verbose()->bool{false}}
macro_rules! pr_info {($($tt:tt)*)=>{}}
macro_rules! dev_info {
 ($dev:expr, "G17PDUMP {} {} {:#05x} {:02x?}\n", $who:expr, $label:expr, $offset:expr, $bytes:expr) => { assert!($who=="opt-TA"||$who=="opt-3D"); ROWS.with(|rows|rows.borrow_mut().push(($offset,$bytes))); };
 ($($tt:tt)*)=>{}
}
mod module_parameters {pub struct Param(u32);impl Param{pub fn value(&self)->&u32{&self.0}}pub static g17p_render_dump_descriptor:Param=Param(0);}
mod g17_submission {#[derive(Debug,Clone,Copy,PartialEq,Eq)]pub struct G17PClMcacheAperture{pub address:u64,pub index:u8,pub count:u8} pub const G17P_COLD_OPENING_OPTIONAL_SIZE:usize=0xc0;}
'''
    out+='mod mmu {\n'+constant(uart,'UAT_PGBIT')+constant(uart,'UAT_PGSZ')+'}\n'
    out+='mod g17_uapi {\n'+constant(u,'UAPI_MAX_ATTACHMENTS')
    for name in ('UapiAttachment','UapiAttachmentList'):
        out+='#[derive(Debug,Copy,Clone)]\n'+block(u,'pub(crate) struct '+name+' {')+'\n'
    out+=block(u,'impl UapiAttachmentList {').split('    fn parse(',1)[0]+'}\n}\n'
    for n in ('G17P_RENDER_DESCRIPTOR_SLOT_COUNT','G17P_RENDER_TA_DESCRIPTOR_STRIDE','G17P_RENDER_3D_DESCRIPTOR_STRIDE','G17P_RENDER_TA_DESCRIPTOR_ARRAY_SIZE','G17P_RENDER_3D_DESCRIPTOR_ARRAY_SIZE','G17P_RENDER_3D_DESCRIPTOR_OFFSET','G17P_RENDER_DESCRIPTOR_STORAGE_SIZE','G17P_RENDER_STATE_TA_STATUS','G17P_RENDER_STATE_AUX_FB_SIZE','G17P_BOOTSTRAP_DESCRIPTOR_ZERO_A_VA','G17P_BOOTSTRAP_DESCRIPTOR_ZERO_B_VA','G17P_FRAGMENT_MCACHE_INLINE_END','G17P_FRAGMENT_MCACHE_SPILL_OFFSET','G17P_FRAGMENT_MCACHE_MAX_ENTRIES','G17P_FRAGMENT_MCACHE_SPILL_END','G17P_RENDER_CONFIG_FRAGMENT_BASE','G17P_RENDER_OPTIONAL_STRIDE'):
        out+=constant(r,n)+'\n'
    for marker in ('fn g17p_render_descriptor_slot(','fn g17p_render_ta_descriptor_offset(','fn g17p_render_3d_descriptor_offset(','fn g17p_render_descriptor_views(','fn g17p_render_device_bytes<const N: usize>(', 'fn g17p_render_descriptor_backing_valid(', 'fn g17p_render_config_offset(','fn apply_g17p_generic_descriptor_tail(','fn encode_g17p_fragment_mcache(','fn apply_g17p_fragment_mcache(','fn g17p_fragment_copy_size(','pub(crate) fn g17p_render_native_pm_bytes('):
        out+=block(r,marker)+'\n'
    out+=MOCK
    for method,text in [('encode_direct',region(r)),('encode_reference',REFERENCE_ENCODING)]:
        out+='impl Storage { fn '+method+SIGNATURE+PREAMBLE
        if method=='encode_direct':
            out+='let (tiling,fragment)=g17p_render_descriptor_views(raw,submission_ordinal).ok_or(EINVAL)?;\nlet result={\n'+text+'\n}; Ok(result)\n'
        else:
            out+='let mut staging=vec![0x93u8;0x8000]; let (tiling,fragment)=staging.split_at_mut(0x4000);\nlet result={\n'+text+'\n};\n'
            out+='let ta=g17p_render_ta_descriptor_offset(submission_ordinal);let fr=g17p_render_3d_descriptor_offset(submission_ordinal);raw[ta..ta+g17_render::TA_DESCRIPTOR_SIZE].copy_from_slice(&staging[..g17_render::TA_DESCRIPTOR_SIZE]);let size=g17p_fragment_copy_size(result.as_ref());raw[fr..fr+size].copy_from_slice(&staging[0x4000..0x4000+size]);Ok(result)\n'
        out+='} }\n'
    out+=CONFIG_MOCK
    out+='impl ConfigStorage {\n'+block(r,'    pub(crate) fn dump_config_update_region(')+'\n}\n'
    return out+TESTS

SIGNATURE='''(&mut self,raw:&mut[u8],submission_ordinal:u64,descriptor_context_id:u16,translated:&Command)->Result<Option<g17_submission::G17PClMcacheAperture>>{\n'''
PREAMBLE='''let warm_first_kick=submission_ordinal==0;let command_buffer_state_words=[0x83,0x83];let render_root_slot=descriptor_context_id as u8;let mcache_hwsid=24;let render_state_va=0xfffffc2009000000;let fragment_rce_va=0x1009000000;let descriptor_ta_rce_va=0x7000400000+g17p_render_ta_descriptor_offset(submission_ordinal) as u64;let descriptor_3d_rce_va=0x7000500000+(g17p_render_3d_descriptor_offset(submission_ordinal)-G17P_RENDER_3D_DESCRIPTOR_OFFSET) as u64;let objects=g17_render::RenderDescriptorObjects{record_a:0xfffffc200a000000,shared:0xfffffc200b000000,record_b:0xfffffc200c000000,zero:0xfffffc200d000000};let tiling_objects=objects;let fragment_rce=&mut self.fragment_rce;\n'''
MOCK=r'''
fn put_u64(raw:&mut[u8],offset:usize,value:u64){raw[offset..offset+8].copy_from_slice(&value.to_le_bytes());}
struct Object;impl Object{fn iova(&self)->u64{0xfffffc2010000000}fn with_bytes_mut<R>(&mut self,f:impl FnOnce(&mut[u8])->Result<R>)->Result<R>{f(&mut[0u8;0x4000])}}
struct FList;impl FList{fn control_shared_va(&self)->u64{0xfffffc2007000000}}
struct Pair{tiling:u16,fragment:u16}
struct Storage{queue_pair:Pair,render_flist:FList,fragment_status:Object,fragment_rce:Object}
struct Command{parameters:g17_render::G17pRenderParameters,fragment_attachments:g17_uapi::UapiAttachmentList}
fn storage()->Storage{Storage{queue_pair:Pair{tiling:26,fragment:27},render_flist:FList,fragment_status:Object,fragment_rce:Object}}
fn command(n:usize,width:u32)->Command{let mut parameters=g17_render::G17pRenderParameters::source_rsrc8();parameters.width=width;parameters.height=width;let mut a=g17_uapi::UapiAttachmentList::EMPTY;for i in 0..n{a.entries[i]=g17_uapi::UapiAttachment{address:0x1001000000+i as u64*0x200000,size:0x10000};}a.count=n as u8;Command{parameters,fragment_attachments:a}}
'''
CONFIG_MOCK=r'''
thread_local!{static ROWS:std::cell::RefCell<Vec<(usize,[u8;16])>>=const{std::cell::RefCell::new(Vec::new())};}
struct AsahiDevice;
struct ConfigObject{data:Vec<u8>,fail:bool}
impl ConfigObject{fn vmap(&self)->Result<&Self>{if self.fail{Err(5)}else{Ok(self)}}fn as_ptr(&self)->*const u8{self.data.as_ptr()}}
struct ConfigMapping{object:ConfigObject,logical_size:usize}
impl ConfigMapping{fn iova(&self)->u64{0xfffffc2020000000}}
struct ConfigStorage{config_updates:ConfigMapping,descriptor_ordinal:u64}
fn config_checks(){
 for ordinal in [0,1,31,32,65]{for tiling in [false,true]{let mut s=ConfigStorage{descriptor_ordinal:ordinal,config_updates:ConfigMapping{object:ConfigObject{data:(0..0x8000).map(|i|(i^(i>>8))as u8).collect(),fail:false},logical_size:0x8000}};let at=g17p_render_config_offset(tiling,ordinal);ROWS.with(|r|r.borrow_mut().clear());s.dump_config_update_region(&AsahiDevice,tiling,"actual");ROWS.with(|r|{let rows=r.borrow();assert_eq!(rows.len(),12);for (offset,data) in rows.iter(){assert_eq!(*data,s.config_updates.object.data[at+offset..at+offset+16]);}});for invalid in [0,1]{ROWS.with(|r|r.borrow_mut().clear());if invalid==0{s.config_updates.object.fail=true;}else{s.config_updates.object.fail=false;s.config_updates.logical_size=at+0xbf;}s.dump_config_update_region(&AsahiDevice,tiling,"invalid");ROWS.with(|r|assert!(r.borrow().is_empty()));}}}
 println!("PASS actual ConfigUpdate volatile-row reader both engines/wrap/map failure/truncation");
}
'''
TESTS=r'''
fn main(){
 for size in [0,G17P_RENDER_DESCRIPTOR_STORAGE_SIZE-1,G17P_RENDER_DESCRIPTOR_STORAGE_SIZE,G17P_RENDER_DESCRIPTOR_STORAGE_SIZE+1] {for wc in [false,true] {assert_eq!(g17p_render_descriptor_backing_valid(size,wc),size==G17P_RENDER_DESCRIPTOR_STORAGE_SIZE&&!wc);}}
 let mut cases=0;
 for ordinal in [0,1,15,16,31,32,33,255,256,65535,65536,1<<32] {
  for n in [0,1,2,15,16] {for ctx in [1,7,32] {for width in [32,1024,4096] {
   let c=command(n,width);let mut direct=vec![0xa5;G17P_RENDER_DESCRIPTOR_STORAGE_SIZE];let mut reference=direct.clone();
   let got=storage().encode_direct(&mut direct,ordinal,ctx,&c).unwrap();let want=storage().encode_reference(&mut reference,ordinal,ctx,&c).unwrap();
   assert_eq!(got,want);assert_eq!(direct,reference,"ordinal={ordinal} n={n} ctx={ctx} width={width}");cases+=1;
  }}}
 }
 // Whole-backing comparison already proves all inactive neighbors unchanged.
 // Repeated reuse also resets firmware-output-looking sentinels in prefixes.
 let mut direct=vec![0x55;G17P_RENDER_DESCRIPTOR_STORAGE_SIZE];let mut reference=direct.clone();
 for i in 0..256u64 {let c=command((i%17)as usize,if i%2==0{1024}else{2048});assert_eq!(storage().encode_direct(&mut direct,i,7,&c),storage().encode_reference(&mut reference,i,7,&c));assert_eq!(direct,reference);}
 // Bounds must reject before mutating any byte, including last slot spill.
 for ordinal in [0,15,31,32,63] {let end=g17p_render_3d_descriptor_offset(ordinal)+G17P_RENDER_3D_DESCRIPTOR_STRIDE;for len in [0,end-1]{let mut raw=vec![0xaa;len];assert!(g17p_render_descriptor_views(&mut raw,ordinal).is_none());assert!(raw.iter().all(|b|*b==0xaa));}}
 // A real builder error may modify only its reserved inactive slot. Retry
 // rebuilds it completely; no prior live slot can be altered by the failure.
 for ordinal in [0,1,31,32] {let mut raw=vec![0xa5;G17P_RENDER_DESCRIPTOR_STORAGE_SIZE];let mut bad=command(2,1024);bad.parameters.deflake_1=0x1100000000;assert!(storage().encode_direct(&mut raw,ordinal,7,&bad).is_err());let ta=g17p_render_ta_descriptor_offset(ordinal);let fr=g17p_render_3d_descriptor_offset(ordinal);for (i,b) in raw.iter().enumerate(){if !(ta..ta+g17_render::TA_DESCRIPTOR_SIZE).contains(&i)&&!(fr..fr+G17P_RENDER_3D_DESCRIPTOR_STRIDE).contains(&i){assert_eq!(*b,0xa5);}}let c=command(16,2048);storage().encode_direct(&mut raw,ordinal,7,&c).unwrap();let mut reference=vec![0xa5;G17P_RENDER_DESCRIPTOR_STORAGE_SIZE];storage().encode_reference(&mut reference,ordinal,7,&c).unwrap();assert_eq!(raw,reference);}
 for len in [0,1,7,32] {let raw=vec![0x9a;len];for at in 0..=len {let got=g17p_render_device_bytes::<8>(&raw,at);assert_eq!(got,if len-at>=8{Some([0x9a;8])}else{None});}}
 config_checks();
 println!("PASS {cases} actual retained encoding blocks direct-vs-staged, 256 wrap/reuse, bounds and partial-error retry");
}
'''
if __name__=='__main__':
    r=(ROOT/'g17_resources.rs').read_text();method=block(r,'    pub(crate) fn restage_command(')
    assert 'next_descriptors' not in method and 'staging_descriptors' not in r
    assert method.index('g17p_render_descriptor_backing_valid(')<method.index('let destination_mapping =')<method.index('let support_mapping =')<method.index('let fragment_mcache =')
    assert 'self.support.logical_size < G17P_RENDER_SUPPORT_SIZE' in method
    assert r.count('g17p_render_descriptor_backing_valid(')==3
    drm=(ROOT/'g17_drm.rs').read_text()
    a=drm.index('let prepared_result = prepared.prepare(');b=drm.index('let result = runtime.submit_translated_render(',a)
    assert 'if let Err(error) = prepared_result' in drm[a:b] and 'runtime.cancel_render_restage(render_slot)?;' in drm[a:b]
    with tempfile.TemporaryDirectory(prefix='g17-direct-desc-') as d:
        src=Path(d)/'main.rs';src.write_text(source());exe=Path(d)/'main'
        subprocess.run(['rustc','--edition=2021','-Awarnings',str(src),'-o',str(exe)],check=True)
        subprocess.run([str(exe)],check=True)
    subprocess.run(['python',str(ROOT/'tests/test_g17_unpublished_render_cancel.py')],check=True)
