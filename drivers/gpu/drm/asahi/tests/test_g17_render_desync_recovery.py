#!/usr/bin/env python3
"""A drifted ring producer must rebuild the graph, not retire the client.

`restage_command`'s admission guard tests two independent things at once:
whether the firmware still owns the previous group (job list, queue idleness)
and whether the host ordinal still agrees with the ring producer this graph has
published. A submission stages its items -- advancing `write` -- before the
outer publication that makes them visible, so any failure in between advances
`write` without advancing the ordinal. Every later restage on that graph is then
refused for a mismatch that has nothing to do with the GPU, and the client is
retired permanently by a single lost frame. For the compositor that is a dead
session.

This compiles the real `G17PRenderItemPlan` and the real predicate that
separates the two conditions, so the recovery can never fire while the firmware
is still running a group.
"""
from pathlib import Path
import subprocess, tempfile

SOURCE = Path(__file__).resolve().parents[1]
resources = (SOURCE / 'g17_resources.rs').read_text()
manager = (SOURCE / 'g17_manager.rs').read_text()


def item(source, needle):
    a = source.index(needle)
    i = source.index('{', a) + 1
    depth = 1
    while depth:
        depth += (source[i] == '{') - (source[i] == '}')
        i += 1
    return source[a:i]


# --- where the recovery runs ----------------------------------------------
# v92d: per-pair storage; the reservation (reached from begin_translated_render
# and the off-lock restage) recycles before anything is allocated.
begin = item(manager, '    fn reserve_translated_render(')
assert 'self.recycle_desynced_render_storage(pair_slot)?' in begin, \
    'nothing recycles a desynced graph before a submission is built'
assert begin.index('self.recycle_desynced_render_storage(pair_slot)?') \
    < begin.index('.allocate_with_token()'), \
    'the recycle runs after this submission has already taken a HardwareBufferID'

recycle = item(manager, '    fn recycle_desynced_render_storage(')
# It may only drop a graph on the same evidence a cross-VM handoff requires.
assert 'self.require_retired_render_owner(pair_slot).is_err()' in recycle, \
    'the recycle is not gated on the retirement predicate'
# The detached graph waits in its queue's deferred teardown.
assert recycle.index('restage_admission_desynced') < recycle.index('storage.take()') \
    < recycle.index('logical_queue_teardowns'), \
    'the graph is dropped before the desync is established'

plan = '#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n' + '\n'.join(item(resources, n) for n in [
    'struct G17PRenderItemPlan {',
    'impl G17PRenderItemPlan {',
])
ordinals = resources[resources.index('pub(crate) const G17P_RETAINED_RENDER_ORDINALS'):]
ordinals = ordinals[:ordinals.index(';') + 1]
predicate = '\n'.join(item(resources, n) for n in [
    '    pub(crate) fn restage_admission_desynced(',
    '    pub(crate) fn restage_admission_cursors(',
    '    fn render_item_bases(',
])
# v91 word stores used by the item plan.
live_words = '\n'.join(item(resources, n) for n in [
    'fn g17p_live_word_bounds(', 'fn g17p_live_write_u32(', 'fn g17p_live_write_u64('])

code = r'''
#![allow(dead_code)]
#![allow(unused_variables)]
use std::sync::atomic::Ordering;
#[derive(Debug,PartialEq,Eq,Copy,Clone)]struct Error(i32);
type Result<T=()>=std::result::Result<T,Error>;
const EINVAL:Error=Error(-22);
''' + ordinals + r'''

/// Faithful stand-in for the submission crate's producer advance: one slot per
/// item, wrapping at the ring size, refusing a wrap onto the consumer.
mod g17_submission{
 #[derive(Copy,Clone)]pub enum G17ChannelAbiTarget{A18ProG17P}
 pub struct G17PChannelWritePlan{pub entry_byte_offset:u64,pub next_producer:u32}
 pub struct SubmissionError;
 pub const fn prepare_g17p_channel_write(
  _t:G17ChannelAbiTarget,producer:u32,consumer:u32,ring_entry_count:u32,
 )->Result<G17PChannelWritePlan,SubmissionError>{
  if ring_entry_count==0||consumer>=ring_entry_count{return Err(SubmissionError)}
  let next_producer=(producer+1)%ring_entry_count;
  if next_producer==consumer{return Err(SubmissionError)}
  Ok(G17PChannelWritePlan{entry_byte_offset:producer as u64*8,next_producer})
 }
}
fn g17p_render_tag16_enabled()->bool{false}
''' + live_words + r'''
struct RenderPublication{cursors:[u32;2]}

#[derive(Debug,Copy,Clone,PartialEq,Eq,Default)]
struct QueueIndices{done:u32,read:u32,write:u32}
impl QueueIndices{
 const fn idle(self)->bool{self.done==self.read&&self.read==self.write}
 fn at(value:u32)->Self{Self{done:value,read:value,write:value}}
}
#[derive(Debug,Copy,Clone)]
struct G17PUserRenderQueueState{
 tiling:QueueIndices,fragment:QueueIndices,job_list_empty:bool,
}
''' + plan + r'''

// g17p_host_path PRODUCER_READS (bit17): a per-thread mask so one test can
// run the same scenarios with the producer-only prefilter enabled.
mod g17_host_path {
 pub const PRODUCER_READS:u32=1<<17;
 thread_local!{pub static MASK:std::cell::Cell<u32>=const{std::cell::Cell::new(0)};}
 pub fn enabled(bit:u32)->bool{MASK.with(|m|m.get()&bit!=0)}
}

// Fixed-item plans (variable_render_items false), as with RENDER_SKSM items.
struct Storage{state:G17PUserRenderQueueState,render_publication:RenderPublication}
impl Storage{
 fn variable_render_items(&self)->bool{false}
 fn snapshot(&mut self,_ordinal:u64)->Result<G17PUserRenderQueueState>{Ok(self.state)}
 // The two producer words the full snapshot also reports.
 fn render_producer_indices(&mut self)->Result<[u32;2]>{Ok([self.state.tiling.write,self.state.fragment.write])}
''' + predicate + r'''
}

fn storage(tiling:u32,fragment:u32,job_list_empty:bool,busy:bool)->Storage{
 let mut t=QueueIndices::at(tiling);
 let mut f=QueueIndices::at(fragment);
 if busy{t.write=t.write+3;f.write=f.write+3;}
 Storage{state:G17PUserRenderQueueState{tiling:t,fragment:f,job_list_empty},render_publication:RenderPublication{cursors:[0;2]}}
}

fn base(ordinal:u64)->u32{G17PRenderItemPlan::new(ordinal,false).unwrap().base}

#[test]fn an_aligned_graph_is_not_desynced(){
  for ordinal in [1u64,2,7,100,193,1000,1<<24,1<<32]{
  let mut s=storage(base(ordinal),base(ordinal),true,false);
  assert!(!s.restage_admission_desynced(ordinal).unwrap(),"ordinal {}",ordinal);
 }
}

#[test]fn a_staged_but_unpublished_group_is_desynced(){
 // The failure shape: the items went into the ring, the ordinal never moved.
  for ordinal in [1u64,2,7,100,193,1<<24,1<<32]{
  let ahead=base(ordinal+1);
  let mut s=storage(ahead,ahead,true,false);
  assert!(s.restage_admission_desynced(ordinal).unwrap(),"ordinal {}",ordinal);
  assert_eq!(s.restage_admission_cursors(ordinal).unwrap(),([base(ordinal);2],ahead,ahead));
 }
}

#[test]fn only_one_queue_needs_to_have_drifted(){
 let ordinal=42;
 let mut s=storage(base(ordinal+1),base(ordinal),true,false);
 assert!(s.restage_admission_desynced(ordinal).unwrap());
 let mut s=storage(base(ordinal),base(ordinal+1),true,false);
 assert!(s.restage_admission_desynced(ordinal).unwrap());
}

#[test]fn a_running_group_is_never_reported_as_desync(){
 let ordinal=42;
 // Queues not idle: the firmware still owns the previous group. Dropping the
 // graph here would free memory under a live GPU.
 let mut s=storage(base(ordinal+1),base(ordinal+1),true,true);
 assert!(!s.restage_admission_desynced(ordinal).unwrap());
 // Job list not empty: same.
 let mut s=storage(base(ordinal+1),base(ordinal+1),false,false);
 assert!(!s.restage_admission_desynced(ordinal).unwrap());
}

#[test]fn producer_read_prefilter_gives_the_same_answers(){
 g17_host_path::MASK.with(|m|m.set(g17_host_path::PRODUCER_READS));
 an_aligned_graph_is_not_desynced();
 a_staged_but_unpublished_group_is_desynced();
 only_one_queue_needs_to_have_drifted();
 a_running_group_is_never_reported_as_desync();
}

#[test]fn an_unusable_ordinal_is_never_a_desync(){
 let mut s=storage(0,0,true,false);
 assert!(!s.restage_admission_desynced(0).unwrap());
 assert!(!s.restage_admission_desynced(G17P_RETAINED_RENDER_ORDINALS).unwrap());
}
'''

with tempfile.TemporaryDirectory() as d:
    d = Path(d)
    f = d / 'desync.rs'
    f.write_text(code)
    exe = d / 'desync'
    subprocess.run(['rustc', '--edition=2021', '--test', str(f), '-o', str(exe)], check=True)
    subprocess.run([str(exe), '--test-threads=1'], check=True, timeout=60)
print('PASS render desync recovery: a drifted ring producer is separated from a '
      'running group, and only the former rebuilds the graph')
