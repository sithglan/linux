#!/usr/bin/env python3
"""A signalled error dependency must never publish asynchronous compute."""
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
drm = (root / "g17_drm.rs").read_text()
job = (root / "g17_job.rs").read_text()


def block(text, marker):
    a = text.index(marker)
    i = text.index("{", a) + 1
    depth = 1
    while depth:
        depth += (text[i] == "{") - (text[i] == "}")
        i += 1
    return text[a:i]


packet = block(job, "pub(crate) struct G17PComputeJobPacket {")
assert "input_fences: KVec<Fence>" in packet
backend = block(drm, "impl g17_job::G17PQueueBackend for G17PAsyncQueueBackend {")
check = backend.index("for fence in packet.input_fences()")
assert check < backend.index("self.registry.acquire_compute(")
assert check < backend.index("publish_async_compute(")
submit = block(drm, "    fn submit_compute_async(")
assert "checked_dependencies.push(fence.clone()" in submit
assert "append_render_prefix_dependencies(" in submit
assert "&self.render_frontier, translated.render_wait_through" in submit
assert "for fence in packet.firmware_fences()" in backend
assert submit.index("checked_dependencies.push(fence.clone()") < submit.index(
    ".enqueue(packet, dependencies)?"
)

rust = r'''
#[derive(Clone,Copy)] struct Fence(i32);
#[derive(Default)] struct Backend { leases:u32, publications:u32 }
impl Backend {
 fn publish(&mut self, dependencies:&[Fence])->Result<(),i32>{
  for fence in dependencies {
   if fence.0 < 0 { return Err(fence.0); }
  }
  self.leases+=1;
  self.publications+=1;
  Ok(())
 }
}
#[test] fn signalled_error_dependency_is_ready_but_never_published() {
 let mut backend=Backend::default();
 assert_eq!(backend.publish(&[Fence(-5)]),Err(-5));
 assert_eq!((backend.leases,backend.publications),(0,0));
}
#[test] fn all_successful_dependencies_publish_exactly_once() {
 let mut backend=Backend::default();
 assert_eq!(backend.publish(&[Fence(1),Fence(1)]),Ok(()));
 assert_eq!((backend.leases,backend.publications),(1,1));
}
'''

with tempfile.TemporaryDirectory() as d:
    source = Path(d) / "test.rs"
    source.write_text(rust)
    exe = Path(d) / "test"
    subprocess.run(["rustc", "--edition=2021", "--test", str(source), "-o", str(exe)], check=True)
    subprocess.run([str(exe), "--test-threads=1"], check=True)

print("PASS compute dependency errors are consumed before lease/publication")
