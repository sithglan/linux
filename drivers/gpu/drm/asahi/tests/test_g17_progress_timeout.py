#!/usr/bin/env python3
"""Exercise the production renewal decision and scheduler ownership ordering."""
from pathlib import Path
import subprocess
import tempfile
root = Path(__file__).resolve().parents[1]
src = (root / 'g17_job.rs').read_text()
start = src.index('fn renew_progress_timeout(')
end = src.index('\npub(crate) trait G17PQueueBackend', start)
helper = src[start:end]
wrapper = (root.parents[3] / 'rust/kernel/drm/sched.rs').read_text()
callback = wrapper[wrapper.index('unsafe extern "C" fn timedout_job_cb'):wrapper.index('unsafe extern "C" fn free_job_cb')]
assert callback.index('T::false_timeout') < callback.index('bindings::drm_sched_stop(')
assert 'DRM_GPU_SCHED_STAT_NO_HANG' in callback[callback.index('T::false_timeout'):callback.index('bindings::drm_sched_stop(')]
for name in ['G17PScheduledJob', 'G17PRenderScheduledJob']:
    assert name in src
assert src.count('if !cfg!(g17p_progress_timeout_probe) { return false; }') == 2
cases = r'''
#[test]
fn progress_requires_baseline_same_epoch_and_changed_counter() {
    for (old, now, expected) in [
        (None, Some((1, 2)), false),
        (Some((1, 2)), None, false),
        (Some((1, 2)), Some((1, 2)), false),
        (Some((1, 2)), Some((2, 3)), false),
        (Some((1, 2)), Some((1, 3)), true),
        (Some((1, u32::MAX)), Some((1, 0)), true),
    ] {
        let mut previous = old;
        let mut renewals = 0;
        assert_eq!(renew_progress_timeout(&mut previous, &mut renewals, now), expected);
        assert_eq!(previous, now);
        assert_eq!(renewals, u8::from(expected));
    }
}
#[test]
fn unchanged_or_unavailable_sample_does_not_reuse_old_progress() {
    let mut previous = Some((1, 1)); let mut renewals = 0;
    assert!(renew_progress_timeout(&mut previous, &mut renewals, Some((1, 2))));
    assert!(!renew_progress_timeout(&mut previous, &mut renewals, Some((1, 2))));
    assert!(!renew_progress_timeout(&mut previous, &mut renewals, None));
    assert!(!renew_progress_timeout(&mut previous, &mut renewals, Some((1, 3))));
}
#[test]
fn unrelated_progress_cannot_renew_indefinitely() {
    let mut previous = Some((7, 0)); let mut renewals = 0;
    for count in 1..100 {
        assert_eq!(renew_progress_timeout(&mut previous, &mut renewals, Some((7, count))), count <= 3);
    }
    assert_eq!(renewals, 3);
}
'''
with tempfile.TemporaryDirectory() as tmp:
    exe = str(Path(tmp) / 'test')
    subprocess.run(['rustc', '--edition=2021', '--test', '-', '-o', exe], input=helper+cases, text=True, check=True)
    subprocess.run([exe], check=True)
print('progress decision and pre-stop scheduler ownership checks PASS')
