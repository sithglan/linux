#!/usr/bin/env python3
"""Exercise the production independent-fence allocator and ioctl wait wiring."""
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
source = (root / 'g17_drm.rs').read_text()


def block(marker):
    start = source.index(marker)
    end = source.index('{', start) + 1
    depth = 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[start:end]


allocator = block('fn new_independent_submission_fence(')
# All four independently signalled fence creation sites must use this helper.
assert source.count('new_independent_submission_fence(&self.fence_contexts)') == 4
assert source.count('.new_fence(0, G17PSubmissionFence)') == 1
submit = block('    fn submit_guarded(')
assert 'let command_syncs = if first' not in submit
assert submit.count('                    &syncs,') == 2
# Userspace may remove its compatibility chain only after GET_PARAMS advertises
# this exact contract. Other DrmGpu implementations keep the default false.
assert 'fn supports_per_command_input_syncs(&self) -> bool { true }' in source
assert 'fn supports_per_command_input_syncs(&self) -> bool { false }' in (root / 'drm_gpu.rs').read_text()
assert 'FEATURE_PER_COMMAND_INPUT_SYNCS: u64 = 1 << 3' in (root / 'g17_queue_limits.rs').read_text()
file_source = (root / 'file.rs').read_text()
assert 'if gpu.supports_per_command_input_syncs() {' in file_source
assert 'params.features |= crate::g17_queue_limits::FEATURE_PER_COMMAND_INPUT_SYNCS;' in file_source
for name in ['submit_compute_async', 'submit_render_async']:
    body = block('    fn ' + name + '(')
    assert 'syncs: &[file::SyncItem]' in body
    assert 'for sync in syncs {' in body
    assert 'checked_dependencies.push(fence.clone()' in body
    assert 'dependencies.push(fence.clone()' in body
    assert 'last_publication' not in body

program = r'''
#![allow(dead_code)]
use std::{marker::PhantomData,sync::atomic::{AtomicU64,Ordering}};
type Result<T> = std::result::Result<T,()>;
struct G17PSubmissionFence;
struct RawFence {context:u64,seqno:u64,signalled:bool}
struct UniqueFence<T>{raw:Box<RawFence>,_marker:PhantomData<T>}
impl<T> UniqueFence<T>{fn raw(&self)->*mut RawFence{(&*self.raw as *const RawFence).cast_mut()}}
struct UserFence<T>{raw:Box<RawFence>,_marker:PhantomData<T>}
impl<T> From<UniqueFence<T>> for UserFence<T>{
 fn from(f:UniqueFence<T>)->Self{Self{raw:f.raw,_marker:PhantomData}}
}
struct FenceContexts{seq:AtomicU64}
impl FenceContexts{
 fn new_fence<T>(&self,_index:u32,_inner:T)->Result<UniqueFence<T>>{
  Ok(UniqueFence{raw:Box::new(RawFence{context:9,seqno:self.seq.fetch_add(1,Ordering::Relaxed),signalled:false}),_marker:PhantomData})
 }
}
mod bindings{
 use super::*;
 static NEXT:AtomicU64=AtomicU64::new(100);
 pub unsafe fn dma_fence_context_alloc(count:u32)->u64{NEXT.fetch_add(count as u64,Ordering::Relaxed)}
}
''' + allocator + r'''
#[test]fn out_of_order_commands_and_aggregates_keep_distinct_contexts(){
 let contexts=FenceContexts{seq:AtomicU64::new(0)};
 let mut fences:Vec<_>=(0..128).map(|_|new_independent_submission_fence(&contexts).unwrap()).collect();
 for i in 0..fences.len(){for j in 0..i{assert_ne!(fences[i].raw.context,fences[j].raw.context);}}
 // Later independent commands can signal while earlier ones remain pending.
 for i in (1..128).step_by(2){fences[i].raw.signalled=true;}
 for i in (0..128).step_by(2){assert!(!fences[i].raw.signalled);}
 // A context-based latest-fence merge must retain each unfinished command.
 let mut latest=std::collections::BTreeMap::new();
 for f in &fences{latest.insert(f.raw.context,&f.raw);}
 assert_eq!(latest.values().filter(|f|!f.signalled).count(),64);
}
'''
with tempfile.TemporaryDirectory() as directory:
    src = Path(directory) / 'fences.rs'
    exe = Path(directory) / 'fences'
    src.write_text(program)
    subprocess.run(['rustc', '--edition=2021', '--test', str(src), '-o', str(exe)], check=True)
    subprocess.run([str(exe)], check=True)
print('PASS production independent fence allocator and whole-ioctl input wait wiring')
