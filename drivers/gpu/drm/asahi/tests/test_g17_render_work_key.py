#!/usr/bin/env python3
"""Run the production context state pool and exact lease Drop implementation."""
from pathlib import Path
from g17_work_state_test_support import tsq_backing
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
manager = (root / "g17_manager.rs").read_text()
resources = (root / "g17_resources.rs").read_text()
compute = (root / "g17_async_compute.rs").read_text()

def block(source, marker):
    start = source.index(marker)
    end = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]

lease = "\n".join(block(resources, marker) for marker in [
    "pub(crate) struct G17PWorkStateLease",
    "impl G17PWorkStateLease",
    "impl Drop for G17PWorkStateLease",
])
for required in [
    "render_command_buffer_state_pairs: [Option<Arc<g17_resources::G17PWorkStateLease>>; 64]",
    "allocate_active_render_command_buffer_state(pair_slot, work_state)",
    "work_state: Some(work_state)",
    "drop(tracker.work_state.take())",
]:
    assert required in manager, required
assert manager.count("work_ordinal: context.work_state_word") == 3
assert "_work_state: work_state" in compute
assert compute.index("packet.completion()?.work_state()") < compute.index("q.inflight.push(G17PAsyncComputeActive")
assert "render_command_buffer_states: G17PCommandBufferStateAllocator" not in manager

source = f'''
#![allow(dead_code)]
#[path = "{root / 'g17_work_state.rs'}"] mod g17_work_state;
use std::sync::Arc;
use std::sync::atomic::{{AtomicU32, Ordering}};
type Result<T=()> = std::result::Result<T, i32>;
const EBUSY: i32 = 16; const EINVAL:i32=22; const ENOSPC:i32=28; const EOVERFLOW:i32=75;
{tsq_backing()}
struct G17PLogicalQueueContext {{ work_states: g17_work_state::WorkStatePool, tsq:TsqLock, tsq_base:u64 }}
// Diagnostic head views are not used by the allocation/Drop tests below.
impl G17PLogicalQueueContext {{
 fn tsq_head_gpu_va(&self)->u64{{self.tsq_base+0x4400}}
 fn tsq_head_snapshot(&self)->Result<[u64;2]>{{Err(EINVAL)}}
}}
{lease}
fn context() -> Arc<G17PLogicalQueueContext> {{
    Arc::new(G17PLogicalQueueContext {{ work_states: g17_work_state::WorkStatePool::new(), tsq:TsqLock::new(), tsq_base:0x10000 }})
}}
#[test]
fn queued_groups_are_lazy_and_all_engines_share_one_state() {{
    let context = context();
    let queued: Vec<_> = (0..1000).map(|_| Arc::new(G17PWorkStateLease::deferred(context.clone()))).collect();
    assert!(context.work_states.has_free());
    let keys: Vec<_> = (0..63).map(|_| G17PWorkStateLease::new(context.clone()).unwrap()).collect();
    assert!(!context.work_states.has_free());
    drop(keys);
    let group = queued[0].clone();
    let key = group.acquire().unwrap();
    std::thread::scope(|scope| {{
        for _ in 0..8 {{
            let group = group.clone();
            scope.spawn(move || assert_eq!(group.acquire(), Ok(key)));
        }}
    }});
    let render = group.clone();
    let compute = group.clone();
    drop(group); drop(queued); drop(render);
    let remaining: Vec<_> = (0..62).map(|_| G17PWorkStateLease::new(context.clone()).unwrap()).collect();
    assert!(!context.work_states.has_free());
    drop(compute);
    assert!(G17PWorkStateLease::new(context.clone()).is_ok());
    drop(remaining);
}}
#[test]
fn concurrent_first_acquisition_uses_one_group_key() {{
    let context = context();
    let others: Vec<_> = (0..62).map(|_| G17PWorkStateLease::new(context.clone()).unwrap()).collect();
    for _ in 0..100 {{
        let group = Arc::new(G17PWorkStateLease::deferred(context.clone()));
        let start = Arc::new(std::sync::Barrier::new(8));
        std::thread::scope(|scope| {{
            let mut threads = Vec::new();
            for _ in 0..8 {{
                let group = group.clone(); let start = start.clone();
                threads.push(scope.spawn(move || {{
                    start.wait();
                    loop {{
                        if let Ok(word) = group.acquire() {{ break word; }}
                        // The winner may own the last pool slot just before
                        // publishing its shared word. Admission retries.
                        std::thread::yield_now();
                    }}
                }}));
            }}
            for thread in threads {{ assert_eq!(thread.join().unwrap(), group.word()); }}
        }});
        assert!(!context.work_states.has_free());
        drop(group);
        assert!(context.work_states.has_free());
    }}
    drop(others);
}}
#[test]
fn cancellation_retains_state_until_exact_retirement_owner_drops() {{
    let context = context();
    let mut live: Vec<_> = (0..63).map(|_| G17PWorkStateLease::new(context.clone()).unwrap()).collect();
    let published = live.pop().unwrap();
    let packet = Arc::new(published);
    let runtime = packet.clone();
    drop(packet);
    assert!(!context.work_states.has_free());
    drop(runtime);
    let replacement = G17PWorkStateLease::new(context.clone()).unwrap();
    assert_eq!(replacement.word() & 63, 62);
    drop(replacement);
    drop(live);
    for _ in 0..63 {{ assert!(context.work_states.allocate().is_some()); }}
}}
#[test]
fn independent_publishers_never_share_a_live_state() {{
    let context = context();
    let live = Arc::new(std::sync::Mutex::new([false; 64]));
    std::thread::scope(|scope| {{
        for _ in 0..8 {{
            let context = context.clone();
            let live = live.clone();
            scope.spawn(move || {{
                for _ in 0..5000 {{
                    let lease = loop {{
                        if let Ok(lease) = G17PWorkStateLease::new(context.clone()) {{ break lease; }}
                        std::thread::yield_now();
                    }};
                    let index = (lease.word() & 63) as usize;
                    {{ let mut bits = live.lock().unwrap(); assert!(!bits[index]); bits[index] = true; }}
                    std::thread::yield_now();
                    {{ let mut bits = live.lock().unwrap(); assert!(bits[index]); bits[index] = false; }}
                    drop(lease);
                }}
            }});
        }}
    }});
    let all: Vec<_> = (0..63).map(|_| G17PWorkStateLease::new(context.clone()).unwrap()).collect();
    assert!(!context.work_states.has_free());
    drop(all);
}}
'''
with tempfile.TemporaryDirectory(prefix="g17-work-key-") as tmp:
    binary = Path(tmp) / "tests"
    # Optimized build catches release hidden inside debug_assert!.
    subprocess.run(["rustc", "--edition=2021", "-O", "--test", "-", "-o", str(binary)],
                   input=source, text=True, check=True)
    subprocess.run([str(binary)], check=True)
subprocess.run(["python3", str(root / "tests/test_g17_fragment_pb_owner.py")], check=True)
print("PASS context-owned mixed-engine work states, retirement, contention, wrap and register keys")
