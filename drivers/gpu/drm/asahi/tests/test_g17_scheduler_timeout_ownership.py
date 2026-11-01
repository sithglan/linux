#!/usr/bin/env python3
"""Execute the production Rust callback against a scheduler ownership model.

Checks ordering and opt-in scope, not real worker concurrency or GPU recovery.
The model enforces the matching C stop/start contract: the guilty job enters
this callback detached; stop must reattach that exact job before settlement.
"""
from pathlib import Path
import subprocess
import tempfile
from test_g17_compute_scs_encoding import block
root = Path(__file__).resolve().parents[1]
source = (root.parents[3] / 'rust/kernel/drm/sched.rs').read_text()
callback = block(source, 'unsafe extern "C" fn timedout_job_cb<T: JobImpl>(')
callback = callback.replace('crate::container_of!(sched_job, Job<T>, job) as *mut Job<T>', 'sched_job.cast::<Job<T>>()')
unit = r'''
#![allow(non_camel_case_types, dead_code)]
use std::ptr::addr_of_mut;
macro_rules! pr_info { ($($arg:tt)*) => {{ let _ = format_args!($($arg)*); }}; }
struct Error;
impl Error { fn to_errno(&self) -> i32 { -110 } }
const ETIMEDOUT: Error = Error;
#[repr(u32)] enum Status { Nominal = 0 }
#[repr(C)] struct Job<T> { job: bindings::drm_sched_job, inner: T }
trait JobImpl: Sized {
    const MANAGED_TIMEOUT_RECOVERY: bool = false;
    const EXPECT_RENEW: bool = false;
    fn false_timeout(_job: &mut Job<Self>) -> bool { false }
    fn timed_out(job: &mut Job<Self>) -> Status;
}
mod bindings {
    pub type drm_gpu_sched_stat = u32;
    #[repr(C)] pub struct drm_sched_job { pub sched: *mut Scheduler, pub s_fence: *mut SchedFence }
    pub struct Fence { pub status: i32, pub context: u64, pub seqno: u64 }
    pub struct SchedFence { pub finished: Fence }
    pub const drm_gpu_sched_stat_DRM_GPU_SCHED_STAT_NO_HANG: u32 = 2;
    pub unsafe fn dma_fence_get_status(f: *mut Fence) -> i32 { unsafe { (*f).status } }
    pub struct Scheduler {
        pub expected_job: *mut drm_sched_job,
        pub managed: bool, pub completion_raced: bool,
        pub detached: bool, pub stopped: bool, pub settled: bool,
        pub restarted: bool, pub released: bool, pub free_guilty: bool,
    }
    pub unsafe fn drm_sched_stop(s: *mut Scheduler, job: *mut drm_sched_job) {
        let s = unsafe { &mut *s };
        assert_eq!(job, s.expected_job);
        assert!(s.managed && s.detached && !s.settled);
        s.detached = false;
        s.stopped = true;
        // C stop retains a raced-complete guilty job until callback returns.
        s.free_guilty = s.completion_raced;
    }
    pub unsafe fn drm_sched_start(s: *mut Scheduler, errno: i32) {
        let s = unsafe { &mut *s };
        assert_eq!(errno, 0);
        assert!(s.stopped && s.settled && !s.detached);
        s.restarted = true;
        if !s.free_guilty { s.released = true; }
    }
}
struct Managed;
impl JobImpl for Managed {
    const MANAGED_TIMEOUT_RECOVERY: bool = true;
    fn timed_out(job: &mut Job<Self>) -> Status {
        let s = unsafe { &mut *job.job.sched };
        assert!(s.stopped && !s.detached && !s.released && !s.restarted);
        s.settled = true;
        Status::Nominal
    }
}
struct Renew;
impl JobImpl for Renew {
    const MANAGED_TIMEOUT_RECOVERY: bool = true;
    const EXPECT_RENEW: bool = true;
    fn false_timeout(job: &mut Job<Self>) -> bool {
        let s = unsafe { &mut *job.job.sched };
        assert!(s.detached && !s.stopped && !s.settled);
        true
    }
    fn timed_out(_job: &mut Job<Self>) -> Status { panic!("renewal must not recover") }
}
struct Legacy;
impl JobImpl for Legacy {
    fn timed_out(job: &mut Job<Self>) -> Status {
        let s = unsafe { &mut *job.job.sched };
        assert!(s.detached && !s.stopped && !s.restarted);
        // Unmanaged drivers own their existing recovery transaction.
        s.settled = true;
        Status::Nominal
    }
}
fn check<T: JobImpl>(inner: T, raced: bool) {
    let mut s = bindings::Scheduler {
        expected_job: std::ptr::null_mut(), managed: T::MANAGED_TIMEOUT_RECOVERY,
        completion_raced: raced, detached: true, stopped: false,
        settled: false, restarted: false, released: false, free_guilty: false,
    };
    let mut sf = bindings::SchedFence { finished: bindings::Fence { status: if raced { 1 } else { 0 }, context: 31, seqno: 72 } };
    let mut job = Job { job: bindings::drm_sched_job { sched: &mut s, s_fence: &mut sf }, inner };
    s.expected_job = &mut job.job;
    let result = unsafe { timedout_job_cb::<T>(&mut job.job) };
    if (T::MANAGED_TIMEOUT_RECOVERY && raced) || T::EXPECT_RENEW {
        assert_eq!(result, bindings::drm_gpu_sched_stat_DRM_GPU_SCHED_STAT_NO_HANG);
        assert!(!s.stopped && !s.settled && !s.restarted);
        return;
    }
    assert_eq!(result, 0);
    assert!(s.settled);
    if T::MANAGED_TIMEOUT_RECOVERY {
        if s.free_guilty { s.released = true; }
        assert!(s.restarted && s.released && !s.detached);
    } else { assert!(!s.restarted && !s.stopped); }
}
#[test] fn progress_renewal_preserves_core_reinsertion_contract() { check(Renew, false); }
#[test] fn genuine_timeout_recovers_job_ownership() { check(Managed, false); }
#[test] fn raced_completion_remains_owned_through_callback() { check(Managed, true); }
#[test] fn legacy_backend_contract_unchanged() { check(Legacy, false); }
'''
with tempfile.TemporaryDirectory() as d:
    src = Path(d) / 'test.rs'
    src.write_text(unit + callback)
    exe = Path(d) / 'test'
    subprocess.run(['rustc', '--edition=2021', '--test', str(src), '-o', str(exe)], check=True)
    subprocess.run([str(exe)], check=True)
