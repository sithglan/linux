#!/usr/bin/env python3
"""Execute production role-ring reader with real initdata/record decoders."""
from pathlib import Path
from g17_config_test_support import with_g17_config
import subprocess,tempfile
p=Path(__file__).resolve().parents[1];s=(p/'g17_resources.rs').read_text()
def item(needle):
 a=s.index(needle);e=s.index('{',a)+1;d=1
 while d:d+=(s[e]=='{')-(s[e]=='}');e+=1
 return s[a:e]
hp=(p/'g17_host_path.rs').read_text()
def hpitem(needle):
 a=hp.index(needle)
 if needle.startswith('pub(crate) const'):return hp[a:hp.index(';',a)+1]
 e=hp.index('{',a)+1;d=1
 while d:d+=(hp[e]=='{')-(hp[e]=='}');e+=1
 return hp[a:e]
code=r'''
#![allow(dead_code,unused_variables)]
type Result<T=()>=std::result::Result<T,i32>;const EINVAL:i32=22;const EIO:i32=5;const PRIMARY_FIRMWARE_EVENT_CHANNEL_INDEX:usize=13;
macro_rules! dev_info{($($x:tt)*)=>{()}}macro_rules! dev_err{($($x:tt)*)=>{()}}
mod util{pub fn g17p_verbose()->bool{false}}
mod module_parameters{pub struct Param;impl Param{pub fn value(&self)->&u32{&0}}pub static g17p_fw_log:Param=Param;pub static g17p_progress_watchdog:Param=Param;pub static g17p_idle_selector:Param=Param;}
'''+f'#[path="{p}/g17_initdata.rs"]mod g17_initdata;\n#[path="{p}/g17_completion.rs"]mod g17_completion;\n'+r'''
use g17_initdata::InstanceRole;
mod g17_host_path{
 use std::sync::atomic::{AtomicBool,AtomicU64,Ordering};
 pub const RING_CACHE:u32=1<<5;
 pub static CACHE_ON:AtomicBool=AtomicBool::new(true); // production default mask includes RING_CACHE
 pub static RING_CACHE_HITS:AtomicU64=AtomicU64::new(0);
 pub fn enabled(bit:u32)->bool{bit==RING_CACHE&&CACHE_ON.load(Ordering::Relaxed)}
 pub fn count(c:&AtomicU64){c.fetch_add(1,Ordering::Relaxed);}
'''+hpitem('pub(crate) const RING_REVALIDATE')+'\n'+hpitem('pub(crate) fn ring_cache_hit(')+'\n'+hpitem('pub(crate) fn ring_checked(')+r'''
}
mod g17_timing_buffer{pub const DIAG_MSGLOG:u32=1<<10;pub const MSG_EVENT:u64=5;pub const MSG_EVENT_TAIL:u64=6;pub fn diag_knob(_:u32)->bool{false}pub fn msglog(_:u64,_:u64,_:u64,_:u64){}}
'''+item('fn g17p_live_word_bounds(')+'\n'+item('fn g17p_live_read_u32(')+'\n'+item('fn g17p_snapshot_fw_record(')+r'''
struct Object{base:u64,bytes:Vec<u8>}
impl Object{
 fn iova(&self)->u64{self.base}
 fn read_u32(&mut self,a:u64)->Result<u32>{let o=a.checked_sub(self.base).ok_or(EINVAL)? as usize;Ok(u32::from_le_bytes(self.bytes.get(o..o+4).ok_or(EINVAL)?.try_into().unwrap()))}
 fn read_live_u32(&mut self,a:u64)->Result<u32>{let o=a.checked_sub(self.base).ok_or(EINVAL)? as usize;g17p_live_read_u32(&self.bytes,o)}
 fn read_u64(&mut self,a:u64)->Result<u64>{let o=a.checked_sub(self.base).ok_or(EINVAL)? as usize;Ok(u64::from_le_bytes(self.bytes.get(o..o+8).ok_or(EINVAL)?.try_into().unwrap()))}
 fn write_u32(&mut self,a:u64,v:u32)->Result{let o=a.checked_sub(self.base).ok_or(EINVAL)? as usize;self.bytes.get_mut(o..o+4).ok_or(EINVAL)?.copy_from_slice(&v.to_le_bytes());Ok(())}
 fn write_u64(&mut self,a:u64,v:u64)->Result{let o=a.checked_sub(self.base).ok_or(EINVAL)? as usize;self.bytes.get_mut(o..o+8).ok_or(EINVAL)?.copy_from_slice(&v.to_le_bytes());Ok(())}
 fn with_bytes_mut<T>(&mut self,f:impl FnOnce(&mut[u8])->Result<T>)->Result<T>{f(&mut self.bytes)}
}
struct Instance{main_config:u64,status_a:u64}struct Role{instance:Instance,state_grid:u64}struct Shared{hw_data_bundle:u64}struct Handoff{primary:Role,secondary:Role,shared:Shared}struct Resources{shared_cluster:Object}struct Owner{handoff:Handoff,resources:Resources,primary:Object,secondary:Object,event_ring_checked:[(u64,u64,u32);2]}
'''+item('pub(crate) struct G17PPrimaryFirmwareEventRecord {')+'\nimpl Owner{\n'+item('    pub(crate) fn drain_firmware_event(')+r'''
 fn ktrace_object(&mut self,role:InstanceRole)->&mut Object{match role{InstanceRole::Primary=>&mut self.primary,InstanceRole::Secondary=>&mut self.secondary}}
 fn pair(&self,role:InstanceRole)->[u64;2]{let r=match role{InstanceRole::Primary=>&self.handoff.primary,InstanceRole::Secondary=>&self.handoff.secondary};let e=g17_initdata::derive_channel_table(role,r.state_grid,r.instance.status_a,r.instance.main_config,self.handoff.shared.hw_data_bundle)[13];[e.states[0],e.states[1]]}
 fn new()->Self{let mut s=Self{handoff:Handoff{primary:Role{instance:Instance{main_config:0x10000,status_a:0x100000},state_grid:0x50000},secondary:Role{instance:Instance{main_config:0x20000,status_a:0x300000},state_grid:0x60000},shared:Shared{hw_data_bundle:0x800000}},resources:Resources{shared_cluster:Object{base:0,bytes:vec![0;0x40000]}},primary:Object{base:0x100000,bytes:vec![0;0x100000]},secondary:Object{base:0x300000,bytes:vec![0;0x100000]},event_ring_checked:[(0,0,0);2]};let off=g17_initdata::channel_table_entry_range(13).unwrap().start as u64;for(role,main)in[(InstanceRole::Primary,0x10000),(InstanceRole::Secondary,0x20000)]{let pair=s.pair(role);s.resources.shared_cluster.write_u64(main+off,pair[0]).unwrap();s.resources.shared_cluster.write_u64(main+off+8,pair[1]).unwrap();}s}
 fn inject(&mut self,role:InstanceRole,index:u32,kind:u32){let [cursor,ring]=self.pair(role);let o=self.ktrace_object(role);o.write_u32(cursor,index).unwrap();o.write_u32(cursor+0x20,(index+1)%256).unwrap();let at=ring-o.base+u64::from(index)*72;o.bytes[at as usize..at as usize+72].fill(0);o.bytes[at as usize..at as usize+4].copy_from_slice(&kind.to_le_bytes());}
}
#[test]fn role_secondary_consumption_never_moves_primary_cursor(){let mut o=Owner::new();o.inject(InstanceRole::Primary,7,1);o.inject(InstanceRole::Secondary,9,1);let r=o.drain_firmware_event(InstanceRole::Secondary).unwrap().unwrap();assert_eq!(r.consumer_before,9);assert_eq!(r.consumer_published,10);let p=o.pair(InstanceRole::Primary);assert_eq!(o.primary.read_u32(p[0]).unwrap(),7);assert!(o.drain_firmware_event(InstanceRole::Secondary).unwrap().is_none());assert_eq!(o.drain_firmware_event(InstanceRole::Primary).unwrap().unwrap().consumer_published,8);}
#[test]fn role_wrap_and_unmodelled_records_are_consumed_after_copy(){let mut o=Owner::new();o.inject(InstanceRole::Secondary,255,0xdead);let r=o.drain_firmware_event(InstanceRole::Secondary).unwrap().unwrap();assert_eq!(r.consumer_published,0);assert_eq!(u32::from_le_bytes(r.raw[..4].try_into().unwrap()),0xdead);}
#[test]fn role_pointer_corruption_and_partial_record_never_advance_consumer(){let mut o=Owner::new();o.inject(InstanceRole::Secondary,1,1);let p=o.pair(InstanceRole::Secondary);let off=g17_initdata::channel_table_entry_range(13).unwrap().start as u64;o.resources.shared_cluster.write_u64(0x20000+off,o.pair(InstanceRole::Primary)[0]).unwrap();assert!(o.drain_firmware_event(InstanceRole::Secondary).is_err());assert_eq!(o.secondary.read_u32(p[0]).unwrap(),1);o.resources.shared_cluster.write_u64(0x20000+off,p[0]).unwrap();o.secondary.bytes.truncate((p[1]-o.secondary.base)as usize+72+20);assert!(o.drain_firmware_event(InstanceRole::Secondary).is_err());assert_eq!(o.secondary.read_u32(p[0]).unwrap(),1);}
'''
with tempfile.TemporaryDirectory() as t:
 f=Path(t)/'test.rs';f.write_text(with_g17_config(code));exe=Path(t)/'test';subprocess.run(['rustc','--edition=2021','--test',str(f),'-o',str(exe)],check=True);subprocess.run([str(exe),'role_','--test-threads=1'],check=True)
print('PASS production role-ring reader + native initdata/decoders: separate cursors, wrap, unknown records, wrong-role pointer rejection, partial-copy safety')
