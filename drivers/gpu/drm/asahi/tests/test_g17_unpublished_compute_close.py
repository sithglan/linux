#!/usr/bin/env python3
"""Compile real close/cancel/lease logic; published or ambiguous owners survive."""
from pathlib import Path
from g17_compute_inflight_test_support import completion_mod, queue_limits_mock
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def item(file, marker):
    source = (ROOT / file).read_text()
    start = source.index(marker)
    end = source.index('{', start) + 1
    depth = 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[start:end]


code = r'''
#![allow(dead_code)]
use std::sync::{Arc,atomic::{AtomicUsize,Ordering}};
type Result<T=()> = std::result::Result<T,i32>;
const EIO:i32=5;
''' + f'#[path="{ROOT / "g17_channel.rs"}"] mod g17_channel;\n' + r'''
use g17_channel::{ChannelTable,ChannelId,Engine,Lease};
struct Context;
struct Binding(Arc<AtomicUsize>);
impl Drop for Binding {fn drop(&mut self){self.0.fetch_add(1,Ordering::SeqCst);}}
impl Binding {fn retarget_logical_context(&mut self,_:Arc<Context>)->Result{Ok(())}}
#[derive(PartialEq)]enum G17PSksmQueueHardwareState{PreparedMemory,HardwareRegistered}
struct G17PSksmQueue {channel_lease:Lease,hardware_state:G17PSksmQueueHardwareState,submission_ordinal:u64}
impl G17PSksmQueue {
 fn bind_logical_channel_control(&mut self,_:&Context)->Result{Ok(())}
''' + item('g17_manager.rs', 'pub(crate) fn requires_first_install(') + r'''
}
struct Manager {channel_ids:ChannelTable}
impl Manager {
''' + item('g17_manager.rs', 'pub(crate) fn cancel_unpublished_compute_queue(') + r'''
}
struct Previous {_context:Arc<Context>,_binding:Option<Binding>,publication:Option<()>}
''' + completion_mod() + queue_limits_mock() + r'''
type Inflight=g17_completion::PairFifo<(),{g17_queue_limits::MAX_COMPUTE_IN_FLIGHT_PER_QUEUE as usize}>;
struct Queue {owner:Option<u64>,queue:G17PSksmQueue,inflight:Inflight,quarantined:bool,
 previous_binding:Option<Previous>,binding:Option<Binding>,context:Option<Arc<Context>>,
 released:bool,retired_by_teardown:bool,retirement_ready:bool}
struct Runtime {async_compute_queues:Vec<Queue>,manager:Option<Manager>}
impl Runtime {
 // recovery-leak witness (tested in test_g17_async_retirement.py): no queue here is quarantined.
 fn release_firmware_retired_quarantined_compute_nested(&mut self,_reason:&'static str){}
''' + item('g17_async_compute.rs', 'fn drop_unpublished_compute_queue(') + item('g17_async_compute.rs', 'pub(crate) fn release_async_compute_queue(') + r'''
}
fn runtime()->Runtime{Runtime{async_compute_queues:vec![],manager:Some(Manager{channel_ids:ChannelTable::new()})}}
fn add(r:&mut Runtime,id:u64,drops:&Arc<AtomicUsize>)->Lease{
 let lease=r.manager.as_mut().unwrap().channel_ids.acquire(ChannelId(id),Engine::Compute,None).unwrap();
 r.async_compute_queues.push(Queue{owner:Some(id),queue:G17PSksmQueue{channel_lease:lease,
  hardware_state:G17PSksmQueueHardwareState::PreparedMemory,submission_ordinal:0},
  inflight:Inflight::new(),quarantined:false,previous_binding:None,binding:Some(Binding(drops.clone())),
  context:Some(Arc::new(Context)),released:false,retired_by_teardown:false,
  retirement_ready:false});lease
}
#[test]fn ten_thousand_unpublished_closes_restore_exact_credit_and_drop_bindings(){
 let mut r=runtime();let drops=Arc::new(AtomicUsize::new(0));
 for id in 1..=10_000 {
  let lease=add(&mut r,id,&drops);assert_eq!(lease.qid,0);
  r.release_async_compute_queue(id);
  assert!(r.async_compute_queues.is_empty());
  assert!(!r.manager.as_ref().unwrap().channel_ids.matches(lease));
  assert_eq!(drops.load(Ordering::SeqCst),id as usize);
 }
}
#[test]fn sealed_lease_is_retained_even_before_hardware_state_advances(){
 let mut r=runtime();let drops=Arc::new(AtomicUsize::new(0));let lease=add(&mut r,1,&drops);
 assert!(r.manager.as_mut().unwrap().channel_ids.publish(lease));
 r.release_async_compute_queue(1);
 assert_eq!(r.async_compute_queues.len(),1);assert_eq!(drops.load(Ordering::SeqCst),0);
 assert!(r.manager.as_ref().unwrap().channel_ids.matches(lease));
}
#[test]fn active_quarantined_previous_and_installed_owners_are_retained(){
 for state in 0..5 {
  let mut r=runtime();let drops=Arc::new(AtomicUsize::new(0));let lease=add(&mut r,1,&drops);
  let q=&mut r.async_compute_queues[0];
  match state {0=>{let _=q.inflight.push(());},1=>q.quarantined=true,
   2=>q.previous_binding=Some(Previous{_context:Arc::new(Context),_binding:None,publication:Some(())}),
   3=>q.queue.hardware_state=G17PSksmQueueHardwareState::HardwareRegistered,
   _=>q.queue.submission_ordinal=1}
  r.release_async_compute_queue(1);
  assert_eq!(r.async_compute_queues.len(),1);assert_eq!(drops.load(Ordering::SeqCst),0);
  assert!(r.manager.as_ref().unwrap().channel_ids.matches(lease));
 }
}
#[test]fn stale_close_never_cancels_a_replacement_lease(){
 let mut r=runtime();let drops=Arc::new(AtomicUsize::new(0));let old=add(&mut r,1,&drops);
 let table=&mut r.manager.as_mut().unwrap().channel_ids;
 assert!(table.cancel_unpublished(old));let replacement=table.acquire(ChannelId(2),Engine::Compute,None).unwrap();
 assert_eq!(old.qid,replacement.qid);r.release_async_compute_queue(1);
 assert!(r.manager.as_ref().unwrap().channel_ids.matches(replacement));
 assert_eq!(r.async_compute_queues.len(),1);assert_eq!(drops.load(Ordering::SeqCst),0);
}
#[test]fn one_unpublished_close_in_a_full_table_restores_only_its_credit(){
 let mut r=runtime();let drops=Arc::new(AtomicUsize::new(0));let mut leases=vec![];
 for id in 1..=128{leases.push(add(&mut r,id,&drops));}
 r.release_async_compute_queue(65);
 let table=&mut r.manager.as_mut().unwrap().channel_ids;
 for (i,lease) in leases.iter().enumerate(){assert_eq!(table.matches(*lease),i!=64);}
 let replacement=table.acquire(ChannelId(129),Engine::Fragment,None).unwrap();
 assert_eq!(replacement.qid,64);assert!(table.acquire(ChannelId(130),Engine::Compute,None).is_none());
 assert_eq!(r.async_compute_queues.len(),127);assert_eq!(drops.load(Ordering::SeqCst),1);
}
'''

with tempfile.TemporaryDirectory(prefix='g17-unpublished-close-') as temp:
    path = Path(temp)
    (path / 'test.rs').write_text(code)
    subprocess.run(['rustc', '--edition=2021', '--test', str(path / 'test.rs'),
                    '-o', str(path / 'test')], check=True)
    subprocess.run([str(path / 'test')], check=True)
print('PASS production close, exact-lease cancellation and 10,000 unpublished lifetimes')
