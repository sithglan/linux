#!/usr/bin/env python3
"""v89: execute the production native statistics drain against a mock firmware.

The method under test is extracted verbatim from g17_resources.rs together
with the production cursor/type helpers from g17_completion.rs. The mock
firmware advances its producer while the host drains, records every consumer
publication, and owns the entry type words, so the test checks the native
AGXFirmware::drainFirmwareStatisticsRing / fetchNextEntry contract:
snapshot-bounded prefix, one published consumer step per entry, modulo-0x100
wrap, legal-type mask 0x5fff0000, and no write past an illegal entry or a bad
cursor (quarantine instead of the native assert).
"""
from pathlib import Path
import json, subprocess, tempfile

p = Path(__file__).resolve().parents[1]
text = (p / 'g17_resources.rs').read_text()


def item(marker):
    start = text.index(marker)
    end = text.index('{', start) + 1
    depth = 1
    while depth:
        depth += (text[end] == '{') - (text[end] == '}')
        end += 1
    return text[start:end]


def static(marker):
    start = text.index(marker)
    return text[start:text.index(';', start) + 1]


source = r'''
#![allow(dead_code, unused_variables, unused_macros)]
type Result<T=()> = std::result::Result<T,i32>;
type Error = i32;
const EINVAL:i32=22; const EIO:i32=5;
macro_rules! dev_err { ($($x:tt)*) => { REPORTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) } }
static REPORTS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
mod kernel { pub mod device { pub struct Device; } }
// v93: the default-off pstate sampler (exercised in test_g17_v93_fw_deps.py).
mod g17_timing_buffer { pub fn stats_pstate_enabled() -> bool { false }
 pub fn stats_pstate_note(_: usize, _: u32, _: u64, _: u64) { unreachable!() }
 pub fn stats_pstate_read_failed() { unreachable!() } }
#[derive(Debug, Clone, Copy, PartialEq, Eq)] enum InstanceRole { Primary, Secondary }
struct Instance { status_a:u64 }
struct Role { instance:Instance }
struct Handoff { primary:Role, secondary:Role }
const STATUS_A:u64 = 0x10_0000;
const STATE:usize = 0x280; const RING:usize = 0xa_fac0; const ENTRY:usize = 0x48;
struct Object { base:u64, bytes:Vec<u8>, publications:Vec<u32>, reads:usize,
                advance_after_writes:Option<(usize,u32)> }
impl Object {
 fn new() -> Self { Object { base: STATUS_A, bytes: vec![0; RING + 0x100*ENTRY + 0x100],
                             publications: vec![], reads: 0, advance_after_writes: None } }
 fn iova(&self)->u64 { self.base }
 fn read_live_u32(&mut self,address:u64)->Result<u32> {
  self.reads += 1;
  let offset=address.checked_sub(self.base).ok_or(EINVAL)? as usize;
  g17p_live_read_u32(&self.bytes,offset)
 }
 fn write_u32(&mut self,address:u64,value:u32)->Result {
  let offset=address.checked_sub(self.base).ok_or(EINVAL)? as usize;
  g17p_live_write_u32(&mut self.bytes,offset,value)?;
  if offset == STATE { self.publications.push(value); }
  if let Some((after, next)) = self.advance_after_writes {
   if self.publications.len() == after { self.set(STATE+0x20, next); self.advance_after_writes = None; }
  }
  Ok(())
 }
 fn set(&mut self, offset:usize, value:u32) { self.bytes[offset..offset+4].copy_from_slice(&value.to_le_bytes()); }
 fn get(&self, offset:usize) -> u32 { u32::from_le_bytes(self.bytes[offset..offset+4].try_into().unwrap()) }
 fn entry_type(&mut self, index:u32, kind:u32) { self.set(RING + index as usize*ENTRY, kind); }
}
struct Dev; impl Dev { fn as_ref(&self) -> &kernel::device::Device { &kernel::device::Device } }
struct Resources { handoff:Handoff, objects:[Object;2], dev:Dev }
impl Resources {
 fn new()->Self {
  Self { handoff:Handoff { primary:Role{instance:Instance{status_a:STATUS_A}}, secondary:Role{instance:Instance{status_a:STATUS_A}} },
         objects:[Object::new(),Object::new()], dev:Dev }
 }
 fn dev(&self)->&Dev { &self.dev }
 fn ktrace_object(&mut self,role:InstanceRole)->&mut Object { &mut self.objects[match role{InstanceRole::Primary=>0,InstanceRole::Secondary=>1}] }
'''
source += item('    pub(crate) fn drain_statistics_ring_native(') + '\n}\n'
source += '#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]\n' + item('pub(crate) struct G17PNativeRingDrainStats {') + '\n'
source += static('pub(crate) static G17P_NATIVE_RING_QUARANTINE') + '\n'
for name in ['pub(crate) const fn g17p_native_ring_quarantine_bit(', 'fn g17p_native_ring_quarantine(',
             'fn g17p_live_word_bounds(', 'fn g17p_live_read_u32(', 'fn g17p_live_write_u32(']:
    source += item(name) + '\n'
source += '#[path=' + json.dumps(str(p / 'g17_completion.rs')) + '] mod g17_completion;\n'
source += r'''
use core::sync::atomic::Ordering::Relaxed;
fn reset() { G17P_NATIVE_RING_QUARANTINE.store(0, Relaxed); REPORTS.store(0, Relaxed); }
fn legal(o:&mut Object) { for i in 0..0x100 { o.entry_type(i, 0x19); } }

#[test] fn each_entry_publishes_one_consumer_step_and_only_the_snapshot() {
 reset();
 for role in [InstanceRole::Primary, InstanceRole::Secondary] {
  let mut r = Resources::new();
  { let o = r.ktrace_object(role); legal(o); o.set(STATE, 250); o.set(STATE+0x20, 3);
    // Firmware keeps producing while the host drains.
    o.advance_after_writes = Some((2, 9)); }
  let s = r.drain_statistics_ring_native(role).unwrap();
  assert_eq!((s.consumer_before, s.producer, s.consumed, s.quarantined), (250, 3, 9, false));
  let o = r.ktrace_object(role);
  assert_eq!(o.publications, vec![251,252,253,254,255,0,1,2,3]);
  assert_eq!(o.get(STATE), 3); assert_eq!(o.get(STATE+0x20), 9);
  // The next drain releases the late entries.
  let s = r.drain_statistics_ring_native(role).unwrap();
  assert_eq!(s.consumed, 6); assert_eq!(r.ktrace_object(role).get(STATE), 9);
 }
 // The other role's object was never touched by a role's drain.
 assert_eq!(REPORTS.load(Relaxed), 0);
}

#[test] fn empty_ring_reads_cursors_and_never_writes() {
 reset();
 let mut r = Resources::new(); r.objects[0].set(STATE, 17); r.objects[0].set(STATE+0x20, 17);
 let s = r.drain_statistics_ring_native(InstanceRole::Primary).unwrap();
 assert_eq!(s.consumed, 0); assert!(r.objects[0].publications.is_empty());
}

#[test] fn every_legal_statistics_type_and_only_those() {
 for kind in 0u32..70 {
  reset();
  let mut r = Resources::new(); r.objects[1].set(STATE, 5); r.objects[1].set(STATE+0x20, 6); r.objects[1].entry_type(5, kind);
  let legal = (0x10..=0x1c).contains(&kind) || kind == 0x1e;
  let result = r.drain_statistics_ring_native(InstanceRole::Secondary);
  assert_eq!(result.is_ok(), legal, "type {kind:#x}");
  assert_eq!(r.objects[1].publications.len(), legal as usize);
 }
}

#[test] fn illegal_entry_stops_before_publication_and_quarantines_once() {
 reset();
 let mut r = Resources::new();
 { let o = &mut r.objects[0]; legal(o); o.set(STATE, 10); o.set(STATE+0x20, 14); o.entry_type(12, 0x5); }
 assert_eq!(r.drain_statistics_ring_native(InstanceRole::Primary), Err(EIO));
 assert_eq!(r.objects[0].publications, vec![11, 12]);
 assert_eq!(r.objects[0].get(STATE), 12);
 assert_eq!(REPORTS.load(Relaxed), 1);
 // Quarantined: no further cursor reads or writes, no repeated report.
 let reads = r.objects[0].reads;
 let s = r.drain_statistics_ring_native(InstanceRole::Primary).unwrap();
 assert!(s.quarantined); assert_eq!(r.objects[0].reads, reads); assert_eq!(REPORTS.load(Relaxed), 1);
 // The secondary role is independent.
 { let o = &mut r.objects[1]; legal(o); o.set(STATE, 0); o.set(STATE+0x20, 2); }
 assert_eq!(r.drain_statistics_ring_native(InstanceRole::Secondary).unwrap().consumed, 2);
}

#[test] fn out_of_range_cursors_are_never_published() {
 for (c, p) in [(256u32, 0u32), (0, 256), (u32::MAX, 1), (1, u32::MAX)] {
  reset();
  let mut r = Resources::new(); legal(&mut r.objects[0]); r.objects[0].set(STATE, c); r.objects[0].set(STATE+0x20, p);
  assert_eq!(r.drain_statistics_ring_native(InstanceRole::Primary), Err(EIO));
  assert!(r.objects[0].publications.is_empty()); assert_eq!(r.objects[0].get(STATE), c);
  assert!(G17P_NATIVE_RING_QUARANTINE.load(Relaxed) & 4 != 0);
 }
}

#[test] fn full_ring_minus_one_is_drained_in_order() {
 reset();
 let mut r = Resources::new(); legal(&mut r.objects[0]); r.objects[0].set(STATE, 7); r.objects[0].set(STATE+0x20, 6);
 let s = r.drain_statistics_ring_native(InstanceRole::Primary).unwrap();
 assert_eq!(s.consumed, 255);
 let expected: Vec<u32> = (1..=255).map(|k| (7 + k) % 256).collect();
 assert_eq!(r.objects[0].publications, expected);
}

#[test] fn trace_and_cursor_helpers_match_native_constants() {
 use g17_completion::*;
 for kind in 0u32..80 { assert_eq!(g17p_ring_type_legal(G17P_KTRACE_TYPE_MASK, kind), kind == 5); }
 assert_eq!(g17p_native_ring_pending(511, 0, G17P_KTRACE_CAPACITY), Ok(1));
 assert_eq!(g17p_native_ring_pending(3, 3, G17P_KTRACE_CAPACITY), Ok(0));
 assert!(g17p_native_ring_pending(512, 0, G17P_KTRACE_CAPACITY).is_err());
 assert!(g17p_native_ring_pending(0, 0, 0).is_err());
 assert_eq!(g17p_native_ring_next(0x1ff, G17P_KTRACE_CAPACITY), 0);
 assert_eq!(g17p_native_ring_next(0xff, G17P_STATISTICS_CAPACITY), 0);
 // Same cursor step as the pre-existing trace reader for every cursor pair.
 for c in 0..G17P_KTRACE_CAPACITY { for q in [0u32, 1, 255, 511] {
  let old = prepare_g17p_ktrace_read(c, q);
  if c == q { assert!(old.is_err()); continue; }
  assert_eq!(old.unwrap().1, g17p_native_ring_next(c, G17P_KTRACE_CAPACITY));
 } }
 assert_eq!(G17P_STATISTICS_STATE_STATUS_A_OFFSET, 0x280);
 assert_eq!(G17P_STATISTICS_RING_STATUS_A_OFFSET, 0xafac0);
 assert_eq!(G17P_KTRACE_RING_STATUS_A_OFFSET + G17P_KTRACE_CAPACITY as u64 * G17P_KTRACE_ENTRY_SIZE as u64,
            G17P_STATISTICS_RING_STATUS_A_OFFSET);
}
'''
cfg = (p / 'g17_config.rs')
source += '\n#[path=' + json.dumps(str(cfg)) + '] mod g17_config;\n'
with tempfile.TemporaryDirectory(prefix='g17-native-ring-') as directory:
    root = Path(directory); (root / 'test.rs').write_text(source)
    subprocess.run(['rustc', '--edition=2021', '--test', str(root / 'test.rs'), '-o', str(root / 'test')], check=True)
    subprocess.run([str(root / 'test'), '--test-threads=1', '--quiet'], check=True)
print('PASS native statistics drain: per-entry publication, snapshot bound, wrap, type mask, quarantine, trace helpers')
