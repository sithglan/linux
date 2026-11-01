#!/usr/bin/env python3
"""Actual completion-record extraction must use one pair, not mixed timestamps."""
from pathlib import Path
import re,subprocess,tempfile
s=(Path(__file__).resolve().parents[1]/'g17_resources.rs').read_text();a=s.index('    pub(crate) fn read_compute_execution_interval(');e=s.index('{',a)+1;d=1
while d:d+=(s[e]=='{')-(s[e]=='}');e+=1
method=s[a:e]
constants='\n'.join(re.search(r'const '+n+r'[^;]+;',s).group() for n in ['G17P_COMPUTE_COMPLETION_RECORD_SIZE','G17P_COMPUTE_COMPLETION_DESCRIPTOR_MASK','G17P_COMPUTE_COMPLETION_TIMESTAMP_MASK'])
source=r'''
type Result<T=()>=std::result::Result<T,i32>;const ERANGE:i32=34;
mod g17_initdata{pub const G17P_KSM_COMPLETION_CAPACITY:u32=2048;}
struct Object{logical_size:usize,bytes:Vec<u8>}
impl Object{fn with_bytes_mut<T>(&mut self,f:impl FnOnce(&mut[u8])->Result<T>)->Result<T>{f(&mut self.bytes)}}
struct Resources{completion_ordinal_0:Object}struct Owner{resources:Resources}
'''+constants+'\nimpl Owner{\n'+method+r'''
}
fn object()->Owner{Owner{resources:Resources{completion_ordinal_0:Object{logical_size:128,bytes:vec![0;128]}}}}
fn put(o:&mut Owner,slot:usize,offset:usize,v:u64){let at=slot*64+offset;o.resources.completion_ordinal_0.bytes[at..at+8].copy_from_slice(&v.to_le_bytes());}
fn record(o:&mut Owner,slot:usize,desc:u64,a:u64,b:u64,end:u64){for(at,v)in[(0x10,a),(0x18,a+1),(0x20,b),(0x28,end),(0x30,desc|1),(0x38,0x9000)]{put(o,slot,at,v);}}
#[test]fn same_record_pair_b_is_not_mixed_with_earlier_pair_a(){let mut o=object();record(&mut o,0,0x100,1,11,20);assert_eq!(o.read_compute_execution_interval(0x100,20).unwrap(),Some([11,20]));}
#[test]fn selects_exact_descriptor_end_and_masks_native_tags(){let mut o=object();record(&mut o,0,0x100,1,2,3);let tag=0x3ffu64<<54;record(&mut o,1,0x200,4,tag|30,tag|40);assert_eq!(o.read_compute_execution_interval(0x200,40).unwrap(),Some([30,40]));assert_eq!(o.read_compute_execution_interval(0x100,40).unwrap(),None);assert_eq!(o.read_compute_execution_interval(0x200,3).unwrap(),None);}
#[test]fn absent_invalid_or_truncated_pair_never_proves_overlap(){let mut o=object();record(&mut o,0,0x100,1,0,20);assert_eq!(o.read_compute_execution_interval(0x100,20).unwrap(),None);put(&mut o,0,0x20,20);assert_eq!(o.read_compute_execution_interval(0x100,20).unwrap(),None);put(&mut o,0,0x20,21);assert_eq!(o.read_compute_execution_interval(0x100,20).unwrap(),None);o.resources.completion_ordinal_0.bytes.truncate(32);assert_eq!(o.read_compute_execution_interval(0x100,20),Err(ERANGE));}
'''
with tempfile.TemporaryDirectory() as t:
 f=Path(t)/'test.rs';f.write_text(source);exe=Path(t)/'test';subprocess.run(['rustc','--edition=2021','--test',str(f),'-o',str(exe)],check=True);subprocess.run([str(exe)],check=True)
print('PASS actual GPU pair-B extraction: no mixed-pair false overlap, exact descriptor/end, tag masking, invalid/absent/bounds cases')
