#!/usr/bin/env python3
"""Exercise the production control publisher with fallible host-backed mappings."""
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
resources = (ROOT / 'g17_resources.rs').read_text()
initdata = (ROOT / 'g17_initdata.rs').read_text()


def item(source, marker):
    start = source.index(marker)
    end = source.index('{', start) + 1
    depth = 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[start:end]


code = r'''
#![allow(dead_code)]
use std::cell::{Cell,UnsafeCell};
#[derive(Debug,Copy,Clone,PartialEq,Eq)]struct Error(i32);
impl From<std::num::TryFromIntError> for Error{fn from(_:std::num::TryFromIntError)->Self{EINVAL}}
type Result<T=()> = std::result::Result<T,Error>;
const EINVAL:Error=Error(22);const EAGAIN:Error=Error(11);const EIO:Error=Error(5);
mod g17_initdata {
''' + '\n'.join(line for line in initdata.splitlines() if any(
    line.startswith('pub(crate) const ' + name + ':') for name in [
        'CONTROL_RECORD_SIZE', 'CONTROL_RING_ENTRIES', 'CONTROL_RING_INDEX_MASK',
        'CHANNEL_STATE_CONSUMER', 'CHANNEL_STATE_PRODUCER', 'CHANNEL_STATE_SPACING',
    ])) + '\n' + item(initdata, 'pub(crate) const fn device_control_asserts_gpu_power(') + r'''
 pub const CONTROL_CHANNEL_INDEX:usize=12;
 pub const WORK_STATE_GRID_OFFSETS:[usize;17]=[0;17];
 pub const REGION_C_GPU_CORE_PWR_ASSERT_COUNTER_OFFSET:usize=0;
 pub struct Channel{pub ring:u64}
 pub fn derive_channel_table(_:super::InstanceRole,_:u64,_:u64,_:u64,_:u64)->[Channel;17]{std::array::from_fn(|_|Channel{ring:0x1000})}
}
enum InstanceRole{Primary}
// Diagnostic message log (g17_timing_buffer::msglog) is a no-op here.
mod g17_timing_buffer { pub const MSG_DEVICE_CONTROL:u64=4; pub fn msglog(_:u64,_:u64,_:u64,_:u64){} }
// v93r CONTROL_TRACE (g17p_recovery_path bit1, default off): publish-time bookkeeping only.
mod g17_recovery_path { pub fn note_control_publish(_slot:u32,_opcode:u32,_arg:u32){} }
''' + item(resources, 'fn g17p_live_word_bounds(') + '\n' + item(resources, 'fn g17p_live_write_u32(') + r'''
struct Object{raw:UnsafeCell<Vec<u8>>,fail:Cell<bool>,views:Cell<usize>}
struct View(*mut u8);impl View{fn as_mut_ptr(&self)->*mut u8{self.0}}
impl Object{
 fn new(n:usize)->Self{Self{raw:UnsafeCell::new(vec![0xa5;n]),fail:Cell::new(false),views:Cell::new(0)}}
 fn vmap(&self)->Result<View>{self.views.set(self.views.get()+1);if self.fail.get(){Err(EIO)}else{Ok(View(unsafe{(*self.raw.get()).as_mut_ptr()}))}}
 fn bytes(&self)->Vec<u8>{unsafe{(&*self.raw.get()).clone()}}
 fn word(&self,off:usize)->u32{u32::from_le_bytes(unsafe{(&*self.raw.get())[off..off+4].try_into().unwrap()})}
 fn set_word(&self,off:usize,v:u32){unsafe{(&mut *self.raw.get())[off..off+4].copy_from_slice(&v.to_le_bytes())}}
}
struct Mapped{logical_size:usize,object_offset:usize,object:Object}
impl Mapped{fn new(n:usize)->Self{Self{logical_size:n,object_offset:16,object:Object::new(n+32)}}fn iova(&self)->u64{0x1000}}
struct Resources{shared_cluster:Mapped,primary_state:Mapped,region_c:Mapped}
struct Instance{status_a:u64,main_config:u64}
struct Primary{state_grid:u64,instance:Instance}
struct Shared{hw_data_bundle:u64}
struct Handoff{primary:Primary,shared:Shared}
struct Counters{primary:[u32;3]}
#[derive(Debug,PartialEq,Eq)]
''' + item(resources, 'pub(crate) struct G17PDeviceControlPublication {') + r'''
struct Owner{resources:Resources,handoff:Handoff,consumer:u32}
impl Owner{
 fn new(r:u32,w:u32)->Self{let s=Self{resources:Resources{shared_cluster:Mapped::new(256*64),primary_state:Mapped::new(64),region_c:Mapped::new(16)},handoff:Handoff{primary:Primary{state_grid:0,instance:Instance{status_a:0,main_config:0}},shared:Shared{hw_data_bundle:0}},consumer:r};s.resources.primary_state.object.set_word(16+32,w);s.resources.region_c.object.set_word(16,7);s}
 fn producer(&self)->u32{self.resources.primary_state.object.word(16+32)}
 fn control_counters(&self)->Result<Counters>{Ok(Counters{primary:[self.consumer,0,self.producer()]})}
 fn snapshot(&self)->[Vec<u8>;3]{[self.resources.shared_cluster.object.bytes(),self.resources.primary_state.object.bytes(),self.resources.region_c.object.bytes()]}
''' + item(resources, '    pub(crate) fn publish_primary_device_control_raw(') + r'''
}
fn record(opcode:u32)->[u8;64]{let mut r=[0x69;64];r[..4].copy_from_slice(&opcode.to_le_bytes());r}
#[test]fn full_ring_rejects_every_wrap_position_without_writing_or_mapping(){
 for w in 0..256{for opcode in [0x20,0x2e]{let mut o=Owner::new((w+1)&255,w);let before=o.snapshot();
  assert_eq!(o.publish_primary_device_control_raw(&record(opcode)),Err(EAGAIN),"producer={w}");
  assert_eq!(o.snapshot(),before);assert_eq!(o.resources.shared_cluster.object.views.get(),0);
 }}
}
#[test]fn corrupted_cursors_never_alias_valid_slots(){
 for invalid in [256,257,511,65535,u32::MAX]{for (r,w) in [(invalid,0),(0,invalid),(invalid,invalid)]{
  let mut o=Owner::new(r,w);let before=o.snapshot();assert_eq!(o.publish_primary_device_control_raw(&record(0x20)),Err(EIO));assert_eq!(o.snapshot(),before);
 }}
}
#[test]fn all_65536_cursor_pairs_preserve_capacity_and_publish_exact_slot(){
 let mut o=Owner::new(0,0);let r=record(0x20);
 for consumer in 0..256{for producer in 0..256{
  o.consumer=consumer;o.resources.primary_state.object.set_word(48,producer);
  let power=o.resources.region_c.object.word(16);let result=o.publish_primary_device_control_raw(&r);
  if (producer+1)&255==consumer{assert_eq!(result,Err(EAGAIN));assert_eq!(o.producer(),producer);assert_eq!(o.resources.region_c.object.word(16),power);}
  else{let p=result.unwrap();assert_eq!((p.slot,p.consumer_before,p.producer_before,p.producer_after),(producer,consumer,producer,(producer+1)&255));
   assert_eq!(o.producer(),(producer+1)&255);assert_eq!(o.resources.region_c.object.word(16),power.wrapping_add(1));
   let bytes=o.resources.shared_cluster.object.bytes();assert_eq!(&bytes[16+producer as usize*64..16+(producer as usize+1)*64],&r);
  }
 }}
}
#[test]fn stalled_consumer_allows_255_records_then_progress_restores_one_credit(){
 let mut o=Owner::new(0,0);
 for _ in 0..255{o.publish_primary_device_control_raw(&record(0x20)).unwrap();}
 let full=o.snapshot();for _ in 0..1000{assert_eq!(o.publish_primary_device_control_raw(&record(0x20)),Err(EAGAIN));}
 assert_eq!(o.snapshot(),full);o.consumer=1;
 assert_eq!(o.publish_primary_device_control_raw(&record(0x2e)).unwrap().producer_after,0);
 assert_eq!(o.resources.region_c.object.word(16),262);
 assert_eq!(o.publish_primary_device_control_raw(&record(0x2e)),Err(EAGAIN));
}
#[test]fn mapping_failures_and_invalid_opcodes_are_atomic(){
 for fail in 0..3{let mut o=Owner::new(0,0);let before=o.snapshot();
  match fail{0=>o.resources.shared_cluster.object.fail.set(true),1=>o.resources.primary_state.object.fail.set(true),_=>o.resources.region_c.object.fail.set(true)};
  assert_eq!(o.publish_primary_device_control_raw(&record(0x20)),Err(EIO));assert_eq!(o.snapshot(),before);
 }
 for opcode in [0,64,u32::MAX]{let mut o=Owner::new(0,0);let before=o.snapshot();assert_eq!(o.publish_primary_device_control_raw(&record(opcode)),Err(EINVAL));assert_eq!(o.snapshot(),before);}
}
'''
with tempfile.TemporaryDirectory() as directory:
    source = Path(directory) / 'test.rs'
    binary = Path(directory) / 'test'
    source.write_text(code)
    subprocess.run(['rustc', '--edition=2021', '-O', '--test', str(source), '-o', str(binary)], check=True)
    subprocess.run([str(binary)], check=True)
