#!/usr/bin/env python3
"""Run the production output-fence publication against an asynchronous job.

A compute submission is queued, not executed, by the time its ioctl returns.
The fence installed into the caller's output syncobjs therefore has to be the
scheduler's completion fence -- publishing the submit path's own fence, whether
as success or as the guard's ECANCELED failure, tells a DRM syncobj wait that
work which has not run is finished, and the client reads its destination buffer
before the GPU has filled it.

The struct, its Drop and `install_output_fences` are lifted verbatim from
g17_drm.rs, so this exercises the shipped code rather than a restatement.
"""
from pathlib import Path
import subprocess, tempfile

source = (Path(__file__).resolve().parents[1] / 'g17_drm.rs').read_text()
live_source = (Path(__file__).resolve().parents[1] / 'g17_live_boot.rs').read_text()
job_source = (Path(__file__).resolve().parents[1] / 'g17_job.rs').read_text()


def block(marker):
    a = source.index(marker)
    e = source.index('{', a) + 1
    depth = 1
    while depth:
        depth += (source[e] == '{') - (source[e] == '}')
        e += 1
    return source[a:e]


def live_block(marker):
    a = live_source.index(marker)
    e = live_source.index('{', a) + 1
    depth = 1
    while depth:
        depth += (live_source[e] == '{') - (live_source[e] == '}')
        e += 1
    return live_source[a:e]


install = block('    fn install_output_fences(')
guard_struct = block('struct G17POutputFencePublication<')
guard_impl = block("impl G17POutputFencePublication<'_> {")
guard_drop = block("impl Drop for G17POutputFencePublication<'_> {")
render_completion = block("impl G17PRenderCompletion {")

render_completion_struct = block("pub(crate) struct G17PRenderCompletion {")
assert "vm_job: Arc<Mutex<Option<mmu::T8140VmJobGuard>>>" in render_completion_struct, \
    'published render does not retain its VM mappings through completion'

complete = render_completion[render_completion.index("pub(crate) fn complete"):]
assert complete.index("compare_exchange(0, 1") < complete.index("self.fence.signal();"), \
    'render completion does not reject duplicate firmware notifications'
assert complete.index("drop(self.lease.lock().take());") < complete.index("self.fence.signal();"), \
    'render completion wakes userspace before releasing its physical slot lease'

compute_completion_start = job_source.index("impl G17PJobCompletion {")
compute_completion_end = job_source.index("/// Everything a prepared compute", compute_completion_start)
compute_completion = job_source[compute_completion_start:compute_completion_end]
assert compute_completion.index("compare_exchange(0, 1") \
    < compute_completion.index("self.fence.signal();"), \
    'compute completion can copy timestamps or signal its fence more than once'

# The finite scheduler timeout is the only non-event completion fallback. It
# must inspect final device state before consuming the once-only settlement
# gate, so an early event at a stamp rollover cannot become device loss.
timeout_start = live_source.index("    pub(crate) fn timeout_deferred_render(")
timeout_end = live_source.index("    pub(crate) fn abandon_deferred_render(", timeout_start)
timeout = live_source[timeout_start:timeout_end]
settle_render = live_block("    fn settle_deferred_render_completion(")
# v92d gates on the exact completion Arc, which install_render_completion
# records together with render_fence_state.arm() only after preparation.
install_render = live_block("    fn install_render_completion(")
assert install_render.index("self.render_fence_state.arm(render_slot)") < install_render.index("*pending = Some(completion);")
assert timeout.index("if !self.render_completion_pending(render_slot, completion) { return; }") \
    < timeout.index("self.poll_deferred_render_completion(render_slot)"), \
    'pre-publication failures incorrectly enter the hardware watchdog path'
assert timeout.index("self.poll_deferred_render_completion(render_slot)") \
    < timeout.index("self.settle_deferred_render_completion(render_slot, observed)"), \
    'watchdog consumes the settlement gate before its final completion observation'
assert "self.defer_render_completion(signal, observed);" in settle_render, \
    'watchdog cannot salvage a completion that became visible after the event edge'
assert settle_render.index("self.defer_render_completion(signal, observed);") \
    < settle_render.index("retire_render_usc_freelist_at_completion(render_slot)"), \
    'fallible retained cleanup can suppress a proven render completion'
# The rebuild is handed to the recovery worker (stack depth); admission closes
# first, so the state is still never reused before the processor stop.
assert settle_render.index("retire_render_usc_freelist_at_completion(render_slot)") \
    < settle_render.index("self.request_session_rebuild(EAGAIN)"), \
    'failed retained cleanup can be reused without a processor-stop boundary'
request_rebuild = live_block("    fn request_session_rebuild(")
assert "self.preparations.close(false);" in request_rebuild
assert "self.accepting_submissions = false;" in request_rebuild
assert "queue_g17p_recovery_worker" in request_rebuild
# v92d: the watchdog entry itself returns nothing and hands the exact
# still-pending owner to one failure sink after its final observation.
exact_timeout = live_block("    pub(crate) fn timeout_deferred_render(")
assert ") -> bool {" not in exact_timeout and "return !retired;" not in exact_timeout, \
    'watchdog still delegates a late registry quarantine to its caller'
assert exact_timeout.index("self.poll_deferred_render_completion(render_slot)") \
    < exact_timeout.index("self.fail_render_after_final_observation(")
timeout = live_block("    fn fail_render_after_final_observation(")
# Stability patch 0006: the failed render and every queued sibling go through
# one helper that fails the fence while owned, then retains it in quarantine.
signal_error = timeout.index("self.quarantine_failed_render_completion(signal, error);")
quarantine_helper = live_block("    fn quarantine_failed_render_completion(")
assert quarantine_helper.index("completion.fail_while_owned(error)") \
    < quarantine_helper.index("quarantined_render_completions")
assert "lease.registry.quarantine_render_slot(lease.slot);" in render_completion, \
    'error fence can wake before its physical slot is quarantined'
assert "if retired {\n                signal.complete(Err(EIO));\n            } else {" in timeout[:signal_error], \
    'lost render drops its VM guard without proving firmware retirement'

# Runtime recovery may free an unresolved render's VM guard only after both
# processors stop. Failed CPU stop must intentionally retain it instead.
recovery = live_source[live_source.index("impl g17_lifecycle::PersistentRuntimeRecovery"):
                       live_source.index("impl g17_lifecycle::PersistentRuntimeTeardown")]
assert recovery.index("fn stop_cpu_pair") < recovery.index("fn release_runtime_objects")
release = recovery[recovery.index("fn release_runtime_objects"):]
assert "self.release_quarantined_render_after_processor_stop();" in release
teardown = live_source[live_source.index("impl g17_lifecycle::PersistentRuntimeTeardown"):]
retain_failure = teardown[teardown.index("fn retain_after_stop_failure"):
                          teardown.index("fn unregister_queue")]
assert "core::mem::forget(core::mem::take(" in retain_failure
assert "&mut self.quarantined_render_completions" in retain_failure

# Cancellation and submit-error cleanup must classify the final hardware state
# before permanently retiring a physical slot. A successful one-frame process
# can close while its already-completed watch job is still queued.
watch = block("impl g17_job::G17PRenderQueueBackend for G17PRenderQueueBackend {")
# v92d: the trait methods release the scheduler backlog and delegate to
# *_inner; publish delegates to publish_with (v84 early lease on the ioctl).
for trait_fn, inner in (("fn timed_out(", "    fn timed_out_inner("), ("fn cancel(", "    fn cancel_inner(")):
    body = watch[watch.index(trait_fn):]
    body = body[:body.index("\n    }", body.index("{"))]
    assert inner.strip()[3:] in body, trait_fn
    body = block(inner)
    assert "timeout_deferred_render(" in body, inner
    assert "quarantine_render_slot(" not in body, inner
    assert "if quarantine" not in body, inner

render_publish = block("    fn publish_with(")
assert "if quarantine" not in render_publish
assert "quarantine_render_slot(" not in render_publish
assert render_publish.count("timeout_deferred_render(") == 1
assert render_publish.index("packet.begin_publication()") < render_publish.index(
    "self.registry.acquire_lease(self.owner, G17PTileRequest::of(&packet))?"
)
assert render_publish.index("completion.attach_lease(lease)?") < render_publish.index(
    "runtime.submit_translated_render("
)

# Counted per-command members feed one ioctl aggregate. The original guard
# retains the enqueue gate even on partial failure.
submit = source[source.index('impl queue::Queue for G17PQueue {'):]
assert submit.count('G17PSubmitCompletionMember::new(aggregate.clone())') == 2
assert 'publication.aggregate = Some(aggregate.clone());' in submit
assert 'publication.publish_async(&aggregate.fence());' in submit

# A queue can contain successors before an earlier asynchronous job fails.
# DRM considers the predecessor's signalled error fence ready, so the backend
# must consult the VM status before it acquires hardware or publishes.
backend = block("impl g17_job::G17PQueueBackend for G17PAsyncQueueBackend {")
status_gate = backend.index("completion.status.get() != 0")
assert status_gate < backend.index("self.registry.acquire_compute(")
assert status_gate < backend.index("publish_async_compute(")
assert backend.index("completion.complete(Err(EIO));", status_gate) \
    < backend.index("return Err(EIO);", status_gate)

# The output-list allocation itself used to sit before the publication guard.
# ENOMEM there dropped already-resolved syncobjs with no fence installed. The
# early fence must exist first, and that allocation failure must publish it
# directly from the original sync list before returning.
guarded = block('    fn submit_guarded(')
fence_create = guarded.index('let early_output_fence = new_independent_submission_fence(')
output_alloc = guarded.index('KVec::with_capacity(output_count')
alloc_failure = guarded.index('Err(_) => {', output_alloc)
alloc_return = guarded.index('return Err(Self::stage_fail(', alloc_failure)
assert fence_create < output_alloc
assert guarded.index('early_output_fence.set_error(ENOMEM);', alloc_failure) < alloc_return
install_early = guarded.index('Self::install_output_fences(syncs.drain(in_sync_count..)', alloc_failure)
signal_early = guarded.index('early_output_fence.signal();', alloc_failure)
assert install_early < signal_early < alloc_return
assert '.push_within_capacity(item)' in guarded

# Every fallible path after publication is armed runs inside one result scope.
# The exact error and terminal VM state are committed before Drop can wake a
# syncobj waiter.
result_scope = guarded.index('let result = (|| -> Result {')
note_failure = guarded.index('self.note_submission_failure(submission_id, error);', result_scope)
record_error = guarded.index('publication.failed(error);', note_failure)
assert result_scope < note_failure < record_error
drop_body = guard_drop[guard_drop.index('fn drop'):]
assert drop_body.index('install_output_fences(') < drop_body.index('self.fence.signal();'), \
    'the error fence wakes before every output syncobj owns it'

preamble = r'''
#![allow(dead_code, unused_variables)]
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Copy, Clone, PartialEq)]
struct Error(i32);
const ECANCELED: Error = Error(125);
const EIO: Error = Error(5);
type Result<T=()>=std::result::Result<T,Error>;
struct G17PSubmitCompletion;
impl G17PSubmitCompletion { fn finish_enqueue(&self,_result:Result) {} }

/// One dma_fence: unsignalled until whoever owns the work signals it.
#[derive(Default)]
struct FenceState {
    signalled: AtomicBool,
    error: AtomicI32,
}

#[derive(Clone)]
struct Fence(Arc<FenceState>);

impl Fence {
    fn new() -> Self {
        Self(Arc::new(FenceState::default()))
    }
    fn from_fence<T>(user: &UserFence<T>) -> Fence {
        Fence(user.0.clone())
    }
    fn signal(&self) {
        self.0.signalled.store(true, Ordering::SeqCst);
    }
    /// What a drm_syncobj wait observes: an error fence counts as signalled.
    fn is_signalled(&self) -> bool {
        self.0.signalled.load(Ordering::SeqCst)
    }
    fn error(&self) -> i32 {
        self.0.error.load(Ordering::SeqCst)
    }
}

struct UserFence<T>(Arc<FenceState>, PhantomData<T>);

impl<T> UserFence<T> {
    fn new() -> Self {
        Self(Arc::new(FenceState::default()), PhantomData)
    }
    fn set_error(&self, error: Error) {
        self.0.error.store(error.0, Ordering::SeqCst);
    }
    fn signal(&self) {
        self.0.signalled.store(true, Ordering::SeqCst);
    }
}

struct G17PSubmissionFence;

#[derive(Clone, Default)]
struct Syncobj(Arc<Mutex<Option<Fence>>>);

impl Syncobj {
    fn replace_fence(&self, fence: Option<&Fence>) {
        *self.0.lock().unwrap() = fence.cloned();
    }
    fn add_point(&self, _chain: Chain, fence: &Fence, _value: u64) {
        *self.0.lock().unwrap() = Some(fence.clone());
    }
    /// What userspace would wait on. `None` means nothing was ever installed,
    /// which hangs a waiter forever.
    fn installed(&self) -> Option<Fence> {
        self.0.lock().unwrap().clone()
    }
}

struct Chain;

mod file {
    pub(super) struct SyncItem {
        pub syncobj: super::Syncobj,
        pub chain_fence: Option<super::Chain>,
        pub timeline_value: u64,
    }
}

type KVec<T> = Vec<T>;

struct G17PQueue;

fn out_sync(syncobj: &Syncobj) -> file::SyncItem {
    file::SyncItem {
        syncobj: syncobj.clone(),
        chain_fence: None,
        timeline_value: 0,
    }
}

/// The submit ioctl as it stands before it knows render from compute: split
/// the output syncobjs off and arm the guard over them.
fn arm<'a>(
    early: &'a UserFence<G17PSubmissionFence>,
    outputs: KVec<file::SyncItem>,
) -> G17POutputFencePublication<'a> {
    G17POutputFencePublication {
        fence: early,
        outputs,
        failure: ECANCELED,
        succeeded: false, aggregate: None,
    }
}
'''

tests = r'''
/// The asynchronous compute path: the job is queued here and runs later.
#[test]
fn queued_compute_publishes_the_job_fence_not_the_ioctl_fence() {
    let syncobj = Syncobj::default();
    let early = UserFence::<G17PSubmissionFence>::new();
    let finished = Fence::new();
    {
        let mut publication = arm(&early, vec![out_sync(&syncobj)]);
        submit_async(&mut publication, &finished);
    }

    let installed = syncobj.installed().expect(
        "no fence installed into the output syncobj: a waiter hangs forever",
    );
    assert!(
        !installed.is_signalled(),
        "the output syncobj was signalled when the ioctl returned, before the \
         queued job ran: userspace reads its destination buffer stale",
    );

    // The job runs.
    finished.signal();
    assert!(installed.is_signalled());
    assert_eq!(installed.error(), 0);
}

/// A failure before the job is queued must still publish, and must say so.
#[test]
fn refused_compute_publishes_a_cancelled_fence() {
    let syncobj = Syncobj::default();
    let early = UserFence::<G17PSubmissionFence>::new();
    {
        let _publication = arm(&early, vec![out_sync(&syncobj)]);
        // enqueue failed: the guard is never handed the job fence.
    }
    let installed = syncobj.installed().expect("nothing published on failure");
    assert!(installed.is_signalled());
    assert_eq!(installed.error(), ECANCELED.0);
}

/// Once the caller knows the real failure, that errno—not a generic
/// cancellation—must reach the output fence.
#[test]
fn failed_compute_publishes_the_exact_error() {
    let syncobj = Syncobj::default();
    let early = UserFence::<G17PSubmissionFence>::new();
    {
        let mut publication = arm(&early, vec![out_sync(&syncobj)]);
        publication.failed(EIO);
    }
    let installed = syncobj.installed().expect("nothing published on failure");
    assert!(installed.is_signalled());
    assert_eq!(installed.error(), EIO.0);
}

/// A render may complete before its ioctl publishes the scheduler fence.
#[test]
fn completed_render_publishes_a_signalled_fence() {
    let syncobj = Syncobj::default();
    let early = UserFence::<G17PSubmissionFence>::new();
    let finished = Fence::new();
    finished.signal();
    {
        let mut publication = arm(&early, vec![out_sync(&syncobj)]);
        publication.publish_async(&finished);
    }
    let installed = syncobj.installed().expect("nothing published on success");
    assert!(installed.is_signalled());
    assert_eq!(installed.error(), 0);
}
'''

fixed_async = '''
fn submit_async(publication: &mut G17POutputFencePublication<'_>, finished: &Fence) {
    publication.publish_async(finished);
}
'''

# Exactly what the async path did before this fix: the outputs had already been
# split off into the guard, so `install_output_fences` ran over an empty list
# and the guard published its own fence on the way out.
regressed_async = '''
fn submit_async(publication: &mut G17POutputFencePublication<'_>, finished: &Fence) {
    let syncs: KVec<file::SyncItem> = KVec::new();
    G17PQueue::install_output_fences(syncs.into_iter(), finished);
}
'''

body = (preamble + guard_struct + '\n' + guard_impl + '\n' + guard_drop
        + '\nimpl G17PQueue {\n' + install + '\n}\n')

with tempfile.TemporaryDirectory() as d:
    d = Path(d)
    good = d / 'good.rs'
    good.write_text(body + fixed_async + tests)
    exe = d / 'good'
    subprocess.run(['rustc', '--edition=2021', '--test', str(good), '-o', str(exe)],
                   check=True)
    subprocess.run([str(exe), '--test-threads=1'], check=True)

    bad = d / 'bad.rs'
    bad.write_text(body + regressed_async + tests)
    badexe = d / 'bad'
    subprocess.run(['rustc', '--edition=2021', '--test', str(bad), '-o', str(badexe)],
                   check=True)
    result = subprocess.run(
        [str(badexe), 'queued_compute_publishes_the_job_fence_not_the_ioctl_fence'],
        text=True, capture_output=True, timeout=60)
    assert result.returncode != 0, 'the pre-fix async path passed; the test proves nothing'
    assert 'before the queued job ran' in result.stdout, result.stdout + result.stderr

print('PASS production output-fence publication: queued work publishes the scheduler '
      'completion fence; render releases its slot before wake and duplicate firmware '
      'notifications signal once')
