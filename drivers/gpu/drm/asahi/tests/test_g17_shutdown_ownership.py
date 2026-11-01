#!/usr/bin/env python3
"""Actual shutdown methods must retain live DMA and settle stopped-job fences."""
from pathlib import Path
from g17_compute_inflight_test_support import completion_mod, queue_limits_mock
import re
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parents[1]
live = (root / 'g17_live_boot.rs').read_text()
compute = (root / 'g17_async_compute.rs').read_text()
lifecycle = (root / 'g17_lifecycle.rs').read_text()


def item(text, marker):
    start = text.index(marker)
    end = text.index('{', start) + 1
    depth = 1
    while depth:
        depth += (text[end] == '{') - (text[end] == '}')
        end += 1
    return text[start:end]


retain = item(live, '    fn retain_after_stop_failure(')
release = item(compute, '    fn release_async_compute_after_processor_stop(')
assert release.index('completion.complete(Err(EIO))') < release.index(
    'completion.release_after_processor_stop()'
)
assert release.index('completion.release_after_processor_stop()') < release.index(
    'active._lease.rehabilitate_after_processor_stop()'
)
assert release.index('active._lease.rehabilitate_after_processor_stop()') < release.index(
    'self.async_compute_queues.clear()'
)
for marker in ('    fn release_runtime_objects(', '    fn drop_queue('):
    body = item(live, marker)
    assert 'self.release_async_compute_after_processor_stop();' in body
    assert 'self.async_compute_queues.clear()' not in body
    assert body.index('completion.complete(Err(EIO));') < body.index(
        'completion.release_after_processor_stop();'
    )
recovery = item(lifecycle, 'pub(crate) fn recover_persistent_runtime<')
assert recovery.index('owner.stop_cpu_pair()') < recovery.index('owner.release_runtime_objects()')
assert recovery.index('owner.fail_outstanding_jobs_while_owned()') < recovery.index(
    'return Err(error)'
)
assert recovery.index('let disable_result = owner.disable_sksm_queue();') < recovery.index(
    'owner.release_runtime_objects()'
)
assert recovery.index('owner.release_runtime_objects()') < recovery.index('disable_result?')
failed_stop = item(live, '    fn fail_outstanding_jobs_while_owned(')
assert 'self.pending_render.iter().flatten()' in failed_stop
assert 'for queue in &mut self.async_compute_queues' in failed_stop
assert failed_stop.count('fail_while_owned(EIO)') == 3
assert 'queue.quarantined = true;' in failed_stop
assert 'active._lease.quarantine();' in failed_stop
for premature_release in ('.take()', 'release_after_processor_stop', '= None'):
    assert premature_release not in failed_stop
teardown = item(lifecycle, 'pub(crate) fn teardown_persistent_runtime<')
assert 'owner.retain_after_stop_failure();\n        return;' in teardown
assert teardown.index('owner.stop_cpu_pair()') < teardown.index('owner.drop_queue()')

# Explicit inventory: adding an owned runtime field should prompt a review of
# its failed-stop lifetime, rather than silently escaping the retention test.
fields = re.findall(r'^    (\w+):', item(live, 'pub(crate) struct G17PLiveRuntime {'), re.M)
host_only = set('completion_indices render_tracker test_stage accepting_submissions queue_setup_pending compute_owner compute_recycle_pending compute_abandoned crash_recoveries compute_gate_suspect completion_signal render_event_masks render_fence_state boot_stop_unproven session_recovery_pending runtime_pm_armed'.split())
# G17PFirmwareDataSnapshot owns CPU-side KVVec copies of the firmware bytes,
# not firmware-visible mappings. Dropping that diagnostic copy is safe even
# when the processor stop fails; the live DMA owners below must remain held.
host_only.add('firmware_data')
host_only.add('queue_registry')
host_only.update(('platform_removed', 'session_rebuild_requested', 'preparations'))
# Timing-buffer sample and drain-ownership flag: plain host state.
host_only.update(('trace_service_entry_counter', 'deferred_completion_drain_owned'))
# v83-v92d host policy/bookkeeping (plain values, no DMA): submit bracket and
# counter, DVFS/CE parity policy and bracket flags, idle-hold publication flag,
# deferred-render-timeout slot mask.
host_only.update(('submit_open', 'submit_count', 'dvfs_native_bracket', 'clpc_effort',
                  'dvfs_ce', 'idle_hold_published', 'deferred_render_timeouts'))
# v92e published GPU power budget (a u32) and the v93r drained-pair reclaim
# poll start (an Instant): plain host values.
host_only.update(('power_target_mw', 'reclaim_pending_since'))
# Render publication's dependency-set scratch (a host KBox; no DMA).
host_only.add('render_dependency_scratch')
options = 'pdev drm registers manager cpus gfx_state gfx_rtkit gfx1_state gfx1_rtkit queue compute_binding compute_timestamps pending_compute'.split()
vectors = 'retired_queues quarantined_bindings pending_render pending_render_queued quarantined_render_completions'.split()
# Completions the firmware has already retired, waiting for their fence to be
# signalled with the runtime mutex released. They own nothing a failed stop
# must retain, so every teardown path signals them in place (tested below).
deferred = ['deferred_completions']
signal_deferred = item(live, '    fn signal_deferred_completions_in_place(')
assert set(fields) == host_only | set(options) | set(vectors) | set(deferred) | {'async_compute_queues'}, set(fields)

if '--negative-control' in sys.argv:
    retain = retain.replace('        core::mem::forget(self.pending_compute.take());\n', '')
    retain = retain.replace('        core::mem::forget(core::mem::take(&mut self.async_compute_queues));\n', '')
    release = 'fn release_async_compute_after_processor_stop(&mut self) {self.async_compute_queues.clear();}'

source = r'''
#![allow(dead_code)]
use std::sync::{Arc,atomic::{AtomicUsize,Ordering}};
#[derive(Clone,Copy)] struct Error;
const EIO:Error=Error;
type Result<T=()> = std::result::Result<T,Error>;
struct Probe(Arc<AtomicUsize>);
impl Probe {fn rehabilitate_after_processor_stop(&self){}}
impl Drop for Probe {fn drop(&mut self){self.0.fetch_add(1,Ordering::SeqCst);}}
struct Completion {signals:AtomicUsize}
impl Completion {fn complete(&self,_:Result){let _=self.signals.compare_exchange(0,1,Ordering::SeqCst,Ordering::SeqCst);}fn release_after_processor_stop(&self){}}
struct Packet {completion:Arc<Completion>,_mapping:Probe}
impl Packet {fn completion(&self)->Result<&Completion>{Ok(&self.completion)}}
struct Active {packet:Arc<Packet>,_lease:Probe}
'''+completion_mod()+queue_limits_mock()+r'''
type Inflight=g17_completion::PairFifo<Active,{g17_queue_limits::MAX_COMPUTE_IN_FLIGHT_PER_QUEUE as usize}>;
type RetiredLeases=g17_completion::PairFifo<Probe,{g17_queue_limits::MAX_COMPUTE_IN_FLIGHT_PER_QUEUE as usize}>;
struct Queue {inflight:Inflight,retired_leases:RetiredLeases,_graph:Probe,_root:Probe}
struct Registry;impl Registry {fn release_retained_owners_after_processor_stop(&self){}}
mod g17_exit_kill {pub fn forget_all_acks(){}}
struct Deferred{completion:Arc<Completion>}
impl Deferred{fn signal(self){self.completion.complete(Ok(()))}}
struct Runtime {
 queue_registry:Option<Registry>,
 accepting_submissions:bool,
 async_compute_queues:Vec<Queue>,
'''
source += ''.join(f'{name}:Option<Probe>,\n' for name in options)
source += ''.join(f'{name}:Vec<Probe>,\n' for name in vectors)
source += ''.join(f'{name}:Vec<Deferred>,\n' for name in deferred)
source += '}\nimpl Runtime {\n' + retain + '\n' + release + '\n' + signal_deferred + '\n}\n'
source += '''fn runtime(drops:&Arc<AtomicUsize>)->Runtime {Runtime {queue_registry:None,accepting_submissions:true,async_compute_queues:vec![],
'''
source += ''.join(f'{name}:Some(Probe(drops.clone())),\n' for name in options)
source += ''.join(f'{name}:vec![Probe(drops.clone())],\n' for name in vectors)
source += ''.join(f'{name}:vec![],\n' for name in deferred)
source += r'''}}
fn install(r:&mut Runtime,d:&Arc<AtomicUsize>)->Arc<Packet>{
 let p=Arc::new(Packet{completion:Arc::new(Completion{signals:AtomicUsize::new(0)}),_mapping:Probe(d.clone())});
 let mut inflight=Inflight::new();if inflight.push(Active{packet:p.clone(),_lease:Probe(d.clone())}).is_err(){unreachable!()}
 r.async_compute_queues.push(Queue{inflight,retired_leases:RetiredLeases::new(),_graph:Probe(d.clone()),_root:Probe(d.clone())});p
}
#[test] fn failed_stop_retains_all_firmware_visible_owners_after_runtime_drop(){
 let d=Arc::new(AtomicUsize::new(0));let mut r=runtime(&d);let scheduler=install(&mut r,&d);
 r.retain_after_stop_failure();assert!(!r.accepting_submissions);drop(r);drop(scheduler);
 assert_eq!(d.load(Ordering::SeqCst),0,"possibly live firmware owner was freed");
}
#[test] fn failed_stop_signals_firmware_retired_deferred_completions_and_retains_the_rest(){
 let d=Arc::new(AtomicUsize::new(0));let mut r=runtime(&d);let scheduler=install(&mut r,&d);
 let retired=Arc::new(Completion{signals:AtomicUsize::new(0)});r.deferred_completions.push(Deferred{completion:retired.clone()});
 r.retain_after_stop_failure();
 assert_eq!(retired.signals.load(Ordering::SeqCst),1,"a firmware-retired completion was stranded by the failed stop");
 assert!(r.deferred_completions.is_empty());drop(r);drop(scheduler);
 assert_eq!(d.load(Ordering::SeqCst),0,"possibly live firmware owner was freed");
}
#[test] fn successful_stop_settles_every_active_fence_despite_scheduler_refs(){
 let d=Arc::new(AtomicUsize::new(0));let mut r=runtime(&d);let a=install(&mut r,&d);let b=install(&mut r,&d);
 r.release_async_compute_after_processor_stop();assert!(r.async_compute_queues.is_empty());
 assert_eq!(a.completion.signals.load(Ordering::SeqCst),1);assert_eq!(b.completion.signals.load(Ordering::SeqCst),1);
 assert_eq!(d.load(Ordering::SeqCst),6,"queue graphs, roots and leases must retire");
 drop(a);drop(b);assert_eq!(d.load(Ordering::SeqCst),8);
}
#[test] fn successful_stop_settles_every_stacked_kick_of_a_queue(){
 let d=Arc::new(AtomicUsize::new(0));let mut r=runtime(&d);let a=install(&mut r,&d);
 let b=Arc::new(Packet{completion:Arc::new(Completion{signals:AtomicUsize::new(0)}),_mapping:Probe(d.clone())});
 if r.async_compute_queues[0].inflight.push(Active{packet:b.clone(),_lease:Probe(d.clone())}).is_err(){unreachable!()}
 r.release_async_compute_after_processor_stop();
 assert_eq!(a.completion.signals.load(Ordering::SeqCst),1);assert_eq!(b.completion.signals.load(Ordering::SeqCst),1);
 assert_eq!(d.load(Ordering::SeqCst),4,"graph, root and both leases retire");
}
#[test] fn stopped_quarantine_and_idle_queue_are_safe_to_release(){
 let d=Arc::new(AtomicUsize::new(0));let mut r=runtime(&d);let p=install(&mut r,&d);p.completion.complete(Err(EIO));
 r.async_compute_queues.push(Queue{inflight:Inflight::new(),retired_leases:RetiredLeases::new(),_graph:Probe(d.clone()),_root:Probe(d.clone())});
 r.release_async_compute_after_processor_stop();r.release_async_compute_after_processor_stop();
 assert_eq!(p.completion.signals.load(Ordering::SeqCst),1);assert_eq!(d.load(Ordering::SeqCst),5);
}
'''
with tempfile.TemporaryDirectory() as tmp:
    src, exe = Path(tmp) / 'shutdown.rs', Path(tmp) / 'shutdown'
    src.write_text(source)
    subprocess.run(['rustc', '--edition=2021', '--test', str(src), '-o', str(exe)], check=True)
    result = subprocess.run([str(exe), '--test-threads=1'])
    if '--negative-control' in sys.argv:
        assert result.returncode != 0, 'old shutdown ownership unexpectedly passed'
        print('PASS negative control: old shutdown drops live memory and strands active fences')
    else:
        result.check_returncode()
