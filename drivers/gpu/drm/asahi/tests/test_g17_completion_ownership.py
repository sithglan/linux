#!/usr/bin/env python3
"""Exercise actual completion methods: error notification is not GPU retirement."""
from pathlib import Path
import subprocess
import sys
import tempfile
from g17_recovery_isolation_test_support import HOST_PATH_V93, recovery_isolation_mod

root = Path(__file__).resolve().parents[1]


def item(text, marker):
    start = text.index(marker)
    end = text.index('{', start) + 1
    depth = 1
    while depth:
        depth += (text[end] == '{') - (text[end] == '}')
        end += 1
    return text[start:end]


drm = (root / 'g17_drm.rs').read_text()
job = (root / 'g17_job.rs').read_text()
live = (root / 'g17_live_boot.rs').read_text()
compute = (root / 'g17_async_compute.rs').read_text()
render_impl = item(drm, 'impl G17PRenderCompletion {')
compute_impl = item(job, 'impl G17PJobCompletion {')
# v92d: settle_failed_deferred_render delegates the exact failure to this sink.
assert 'self.fail_render_after_final_observation(' in item(live, '    fn settle_failed_deferred_render(')
timeout = item(live, '    fn fail_render_after_final_observation(')
assert 'if retired {\n                signal.complete(Err(EIO));\n            } else {' in timeout
# Stability patch 0006: the failed render and its queued siblings are failed
# while owned by one helper, which also retains them in quarantine.
assert 'self.quarantine_failed_render_completion(signal, error);' in timeout
assert timeout.index('note_session_recovery_pending(') < timeout.index(
    'self.quarantine_failed_render_completion(signal, error)'
)
assert 'completion.fail_while_owned(error)' in item(live, '    fn quarantine_failed_render_completion(')
release_quarantine = item(live, '    fn release_quarantined_render_after_processor_stop(')
assert 'completion.release_after_processor_stop();' in release_quarantine
for marker in ('    fn release_runtime_objects(', '    fn drop_queue('):
    assert 'self.release_quarantined_render_after_processor_stop();' in item(live, marker)
vm_sweep = item(compute, '    pub(crate) fn fail_outstanding_jobs_for_vm(')
assert 'self.fail_outstanding_jobs_for_vm_after_compute_observation(status, error)' in vm_sweep
for marker in ('    fn service_async_compute_completions_matching(',
               '    fn fail_outstanding_jobs_for_vm_after_compute_observation(',
               '    pub(crate) fn quarantine_async_compute('):
    body = item(compute, marker)
    assert '.complete(Err(' not in body, marker
    assert '.fail_while_owned(error)' in body, marker
    # A VM-scoped fan-out never declares the session crashed (first-submit
    # hang, 2026-09-26): only the two completion-classification routes still
    # escalate, and only with QUARANTINE_SCOPE bit 0 clear.
    if marker != '    fn fail_outstanding_jobs_for_vm_after_compute_observation(':
        assert body.index('note_session_recovery_pending(') < body.index(
            '.fail_while_owned(error)'
        ), marker
    else:
        assert 'note_session_recovery_pending(' not in body, marker

source = r'''
#![allow(dead_code)]
use std::sync::{Arc, Mutex as StdMutex};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, AtomicBool, AtomicI32, Ordering};
macro_rules! pr_info { ($($arg:tt)*) => { let _ = format_args!($($arg)*); }; }
// v81/v87 instrumentation: e2e ring disarmed, feed-perf detector a no-op here.
mod g17_e2e { pub const K_COMPLETE:u8=18; pub fn stamp()->u64{0} pub fn record(_:u8,_:u32,_:u64,_:[u64;5]){} }
mod g17_host_path { pub fn note_render_pass(_:u64,_:u64){}
'''+HOST_PATH_V93+r'''}
'''+recovery_isolation_mod()+r'''
static G17P_RENDER_TIMING_RECORDS: AtomicU32 = AtomicU32::new(0);
mod g17_timing_buffer {pub const WORDS:usize=24;pub const KIND_RENDER_ADMISSION:u64=1;pub const KIND_RENDER_COMPLETION:u64=2;pub const KIND_RENDER_PUBLISH_STAGES:u64=5;pub const RENDER_COMPLETION_LAYOUT:u64=3;pub fn now()->u64{0} pub fn arch_counter()->u64{0} pub fn record(_words:[u64;24]){}}
mod module_parameters {
 pub struct Param(u32); impl Param {pub fn value(&self)->&u32{&self.0}}
 pub static g17p_trace_depth:Param=Param(0);
 pub static g17p_completion_trace:Param=Param(0);
}
#[derive(Clone,Copy,Debug,PartialEq)] struct Error(i32);
impl Error {fn to_errno(self)->i32 {self.0}}
const EIO:Error=Error(-5); const EINVAL:Error=Error(-22); const ECANCELED:Error=Error(-125);
type Result<T=()> = std::result::Result<T,Error>;
struct Aggregate; impl Aggregate {fn complete(&self,_result:Result){}}
struct Mutex<T>(StdMutex<T>);
impl<T> Mutex<T> {fn lock(&self)->std::sync::MutexGuard<'_,T>{self.0.lock().unwrap()}}
#[derive(Default)] struct Memory {active:AtomicUsize,closed:AtomicBool,unmapped:AtomicBool,quarantined:AtomicBool}
impl Memory {
 fn close(&self) { self.closed.store(true,Ordering::SeqCst); if self.active.load(Ordering::SeqCst)==0 {self.unmapped.store(true,Ordering::SeqCst);} }
}
struct VmGuard(Arc<Memory>);
impl VmGuard {
 fn new(m:&Arc<Memory>)->Self{m.active.fetch_add(1,Ordering::SeqCst);Self(m.clone())}
 fn quarantine(&self){self.0.quarantined.store(true,Ordering::SeqCst);}
}
impl Drop for VmGuard {fn drop(&mut self){if self.0.active.fetch_sub(1,Ordering::SeqCst)==1 && self.0.closed.load(Ordering::SeqCst){self.0.unmapped.store(true,Ordering::SeqCst);}}}
struct Registry(Arc<AtomicUsize>);
impl Registry {
 fn quarantine_render_slot(&self,_slot:u8){self.0.fetch_add(10,Ordering::SeqCst);}
 fn rehabilitate_render_slot_after_processor_stop(&self,_slot:u8){self.0.fetch_add(100,Ordering::SeqCst);}
}
struct Lease {released:Arc<AtomicUsize>,registry:Registry,slot:u8}
impl Drop for Lease {fn drop(&mut self){self.released.fetch_add(1,Ordering::SeqCst);}}
#[derive(Default)] struct Status(AtomicI32);
impl Status {fn record(&self,e:i32){let _=self.0.compare_exchange(0,e,Ordering::AcqRel,Ordering::Acquire);}fn record_if_device_loss(&self,e:i32){if e != -61 {self.record(e)}}}
struct Fence {count:AtomicUsize,error:AtomicI32,status:Arc<Status>,status_at_signal:AtomicI32,active_at_signal:AtomicUsize,quarantined_at_signal:AtomicBool,memory:Arc<Memory>}
impl Fence {
 fn set_error(&self,e:Error){self.error.store(e.0,Ordering::SeqCst);}
 fn signal(&self){self.status_at_signal.store(self.status.0.load(Ordering::SeqCst),Ordering::SeqCst);self.active_at_signal.store(self.memory.active.load(Ordering::SeqCst),Ordering::SeqCst);self.quarantined_at_signal.store(self.memory.quarantined.load(Ordering::SeqCst),Ordering::SeqCst);self.count.fetch_add(1,Ordering::SeqCst);}
}
fn fence(status:&Arc<Status>,memory:&Arc<Memory>)->Fence {Fence{count:AtomicUsize::new(0),error:AtomicI32::new(0),status:status.clone(),status_at_signal:AtomicI32::new(0),active_at_signal:AtomicUsize::new(99),quarantined_at_signal:AtomicBool::new(false),memory:memory.clone()}}
mod g17_completion {
 #[derive(Default)]pub struct Execution{pub tiling_timestamps:[u64;2],pub fragment_timestamps:[u64;2]}
 pub struct PairedRenderCompletion {pub complete:bool,pub execution:Execution}
}
struct Mapping;
impl Mapping {fn with_cpu_bytes<F:FnOnce(&mut [u8])->Result>(&self,f:F)->Result{f(&mut [0;16])}}
struct Destination {mapping:Mapping,offset:usize}
impl Destination {fn write(&self,_timestamp:u64)->Result{Ok(())}}
'''

for name, impl, extra in [
    ('Render', render_impl, 'lease:Mutex<Option<Lease>>,timestamps:[Option<Destination>;4],created:std::time::Instant,trace_created_ns:u64, trace_attach_ns:AtomicU64, trace_service_counters:[AtomicU64;4], attached_ns:AtomicU64,published_ns:AtomicU64,submission_id:u64,vdm_base:u64,owner:u64,render_slot:AtomicU32,'),
    ('Compute', compute_impl, 'destinations:[Option<Destination>;2],'),
]:
    source += f'''struct {name} {{ fence:Fence,status:Arc<Status>,vm_job:Mutex<Option<VmGuard>>,signalled:AtomicU32,aggregate_member:Aggregate,recovery_class:std::sync::atomic::AtomicU8,{extra}}}
impl {name} {{
'''
    if '--negative-control' in sys.argv:
        # The former runtime wiring used complete(Err) for unresolved jobs.
        source += 'fn fail_while_owned(&self,error:Error){self.complete(Err(error));}'
    else:
        source += item(impl, '    pub(crate) fn fail_while_owned(')
    source += item(impl, '    pub(crate) fn complete(')
    source += item(impl, '    pub(crate) fn recovery_spared(')
    source += item(impl, '    pub(crate) fn release_after_processor_stop(')
    source += '\n}\n'

source += r'''
fn render(m:&Arc<Memory>,released:&Arc<AtomicUsize>)->Arc<Render>{
 let s=Arc::new(Status::default()); Arc::new(Render{fence:fence(&s,m),status:s,vm_job:Mutex(StdMutex::new(Some(VmGuard::new(m)))),signalled:AtomicU32::new(0),aggregate_member:Aggregate,recovery_class:Default::default(),timestamps:[None,None,None,None],lease:Mutex(StdMutex::new(Some(Lease{released:released.clone(),registry:Registry(released.clone()),slot:0}))),created:std::time::Instant::now(),trace_created_ns:0,trace_attach_ns:AtomicU64::new(0),trace_service_counters:Default::default(),attached_ns:AtomicU64::new(0),published_ns:AtomicU64::new(0),submission_id:1,vdm_base:0,owner:0,render_slot:AtomicU32::new(0)})
}
fn compute(m:&Arc<Memory>)->Arc<Compute>{let s=Arc::new(Status::default());Arc::new(Compute{fence:fence(&s,m),status:s,vm_job:Mutex(StdMutex::new(Some(VmGuard::new(m)))),signalled:AtomicU32::new(0),aggregate_member:Aggregate,recovery_class:Default::default(),destinations:[None,None]})}
#[test] fn render_error_keeps_mapping_and_slot_through_file_and_scheduler_close(){
 let m=Arc::new(Memory::default());let released=Arc::new(AtomicUsize::new(0));
 let runtime=render(&m,&released);let scheduler=runtime.clone();
 m.close();runtime.fail_while_owned(EIO);drop(scheduler);
 assert!(!m.unmapped.load(Ordering::SeqCst));assert_eq!(released.load(Ordering::SeqCst),10);
 assert_eq!(runtime.fence.count.load(Ordering::SeqCst),1);
 assert_eq!(runtime.fence.status_at_signal.load(Ordering::SeqCst),-5);
 assert_eq!(runtime.fence.active_at_signal.load(Ordering::SeqCst),1);
 assert!(runtime.fence.quarantined_at_signal.load(Ordering::SeqCst));
 runtime.fail_while_owned(EIO);assert_eq!(runtime.fence.count.load(Ordering::SeqCst),1);
 // Runtime quarantine owner is dropped only after proven processor stop.
 drop(runtime);assert!(m.unmapped.load(Ordering::SeqCst));assert_eq!(released.load(Ordering::SeqCst),11);
}
#[test] fn compute_error_keeps_mapping_until_quarantine_owner_drops(){
 let m=Arc::new(Memory::default());let runtime=compute(&m);let scheduler=runtime.clone();
 runtime.fail_while_owned(EIO);m.close();drop(scheduler);
 assert!(!m.unmapped.load(Ordering::SeqCst));assert_eq!(runtime.fence.active_at_signal.load(Ordering::SeqCst),1);
 assert_eq!(runtime.fence.status_at_signal.load(Ordering::SeqCst),-5);
 assert!(runtime.fence.quarantined_at_signal.load(Ordering::SeqCst));
 runtime.fail_while_owned(EIO);assert_eq!(runtime.fence.count.load(Ordering::SeqCst),1);
 drop(runtime);assert!(m.unmapped.load(Ordering::SeqCst));
}
#[test] fn retired_render_releases_before_success_fence_despite_scheduler_arc(){
 let m=Arc::new(Memory::default());let released=Arc::new(AtomicUsize::new(0));let c=render(&m,&released);m.close();
 c.complete(Ok(g17_completion::PairedRenderCompletion{complete:true,execution:Default::default()}));
 assert!(m.unmapped.load(Ordering::SeqCst));assert_eq!(released.load(Ordering::SeqCst),1);
 assert_eq!(c.fence.active_at_signal.load(Ordering::SeqCst),0);
 c.fail_while_owned(EIO);assert_eq!(c.fence.count.load(Ordering::SeqCst),1);assert_eq!(c.fence.error.load(Ordering::SeqCst),0);
}
#[test] fn proven_retired_error_releases_but_unresolved_error_does_not(){
 let m=Arc::new(Memory::default());let released=Arc::new(AtomicUsize::new(0));let c=render(&m,&released);m.close();
 c.complete(Err(EIO));assert!(m.unmapped.load(Ordering::SeqCst));assert_eq!(released.load(Ordering::SeqCst),1);
 assert_eq!(c.fence.active_at_signal.load(Ordering::SeqCst),0);assert_eq!(c.fence.status_at_signal.load(Ordering::SeqCst),-5);
}
#[test] fn retired_compute_releases_before_signal(){
 let m=Arc::new(Memory::default());let c=compute(&m);m.close();c.complete(Ok([1,2]));
 assert!(m.unmapped.load(Ordering::SeqCst));assert_eq!(c.fence.active_at_signal.load(Ordering::SeqCst),0);
}
#[test] fn stopped_compute_error_releases_vm_pin_despite_scheduler_arc(){
 let m=Arc::new(Memory::default());let runtime=compute(&m);let scheduler=runtime.clone();
 m.close();runtime.fail_while_owned(EIO);assert!(!m.unmapped.load(Ordering::SeqCst));
 runtime.release_after_processor_stop();assert!(m.unmapped.load(Ordering::SeqCst));
 runtime.release_after_processor_stop();drop(runtime);drop(scheduler);
}
#[test] fn competing_error_notifications_keep_ownership_and_signal_once(){
 let m=Arc::new(Memory::default());let released=Arc::new(AtomicUsize::new(0));let c=render(&m,&released);m.close();
 let mut threads=vec![];for _ in 0..8{let c=c.clone();threads.push(std::thread::spawn(move||c.fail_while_owned(EIO)));}
 for t in threads{t.join().unwrap();}assert_eq!(c.fence.count.load(Ordering::SeqCst),1);
 assert!(!m.unmapped.load(Ordering::SeqCst));assert_eq!(released.load(Ordering::SeqCst),10);
 drop(c);assert!(m.unmapped.load(Ordering::SeqCst));
}
#[test] fn processor_stop_releases_a_signalled_quarantine_despite_scheduler_refs(){
 let m=Arc::new(Memory::default());let released=Arc::new(AtomicUsize::new(0));let runtime=render(&m,&released);let scheduler=runtime.clone();
 m.close();runtime.fail_while_owned(EIO);assert!(!m.unmapped.load(Ordering::SeqCst));assert_eq!(released.load(Ordering::SeqCst),10);
 runtime.release_after_processor_stop();
 assert!(m.unmapped.load(Ordering::SeqCst));assert_eq!(released.load(Ordering::SeqCst),111,"quarantine, rehabilitate, then release exactly once");
 runtime.release_after_processor_stop();assert_eq!(released.load(Ordering::SeqCst),111);
 drop(runtime);drop(scheduler);assert_eq!(released.load(Ordering::SeqCst),111);
}
'''
with tempfile.TemporaryDirectory() as tmp:
    tmp = Path(tmp)
    src, exe = tmp / 'ownership.rs', tmp / 'ownership'
    src.write_text(source)
    subprocess.run(['rustc', '--edition=2021', '--test', str(src), '-o', str(exe)], check=True)
    run = subprocess.run([str(exe), '--test-threads=1'])
    if '--negative-control' in sys.argv:
        assert run.returncode != 0, 'old complete(Err) wiring unexpectedly passed'
        print('PASS negative control: old unresolved-error wiring is rejected')
        sys.exit(0)
    run.check_returncode()
print('PASS actual completion methods: error signalling preserves unresolved VM/slot ownership')
