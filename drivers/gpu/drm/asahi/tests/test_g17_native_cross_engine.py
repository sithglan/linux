#!/usr/bin/env python3
"""Run the actual publication-frontier/token/error helpers with mock fences."""
from pathlib import Path
from g17_compute_inflight_test_support import completion_mod, queue_limits_mock
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
drm = (root / 'g17_drm.rs').read_text()
job = (root / 'g17_job.rs').read_text()
live = (root / 'g17_live_boot.rs').read_text()
comp = (root / 'g17_async_compute.rs').read_text()
manager = (root / 'g17_manager.rs').read_text()
codec = (root / 'g17_submission.rs').read_text()

def block(text, marker):
    a = text.index(marker)
    b = text.index('{', a) + 1
    depth = 1
    while depth:
        depth += (text[b] == '{') - (text[b] == '}')
        b += 1
    return text[a:b]

def item(name):
    match = re.search(r'^(?:pub\(crate\) )?(?:(?:const )?fn|enum|struct) ' + name + r'\b', codec, re.M)
    assert match, name
    return block(codec, match.group())

constants = []
for name in ['KICK_TS_BITS', 'KICK_TS_MASK', 'KICK_QID_BITS', 'KICK_QID_MASK',
             'KICK_QID_SHIFT', 'KICK_STAMP_SHIFT', 'KICK_STAMP_INDEX_MASK',
             'KICK_RESERVED_MASK', 'KICK_SELECTOR_BIT']:
    constants.append(re.search(r'^(?:pub\(crate\) )?const ' + name + r': .*?;', codec, re.M | re.S).group())

program = r'''
#![allow(dead_code,unused_imports)]
use std::sync::{Arc,atomic::{AtomicI32,AtomicU64,Ordering}};
#[derive(Debug,Copy,Clone,PartialEq)]struct Error(i32);
impl Error {fn from_errno(e:i32)->Self{Self(e)}}
type Result<T=()> = std::result::Result<T,Error>;
const EIO:Error=Error(-5);const ENODEV:Error=Error(-19);const GFP_KERNEL:()=();
#[derive(Clone)]struct Fence {id:u64,status:Arc<AtomicI32>}
impl Fence {fn raw(&self)->*const Self{self}}
mod bindings {pub unsafe fn dma_fence_get_status(f:*const super::Fence)->i32{(*f).status.load(super::Ordering::Acquire)}}
mod kernel {pub(crate) use crate::bindings;pub mod dma_fence {
 pub trait RawDmaFence{fn raw(&self)->*const crate::Fence;}
 impl RawDmaFence for crate::Fence{fn raw(&self)->*const crate::Fence{self}}
}}
mod g17_status {pub struct VmStatus;}
#[derive(Default)]struct KVec<T>(Vec<T>);
impl<T>KVec<T>{fn push(&mut self,x:T,_f:())->Result{self.0.push(x);Ok(())}}
impl<T>std::ops::Deref for KVec<T>{type Target=[T];fn deref(&self)->&[T]{&self.0}}
mod g17_job {#[derive(Clone)]pub struct G17PCommandFences {pub ready:crate::Fence,pub completed:crate::Fence}}
'''
program += '\nmod g17_submission {\n' + '\n'.join(constants)
for name in ['SubmissionError', 'KickTimestamp', 'SksmKickWord', 'G17PClBarrierDependency', 'G17PRenderOrder']:
    program += '\n#[derive(Debug,Copy,Clone,PartialEq,Eq)]\n' + item(name)
program += '''
#[derive(Copy,Clone)]pub struct Geometry;impl Geometry{fn stamp_count(self)->u16{256}}
#[derive(Copy,Clone)]pub struct G17PClKickProducerState{pub base_timestamp:u64,pub geometry:Geometry}
'''
for name in ['encode_kick_word', 'decode_kick_word', 'encode_kick_timestamp',
             'g17p_render_barrier_from_timestamp', 'g17p_render_prefix_contains',
             'g17p_insert_prefix_dependency', 'g17p_render_order',
             'g17p_live_render_dependency', 'encode_g17p_cl_dependency',
             'encode_g17p_cl_barrier_dependency']:
    program += '\n' + item(name)
program += '''
pub fn native_word(d:G17PClBarrierDependency)->u64{
 encode_g17p_cl_barrier_dependency(G17PClKickProducerState{base_timestamp:0xabc00,geometry:Geometry},d).unwrap()
}
}\n'''
program += block(drm, 'fn append_render_prefix_dependencies(')
program += '\nstruct G17PQueue;impl G17PQueue {' + block(drm, '    fn check_firmware_fence(') + '}\n'
program += r'''
struct Render {owner:u64,status:Arc<g17_status::VmStatus>,render_order:g17_submission::G17PRenderOrder,fragment_dependency:AtomicU64,fence:Fence}
impl Render {fn fence(&self)->Fence{self.fence.clone()}
'''+block(drm, '    pub(crate) fn compute_predecessor_dependency(')+'}\n'
program += r'''
struct Producer{submitted:u64,previous_valid:bool,last_add_kicks_timestamp:u64}
struct HardwareQueue{queue_id:u8,producer:Producer}
impl HardwareQueue {
'''+block(manager, '    pub(crate) fn published_compute_dependency(')+'}\n'
program += r'''
struct ComputeCompletion{status:Arc<g17_status::VmStatus>,fence:Fence}
impl ComputeCompletion{fn fence(&self)->Fence{self.fence.clone()}}
struct Command{compute_sequence:u64}
struct Packet{completion:ComputeCompletion,command:Command}
impl Packet{fn completion(&self)->Result<&ComputeCompletion>{Ok(&self.completion)} fn command(&self)->&Command{&self.command}}
struct FirmwareTicket{timestamp:u64}
impl FirmwareTicket{fn kick_timestamp(&self)->u64{self.timestamp}}
struct Ticket{recovery_epoch:u64,firmware:FirmwareTicket}
impl HardwareQueue{fn queue_id(&self)->u8{self.queue_id}}
struct Active{packet:Packet,retirement:Option<Ticket>}
'''+completion_mod()+queue_limits_mock()+r'''
type Inflight=g17_completion::PairFifo<Active,{g17_queue_limits::MAX_COMPUTE_IN_FLIGHT_PER_QUEUE as usize}>;
fn one(active:Active)->Inflight{let mut f=Inflight::new();if f.push(active).is_err(){unreachable!()}f}
struct Queue{owner:Option<u64>,inflight:Inflight,quarantined:bool,queue:HardwareQueue}
#[derive(Copy,Clone)]struct Handshake{state:u32,epoch:u64}
struct Manager{h:Handshake}
impl Manager{fn firmware_recovery_handshake_snapshot(&mut self)->Result<Handshake>{Ok(self.h)}}
struct Runtime{manager:Option<Manager>,async_compute_queues:Vec<Queue>}
impl Runtime {
'''+block(comp, '    pub(crate) fn async_compute_native_dependency(')+'}\n'
program += r'''
fn fence(id:u64,status:i32)->Fence{Fence{id,status:Arc::new(AtomicI32::new(status))}}
fn pair(id:u64)->g17_job::G17PCommandFences{g17_job::G17PCommandFences{ready:fence(id+100,1),completed:fence(id,0)}}
fn render(vm:&Arc<g17_status::VmStatus>,seq:u64,qid:u8,stamp:u64)->Render{
 Render{owner:7,status:vm.clone(),render_order:g17_submission::G17PRenderOrder{sequence:seq,wait_through:None},
 fragment_dependency:AtomicU64::new(g17_submission::encode_kick_word(stamp,qid).unwrap()),fence:fence(seq,0)}
}
fn runtime(vm:&Arc<g17_status::VmStatus>)->Runtime{
 Runtime{manager:Some(Manager{h:Handshake{state:0,epoch:4}}),async_compute_queues:vec![Queue{
 owner:Some(7),quarantined:false,queue:HardwareQueue{queue_id:11,producer:Producer{submitted:1,previous_valid:true,last_add_kicks_timestamp:0x1000005}},
 inflight:one(Active{packet:Packet{completion:ComputeCompletion{status:vm.clone(),fence:fence(1,0)},command:Command{compute_sequence:41}},retirement:Some(Ticket{recovery_epoch:4,firmware:FirmwareTicket{timestamp:0x1000005}})})}]}
}
#[test]fn prefix_waits_for_publication_without_waiting_for_hardware(){
 let f=[(1,pair(1)),(2,pair(2)),(3,pair(3))];let mut d=KVec(Vec::new());let mut c=KVec(Vec::new());
 append_render_prefix_dependencies(&f,Some(2),&mut d,&mut c).unwrap();
 assert_eq!(d.iter().map(|f|f.id).collect::<Vec<_>>(),vec![101,102]);
 assert_eq!(c.iter().map(|f|f.id).collect::<Vec<_>>(),vec![1,2]);
 for f in d.iter(){assert_eq!(f.status.load(Ordering::Acquire),1)}
 for f in c.iter(){assert_eq!(f.status.load(Ordering::Acquire),0);assert!(G17PQueue::check_firmware_fence(f).is_ok())}
 c[0].status.store(-5,Ordering::Release);assert_eq!(G17PQueue::check_firmware_fence(&c[0]),Err(EIO));
}
#[test]fn complete_live_render_prefix_encodes_both_physical_pairs(){
 let vm=Arc::new(g17_status::VmStatus);let a=render(&vm,41,7,0x1ffffff);let b=render(&vm,42,9,0x2000000);
 let da=a.compute_predecessor_dependency(7,&vm,Some(42),None).unwrap().unwrap();
 let db=b.compute_predecessor_dependency(7,&vm,Some(42),None).unwrap().unwrap();
 assert_eq!(g17_submission::native_word(da),g17_submission::encode_kick_word(0x1ffffff,7).unwrap());
 assert_eq!(g17_submission::native_word(db),g17_submission::encode_kick_word(0x2000000,9).unwrap());
 assert_eq!(b.compute_predecessor_dependency(7,&vm,Some(41),None),Ok(None));
 assert!(b.compute_predecessor_dependency(7,&vm,Some(41),Some(42)).unwrap().is_some());
}
#[test]fn render_owner_vm_failure_and_missing_token_are_checked(){
 let vm=Arc::new(g17_status::VmStatus);let other=Arc::new(g17_status::VmStatus);let a=render(&vm,1,7,3);
 assert_eq!(a.compute_predecessor_dependency(8,&vm,Some(1),None),Ok(None));
 assert_eq!(a.compute_predecessor_dependency(7,&other,Some(1),None),Ok(None));
 a.fragment_dependency.store(u64::MAX,Ordering::Release);assert!(a.compute_predecessor_dependency(7,&vm,Some(1),None).is_err());
 a.fence.status.store(1,Ordering::Release);assert_eq!(a.compute_predecessor_dependency(7,&vm,Some(1),None),Ok(None));
 a.fence.status.store(-125,Ordering::Release);assert!(a.compute_predecessor_dependency(7,&vm,Some(1),None).is_err());
}
#[test]fn compute_to_render_uses_published_stamp_not_consumer_seed(){
 let vm=Arc::new(g17_status::VmStatus);let mut rt=runtime(&vm);
 let d=rt.async_compute_native_dependency(7,&vm,true).unwrap().unwrap();
 assert_eq!(g17_submission::native_word(d),g17_submission::encode_kick_word(0x1000005,11).unwrap());
 // Both physical render pairs see the same live compute predecessor.
 assert_eq!(rt.async_compute_native_dependency(7,&vm,true).unwrap(),Some(d));
 rt.async_compute_queues[0].inflight=Inflight::new();assert_eq!(rt.async_compute_native_dependency(7,&vm,true),Ok(None));
}
#[test]fn barrier_none_adds_no_cross_engine_execution_dependency(){
 let vm=Arc::new(g17_status::VmStatus);let mut rt=runtime(&vm);
 // An unfinished compute kick must not hold up an independent render.
 assert_eq!(rt.async_compute_native_dependency(7,&vm,false),Ok(None));
 // No hardware observation or unrelated queue failure is needed in this case.
 rt.manager=None;
 assert_eq!(rt.async_compute_native_dependency(7,&vm,false),Ok(None));
 let a=render(&vm,42,9,0x2000000);
 assert_eq!(a.compute_predecessor_dependency(7,&vm,None,None),Ok(None));
 a.fence.status.store(-125,Ordering::Release);
 assert_eq!(a.compute_predecessor_dependency(7,&vm,None,None),Ok(None));
 // An explicitly requested prefix still observes the failed predecessor.
 assert!(a.compute_predecessor_dependency(7,&vm,Some(42),None).is_err());
}
#[test]fn compute_token_refuses_open_recovery_quarantine_and_cancellation(){
 let vm=Arc::new(g17_status::VmStatus);let mut rt=runtime(&vm);
 // a9502decf205a: a CLOSED recovery that moved the epoch leaves the
 // compute's native stamp meaningful; only an open one refuses.
 rt.manager.as_mut().unwrap().h.epoch=5;assert!(rt.async_compute_native_dependency(7,&vm,true).unwrap().is_some());
 rt.manager.as_mut().unwrap().h.epoch=4;rt.manager.as_mut().unwrap().h.state=1;assert!(rt.async_compute_native_dependency(7,&vm,true).is_err());
 rt.manager.as_mut().unwrap().h.state=0;rt.async_compute_queues[0].quarantined=true;assert!(rt.async_compute_native_dependency(7,&vm,true).is_err());
 rt.async_compute_queues[0].quarantined=false;rt.async_compute_queues[0].inflight.back().unwrap().packet.completion.fence.status.store(-125,Ordering::Release);
 assert!(rt.async_compute_native_dependency(7,&vm,true).is_err());
}
#[test]fn stacked_computes_give_renders_the_newest_kick_stamp(){
 // Two kicks of the queue in flight: a render must follow the newest, whose
 // implicit parent is the older one, so one native barrier covers both.
 let vm=Arc::new(g17_status::VmStatus);let mut rt=runtime(&vm);
 let newer=Active{packet:Packet{completion:ComputeCompletion{status:vm.clone(),fence:fence(2,0)},command:Command{compute_sequence:42}},retirement:Some(Ticket{recovery_epoch:4,firmware:FirmwareTicket{timestamp:0x1000006}})};
 if rt.async_compute_queues[0].inflight.push(newer).is_err(){unreachable!()}
 rt.async_compute_queues[0].queue.producer.submitted=2;rt.async_compute_queues[0].queue.producer.last_add_kicks_timestamp=0x1000006;
 let d=rt.async_compute_native_dependency(7,&vm,true).unwrap().unwrap();
 assert_eq!(g17_submission::native_word(d),g17_submission::encode_kick_word(0x1000006,11).unwrap());
 // Retirement is in order: once the newest fence has signalled, every older one has too.
 rt.async_compute_queues[0].inflight.back().unwrap().packet.completion.fence.status.store(1,Ordering::Release);
 assert_eq!(rt.async_compute_native_dependency(7,&vm,true),Ok(None));
}
#[test]fn compute_token_requires_exact_publication_and_vm(){
 let vm=Arc::new(g17_status::VmStatus);let mut rt=runtime(&vm);
 assert_eq!(rt.async_compute_native_dependency(8,&vm,true),Ok(None));
 assert!(rt.async_compute_native_dependency(7,&Arc::new(g17_status::VmStatus),true).is_err());
 // Selection uses the exact ticket, not the mutable latest-producer stamp.
 rt.async_compute_queues[0].queue.producer.previous_valid=false;assert!(rt.async_compute_native_dependency(7,&vm,true).is_ok());
 rt.async_compute_queues[0].queue.producer.previous_valid=true;rt.async_compute_queues[0].inflight.back_mut().unwrap().retirement=None;
 assert!(rt.async_compute_native_dependency(7,&vm,true).is_err());
}
'''

# Existing all-prefix cases plus exact indexed-prefix and NONE behavior.
program = program.replace('&vm,true)', '&vm,Some(42))').replace('&vm,false)', '&vm,None)')
program = program.replace('VmStatus),true)', 'VmStatus),Some(42))')
program += r'''
#[test]fn explicit_compute_prefix_excludes_later_kicks(){
 let vm=Arc::new(g17_status::VmStatus);let mut rt=runtime(&vm);
 let newer=Active{packet:Packet{completion:ComputeCompletion{status:vm.clone(),fence:fence(2,-125)},command:Command{compute_sequence:42}},retirement:Some(Ticket{recovery_epoch:4,firmware:FirmwareTicket{timestamp:0x1000006}})};
 if rt.async_compute_queues[0].inflight.push(newer).is_err(){unreachable!()}
 // A later failed kick is outside this render's requested dependency prefix.
 let d=rt.async_compute_native_dependency(7,&vm,Some(41)).unwrap().unwrap();
 assert_eq!(g17_submission::native_word(d),g17_submission::encode_kick_word(0x1000005,11).unwrap());
 assert_eq!(rt.async_compute_native_dependency(7,&vm,Some(40)),Ok(None));
 assert!(rt.async_compute_native_dependency(7,&vm,Some(42)).is_err());
}
#[test]fn compute_prefix_index_is_snapshot_relative_and_rejects_future_and_overflow(){
 use g17_submission::g17p_render_order as order;
 assert_eq!(order(100,3,0).unwrap().wait_through,Some(100));
 assert_eq!(order(100,3,2).unwrap().wait_through,Some(102));
 assert_eq!(order(100,3,u16::MAX).unwrap().wait_through,None);
 assert!(order(100,3,4).is_err());
 assert!(order(u64::MAX,0,0).is_err());
}
fn dep(q:u8,t:u64)->g17_submission::G17PClBarrierDependency{
 let mut d=g17_submission::g17p_render_barrier_from_timestamp(t,q).unwrap();
 d.native_timestamp=Some(t);d
}
#[test]fn full_depth_prefix_collapses_without_reducing_inflight_depth(){
 use g17_submission::g17p_insert_prefix_dependency as insert;
 let mut d=[dep(0,0);31];let mut seq=[0;31];let mut n=0;
 // The pending and queued tables need not be in logical sequence order.
 for s in (1..=64).rev(){insert(&mut d,&mut seq,&mut n,s,dep((s%2) as u8+7,s)).unwrap();}
 assert_eq!(n,2);
 for i in 0..n{assert_eq!(seq[i],if d[i].queue_id==7{64}else{63});}
 // Logical order selects across a 40-bit firmware timestamp wrap.
 insert(&mut d,&mut seq,&mut n,65,dep(8,0)).unwrap();
 let i=d[..n].iter().position(|x|x.queue_id==8).unwrap();
 assert_eq!(seq[i],65);assert_eq!(d[i].native_timestamp,Some(0));
}
#[test]fn distinct_qids_remain_distinct_and_exhaustion_is_transactional(){
 use g17_submission::g17p_insert_prefix_dependency as insert;
 let mut d=[dep(0,0);31];let mut seq=[0;31];let mut n=0;
 for q in 0..31{insert(&mut d,&mut seq,&mut n,q as u64+1,dep(q,1)).unwrap();}
 let before=d;
 assert!(insert(&mut d,&mut seq,&mut n,32,dep(31,2)).is_err());
 assert_eq!(n,31);assert_eq!(d,before);
 // An existing QID can still advance at capacity.
 insert(&mut d,&mut seq,&mut n,33,dep(0,3)).unwrap();
 assert_eq!(n,31);assert_eq!(d[0],dep(0,3));
}
'''

# The kernel scheduled fence is signalled only after native publish returns.
sched = (root.parent / 'scheduler/sched_main.c').read_text()
assert sched.index('fence = sched->ops->run_job(sched_job);') < sched.index('drm_sched_fence_scheduled(s_fence, fence);')
assert job.count('let published = job.fences().scheduled();') == 2
assert job.count('Ok(G17PCommandFences { ready: published, completed: hardware })') == 2
assert 'barriers: &dependencies[..dependency_count]' in comp
assert 'packet.command().render_wait_through, packet.command().render_wait_last' in comp
assert 'completion.owner(), &completion.vm_status(),' in live
assert 'command.compute_wait_through' in live
assert 'translated.render_wait_last = Some(' not in drm
assert '&self.compute_frontier, translated.compute_wait_through' in drm
assert 'get_mut(dependency_count).ok_or(EOVERFLOW)?' in live
for name in ['submit_compute_async', 'submit_render_async']:
    body = block(drm, '    fn '+name+'(')
    assert 'last_publication' not in body
    assert 'append_render_prefix_dependencies(' in body
    assert 'checked_dependencies.push(prior.clone()' not in body
assert 'dma_fence_wait_timeout' not in block(drm, '    fn check_firmware_fence(')
assert 'MAX_IN_FLIGHT_PER_QUEUE: u32 = 2' in (root / 'g17_queue_limits.rs').read_text()

with tempfile.TemporaryDirectory() as directory:
    src = Path(directory) / 'cross.rs'
    exe = Path(directory) / 'cross'
    src.write_text(program)
    subprocess.run(['rustc', '--edition=2021', '--test', str(src), '-o', str(exe)], check=True)
    subprocess.run([str(exe), '--test-threads=1'], check=True, timeout=60)
print('PASS actual cross-engine publication, native token, owner/epoch/error helpers; no hardware claim')
