#!/usr/bin/env python3
"""Execute stable render-owner admission, contention and stopped reclamation."""
from pathlib import Path
import subprocess, tempfile

s = (Path(__file__).resolve().parents[1] / 'g17_drm.rs').read_text()


def item(needle):
    a = s.index(needle)
    i = s.index('{', a) + 1
    depth = 1
    while depth:
        depth += (s[i] == '{') - (s[i] == '}')
        i += 1
    return s[a:i]


slots = s[s.index('const G17P_RENDER_SLOTS: usize ='):]
slots = slots[:slots.index('const _: () = assert!(G17P_ACTIVE_RENDER_SLOTS > 0);')]
# Exercise the production owner capacity.
assert 'const G17P_ACTIVE_RENDER_SLOTS: usize = G17P_RENDER_SLOTS;' in slots
state = '\n'.join(item(n) for n in [
    'struct G17PQueueLeaseState {',
    'impl Default for G17PQueueLeaseState {',
    'impl G17PQueueLeaseState {',
])
methods = '\n'.join(item(n) for n in [
    '    fn acquire_lease(',
    '    fn release_render_owner(',
    '    pub(crate) fn release_retained_owners_after_processor_stop(',
    '    fn quarantine_render_slot(',
    '    fn rehabilitate_render_slot_after_processor_stop(',
    '    fn compute_poison_is_device_wide(',
])
render_backend = item('impl g17_job::G17PRenderQueueBackend for G17PRenderQueueBackend {')
assert 'self.registry.acquire_lease(self.owner)?' in render_backend
assert 'self.vm.id()' not in render_backend
guards = '\n'.join(item(n) for n in [
    'struct G17PLeaseGuard {',
    'impl G17PLeaseGuard {',
    'impl Drop for G17PLeaseGuard {',
])

# Releasing a frame lease must preserve the queue's retained-slot affinity.
queue_drop = item('impl Drop for G17PQueue {')
assert 'release_render_claim' not in queue_drop, \
    'destroying one logical queue unexpectedly clears registry affinity'
assert queue_drop.index(
    '.release_render_owner(self.id, &self.context)'
) < queue_drop.index('self.registry.release_render_owner(self.id)'), \
    'registry exposed a released physical pair before manager context retirement'

manager = (Path(__file__).resolve().parents[1] / 'g17_manager.rs').read_text()
handoff_start = manager.index('pub(crate) fn handoff_retained_render_vm(')
handoff = manager[handoff_start:manager.index('    /// Close one logical queue', handoff_start)]
assert 'execution_context: Arc<crate::g17_resources::G17PLogicalQueueContext>' in handoff
assert 'u16::try_from(execution_context.context_id())' in handoff
assert 'retarget_retained_render_context(pair_slot, vm, execution_context.clone())' in handoff
assert 'reserve_logical_queue_teardown_graph(' in handoff
assert 'G17PRetiredRenderGraph {' in handoff

# The lease is what decides which retained graph a submission lands on, so the
# real methods are compiled; everything below them is host-side scaffolding.
preamble = r'''
#![allow(dead_code)]
#![allow(unused_variables)]
use std::sync::{Arc,atomic::{AtomicU32,Ordering}};
use std::ops::{Deref,DerefMut};
use std::time::Duration;
type Result<T=()>=std::result::Result<T,i32>;
const EINVAL:i32=22;const ENOSPC:i32=28;const EIO:i32=5;const EBUSY:i32=16;const ERESTARTSYS:i32=512;
macro_rules! pr_warn{($($t:tt)*)=>{{}}}
#[derive(Default)]struct G17PComputeBurstState;
mod g17_resources { pub const G17P_RENDER_MAX_IN_FLIGHT:usize=16; pub struct G17PRenderQueuePair; impl G17PRenderQueuePair { pub const SLOT_COUNT:u8=64; } }
mod g17_queue_limits { pub const MAX_IN_FLIGHT_PER_QUEUE:u32=2; pub const RENDER_KICKS_PER_PAIR:u32=crate::g17_resources::G17P_RENDER_MAX_IN_FLIGHT as u32; }
mod g17_config {pub const QUARANTINE_SCOPE:u32=3;}
struct Mutex<T>(std::sync::Mutex<T>);
struct Guard<'a,T>(Option<std::sync::MutexGuard<'a,T>>);
impl<T>Mutex<T>{fn lock(&self)->Guard<'_,T>{Guard(Some(self.0.lock().unwrap()))}}
impl<T>Deref for Guard<'_,T>{type Target=T;fn deref(&self)->&T{self.0.as_ref().unwrap()}}
impl<T>DerefMut for Guard<'_,T>{fn deref_mut(&mut self)->&mut T{self.0.as_mut().unwrap()}}
struct CondVar(std::sync::Condvar);
impl CondVar{
 fn wait_interruptible<T>(&self,g:&mut Guard<'_,T>)->bool{
  let(inner,t)=self.0.wait_timeout(g.0.take().unwrap(),Duration::from_secs(2)).unwrap();
  g.0=Some(inner);t.timed_out()
 }
 fn notify_all(&self){self.0.notify_all()}
}
'''+slots+r'''
'''+state+r'''
struct G17PQueueRegistry{lease:Mutex<G17PQueueLeaseState>,lease_free:CondVar,compute_faulted:AtomicU32}
impl G17PQueueRegistry{
 fn new()->Arc<Self>{Arc::new(Self{
  lease:Mutex(std::sync::Mutex::new(G17PQueueLeaseState::default())),
  lease_free:CondVar(std::sync::Condvar::new()),
  compute_faulted:AtomicU32::new(0)})}
'''+methods+r'''
}
'''+guards+r'''

/// Which logical queue's retained render storage each slot holds. Zero means the
/// slot has never built one.
struct RetainedStorage{owner:[u64;G17P_RENDER_SLOTS],rebuilds:u32}
impl Default for RetainedStorage{fn default()->Self{Self{owner:[0;G17P_RENDER_SLOTS],rebuilds:0}}}
impl RetainedStorage{
 /// One submission. Returns true when the incoming VM had to take over another
 /// client's slot, which in the driver means the whole graph is destroyed and
 /// rebuilt in the incoming VM before this frame can be staged.
 fn submit(&mut self,slot:u8,queue:u64)->bool{
  let previous=self.owner[slot as usize];
  self.owner[slot as usize]=queue;
  if previous!=0&&previous!=queue{self.rebuilds+=1;return true}
  false
 }
}

/// Historical policy: take the lowest free slot regardless of who built the
/// graph on it. Kept here so the counts below are shown to discriminate.
fn acquire_lowest_free(state:&mut G17PQueueLeaseState,owner:u64)->Option<u8>{
 let free=(!state.render_slots)&G17PQueueLeaseState::render_mask();
 if free==0{return None}
 let slot=free.trailing_zeros() as u8;
 Some(state.take_render_slot(slot,owner))
}
'''

tests = r'''
/// Two clients alternating frames, exactly the GNOME-plus-vkcube shape.
#[test]fn two_alternating_clients_keep_their_own_retained_storage(){
 let r=G17PQueueRegistry::new();
 let mut storage=RetainedStorage::default();
 let mut slots=Vec::new();
 for frame in 0..32u64{
  let queue=if frame%2==0{11}else{22};
  let lease=r.acquire_lease(queue).unwrap();
  storage.submit(lease.slot(),queue);
  slots.push((queue,lease.slot()));
  drop(lease);
 }
 // The first frame of each client claims a slot; after that nothing may move.
 assert_eq!(storage.rebuilds,0,"a client took over another client's retained graph");
 let a:Vec<u8>=slots.iter().filter(|(vm,_)|*vm==11).map(|(_,s)|*s).collect();
 let b:Vec<u8>=slots.iter().filter(|(vm,_)|*vm==22).map(|(_,s)|*s).collect();
 assert!(a.windows(2).all(|w|w[0]==w[1]),"client A moved between slots: {:?}",a);
 assert!(b.windows(2).all(|w|w[0]==w[1]),"client B moved between slots: {:?}",b);
 assert_ne!(a[0],b[0],"both clients were given the same slot");
}

/// The same traffic under the old policy rebuilds a graph on every alternation.
/// Without this the assertion above could pass for the wrong reason.
#[test]fn lowest_free_slot_policy_rebuilds_on_every_alternation(){
 let mut state=G17PQueueLeaseState::default();
 let mut storage=RetainedStorage::default();
 for frame in 0..32u64{
  let vm=if frame%2==0{11}else{22};
  let slot=acquire_lowest_free(&mut state,vm).unwrap();
  storage.submit(slot,vm);
  state.release_render_slot(slot);
 }
 assert!(storage.rebuilds>=30,"expected the old policy to thrash, saw {}",storage.rebuilds);
}

/// Overlapping submissions: while A holds its slot, B must be given the other
/// one rather than waiting or failing.
#[test]fn a_second_client_runs_concurrently_on_its_own_slot(){
 let r=G17PQueueRegistry::new();
 let a=r.acquire_lease(11).unwrap();
 let b=r.acquire_lease(22).unwrap();
 assert_ne!(a.slot(),b.slot());
 drop(b);
 // B comes back to the slot holding its own graph even though A's is free.
 let b=r.acquire_lease(22).unwrap();
 assert_ne!(a.slot(),b.slot());
 drop(b);drop(a);
}

/// Two queues in one VM own different descriptor contexts and therefore cannot
/// share one retained graph while both physical pairs are available.
#[test]fn one_vm_two_queues_do_not_share_a_graph(){
 let r=G17PQueueRegistry::new();
 let first=r.acquire_lease(11).unwrap();
 let second=r.acquire_lease(12).unwrap();
 assert_ne!(first.slot(),second.slot());
}

#[test]fn same_owner_admits_full_depth_on_distinct_pairs_then_waits(){
 let r=G17PQueueRegistry::new();let a=r.acquire_lease(11).unwrap();let b=r.acquire_lease(11).unwrap();let slot=a.slot();
 assert_ne!(b.slot(),slot,"two admitted renders reused one physical QID pair");
 let rest:Vec<_>=(2..2*g17_resources::G17P_RENDER_MAX_IN_FLIGHT).map(|_|r.acquire_lease(11).unwrap()).collect();
 let(tx,rx)=std::sync::mpsc::channel();let rr=r.clone();
 let worker=std::thread::spawn(move||{tx.send(rr.acquire_lease(11).map(|l|l.slot())).unwrap();});
 assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
 let other=r.acquire_lease(22).unwrap();assert_ne!(other.slot(),slot);assert_ne!(other.slot(),b.slot());
 drop(a);assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(),Ok(slot));worker.join().unwrap();drop(b);drop(rest);
}

#[test]fn same_owner_rotates_distinct_pairs_for_more_than_ten_thousand_turns(){
 let r=G17PQueueRegistry::new();
 for turn in 0..12_288u32{
  let a=r.acquire_lease(11).unwrap();let b=r.acquire_lease(11).unwrap();
  assert_ne!(a.slot(),b.slot(),"same physical pair at turn {}",turn);
  let held=b.slot();drop(a);
  let c=r.acquire_lease(11).unwrap();
  assert_ne!(c.slot(),held,"active physical pair reused at turn {}",turn);
  drop(c);drop(b);
 }
 let held=r.lease.lock();
 assert_eq!(held.render_owner.iter().filter(|id|**id==11).count(),2);
 assert_eq!(held.render_refs.iter().map(|v|*v as u32).sum::<u32>(),0);
}
#[test]fn idle_owners_are_not_evicted_and_capacity_is_reported(){
 let r=G17PQueueRegistry::new();
 let mut leases=Vec::new();let mut slots=Vec::new();
 for owner in 1..=G17P_ACTIVE_RENDER_SLOTS as u64 {
  match r.acquire_lease(owner){Ok(lease)=>{slots.push((owner,lease.slot()));leases.push(lease);},Err(error)=>{assert_eq!(error,ENOSPC);break;}}
 }
 // Creation claims two physical pairs where available, including the idle sibling.
 let pairs=g17_queue_limits::MAX_IN_FLIGHT_PER_QUEUE as usize;
 assert_eq!(slots.len(),(G17P_ACTIVE_RENDER_SLOTS-1+pairs-1)/pairs);
 assert_eq!(r.lease.lock().render_owner.iter().filter(|id|**id!=0).count(),G17P_ACTIVE_RENDER_SLOTS-1);
 drop(leases);
 for (owner,slot) in slots.iter().rev(){assert_eq!(r.acquire_lease(*owner).unwrap().slot(),*slot);}
 assert_eq!(r.acquire_lease(10000).err(),Some(ENOSPC));
 assert_eq!(r.acquire_lease(0).err(),Some(EINVAL));
}
#[test]fn a_quarantined_owner_cannot_migrate_to_an_empty_slot(){
 let r=G17PQueueRegistry::new();let first=r.acquire_lease(11).unwrap();let retired=first.slot();
 drop(first);r.quarantine_render_slot(retired);
 assert_eq!(r.acquire_lease(11).err(),Some(EIO));
 assert_ne!(r.acquire_lease(22).unwrap().slot(),retired);
}
#[test]fn closed_idle_owner_is_replaced_but_active_release_waits_for_completion(){
 let r=G17PQueueRegistry::new();let first=r.acquire_lease(11).unwrap();let slot=first.slot();
 r.release_render_owner(11);
 let mut concurrent=Vec::new();
 for offset in 0..G17P_ACTIVE_RENDER_SLOTS{
  match r.acquire_lease(22+offset as u64){Ok(lease)=>{assert_ne!(lease.slot(),slot);concurrent.push(lease);},Err(error)=>{assert_eq!(error,ENOSPC);break;}}
 }
 drop(first);
 let replacement=r.acquire_lease(10_000).unwrap();assert_eq!(replacement.slot(),slot);
 assert_eq!(r.lease.lock().render_owner[slot as usize],10_000);
 assert_eq!(r.lease.lock().render_released&(1u64<<slot),0);
 drop(replacement);drop(concurrent);
}

#[test]fn retired_slot_survives_more_than_ten_thousand_short_lived_owners(){
 let r=G17PQueueRegistry::new();let mut seen=[false;G17P_RENDER_SLOTS];
 for owner in 1..=12_288u64{
  let lease=r.acquire_lease(owner).unwrap();
  let slot=lease.slot();assert!(slot>0);seen[slot as usize]=true;
  r.release_render_owner(owner);drop(lease);
  let held=r.lease.lock();assert_eq!(held.render_slots&(1u64<<slot),0);
  assert_ne!(held.render_released&(1u64<<slot),0);
 }
 assert_eq!(seen[1..].iter().filter(|used|**used).count(),1);
 assert!(r.acquire_lease(12_289).is_ok());
}

/// Registry turnover reuses released slots. This does not model or claim
/// anything about firmware acknowledgement or the VM graph's lifetime.
#[test]fn three_client_turnover_reuses_released_slots(){
 let r=G17PQueueRegistry::new();let mut seen=std::collections::BTreeSet::new();
 for round in 0..4001u64{
  let mut leases=Vec::new();
  for worker in 0..3u64{
   let owner=1+round*3+worker;let lease=r.acquire_lease(owner).unwrap();
   let slot=lease.slot() as usize;
   seen.insert(slot);
   leases.push((owner,lease));
  }
  assert_eq!(seen.len(),3);
  for (owner,lease) in leases{r.release_render_owner(owner);drop(lease);}
 }
 assert_eq!(r.lease.lock().render_refs.iter().sum::<u8>(),0);
}

/// Releasing a frame must not erase its queue's retained ownership.
#[test]fn releasing_one_frame_does_not_forget_the_queue_s_storage(){
 let r=G17PQueueRegistry::new();
 let mut storage=RetainedStorage::default();
 let owned={let l=r.acquire_lease(11).unwrap();storage.submit(l.slot(),11);l.slot()};
 let other={let l=r.acquire_lease(22).unwrap();storage.submit(l.slot(),22);l.slot()};
 assert_ne!(owned,other);
 let next_frame=r.acquire_lease(11).unwrap();
 assert_eq!(next_frame.slot(),owned);
 assert!(!storage.submit(next_frame.slot(),11));
}

#[test]fn quarantine_wakes_a_same_owner_waiter(){
 let r=G17PQueueRegistry::new();let first=r.acquire_lease(11).unwrap();let second=r.acquire_lease(11).unwrap();let slot=first.slot();
 let rest:Vec<_>=(2..2*g17_resources::G17P_RENDER_MAX_IN_FLIGHT).map(|_|r.acquire_lease(11).unwrap()).collect();
 let(tx,rx)=std::sync::mpsc::channel();let rr=r.clone();
 let worker=std::thread::spawn(move||{tx.send(rr.acquire_lease(11).err()).unwrap();});
 assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());r.quarantine_render_slot(slot);
 assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(),Some(EIO));worker.join().unwrap();drop(second);drop(rest);
}

/// A proven processor stop discards the abandoned hardware graph. Clear its
/// quarantine while its old completion still holds the lease, then make the
/// slot available exactly when that lease drops.
#[test]fn processor_stop_rehabilitates_a_quarantined_slot(){
 let r=G17PQueueRegistry::new();
 let a=r.acquire_lease(11).unwrap();
 let b=r.acquire_lease(22).unwrap();
 let slot=a.slot();
 r.quarantine_render_slot(slot);
 r.rehabilitate_render_slot_after_processor_stop(slot);
 drop(a);
 let recovered=r.acquire_lease(11).unwrap();
 assert_eq!(recovered.slot(),slot);
 drop(recovered);drop(b);
}
'''

code = preamble + tests
with tempfile.TemporaryDirectory() as d:
    d = Path(d)
    f = d / 'affinity.rs'
    f.write_text(code)
    exe = d / 'affinity'
    subprocess.run(['rustc', '--edition=2021', '--test', str(f), '-o', str(exe)], check=True)
    subprocess.run([str(exe), '--test-threads=1'], check=True, timeout=60)
print('PASS stable render ownership, independent concurrency, exhaustion and stopped reclamation')
