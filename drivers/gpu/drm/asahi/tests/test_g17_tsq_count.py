#!/usr/bin/env python3
"""Actual host count publisher plus saved B1 AArch64 retirement predicate."""
from pathlib import Path
import subprocess,tempfile,re
p=Path(__file__).resolve().parents[1]
s=(p/'g17_resources.rs').read_text();start=s.index('    pub(crate) fn publish_compute_submitted_count(');end=s.index('\n    /// Zero the completion stamp',start);method=s[start:end]
mock=r'''
#![allow(dead_code)]
type Result<T=()> = std::result::Result<T,i32>;
mod mmu{pub const UAT_PGSZ:usize=0x4000;}
struct Memory{bytes:Vec<u8>,fail:bool,writes:Vec<(u64,u32)>}
impl Memory{fn iova(&self)->u64{0x100000}fn read_u32(&self,a:u64)->Result<u32>{let i=(a-self.iova())as usize;Ok(u32::from_le_bytes(self.bytes[i..i+4].try_into().unwrap()))}fn read_live_u32(&mut self,a:u64)->Result<u32>{let i=(a-self.iova())as usize;g17p_live_read_u32(&self.bytes,i)}fn write_u32(&mut self,a:u64,v:u32)->Result{if self.fail{return Err(-14)}let i=(a-self.iova())as usize;self.bytes[i..i+4].copy_from_slice(&v.to_le_bytes());self.writes.push((a,v));Ok(())}}
struct Compute{support:Memory}
'''
def block(src, marker):
    a=src.index(marker); i=src.index('{',a)+1; depth=1
    while depth:
        depth+=(src[i]=='{')-(src[i]=='}'); i+=1
    return src[a:i]
mock+='const EINVAL:i32=22;\n'+block(s,'fn g17p_live_word_bounds(')+'\n'+block(s,'fn g17p_live_read_u32(')+'\n'
tests=r'''
#[cfg(test)]mod tests{use super::*;
fn state()->Compute{let mut c=Compute{support:Memory{bytes:vec![0xa5;0xc000],fail:false,writes:vec![]}};c.support.bytes[0x4004..0x4008].copy_from_slice(&1u32.to_le_bytes());c.support.bytes[0x10c..0x110].copy_from_slice(&0u32.to_le_bytes());c}
#[test]fn only_host_target_changes_and_retry_is_idempotent(){let mut c=state();let old=c.support.bytes.clone();assert_eq!(c.publish_compute_submitted_count(1),Ok([1,0]));c.support.bytes[0x10c..0x110].copy_from_slice(&1u32.to_le_bytes());assert_eq!(c.publish_compute_submitted_count(2),Ok([1,1]));assert_eq!(c.publish_compute_submitted_count(2),Ok([2,1]));for i in 0..old.len(){if !(0x4004..0x4008).contains(&i)&&!(0x10c..0x110).contains(&i){assert_eq!(c.support.bytes[i],old[i]);}}assert!(c.support.writes.iter().all(|(a,_)|*a==0x104004));}
#[test]fn failed_publication_does_not_mutate_target(){let mut c=state();c.support.fail=true;let old=c.support.bytes.clone();assert_eq!(c.publish_compute_submitted_count(2),Err(-14));assert_eq!(c.support.bytes,old);c.support.fail=false;assert_eq!(c.publish_compute_submitted_count(2),Ok([1,0]));}
#[test]fn independent_queues_and_retained_owner_count(){let mut a=state();let mut b=state();a.publish_compute_submitted_count(2).unwrap();b.publish_compute_submitted_count(9).unwrap();assert_eq!(a.support.read_u32(0x104004),Ok(2));assert_eq!(b.support.read_u32(0x104004),Ok(9));a.publish_compute_submitted_count(u32::MAX).unwrap();assert_eq!(a.support.read_u32(0x104004),Ok(u32::MAX));}
}
'''
with tempfile.TemporaryDirectory() as d:
 f=Path(d)/'test.rs';f.write_text(mock+'\nimpl Compute{'+method+'}\n'+tests)
 subprocess.run(['rustc','--edition=2021','--test',str(f),'-o',str(Path(d)/'test')],check=True)
 subprocess.run([str(Path(d)/'test')],check=True)
# Execute the decisive compare/conditional-compare instructions from the saved
# firmware disassembly. This is a predicate test, not a GPU/firmware run.
raw=Path(__file__).with_name('fixtures').joinpath('b1-retirement-predicate.txt').read_text().splitlines()
lines=[l for l in raw if re.match(r'fffffc0000016a(?:3[048c]|4[048c]|5[048]):',l)]
assert len(lines)==11,lines
decoded=[l.split(':',1)[1].strip().split(None,1)[1] for l in lines]
compare=[l for l in decoded if l.split()[0] in ('cmp','ccmp','cset')]
assert len(compare)==5,compare
# LDP/loads establish these registers. Each later CCMP reads the post-LDP
# engine1/2 counts (CMP at16a40 uses engine0 before that LDP).
def native_gate(completed,target,batches=(1,0,0),threshold=80):
 regs={'w3':completed,'w9':target,'w10':batches[0],'w11':threshold,'w12':batches[2]};flags=0
 def sub(a,b):
  r=(a-b)&0xffffffff;return ((r>>31)<<3)|((r==0)<<2)|((a>=b)<<1)|(((a^b)&(a^r))>>31)
 def condition(c):return {'cc':not(flags&2),'ne':not(flags&4),'cs':bool(flags&2)}[c]
 for i,line in enumerate(compare):
  op,args=line.split(None,1);args=[x.strip() for x in args.split(',')]
  if op=='cmp':flags=sub(regs[args[0]],regs[args[1]]);regs['w10']=batches[1]
  elif op=='ccmp':flags=sub(regs[args[0]],regs[args[1]]) if condition(args[3]) else int(args[2][1:],0)
  elif op=='cset':return int(condition(args[1]))
assert native_gate(1,1)==1
assert native_gate(2,1)==0, 'old pinned target must reproduce deferred repeat'
assert native_gate(2,2)==1
assert native_gate(1,2)==0, 'outstanding work must not be retired early'
assert native_gate(2,3)==0
assert native_gate(3,3)==1
assert native_gate(81,1,(80,0,0))==1, 'native batching threshold remains intact'
print('PASS saved B1 instruction predicate: old target defers repeat, matching target retires, incomplete batch stays pending, threshold unchanged')
manager=(p/'g17_manager.rs').read_text();arm=block(manager,'    fn arm_compute_completion(')
# Independent queues count kicks on the shared submit-owned TSQ node
# (test_g17_tsq_accounting.py); arm must not rewrite the per-queue node.
assert 'publish_compute_submitted_count' not in arm and 'Never\n        // rewrite or compare the obsolete per-queue node' in arm
commit=(p/'g17_async_compute.rs').read_text();commit=commit[commit.index('work_state.add_submitted_kicks(1)?;')-2000:]
assert commit.index('work_state.add_submitted_kicks(1)?;')<commit.index('manager.submit_translated_compute(')
repeat=manager[manager.index('    fn submit_translated_repeat_channel('):];assert repeat.index('self.arm_compute_completion(')<repeat.index('let after_command = self.publish_repeat_record(')
print('PASS production ordering: shared TSQ kick count before the first firmware-visible record; stamp arm before repeat record')
