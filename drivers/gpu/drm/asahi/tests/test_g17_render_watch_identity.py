#!/usr/bin/env python3
"""Compile the real watchdog entry point against a mock settlement sink."""
from pathlib import Path
import argparse
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--baseline-ref', help='Run the same ownership assertions against an older implementation')
args = parser.parse_args()
source = (subprocess.check_output(['git', '-C', str(root), 'show',
          args.baseline_ref + ':drivers/gpu/drm/asahi/g17_live_boot.rs'], text=True)
          if args.baseline_ref else (root / 'g17_live_boot.rs').read_text())
def item(marker):
    start = source.index(marker)
    end = source.index('{', start) + 1
    depth = 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[start:end]
method = item('    pub(crate) fn timeout_deferred_render(')
helpers = ''
if 'render_completion_pending' in method:
    # v92d: the watchdog also finds the exact completion in the pair's
    # queued FIFO, defers while its graph is borrowed, and retires the
    # completed prefix before failing only an exact still-pending owner.
    helpers = (item('    fn render_completion_pending(') + '\n'
               + item('    pub(crate) fn defer_render_timeout_if_borrowed('))
if args.baseline_ref and 'completion:' not in method:
    # Adapt only the signature so the same test invokes the old body. The
    # deliberately unused argument exposes its missing ownership check.
    method = method.replace('submission_id: u64,',
        'submission_id: u64, completion: &Arc<crate::g17_drm::G17PRenderCompletion>,')
body = r'''
#![allow(dead_code)]
use std::sync::Arc;
type Error = i32;
type Result<T = ()> = std::result::Result<T, Error>;
mod g17_drm {
    pub struct G17PRenderCompletion { pub deferred: std::cell::Cell<Option<i32>> }
    impl G17PRenderCompletion {
        pub fn new() -> Self { Self { deferred: std::cell::Cell::new(None) } }
        pub fn defer_timeout(&self, error: i32) { self.deferred.set(Some(error)); }
    }
}
mod g17_resources { pub const G17P_RENDER_MAX_IN_FLIGHT: usize = 16; }
use g17_drm::G17PRenderCompletion;
struct Manager { preparing: [bool; 2] }
impl Manager { fn render_pair_preparing(&self, slot: u8) -> bool { self.preparing[slot as usize] } }
struct Runtime {
    pending_render: [Option<Arc<G17PRenderCompletion>>; 2],
    pending_render_queued: [Vec<Arc<G17PRenderCompletion>>; 2],
    manager: Option<Manager>,
    deferred_render_timeouts: u64,
    settled: Vec<(u8, Option<u64>, Error)>,
}
impl Runtime {
    fn new(pending: [Option<Arc<G17PRenderCompletion>>; 2]) -> Self {
        Self { pending_render: pending, pending_render_queued: [vec![], vec![]],
               manager: Some(Manager { preparing: [false; 2] }), deferred_render_timeouts: 0,
               settled: vec![] }
    }
    // Legacy sink name (baseline) and the v92d final-observation sink.
    fn settle_failed_deferred_render(&mut self, slot: u8, id: Option<u64>,
                                    stamp: Option<u8>, error: Error) {
        assert!(stamp.is_none());
        self.settled.push((slot, id, error));
        self.pending_render[slot as usize] = None;
    }
    fn fail_render_after_final_observation(&mut self, slot: u8, id: Option<u64>,
                                           stamp: Option<u8>, error: Error) {
        self.settle_failed_deferred_render(slot, id, stamp, error);
    }
    // No firmware completion is observed in these scenarios.
    fn poll_deferred_render_completion(&mut self, _slot: u8) -> Result<Option<()>> { Ok(None) }
    fn settle_deferred_render_completion(&mut self, _slot: u8, _observed: ()) -> Result<bool> {
        unreachable!()
    }
HELPERS
METHOD
}
#[test]
fn completed_watch_cannot_cancel_a_reused_slot() {
    let old = Arc::new(G17PRenderCompletion::new());
    let next = Arc::new(G17PRenderCompletion::new());
    let mut r = Runtime::new([Some(next.clone()), None]);
    // The callback must validate the actual object, not just a log identifier.
    r.timeout_deferred_render(0, 7, &old, -125);
    r.timeout_deferred_render(0, 7, &old, -110);
    assert!(r.settled.is_empty());
    assert!(Arc::ptr_eq(r.pending_render[0].as_ref().unwrap(), &next));
    r.timeout_deferred_render(0, 7, &next, -110);
    assert_eq!(r.settled, [(0, Some(7), -110)]);
}
#[test]
fn duplicate_and_invalid_callbacks_do_not_settle_anything() {
    let c = Arc::new(G17PRenderCompletion::new());
    let mut r = Runtime::new([None, Some(c.clone())]);
    r.timeout_deferred_render(0, 9, &c, -125);
    r.timeout_deferred_render(255, 9, &c, -125);
    assert!(r.settled.is_empty());
    r.timeout_deferred_render(1, 9, &c, -125);
    r.timeout_deferred_render(1, 9, &c, -125);
    assert_eq!(r.settled, [(1, Some(9), -125)]);
}
#[test]
fn a_borrowed_graph_defers_the_exact_owner_instead_of_settling() {
    let c = Arc::new(G17PRenderCompletion::new());
    let mut r = Runtime::new([Some(c.clone()), None]);
    r.manager.as_mut().unwrap().preparing[0] = true;
    r.timeout_deferred_render(0, 3, &c, -110);
    assert!(r.settled.is_empty());
    assert_eq!(c.deferred.get(), Some(-110));
    assert_eq!(r.deferred_render_timeouts, 1);
    let stale = Arc::new(G17PRenderCompletion::new());
    r.timeout_deferred_render(0, 3, &stale, -110);
    assert_eq!(stale.deferred.get(), None, "a stale watch must not defer the slot");
}
'''.replace('HELPERS', helpers).replace('METHOD', method)
with tempfile.TemporaryDirectory(prefix='g17-watch-identity-') as directory:
    exe = str(Path(directory) / 'test')
    subprocess.run(['rustc', '--edition=2021', '--test', '-', '-o', exe],
                   input=body, text=True, check=True)
    subprocess.run([exe], check=True)

if not args.baseline_ref:
    drm = (root / 'g17_drm.rs').read_text()
    assert drm.count('packet.submission_id(), packet.completion(),') == 2
    assert drm.count('render_slot, submission_id, completion, error,') == 1
    assert drm.count('render_slot, packet.submission_id(), packet.completion(), ETIMEDOUT,') == 2
print('PASS exact completion identity on every deferred-render failure entry')
