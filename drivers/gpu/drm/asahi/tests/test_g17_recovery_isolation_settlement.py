#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only OR MIT
"""Exercise the production spared-render service with controlled firmware witnesses."""
from pathlib import Path
import subprocess
import tempfile
import sys

root = Path(__file__).resolve().parents[1]
live = (root / 'g17_live_boot.rs').read_text()
async_ = (root / 'g17_async_compute.rs').read_text()
mmu = (root / 'mmu.rs').read_text()
file = (root / 'file.rs').read_text()
drm = (root / 'g17_drm.rs').read_text()

def item(text, marker):
    start = text.index(marker)
    end = text.index('{', start) + 1
    depth = 1
    while depth:
        depth += (text[end] == '{') - (text[end] == '}')
        end += 1
    return text[start:end]

recovery = item(live, '    fn service_primary_recovery_body(')
service = item(async_, '    pub(crate) fn service_spared_recovery_renders(')
if '--negative-control' in sys.argv:
    service = service.replace('if !self.manager.as_mut().ok_or(ENODEV)?',
                              'if self.manager.as_mut().ok_or(ENODEV)?')
# The settlement QID comes from classification, never from raw stamp afterward.
assert 'let (guilty_qids, _spared_cancelled)' in recovery
assert recovery.index('classify_recovery_victims(') < recovery.index('for qid in 0..128u8')
assert 'settle_render_blamed_by_recovery(qid, EIO)' in recovery
assert 'settle_render_blamed_by_recovery(stamp, EIO)' not in recovery
assert 'completion.recovery_spared()' in recovery
assert 'return (firmware_guilty_qids, 0)' in async_
assert '(guilty_qids & !collateral_qids, spared_cancelled)' in async_
# No forced failure/quarantine path in innocent retirement; exact tracker proof first.
assert 'settle_failed_deferred_render' not in service
assert 'retire_abandoned_render_if_firmware_retired(render_slot, false)' in service
assert 'Err(ECANCELED)' in service
settle = item(live, '    fn settle_retired_render_completion(')
assert 'completion: signal, observed' in settle
assert 'pending_render_queued[index].take()' in settle
assert 'retire_render_usc_freelist_at_completion' in settle
# Accepted jobs keep registry metadata until their physical mappings are unmapped.
close = item(file, '    pub(crate) fn unbind_gem_object(')
assert 'untrack_t8140_native_context_object' not in close
unmap = item(mmu, '    fn drop_mappings_now(')
assert unmap.index('bo_unmap(') < unmap.index('untrack_t8140_native_context_object(')
# Exact pre-publication refusal identity checked while the same queue is locked.
translate = item(drm, '    fn translate_submission(')
assert translate.index('retain_first_t8140_job') < translate.index('t8140_job_context_view0_unbound')
assert 'context_unbound_refusal = Some(submission_id)' in translate
assert 'ret == Err(ENOENT) && queue.context_unbound_refusal(id)' in file
assert 'Err(ENOENT) if context_unbound' in file
assert 'Err(error) if mmu::is_context_unbound(error)' in drm
aliases = item(mmu, '    pub(crate) fn install_t8140_job_context_aliases(')
assert 'if index == 0 { context_unbound_error() } else { ENOENT }' in aliases
assert 't8140_context_view0_unbound: Arc::new(AtomicBool::new(true)' in mmu


source = r'''
#![allow(dead_code, unused_variables)]
use std::sync::Arc;
use std::collections::VecDeque;
type Result<T=()> = std::result::Result<T, i32>;
const ENODEV: i32 = 19;
const ECANCELED: i32 = 125;
macro_rules! dev_info { ($($args:tt)*) => {()} }
mod g17_resources { pub const G17P_RENDER_MAX_IN_FLIGHT: usize = 2; }
mod g17_recovery_isolation {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    pub const SETTLE_CANCELLED:u32=4;
    pub static ON:AtomicBool=AtomicBool::new(true);
    pub static SETTLED_CANCELLED:AtomicU64=AtomicU64::new(0);
    pub fn enabled(_:u32)->bool { ON.load(Ordering::Relaxed) }
    pub fn count(c:&AtomicU64){c.fetch_add(1,Ordering::Relaxed);}
}
struct Completion { spared:bool }
impl Completion { fn recovery_spared(&self)->bool { self.spared } }
struct Manager { proof:VecDeque<bool>, calls:usize }
impl Manager {
    fn retire_abandoned_render_if_firmware_retired(&mut self, _:u8, accept_executed:bool)->Result<bool>{
        assert!(!accept_executed);self.calls+=1;Ok(self.proof.pop_front().unwrap_or(false))
    }
}
struct Runtime {
    pending_render:Vec<Option<Arc<Completion>>>, queued:VecDeque<Arc<Completion>>,
    manager:Option<Manager>, pdev:Option<()>, recovery:bool, crashed:bool,
    observed:VecDeque<Option<u32>>, results:Vec<Result<u32>>,
}
impl Runtime {
    fn new(spared:bool, proofs:&[bool])->Self {
        Self { pending_render:vec![Some(Arc::new(Completion{spared}))], queued:VecDeque::new(),
            manager:Some(Manager{proof:proofs.iter().copied().collect(),calls:0}), pdev:None,
            recovery:false, crashed:false, observed:VecDeque::new(), results:Vec::new() }
    }
    fn primary_recovery_pending(&self)->bool{self.recovery}
    fn is_crashed(&self)->bool{self.crashed}
    fn poll_deferred_render_completion(&mut self,_:u8)->Result<Option<u32>>{
        Ok(self.observed.pop_front().unwrap_or(None))
    }
    fn settle_deferred_render_completion(&mut self,slot:u8,value:u32)->Result<bool>{
        self.settle_retired_render_completion(slot,Ok(value))
    }
    fn settle_retired_render_completion(&mut self,slot:u8,value:Result<u32>)->Result<bool>{
        assert!(self.pending_render[slot as usize].take().is_some());
        self.results.push(value);self.pending_render[slot as usize]=self.queued.pop_front();Ok(true)
    }
    SERVICE
}
#[test]
fn unproven_ownership_does_not_cancel_or_release(){
 let mut r=Runtime::new(true,&[false]);r.service_spared_recovery_renders().unwrap();
 assert!(r.results.is_empty());assert!(r.pending_render[0].is_some());
}
#[test]
fn exact_retirement_cancels_only_the_retired_head(){
 let mut r=Runtime::new(true,&[true,false]);r.queued.push_back(Arc::new(Completion{spared:true}));
 r.service_spared_recovery_renders().unwrap();assert_eq!(r.results,vec![Err(ECANCELED)]);
 assert!(r.pending_render[0].is_some());assert_eq!(r.manager.unwrap().calls,2);
}
#[test]
fn both_retired_heads_settle_in_publication_order(){
 let mut r=Runtime::new(true,&[true,true]);r.queued.push_back(Arc::new(Completion{spared:true}));
 r.service_spared_recovery_renders().unwrap();assert_eq!(r.results,vec![Err(ECANCELED);2]);
 assert!(r.pending_render[0].is_none());
}
#[test]
fn actual_completion_keeps_its_result(){
 let mut r=Runtime::new(true,&[false]);r.observed.push_back(Some(47));
 r.service_spared_recovery_renders().unwrap();assert_eq!(r.results,vec![Ok(47)]);
 assert_eq!(r.manager.unwrap().calls,0);
}
#[test]
fn guilty_head_is_not_handled_as_innocent(){
 let mut r=Runtime::new(false,&[true]);r.service_spared_recovery_renders().unwrap();
 assert!(r.results.is_empty());assert_eq!(r.manager.unwrap().calls,0);
}
#[test]
fn later_unclassified_work_is_not_cancelled(){
 let mut r=Runtime::new(true,&[true,true]);r.queued.push_back(Arc::new(Completion{spared:false}));
 r.service_spared_recovery_renders().unwrap();assert_eq!(r.results,vec![Err(ECANCELED)]);
 assert!(r.pending_render[0].is_some());assert_eq!(r.manager.unwrap().calls,1);
}
#[test]
fn recovery_open_or_crashed_does_not_mutate_ownership(){
 for crashed in [false,true] {let mut r=Runtime::new(true,&[true]);r.crashed=crashed;r.recovery=!crashed;
 r.service_spared_recovery_renders().unwrap();assert!(r.results.is_empty());assert_eq!(r.manager.unwrap().calls,0);}
}
'''.replace('    SERVICE',service.replace('pub(crate)','pub'))
with tempfile.TemporaryDirectory() as directory:
    p=Path(directory);(p/'settlement.rs').write_text(source)
    subprocess.run(['rustc','--edition=2021','--test',str(p/'settlement.rs'),'-o',str(p/'settlement')],check=True)
    subprocess.run([str(p/'settlement'),'--test-threads=1'],check=True)
print('PASS production spared-render settlement, corrected blame wiring and exact refusal accounting')
