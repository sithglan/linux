#!/usr/bin/env python3
"""Queued cancellation and published teardown keep exact render ownership."""
from pathlib import Path

source = (Path(__file__).resolve().parents[1] / "g17_drm.rs").read_text()


def item(text, marker):
    start = text.index(marker)
    pos = text.index("{", start) + 1
    depth = 1
    while depth:
        depth += (text[pos] == "{") - (text[pos] == "}")
        pos += 1
    return text[start:pos]


wait = item(source, "fn wait_for_render_teardown(")
backend = item(source, "impl g17_job::G17PRenderQueueBackend for G17PRenderQueueBackend {")
timed_out = item(backend, "    fn timed_out(")
cancel = item(backend, "    fn cancel(")

assert "false" in wait and "msecs_to_jiffies(2000)" in wait
assert cancel.index("packet.request_cancel()") < cancel.index("wait_for_render_teardown")
assert cancel.index("wait_for_render_teardown") < cancel.index("timeout_deferred_render")
assert "packet.completion().fail_unpublished(ECANCELED)" in cancel
assert "packet.is_published()" in cancel
assert "packet.request_cancel()" in timed_out
assert "packet.is_published()" in timed_out
assert "timeout_deferred_render" in timed_out

completion = item(source, "impl G17PRenderCompletion {")
fail_unpublished = item(completion, "    pub(crate) fn fail_unpublished(")
assert fail_unpublished.index("drop(self.lease.lock().take())") < fail_unpublished.index(
    "drop(self.vm_job.lock().take())"
)
assert fail_unpublished.index("drop(self.vm_job.lock().take())") < fail_unpublished.index(
    "self.fence.signal()"
)

job = (Path(__file__).resolve().parents[1] / "g17_job.rs").read_text()
packet = item(job, "impl G17PRenderJobPacket {")
assert "compare_exchange(0, 3" in packet
assert packet.index("cancel_requested.store(1") < packet.index("compare_exchange(0, 3")
assert "compare_exchange(0, 1" in packet
assert "publication_state.store(2" in packet

scheduler = item(job, "impl<B: G17PRenderQueueBackend> G17PRenderScheduler<B> {")
enqueue = item(scheduler, "    pub(crate) fn enqueue(")
assert enqueue.index("let hardware = packet.fence();") < enqueue.index("self.entity.new_job")
# enqueue now returns publication and retirement witnesses separately. Keep
# requiring the hardware fence for completion; the scheduled fence may only
# unlock firmware dependency publication, never release the render resources.
assert "let published = job.fences().scheduled();" in enqueue
assert "Ok(G17PCommandFences { ready: published, completed: hardware })" in enqueue
assert "job.fences().finished()" not in enqueue

print("PASS async render teardown: queued cancellation releases unpublished ownership; published cancellation waits before exact timeout/quarantine")
