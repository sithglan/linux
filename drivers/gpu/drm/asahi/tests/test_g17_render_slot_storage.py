#!/usr/bin/env python3
"""Execute production explicit render-pair ownership, including allocation failure and reuse."""
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
source = (root / 'g17_manager.rs').read_text()


def item(marker):
    start = source.index(marker)
    end = source.index('{', start) + 1
    depth = 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[start:end]


slots = item('struct G17PRenderSlots {')
slots_impl = item('impl G17PRenderSlots {')
state = item('struct G17PRenderSlotState {')
fields = dict(re.findall(r'^    (\w+): (.+),$', state, re.M))
# Every per-pair owner lives in the slot state. The manager's trace_capture
# is the separate compute archive, not a selected render archive.
manager = item('pub(crate) struct G17PManagerConstruction {')
for field in fields:
    if field == 'trace_capture':
        continue
    assert f'    {field}:' not in manager, f'{field} must live only in the slot state'
assert 'core::mem::swap' not in slots_impl, 'selection must not copy state'
assert 'KBox::init(try_init!(Self {' in item('impl G17PRenderSlotState {')
assert 'slots: KVec<Option<KBox<G17PRenderSlotState>>>' in slots
assert 'active_slot' not in slots
assert 'active_render_slot(' not in source


def field_type(field):
    if fields[field].startswith('g17_completion::PairFifo<'):
        return 'Vec<Arc<Token>>'
    return 'Option<Arc<Token>>' if fields[field].startswith('Option<') else fields[field]


state_fields = ','.join(f'{f}:{field_type(f)}' for f in fields)
set_fields = '\n'.join(
    f'm.render_slots.get_mut(slot).{f}=Some(token.clone());' if fields[f].startswith('Option<') else
    f'm.render_slots.get_mut(slot).{f}.push(token.clone());' if fields[f].startswith('g17_completion::PairFifo<') else
    f'm.render_slots.get_mut(slot).{f}=true;' if fields[f] == 'bool' else
    f'm.render_slots.get_mut(slot).{f}={fields[f]}::from(slot)+1;'
    for f in fields
)
check_fields = '\n'.join(
    f'assert_eq!(m.render_slots.get(slot).{f}.as_ref().unwrap().owner,slot);' if fields[f].startswith('Option<') else
    f'assert_eq!(m.render_slots.get(slot).{f}.len(),1); assert_eq!(m.render_slots.get(slot).{f}[0].owner,slot);' if fields[f].startswith('g17_completion::PairFifo<') else
    f'assert!(m.render_slots.get(slot).{f});' if fields[f] == 'bool' else
    f'assert_eq!(m.render_slots.get(slot).{f},{fields[f]}::from(slot)+1);'
    for f in fields
)
code = r'''
#![allow(dead_code)]
use std::sync::{Arc,atomic::{AtomicUsize,AtomicBool,Ordering}};
type KBox<T>=Box<T>;type Result<T=()>=std::result::Result<T,i32>;
const EINVAL:i32=22;const EIO:i32=5;const ENODEV:i32=19;const GFP_KERNEL:u32=0;
struct KVec<T>(Vec<T>);
impl<T> KVec<T>{
 fn with_capacity(capacity:usize,_flags:u32)->Result<Self>{Ok(Self(Vec::with_capacity(capacity)))}
 fn push(&mut self,value:T,_flags:u32)->Result{self.0.push(value);Ok(())}
}
impl<T> std::ops::Deref for KVec<T>{type Target=[T];fn deref(&self)->&[T]{&self.0}}
impl<T> std::ops::DerefMut for KVec<T>{fn deref_mut(&mut self)->&mut [T]{&mut self.0}}
static ALLOCATIONS:AtomicUsize=AtomicUsize::new(0);
static FAIL:AtomicBool=AtomicBool::new(false);
struct Token{owner:u8,drops:Arc<AtomicUsize>}
impl Drop for Token{fn drop(&mut self){self.drops.fetch_add(1,Ordering::SeqCst);}}
mod g17_resources{pub struct G17PRenderQueuePair;impl G17PRenderQueuePair{pub const SLOT_COUNT:u8=64;}}
''' + f'''
#[derive(Default)]struct G17PRenderSlotState{{{state_fields}}}
impl G17PRenderSlotState{{fn new()->Result<KBox<Self>>{{
 if FAIL.swap(false,Ordering::SeqCst){{return Err(12)}}
 ALLOCATIONS.fetch_add(1,Ordering::SeqCst);Ok(Box::default())
}}}}
{slots}
{slots_impl}
struct Manager{{render_slots:G17PRenderSlots}}
impl Manager{{
 fn new()->Self{{Self{{render_slots:G17PRenderSlots::new().unwrap()}}}}
 {item('pub(crate) fn ensure_render_slot(')}
}}
#[test]fn owner_switches_preserve_every_reference_without_steady_allocations(){{
 ALLOCATIONS.store(0,Ordering::SeqCst);
 let drops=Arc::new(AtomicUsize::new(0));let mut m=Manager::new();
 for slot in 0..64u8{{
  m.ensure_render_slot(slot).unwrap();let token=Arc::new(Token{{owner:slot,drops:drops.clone()}});
  {set_fields}
 }}
 let allocations=ALLOCATIONS.load(Ordering::SeqCst);assert_eq!(allocations,64);
 for _ in 0..100{{for slot in (0..64u8).rev(){{
  m.ensure_render_slot(slot).unwrap();
  {check_fields}
 }}}}
 assert_eq!(ALLOCATIONS.load(Ordering::SeqCst),allocations);
 assert_eq!(drops.load(Ordering::SeqCst),0);drop(m);assert_eq!(drops.load(Ordering::SeqCst),64);
}}
#[test]fn failed_first_selection_keeps_the_live_owner_intact(){{
 let drops=Arc::new(AtomicUsize::new(0));let mut m=Manager::new();let slot=0u8;
 let token=Arc::new(Token{{owner:slot,drops:drops.clone()}});{set_fields}drop(token);
 FAIL.store(true,Ordering::SeqCst);assert_eq!(m.ensure_render_slot(63),Err(12));
 assert!(m.render_slots.slots[63].is_none());
 {check_fields}
 assert_eq!(drops.load(Ordering::SeqCst),0);
 assert_eq!(m.ensure_render_slot(64),Err(EINVAL));
 m.ensure_render_slot(63).unwrap();m.ensure_render_slot(0).unwrap();{check_fields}
}}
#[test]fn parked_iteration_covers_every_other_allocated_slot_in_order(){{
 let mut m=Manager::new();
 for slot in [5u8,2,9]{{m.ensure_render_slot(slot).unwrap();m.render_slots.get_mut(slot).submission_ordinal=u64::from(slot);}}
 m.ensure_render_slot(2).unwrap();
 let parked:Vec<u64>=m.render_slots.parked(2).map(|state|state.submission_ordinal).collect();
 assert_eq!(parked,vec![0,5,9]);
 assert_eq!(m.render_slots.allocated(2).unwrap().submission_ordinal,2);
 assert_eq!(m.render_slots.allocated(3).err(),Some(ENODEV));
 assert_eq!(m.render_slots.allocated(64).err(),Some(EINVAL));
 m.ensure_render_slot(9).unwrap();
 let taken:Vec<u64>=m.render_slots.take_parked(9).map(|state|state.submission_ordinal).collect();
 assert_eq!(taken,vec![0,2,5]);
 assert_eq!(m.render_slots.get(9).submission_ordinal,9);
 assert!(m.render_slots.slots[0].is_none());assert_eq!(m.render_slots.parked(9).count(),0);
}}
#[test]fn event_visits_cannot_redirect_an_in_progress_publisher(){{
 let mut m=Manager::new();m.ensure_render_slot(7).unwrap();m.ensure_render_slot(19).unwrap();
 m.render_slots.get_mut(7).submission_ordinal=700;
 m.render_slots.get_mut(19).submission_ordinal=1900;
 for _ in 0..1000 {{
  let publishing=7;
  // An event visitor changes another pair between the publisher's accesses.
  m.render_slots.get_mut(19).submission_ordinal+=1;
  m.render_slots.get_mut(publishing).submission_ordinal+=1;
 }}
 assert_eq!(m.render_slots.get(7).submission_ordinal,1700);
 assert_eq!(m.render_slots.get(19).submission_ordinal,2900);
}}
'''
with tempfile.TemporaryDirectory(prefix='g17-slot-storage-') as tmp:
    src = Path(tmp) / 'test.rs'
    exe = Path(tmp) / 'test'
    src.write_text(code)
    subprocess.run(['rustc', '--edition=2021', '--test', str(src), '-o', str(exe)], check=True)
    subprocess.run([str(exe), '--test-threads=1'], check=True)
