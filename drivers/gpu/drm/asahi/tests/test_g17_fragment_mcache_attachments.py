#!/usr/bin/env python3
"""Compile the actual table and KSM pointer encoders; test full attachment domain."""
from pathlib import Path
import subprocess,tempfile
r=Path(__file__).resolve().parents[1]
resources=(r/'g17_resources.rs').read_text();render=(r/'g17_render.rs').read_text();submission=(r/'g17_submission.rs').read_text()
def function(source,marker):
 a=source.index(marker);b=source.index('{',a)+1;depth=1
 while depth:
  depth+=(source[b]=='{')-(source[b]=='}');b+=1
 return source[a:b]
a=resources.index('const G17P_FRAGMENT_MCACHE_INLINE_END:');b=resources.index('/// Convert validated fragment write ranges',a)
constants=resources[a:b].replace("#[cfg(not(test))]\n", "")
encoder=function(resources,'fn encode_g17p_fragment_mcache(')
a=resources.index('const G17P_RENDER_3D_DESCRIPTOR_STRIDE:');b=resources.index(';',a)+1
constants+='\n'+resources[a:b]
mapping=function(render,'pub(crate) fn encode_g17p_mcache_mapping(')
clencoder=function(submission,'fn encode_g17p_cl_mcache(')
unit=r'''
#![allow(dead_code)]
type Result<T=(),E=i32> = std::result::Result<T,E>;
const EINVAL:i32=22;const EOVERFLOW:i32=75;
mod util {pub fn g17p_verbose()->bool{false}}
macro_rules! pr_info{($($arg:tt)*)=>{}}
mod g17_uapi {
 pub const UAPI_MAX_ATTACHMENTS:usize=16;
 #[derive(Clone,Copy)]pub struct UapiAttachment{pub address:u64,pub size:u64}
}
mod g17_submission {
 #[derive(Debug,PartialEq,Eq)]pub struct G17PClMcacheAperture{pub address:u64,pub index:u8,pub count:u8}
 pub struct Context{pub render_root_slot:u8}
 pub const G17P_PARTIAL_OPENING_CONTEXT:Context=Context{render_root_slot:1};
}
mod g17_render {
 pub const FRAGMENT_DESCRIPTOR_SIZE:usize=0x2240;
 pub const G17P_FRAGMENT_MCACHE_OFFSET:usize=0x6a0;
 pub const G17P_MCACHE_MAPPING_SIZE:usize=16;
 #[derive(Debug)]pub enum RenderBuildError{McacheAddressOutOfRange,McacheRangeEmpty,McacheSelectorOutOfRange,ArithmeticOverflow}
 pub struct G17PMcacheMapping{pub address:u64,pub size_units:u32,pub hwsid:u8,pub selector:u8,pub range_shift:u16}
'''+mapping+r'''
}
use g17_uapi::UapiAttachment as A;
use g17_submission::G17PClMcacheAperture;
#[derive(Debug)]enum SubmissionError{KickMcacheFieldOutOfRange,KickMcacheAddressUnencodable}
const G17P_CL_PACKED_ADDRESS_MASK:u64=(1u64<<38)-1;
const BASE:u64=0x7000098000;
const INTERNAL:u64=0xeeee0000;
const INTERNAL_BYTES:u64=0x4d000*128;
fn run(a:&[A],mode:u32,fragment:&mut[u8])->Result<Option<G17PClMcacheAperture>>{
 encode_g17p_fragment_mcache(a,INTERNAL,INTERNAL_BYTES,BASE,0x6a0,24,mode,3,1,fragment)
}
fn att(i:u64)->A{A{address:0x1000e838000+i*0x10000000,size:0x26a80*128}}
const GOLDEN_INTERNAL:[u8;16]=[0x00,0xdc,0xdd,0x01,0x00,0x06,0x8c,0x00,0x00,0xac,0xe2,0x01,0x00,0x12,0x00,0x00];
const GOLDEN_ATTACHMENT:[u8;16]=[0x00,0x07,0x1d,0x00,0x02,0x06,0x8c,0x00,0x80,0x71,0x1f,0x00,0x02,0x0a,0x00,0x00];
fn assert_unchanged_except(raw:&[u8],range:std::ops::Range<usize>){for(i,b)in raw.iter().enumerate(){if !range.contains(&i){assert_eq!(*b,0xa5,"offset {i:x}");}}}
#[test]fn preserves_single_attachment_native_golden(){
 let mut raw=[0xa5;0x4000];let a=run(&[att(0)],1,&mut raw).unwrap().unwrap();
 assert_eq!(a,G17PClMcacheAperture{address:BASE+0x6a0,index:0,count:2});
 assert_eq!(&raw[0x6a0..0x6b0],&GOLDEN_INTERNAL);assert_eq!(&raw[0x6b0..0x6c0],&GOLDEN_ATTACHMENT);
 assert_unchanged_except(&raw,0x6a0..0x6c0);
 assert_eq!(encode_g17p_cl_mcache(a).unwrap(),0x004000e000130d40);
}
#[test]fn colour_depth_encodes_three_complete_ranges(){
 let depth=A{address:0x87ffcf4000,size:0x100000};let mut raw=[0xa5;0x4000];
 let a=run(&[att(0),depth],1,&mut raw).unwrap().unwrap();assert_eq!(a.count,3);
 assert_eq!(&raw[0x6a0..0x6b0],&GOLDEN_INTERNAL);assert_eq!(&raw[0x6b0..0x6c0],&GOLDEN_ATTACHMENT);
 let lo=u64::from_le_bytes(raw[0x6c0..0x6c8].try_into().unwrap());let hi=u64::from_le_bytes(raw[0x6c8..0x6d0].try_into().unwrap());
 assert_eq!((lo&((1u64<<41)-1))*128,depth.address);assert_eq!((hi&((1u64<<41)-1))*128,depth.address+depth.size);
 assert_eq!((lo>>41)&63,3);assert_eq!((lo>>47)&255,24);assert_eq!((hi>>41)&63,5);
 assert_unchanged_except(&raw,0x6a0..0x6d0);
}
#[test]fn full_uapi_domain_uses_inline_then_nonoverlapping_spill(){
 for n in 1..=16 {let attachments:Vec<_>=(0..n).map(att).collect();let mut raw=[0xa5;0x4000];let a=run(&attachments,1,&mut raw).unwrap().unwrap();
 let offset=if n==16{0x2240}else{0x6a0};assert_eq!(a.address,BASE+offset as u64);assert_eq!(a.count,n as u8+1);
 assert_eq!(&raw[offset..offset+16],&GOLDEN_INTERNAL);assert_unchanged_except(&raw,offset..offset+(n as usize+1)*16);
 for trailer in [0x7a0,0xec0,0x15e0,0x1d00]{assert_eq!(&raw[trailer..trailer+32],&[0xa5;32]);}
 let packed=encode_g17p_cl_mcache(a).unwrap();assert_eq!((packed>>54)&63,n);assert_eq!(((packed>>6)&G17P_CL_PACKED_ADDRESS_MASK)<<5,BASE+offset as u64);
 }
}
#[test]fn mode_zero_and_diagnostic_two_keep_policies(){
 let mut raw=[0xa5;0x4000];let attachments:Vec<_>=(0..16).map(att).collect();let a=run(&attachments,0,&mut raw).unwrap().unwrap();
 assert_eq!(a.count,16);assert_eq!(a.address,BASE+0x6a0);assert_eq!(&raw[0x6a0..0x6b0],&GOLDEN_ATTACHMENT);assert_unchanged_except(&raw,0x6a0..0x7a0);
 let mut raw=[0xa5;0x4000];let a=run(&[A{address:INTERNAL,size:INTERNAL_BYTES},att(0)],2,&mut raw).unwrap().unwrap();
 assert_eq!(a.count,2);assert_eq!(&raw[0x6a0..0x6b0],&GOLDEN_INTERNAL);assert_eq!(&raw[0x6b0..0x6c0],&GOLDEN_ATTACHMENT);
 for n in [1,3]{let mut raw=[0xa5;0x4000];assert_eq!(run(&attachments[..n],2,&mut raw),Err(EINVAL));assert_eq!(raw,[0xa5;0x4000]);}
}
#[test]fn errors_are_atomic_even_after_valid_predecessors(){
 for bad in [A{address:1,size:0},A{address:u64::MAX-2,size:4},A{address:0,size:(u32::MAX as u64+1)*128},A{address:1u64<<48,size:128}]{
 let mut raw=[0xa5;0x4000];assert!(run(&[att(0),bad],1,&mut raw).is_err());assert_eq!(raw,[0xa5;0x4000]);}
 let mut raw=[0xa5;0x4000];assert!(encode_g17p_fragment_mcache(&[att(0)],INTERNAL,0,BASE,0x6a0,24,1,3,1,&mut raw).is_err());assert_eq!(raw,[0xa5;0x4000]);
 for (base,offset,hwsid,mode,selector) in [(u64::MAX,0x6a0,24,1,3),(BASE+1,0x6a0,24,1,3),(BASE,usize::MAX,24,1,3),(BASE,0x6a0,256,1,3),(BASE,0x6a0,24,3,3),(BASE,0x6a0,24,1,65),((1u64<<43)-0x6a0,0x6a0,24,1,3)]{
 let mut raw=[0xa5;0x4000];assert!(encode_g17p_fragment_mcache(&[att(0)],INTERNAL,INTERNAL_BYTES,base,offset,hwsid,mode,selector,1,&mut raw).is_err());assert_eq!(raw,[0xa5;0x4000]);}
}
#[test]fn exact_capacity_checks_precede_writes(){
 for (n,len) in [(2,0x6cf),(15,0x79f),(16,0x234f),(16,0x2240)]{let attachments:Vec<_>=(0..n).map(att).collect();let mut raw=vec![0xa5;len];assert_eq!(run(&attachments,1,&mut raw),Err(EINVAL));assert!(raw.iter().all(|b|*b==0xa5));}
 let attachments:Vec<_>=(0..16).map(att).collect();let mut raw=[0xa5;0x2350];assert_eq!(run(&attachments,1,&mut raw).unwrap().unwrap().count,17);
 let attachments:Vec<_>=(0..17).map(att).collect();let mut raw=[0xa5;0x4000];assert_eq!(run(&attachments,1,&mut raw),Err(EINVAL));assert_eq!(raw,[0xa5;0x4000]);
}
#[test]fn live_generations_keep_private_spill_and_publish_all_bytes(){
 let attachments:Vec<_>=(0..16).map(att).collect();
 let stride=G17P_RENDER_3D_DESCRIPTOR_STRIDE;
 assert_eq!(stride%64,0);assert!(stride>=G17P_FRAGMENT_MCACHE_SPILL_END);
 let mut live=vec![0xa5;stride*32];
 // Cold construction must accept the entire UAPI domain in its live slot.
 let cold=run(&attachments,1,&mut live[..stride]).unwrap().unwrap();
 assert_eq!(cold.address,BASE+0x2240);
 assert!(live[stride..].iter().all(|b|*b==0xa5));
 // Restaging must publish every byte addressed by KSM, including the tail,
 // without modifying either neighbour or the final allocation boundary.
 for slot in [1,15,31] {
  let mut staging=[0x5a;0x4000];
  let aperture=run(&attachments,1,&mut staging).unwrap().unwrap();
  let n=g17p_fragment_copy_size(Some(&aperture));assert_eq!(n,0x2350);
  let before=live.clone();live[slot*stride..slot*stride+n].copy_from_slice(&staging[..n]);
  assert_eq!(&live[slot*stride+0x2240..slot*stride+n],&staging[0x2240..n]);
  assert_eq!(&live[..slot*stride],&before[..slot*stride]);
  assert_eq!(&live[slot*stride+n..],&before[slot*stride+n..]);
 }
 let mut staging=[0;0x4000];
 let inline=run(&attachments[..15],1,&mut staging).unwrap().unwrap();
 assert_eq!(g17p_fragment_copy_size(Some(&inline)),0x2240);
 assert_eq!(g17p_fragment_copy_size(None),0x2240);
}
#[test]fn disabled_empty_and_default_selector(){
 let mut raw=[0xa5;0x4000];assert_eq!(run(&[],1,&mut raw),Ok(None));
 assert_eq!(encode_g17p_fragment_mcache(&[att(0)],INTERNAL,INTERNAL_BYTES,BASE,0x6a0,0,1,3,1,&mut raw),Ok(None));assert_eq!(raw,[0xa5;0x4000]);
 let a=encode_g17p_fragment_mcache(&[att(0)],INTERNAL,INTERNAL_BYTES,BASE,0x6a0,24,1,64,1,&mut raw).unwrap().unwrap();assert_eq!(a.count,2);assert_eq!((u64::from_le_bytes(raw[0x6a0..0x6a8].try_into().unwrap())>>41)&63,1);
}
'''
assert '* g17_render::FRAGMENT_DESCRIPTOR_SIZE as u64' not in resources
assert resources.count('* G17P_RENDER_3D_DESCRIPTOR_STRIDE as u64') == 3
assert '.get_mut(..G17P_RENDER_3D_DESCRIPTOR_STRIDE).ok_or(ERANGE)?' in resources
assert 'g17p_render_descriptor_views(destination, submission_ordinal)' in resources
assert 'let fragment_copy_size = g17p_fragment_copy_size(fragment_mcache.as_ref());' not in resources
assert 'source[0x4000..0x4000 + fragment_copy_size]' not in resources
# The support overlays are explicitly a different BO from descriptor storage.
assert 'const G17P_RENDER_PACKED_SHARED: usize = 0x1c000;' in resources
assert 'support_va + G17P_RENDER_PACKED_SHARED as u64' in resources
assert 'support.with_bytes_mut' in resources
assert 'next_descriptors' not in resources
assert 'raw[..FRAGMENT_DESCRIPTOR_SIZE].fill(0);' in render
assert 'const G17P_RENDER_DESCRIPTOR_SLOT_COUNT: usize = 32;' in resources
assert 'G17P_RENDER_TA_DESCRIPTOR_STRIDE * G17P_RENDER_DESCRIPTOR_SLOT_COUNT' in resources
assert 'G17P_RENDER_3D_DESCRIPTOR_STRIDE * G17P_RENDER_DESCRIPTOR_SLOT_COUNT' in resources
with tempfile.TemporaryDirectory(prefix='g17-mcache-attachments-') as tmp:
 test=Path(tmp)/'test.rs';test.write_text(unit+'\n'+constants+'\n'+encoder+'\n'+clencoder)
 exe=Path(tmp)/'test';subprocess.run(['rustc','--edition=2021','--test','-Awarnings',str(test),'-o',str(exe)],check=True)
 subprocess.run([str(exe)],check=True)
print('Actual-source MCache table, native mapping and CL pointer tests passed; separate support-BO overlays verified.')
