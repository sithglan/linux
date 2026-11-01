#!/usr/bin/env python3
"""Execute production compute-prefix collection and render-fence pruning."""
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
source = (root / 'g17_drm.rs').read_text()
codec = (root / 'g17_submission.rs').read_text()


def block(text, marker):
    start = text.index(marker)
    end = text.index('{', start) + 1
    depth = 1
    while depth:
        depth += (text[end] == '{') - (text[end] == '}')
        end += 1
    return text[start:end]


program = r'''
#![allow(dead_code)]
use std::sync::{Arc,atomic::{AtomicI32,Ordering}};
type Result<T=()>=std::result::Result<T,()>;const GFP_KERNEL:()=();
#[derive(Clone)]struct Fence { id:u64, context:u64, status:Arc<AtomicI32> }
impl Fence {fn raw(&self)->*const Self{self}}
mod g17_job {#[derive(Clone)]pub struct G17PCommandFences{pub ready:super::Fence,pub completed:super::Fence}}
mod bindings{pub unsafe fn dma_fence_get_status(f:*const super::Fence)->i32{(*f).status.load(super::Ordering::Acquire)}}
#[derive(Default)]struct KVec<T>(Vec<T>);
impl<T>KVec<T>{fn push(&mut self,t:T,_f:())->Result{self.0.push(t);Ok(())}
 fn retain(&mut self,f:impl FnMut(&mut T)->bool){self.0.retain_mut(f)}}
impl<T>std::ops::Deref for KVec<T>{type Target=[T];fn deref(&self)->&[T]{&self.0}}
''' + 'mod g17_submission {' + block(codec, 'pub(crate) fn g17p_render_prefix_contains(') + '}\n'
program += block(source, 'fn append_render_prefix_dependencies(')
program += '\nstruct G17PQueue { render_frontier: KVec<(u64,g17_job::G17PCommandFences)> }\nimpl G17PQueue {'
program += block(source, '    fn prune_render_frontier(') + '}\n'
program += r'''
fn f(id:u64,status:i32)->g17_job::G17PCommandFences{g17_job::G17PCommandFences{ready:Fence{id:id+100,context:98,status:Arc::new(AtomicI32::new(1))},completed:Fence{id,context:99,status:Arc::new(AtomicI32::new(status))}}}
fn ids(fs:&KVec<Fence>)->Vec<u64>{fs.iter().map(|f|f.id).collect()}
#[test]fn two_independent_same_context_renders_are_retained_in_both_lists(){
 let frontier=[(1,f(11,0)),(2,f(13,0))];let mut deps=KVec(Vec::new());let mut checked=KVec(Vec::new());
 append_render_prefix_dependencies(&frontier,Some(2),&mut deps,&mut checked).unwrap();
 assert_eq!(ids(&deps),vec![111,113]);assert_eq!(ids(&checked),vec![11,13]);
 assert_eq!(checked[0].context,checked[1].context);
 // Simulate DRM coalescing equal-context dependencies: the checked list still
 // contains both hardware states for error checks; firmware tokens carry waits.
 checked[1].status.store(1,Ordering::Release);assert_eq!(checked[0].status.load(Ordering::Acquire),0);
}
#[test]fn index_zero_snapshot_excludes_current_ioctl_and_none_adds_nothing(){
 let frontier=[(40,f(11,0)),(41,f(13,0)),(42,f(17,0))];
 let mut deps=KVec(Vec::new());let mut checked=KVec(Vec::new());
 append_render_prefix_dependencies(&frontier,Some(41),&mut deps,&mut checked).unwrap();
 assert_eq!(ids(&checked),vec![11,13]);
 append_render_prefix_dependencies(&frontier,None,&mut deps,&mut checked).unwrap();
 assert_eq!(ids(&checked),vec![11,13]);
}
#[test]fn positive_non_latest_prefix_does_not_wait_on_later_independent_render(){
 let frontier=[(41,f(11,0)),(42,f(13,0)),(43,f(17,0))];
 let mut deps=KVec(Vec::new());let mut checked=KVec(Vec::new());
 append_render_prefix_dependencies(&frontier,Some(42),&mut deps,&mut checked).unwrap();
 assert_eq!(ids(&checked),vec![11,13]);
}
#[test]fn prune_removes_success_and_error_but_retains_unsignalled_queued_work(){
 let mut q=G17PQueue{render_frontier:KVec(vec![(1,f(11,1)),(2,f(13,-5)),(3,f(17,0))])};
 q.prune_render_frontier();assert_eq!(q.render_frontier.len(),1);assert_eq!(q.render_frontier[0].0,3);
}
'''

compute = block(source, '    fn submit_compute_async(')
assert 'append_render_prefix_dependencies(' in compute
assert 'translated.render_wait_through' in compute
assert compute.index('append_render_prefix_dependencies(') < compute.index('.enqueue(packet, dependencies)?')
backend = block(source, 'impl g17_job::G17PQueueBackend for G17PAsyncQueueBackend {')
assert backend.index('for fence in packet.input_fences()') < backend.index('self.registry.acquire_compute(')
assert backend.index('completion.status.get() != 0') < backend.index('for fence in packet.input_fences()')
# An unpublished failure records only device-loss errors (EIO/ETIMEDOUT/ENODEV);
# EBUSY/ECANCELED/EINVAL leave the VM healthy (fault isolation, 2026-09-26).
fail_unpublished = block(source, '    pub(crate) fn fail_unpublished(')
assert 'self.status.record_if_device_loss(error.to_errno());' in fail_unpublished
assert 'self.status.record(error.to_errno());' not in fail_unpublished
with tempfile.TemporaryDirectory() as directory:
    path = Path(directory) / 'compute_prefix.rs'
    binary = Path(directory) / 'compute_prefix'
    path.write_text(program)
    subprocess.run(['rustc', '--edition=2021', '--test', str(path), '-o', str(binary)], check=True)
    subprocess.run([str(binary), '--test-threads=1'], check=True, timeout=60)
