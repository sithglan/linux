#!/usr/bin/env python3
"""Compile the actual startup-policy and ensure-queue methods with tiny mocks.

This does not emulate hardware. It checks lazy allocation/admission policy,
allocation failure, idempotence and post-recovery compute availability without
requiring the Rust kernel crate to compile as a userspace test target.
"""

from pathlib import Path
import subprocess
import tempfile


SOURCE = Path(__file__).resolve().parents[1] / "g17_live_boot.rs"
source = SOURCE.read_text()


def function(marker):
    start = source.index(marker)
    brace = source.index("{", start)
    depth = 1
    end = brace + 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


policy_start = source.index("#[derive(Debug, Copy, Clone, PartialEq, Eq)]\nenum G17PTestStage")
policy_end = source.index("/// Per-role callback state", policy_start)
policy = source[policy_start:policy_end]
ensure = function("pub(crate) fn ensure_compute_queue(")
resume = function("fn resume_submissions(&mut self)")
finish = function("fn finish_boot_session(")
assert "queue_setup_pending: test_stage.prepares_qid4_at_startup()" in finish
assert "let accepting_submissions = session.compute_uapi_enabled;" in source
assert "session.compute_uapi_enabled && session.queue.is_some()" not in source
assert "self.queue_setup_pending = session.queue_setup_pending;" in source
worker = function("pub(crate) fn service_primary_firmware_events(")
assert "self.queue_setup_pending && !notifications_pending" in worker
submit = function("pub(crate) fn submit_translated_compute(")
assert submit.index("self.ensure_compute_queue()?") < submit.index("self.acquire_compute_owner(owner)?")
assert submit.count("self.ensure_compute_queue()?") >= 2  # Includes post-rebuild ensure.
render = function("pub(crate) fn submit_translated_render(")
assert "ensure_compute_queue" not in render

test = r'''
type Result<T = ()> = core::result::Result<T, i32>;
const EINVAL: i32 = 22;
const ENODEV: i32 = 19;
#[derive(Default)]
struct Manager { allocations: usize, fail: bool }
impl Manager {
    fn prepare_g17p_compute_queue(&mut self, _drm: &u32) -> Result<u8> {
        if self.fail { return Err(12); }
        self.allocations += 1;
        Ok(4)
    }
}
struct Preparations;impl Preparations{fn reopen(&self)->bool{true}}
struct Runtime {
    preparations: Preparations,
    test_stage: G17PTestStage,
    queue: Option<u8>,
    drm: Option<u32>,
    manager: Option<Manager>,
    queue_setup_pending: bool,
    accepting_submissions: bool,
    session_recovery_pending: bool,
}
impl Runtime {
    fn new(stage: G17PTestStage) -> Self {
        Self { preparations: Preparations, test_stage: stage, queue: None, drm: Some(1), session_recovery_pending: false,
               manager: Some(Manager::default()),
               queue_setup_pending: stage.prepares_qid4_at_startup(),
               accepting_submissions: stage.enables_compute_uapi() }
    }
    fn allocations(&self) -> usize { self.manager.as_ref().unwrap().allocations }
}
#[test]
fn normal_startup_allows_render_without_a_compute_graph() {
    let r = Runtime::new(G17PTestStage::ComputeUapi);
    assert!(r.accepting_submissions);
    assert!(!r.queue_setup_pending);
    assert!(r.queue.is_none());
    assert_eq!(r.allocations(), 0);
}
#[test]
fn first_compute_allocates_once_and_repeated_ensure_retains_graph() {
    let mut r = Runtime::new(G17PTestStage::ComputeUapi);
    r.ensure_compute_queue().unwrap();
    assert_eq!(r.queue, Some(4));
    assert_eq!(r.allocations(), 1);
    assert!(r.accepting_submissions);
    r.ensure_compute_queue().unwrap();
    assert_eq!(r.allocations(), 1);
}
#[test]
fn explicit_qid4_stage_still_requests_worker_preparation_not_uapi() {
    let mut r = Runtime::new(G17PTestStage::Qid4);
    assert!(r.queue_setup_pending);
    assert!(!r.accepting_submissions);
    r.ensure_compute_queue().unwrap();
    assert_eq!(r.allocations(), 1);
    assert!(!r.queue_setup_pending);
    assert!(!r.accepting_submissions);
}
#[test]
fn control_stage_never_allocates_or_admits_submissions() {
    let mut r = Runtime::new(G17PTestStage::Control);
    r.ensure_compute_queue().unwrap();
    assert_eq!(r.allocations(), 0);
    assert!(!r.queue_setup_pending);
    assert!(!r.accepting_submissions);
}
#[test]
fn failed_compute_allocation_is_retryable_without_losing_render_admission() {
    let mut r = Runtime::new(G17PTestStage::ComputeUapi);
    r.manager.as_mut().unwrap().fail = true;
    assert_eq!(r.ensure_compute_queue(), Err(12));
    assert!(r.queue.is_none());
    assert!(r.accepting_submissions);
    r.manager.as_mut().unwrap().fail = false;
    r.ensure_compute_queue().unwrap();
    assert_eq!(r.allocations(), 1);
}
#[test]
fn recovery_resume_accepts_render_and_later_compute_ensures_new_graph() {
    let mut r = Runtime::new(G17PTestStage::ComputeUapi);
    r.ensure_compute_queue().unwrap();
    r.accepting_submissions = false;
    r.queue = None; // Stopped-processor teardown released its old graph.
    r.queue_setup_pending = r.test_stage.prepares_qid4_at_startup();
    r.resume_submissions();
    assert!(r.accepting_submissions);
    assert!(!r.queue_setup_pending);
    assert!(r.queue.is_none());
    r.ensure_compute_queue().unwrap();
    assert_eq!(r.allocations(), 2);
}
'''

with tempfile.TemporaryDirectory(prefix="g17-lazy-compute-") as tmp:
    root = Path(tmp)
    unit = root / "test.rs"
    unit.write_text(policy + test + "\nimpl Runtime {\n" + ensure + "\n" + resume + "\n}\n")
    exe = root / "test"
    subprocess.run(["rustc", "-Awarnings", "--edition=2021", "--test", str(unit), "-o", str(exe)], check=True)
    subprocess.run([str(exe)], check=True)
print("lazy-compute source integration checks passed")
