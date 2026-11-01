#!/usr/bin/env python3
"""Execute cancellation/completion methods with observable ownership stand-ins.

The driver methods are extracted verbatim. Hardware retirement is intentionally
not simulated as a fence signal: tests supply that independent witness later.
"""
from pathlib import Path
from g17_compute_inflight_test_support import completion_mod, queue_limits_mock
import subprocess
import tempfile
import sys
from g17_recovery_isolation_test_support import HOST_PATH_V93, recovery_isolation_mod

root = Path(__file__).resolve().parents[1]


def item(text, marker):
    start = text.index(marker)
    pos = text.index("{", start) + 1
    depth = 1
    while depth:
        depth += (text[pos] == "{") - (text[pos] == "}")
        pos += 1
    return text[start:pos]


job = (root / "g17_job.rs").read_text()
compute = (root / "g17_async_compute.rs").read_text()
drm = (root / "g17_drm.rs").read_text()
cancel = item(compute, "    pub(crate) fn cancel_async_compute(")
quarantine = item(compute, "    pub(crate) fn quarantine_async_compute(")
service = item(compute, "    fn service_async_compute_completions_matching(")
poll = item((root / "g17_manager.rs").read_text(),
            "    pub(crate) fn poll_staged_independent_compute_completion(")

# Preserve the independent retirement proof and its placement before pin release.
assert service.index("poll_staged_independent_compute_completion(") < service.index("release_after_retirement()")
assert service.index("drop(q.previous_binding.take())") < service.index("release_after_retirement()")
# A torn-down queue (its watchdog is gone) is examined on every event batch.
assert "q.retired_by_teardown || q.retire_pending || masks.is_none_or(" in service
assert service.count("|| !selected(q)") == 2
manager_src = (root / "g17_manager.rs").read_text()
finished = item(manager_src, "    pub(crate) fn independent_compute_kick_finished(")
# In-order multi-kick retirement: the kick's own per-kick stamp value, its
# own target and item-ring records; a latched record or, without a recovery
# since publication, the firmware counters decide execution.
for guard in ("g17_compute::completion_stamp_covers(stamp, ticket.kick_timestamp)",
              "self.independent_compute_kick_finished(queue, ticket)?",
              "None if recovery_quiet =>", "None if !crossed_recovery => return Ok(None),",
              "if observed.is_some() {\n            queue.completion_last_end"):
    assert guard in poll, guard
# Shared TSQ counts include other engines' completions (test_g17_tsq_accounting),
# so only the kick's own item-ring position proves its in-order retirement.
assert "unconsumed <= published_after" in finished
assert "submitted_target" not in finished
assert "handshake.epoch != ticket.recovery_epoch" in service
assert "q.retired_by_teardown = false;\n                        q.quarantined = true;" in service
assert "queued._lease.quarantine();" in quarantine
assert "return self.cancel_async_compute(owner, submission);" in quarantine
assert "fail_outstanding_jobs_for_vm" not in cancel
assert "_lease.quarantine" not in cancel
assert ".cancel_async_compute(self.owner, packet.submission_id());" in drm

methods = "\n".join(item(job, "    pub(crate) fn " + name + "(") for name in
                    ("cancel_while_owned", "release_after_retirement", "fail_while_owned", "complete",
                     "recovery_spared"))
# Recovery isolation (iso1): spared zero-duration queues fail their fences here.
spared_helper = item(compute, "fn fail_spared_deferred(").replace("G17PAsyncComputeQueue", "Queue")
if "--negative-control" in sys.argv:
    # Reproduce the former sticky-VM-poison half of hardware quarantine.
    methods = methods.replace("self.fence.set_error(ECANCELED);",
                              "self.status.record(-125); self.fence.set_error(ECANCELED);")

source = r'''
#![allow(dead_code,unused_variables)]
use std::sync::{Arc,Mutex as StdMutex};
use std::sync::atomic::{AtomicU32,AtomicI32,AtomicUsize,AtomicBool,Ordering};
#[derive(Copy,Clone,PartialEq,Debug)] struct Error(i32);
impl Error {fn to_errno(self)->i32{self.0}}
const ECANCELED:Error=Error(-125); const EIO:Error=Error(-5);
type Result<T=()> = std::result::Result<T,Error>;
struct Mutex<T>(StdMutex<T>);
impl<T> Mutex<T>{fn lock(&self)->std::sync::MutexGuard<'_,T>{self.0.lock().unwrap()}}
#[derive(Default)] struct Memory {active:AtomicUsize,blocked:AtomicBool,closed:AtomicBool,unmapped:AtomicBool}
struct Pin(Arc<Memory>);
impl Pin {fn quarantine(&self){self.0.blocked.store(true,Ordering::SeqCst);}}
impl Drop for Pin {fn drop(&mut self){if self.0.active.fetch_sub(1,Ordering::SeqCst)==1 {
 self.0.blocked.store(false,Ordering::SeqCst);
 if self.0.closed.load(Ordering::SeqCst){self.0.unmapped.store(true,Ordering::SeqCst);}
}}}
#[derive(Default)] struct Status(AtomicI32);
impl Status {
 fn record(&self,e:i32){let _=self.0.compare_exchange(0,e,Ordering::AcqRel,Ordering::Acquire);}
 fn record_if_device_loss(&self,e:i32){if e != -61 {self.record(e)}}
}
#[derive(Default)] struct Fence {error:AtomicI32,signals:AtomicUsize}
impl Fence {fn set_error(&self,e:Error){self.error.store(e.0,Ordering::SeqCst);}
 fn signal(&self){self.signals.fetch_add(1,Ordering::SeqCst);}}
struct Destination(Arc<AtomicUsize>);
impl Destination {fn write(&self,_v:u64)->Result {self.0.fetch_add(1,Ordering::SeqCst);Ok(())}}
#[derive(Default)] struct Aggregate {calls:AtomicUsize,error:AtomicI32}
impl Aggregate {fn complete(&self,r:Result){self.calls.fetch_add(1,Ordering::SeqCst);if let Err(e)=r{self.error.store(e.0,Ordering::SeqCst);}}}
struct Completion {signalled:AtomicU32,vm_job:Mutex<Option<Pin>>,status:Status,fence:Fence,aggregate_member:Aggregate,destinations:[Option<Destination>;2],recovery_class:std::sync::atomic::AtomicU8}
mod g17_host_path {'''+HOST_PATH_V93+r'''}
'''+recovery_isolation_mod()+r'''
impl Completion {
'''+methods+r'''
}
struct Packet {id:u64,completion:Arc<Completion>}
impl Packet {fn submission_id(&self)->u64{self.id} fn completion(&self)->Result<&Completion>{Ok(&self.completion)}}
struct Active {packet:Packet}
struct HardwareQueue;
impl HardwareQueue {fn queue_id(&self)->u8{11}}
'''+completion_mod()+queue_limits_mock()+r'''
type Inflight=g17_completion::PairFifo<Active,{g17_queue_limits::MAX_COMPUTE_IN_FLIGHT_PER_QUEUE as usize}>;
struct Queue {owner:Option<u64>,queue:HardwareQueue,inflight:Inflight,quarantined:bool,released:bool,retired_by_teardown:bool,spared_deferred:bool}
'''+spared_helper+r'''
struct Device;
impl Device {fn as_ref(&self)->&Self{self}}
macro_rules! dev_info {($($t:tt)*)=>{()}}
struct Runtime {async_compute_queues:Vec<Queue>,pdev:Option<Device>,retire_in_final_scan:bool,scans:usize}
impl Runtime {
 fn service_async_compute_completions(&mut self)->Result{
   self.scans+=1;
   if self.retire_in_final_scan {
     if let Some(active)=self.async_compute_queues[0].inflight.take(){active.packet.completion()?.complete(Ok([1,2]));}
   }
   Ok(())
 }
'''+cancel+r'''
}
fn setup()->(Runtime,Arc<Completion>,Arc<Memory>,Arc<AtomicUsize>){
 let m=Arc::new(Memory::default());m.active.store(1,Ordering::SeqCst);
 let writes=Arc::new(AtomicUsize::new(0));
 let c=Arc::new(Completion{signalled:AtomicU32::new(0),vm_job:Mutex(StdMutex::new(Some(Pin(m.clone())))),status:Status::default(),fence:Fence::default(),aggregate_member:Aggregate::default(),destinations:[Some(Destination(writes.clone())),None],recovery_class:Default::default()});
 let mut inflight=Inflight::new();if inflight.push(Active{packet:Packet{id:28522,completion:c.clone()}}).is_err(){unreachable!()}
 let q=Queue{owner:Some(10),queue:HardwareQueue,inflight,quarantined:false,released:false,retired_by_teardown:false,spared_deferred:false};
 (Runtime{async_compute_queues:vec![q],pdev:None,retire_in_final_scan:false,scans:0},c,m,writes)
}
#[test] fn cancellation_keeps_maps_and_fails_only_waiter(){
 let(mut r,c,m,writes)=setup();assert!(r.cancel_async_compute(10,28522));
 assert_eq!(c.fence.error.load(Ordering::SeqCst),-125);assert_eq!(c.fence.signals.load(Ordering::SeqCst),1);
 assert_eq!(c.status.0.load(Ordering::SeqCst),0);assert!(m.blocked.load(Ordering::SeqCst));
 assert_eq!(m.active.load(Ordering::SeqCst),1);assert!(!r.async_compute_queues[0].quarantined);
 assert!(r.async_compute_queues[0].released);assert!(r.async_compute_queues[0].inflight.is_some());
 m.closed.store(true,Ordering::SeqCst);assert!(!m.unmapped.load(Ordering::SeqCst));
 assert_eq!(writes.load(Ordering::SeqCst),0);
}
#[test] fn exact_retirement_releases_cancelled_pin_without_success_rewrite(){
 let(mut r,c,m,writes)=setup();r.cancel_async_compute(10,28522);m.closed.store(true,Ordering::SeqCst);
 c.complete(Ok([3,4]));assert_eq!(m.active.load(Ordering::SeqCst),1,"error fence alone is not proof");
 c.release_after_retirement();c.complete(Ok([3,4]));
 assert!(m.unmapped.load(Ordering::SeqCst));assert_eq!(c.fence.error.load(Ordering::SeqCst),-125);
 assert_eq!(c.fence.signals.load(Ordering::SeqCst),1);assert_eq!(writes.load(Ordering::SeqCst),0);
 c.release_after_retirement();assert_eq!(m.active.load(Ordering::SeqCst),0);
}
#[test] fn stale_cancel_cannot_touch_another_submission(){
 let(mut r,c,m,_)=setup();assert!(!r.cancel_async_compute(9,28522));assert!(!r.cancel_async_compute(10,28521));
 assert_eq!(c.fence.signals.load(Ordering::SeqCst),0);assert!(!m.blocked.load(Ordering::SeqCst));
}
#[test] fn final_scan_success_wins_over_cancellation(){
 let(mut r,c,m,writes)=setup();r.retire_in_final_scan=true;
 assert!(!r.cancel_async_compute(10,28522));assert_eq!(c.fence.error.load(Ordering::SeqCst),0);
 assert_eq!(m.active.load(Ordering::SeqCst),0);assert_eq!(writes.load(Ordering::SeqCst),1);
}
#[test] fn cancellation_is_idempotent(){
 let(mut r,c,m,_)=setup();assert!(r.cancel_async_compute(10,28522));assert!(r.cancel_async_compute(10,28522));
 assert_eq!(c.fence.signals.load(Ordering::SeqCst),1);assert_eq!(m.active.load(Ordering::SeqCst),1);
 assert_eq!(c.aggregate_member.calls.load(Ordering::SeqCst),1);assert_eq!(c.aggregate_member.error.load(Ordering::SeqCst),-125);
 c.release_after_retirement();c.complete(Ok([3,4]));assert_eq!(c.aggregate_member.calls.load(Ordering::SeqCst),1);
}
#[test] fn unpublished_cancel_releases_pin_without_vm_poison(){
 let(_,c,m,writes)=setup();c.complete(Err(ECANCELED));
 assert_eq!(m.active.load(Ordering::SeqCst),0);assert_eq!(c.status.0.load(Ordering::SeqCst),0);
 assert_eq!(c.fence.error.load(Ordering::SeqCst),-125);assert_eq!(writes.load(Ordering::SeqCst),0);
}
#[test] fn hardware_failure_still_poisoned_and_pinned(){
 let(mut r,c,m,_)=setup();c.fail_while_owned(EIO);r.async_compute_queues[0].quarantined=true;
 assert!(r.cancel_async_compute(10,28522));assert_eq!(c.status.0.load(Ordering::SeqCst),-5);
 assert_eq!(c.fence.error.load(Ordering::SeqCst),-5);assert_eq!(m.active.load(Ordering::SeqCst),1);
 assert!(r.async_compute_queues[0].quarantined);assert!(!r.async_compute_queues[0].retired_by_teardown);
}
'''
with tempfile.TemporaryDirectory() as directory:
    src, binary = Path(directory) / "cancel.rs", Path(directory) / "cancel"
    src.write_text(source)
    subprocess.run(["rustc", "--edition=2021", "--test", str(src), "-o", str(binary)], check=True)
    result = subprocess.run([str(binary)])
    if "--negative-control" in sys.argv:
        assert result.returncode != 0, "former VM poison unexpectedly passed"
        print("PASS: negative control rejects cancellation VM poison")
    else:
        result.check_returncode()
        print("PASS: actual cancellation methods retain ownership until independent retirement")
