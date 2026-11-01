#!/usr/bin/env python3
"""Input/publication dependencies precede acquiring a physical slot."""
from pathlib import Path
import subprocess
import tempfile

source = (Path(__file__).resolve().parents[1] / "g17_drm.rs").read_text()


def function(text, marker):
    start = text.index(marker)
    pos = text.index("{", start) + 1
    depth = 1
    while depth:
        depth += (text[pos] == "{") - (text[pos] == "}")
        pos += 1
    return text[start:pos]


submit = function(source, "    fn submit_render_async(")
backend = function(source, "impl g17_job::G17PRenderQueueBackend for G17PRenderQueueBackend")
assert submit.index("dependencies.push(fence") < submit.index(".enqueue(packet, dependencies)?")
assert submit.index("append_render_prefix_dependencies(") < submit.index(".enqueue(packet, dependencies)?")
prefix = function(source, "fn append_render_prefix_dependencies(")
assert "dependencies.push(fence.published.clone()" in prefix
assert "checked_dependencies.push(fence.completed.clone()" in prefix
assert "acquire_lease" not in submit, "submit ioctl still consumes a physical render slot"
assert backend.index("for fence in packet.input_fences()") < backend.index(
    "self.registry.acquire_lease(self.owner)?"
)
assert backend.index("self.registry.acquire_lease(self.owner)?") < backend.index(
    "runtime.submit_translated_render("
)

rust = r'''
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq, Eq)] struct Fence(u64);
#[derive(Clone, Copy)] struct Job { id:u64, dependency:Option<Fence> }
#[derive(Default)] struct Scheduler {
    queued:VecDeque<Job>, signalled:Vec<Fence>, slot:Option<u64>, published:Vec<u64>
}
impl Scheduler {
    fn enqueue(&mut self, job:Job) { self.queued.push_back(job); }
    fn signal(&mut self, fence:Fence) { self.signalled.push(fence); }
    fn drive(&mut self) {
        let Some(job)=self.queued.front().copied() else{return};
        if job.dependency.is_some_and(|f|!self.signalled.contains(&f)){return}
        if self.slot.is_some(){return}
        self.slot=Some(job.id);self.published.push(job.id);self.queued.pop_front();
    }
    fn complete(&mut self,id:u64){assert_eq!(self.slot.take(),Some(id));self.signal(Fence(id));}
}
#[test] fn queued_dependency_does_not_hold_the_slot() {
 let mut s=Scheduler::default();s.enqueue(Job{id:2,dependency:Some(Fence(1))});s.drive();
 assert_eq!(s.slot,None);assert!(s.published.is_empty());
 s.signal(Fence(1));s.drive();assert_eq!(s.slot,Some(2));
}
#[test] fn ten_thousand_turnovers_preserve_order_and_release() {
 let mut s=Scheduler::default();let mut prior=None;
 for id in 1..=10_000 {s.enqueue(Job{id,dependency:prior});s.drive();assert_eq!(s.slot,Some(id));s.complete(id);prior=Some(Fence(id));}
 assert_eq!(s.published.len(),10_000);assert_eq!(s.slot,None);assert!(s.queued.is_empty());
}
#[test] fn independent_scheduler_can_publish_while_peer_waits() {
 let mut waiting=Scheduler::default();let mut ready=Scheduler::default();
 waiting.enqueue(Job{id:2,dependency:Some(Fence(1))});ready.enqueue(Job{id:7,dependency:None});
 waiting.drive();ready.drive();assert_eq!(waiting.slot,None);assert_eq!(ready.slot,Some(7));
}
'''
with tempfile.TemporaryDirectory() as directory:
    directory = Path(directory)
    src = directory / "ready.rs"
    exe = directory / "ready"
    src.write_text(rust)
    subprocess.run(["rustc", "--edition=2021", "--test", str(src), "-o", str(exe)], check=True)
    subprocess.run([str(exe), "--test-threads=1"], check=True)

print("PASS async render admission: dependencies precede slot acquisition; 10,000 ordered turnovers; independent queues remain runnable")
