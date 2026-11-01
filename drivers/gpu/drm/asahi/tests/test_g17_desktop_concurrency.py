#!/usr/bin/env python3
"""Bounded multi-process stress for G17P VM, queue, and fence ownership."""

from collections import deque
from dataclasses import dataclass, field
import multiprocessing as mp
import os
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MMU = (ROOT / "mmu.rs").read_text()
DRM = (ROOT / "g17_drm.rs").read_text()
ASYNC = (ROOT / "g17_async_compute.rs").read_text()
RESOURCES = (ROOT / "g17_resources.rs").read_text()


def block(source, marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


# Bind the composition oracle to the production ordering it models. The
# smaller extraction tests exercise each method; this test exercises their
# combined contract under independent process/VM/queue lifetimes.
commit = block(MMU, "    pub(crate) fn commit_prepared_user_bind_batch(")
enter_commit = block(MMU, "    fn enter_user_mapping_commit(")
defer_blocked = block(MMU, "    fn defer_blocked_user_bind_batch(")
assert "exec_lock_array" not in commit
assert commit.index("exec_lock(Some(&map.gem), true)") < commit.index(
    "enter_user_mapping_commit"
)
assert "gate.blocked" in enter_commit
assert "mapping_gate.state.lock()" in defer_blocked
assert commit.count("defer_blocked_user_bind_batch(batch)?") == 2
assert "core::mem::drop(inner)" in commit and "core::mem::drop(commit)" in commit
quarantine = block(MMU, "    pub(crate) fn quarantine(")
assert quarantine.index("gate.blocked = true") < quarantine.index(
    "job_lifetime.lock().quarantined = true"
)

render_failure = block(DRM, "    pub(crate) fn fail_while_owned(")
for before, after in (
    ("vm_job.quarantine()", "quarantine_render_slot"),
    ("quarantine_render_slot", "self.status.record"),
    ("self.status.record", "self.fence.signal()"),
):
    assert render_failure.index(before) < render_failure.index(after)

queue_drop = block(DRM, "impl Drop for G17PQueue")
assert queue_drop.index("drop(self.render_scheduler.take())") < queue_drop.index(
    ".release_render_owner("
)
assert queue_drop.index("drop(self.async_scheduler.take())") < queue_drop.index(
    ".release_async_compute_queue("
)
failed_submit = block(DRM, "    fn note_submission_failure(")
assert failed_submit.index("self.vm.status().record") < failed_submit.index(
    ".fail_outstanding_jobs_for_vm("
)
failed_jobs = block(ASYNC, "    pub(crate) fn fail_outstanding_jobs_for_vm(")
assert "fail_outstanding_jobs_for_vm_after_compute_observation" in failed_jobs
failed_jobs_after_observation = block(
    ASYNC, "    fn fail_outstanding_jobs_for_vm_after_compute_observation("
)
assert "active = None" not in failed_jobs_after_observation and \
    ".fail_while_owned(error)" in failed_jobs_after_observation
release_compute = block(ASYNC, "    pub(crate) fn release_async_compute_queue(")
assert "q.inflight.is_none() && !q.quarantined" in release_compute
stop_compute = block(ASYNC, "    fn release_async_compute_after_processor_stop(")
assert stop_compute.index("completion.complete(Err(EIO))") < stop_compute.index(
    "completion.release_after_processor_stop()"
)
assert stop_compute.index("completion.release_after_processor_stop()") < stop_compute.index(
    "active._lease.rehabilitate_after_processor_stop()"
)
assert stop_compute.index("rehabilitate_after_processor_stop") < stop_compute.index(
    "self.async_compute_queues.clear()"
)

pair = block(RESOURCES, "impl G17PRenderQueuePair")
assert "let tiling = slot as u16 * 2" in pair
assert "fragment: tiling + 1" in pair


class InvariantError(AssertionError):
    pass


@dataclass
class Options:
    post_lock_recheck: bool = True
    quarantine_before_fence: bool = True
    qid_from_slot: bool = True
    close_retains_jobs: bool = True
    sweep_failed_jobs: bool = True


@dataclass
class Client:
    index: int
    incarnation: int = 0
    open: bool = True
    lost: bool = False
    close_pending: bool = False
    status: int = 0
    mappings: set = field(default_factory=set)
    deferred_unmaps: set = field(default_factory=set)

    @property
    def vm(self):
        return 1000 + self.index * 100 + self.incarnation

    @property
    def queue(self):
        return 10_000 + self.index * 100 + self.incarnation

    @property
    def context(self):
        return 5 + self.index * 8 + self.incarnation


@dataclass
class Fence:
    signalled: bool = False
    error: int = 0
    status_at_signal: int = 0
    mapped_at_signal: bool = True


@dataclass
class Job:
    ident: tuple
    client: int
    vm: int
    queue: int
    context: int
    token: int
    engine: str
    fence: Fence = field(default_factory=Fence)
    slot: int | None = None
    qids: tuple | None = None
    qid: int | None = None
    published: bool = False
    quarantined: bool = False


class DesktopKernel:
    """Small executable ownership model, deliberately not a GPU simulator."""

    def __init__(self, options=Options()):
        self.options = options
        self.clients = [Client(i) for i in range(4)]
        self.jobs = {}
        self.render_slots = [None, None]
        self.render_quarantined = set()
        self.render_affinity = {}
        self.render_waiters = deque()
        self.compute = {qid: {"owner": None, "job": None, "quarantined": False}
                        for qid in range(4, 8)}
        self.map_epoch = 0
        self.lost_epoch = {}
        self.completed = 0

    def client(self, index):
        return self.clients[index]

    def reopen(self, index):
        old = self.client(index)
        if self._jobs_for_vm(old.vm):
            raise InvariantError("reopened a VM while firmware still owned its jobs")
        self.clients[index] = Client(index, old.incarnation + 1)

    def map_pair(self, index, token, prepared_before_loss=False):
        client = self.client(index)
        if not client.open:
            raise InvariantError("closed client mapped memory")
        if client.lost and (self.options.post_lock_recheck or not prepared_before_loss):
            return False
        client.mappings.update(((token, "low"), (token, "shadow")))
        self.map_epoch += 1
        if client.lost and self.map_epoch > self.lost_epoch[client.vm]:
            raise InvariantError("VM_BIND committed after its error fence/quarantine boundary")
        return True

    def unmap_pair(self, index, token):
        client = self.client(index)
        if self._jobs_for_token(client.vm, token):
            if client.lost:
                client.deferred_unmaps.add(token)
                return
            raise InvariantError("healthy client issued an unsynchronised active unbind")
        client.mappings.discard((token, "low"))
        client.mappings.discard((token, "shadow"))

    def _mapped(self, job):
        client = self.client(job.client)
        return ((job.token, "low") in client.mappings
                and (job.token, "shadow") in client.mappings)

    def submit_render(self, index, token):
        client = self.client(index)
        job = Job((client.vm, token, "render"), index, client.vm, client.queue,
                  client.context, token, "render")
        if not self._mapped(job):
            raise InvariantError("render was admitted without its lower/shadow bind pair")
        self.jobs[job.ident] = job
        slot = self._free_render_slot(client.queue)
        if slot is None:
            self.render_waiters.append(job.ident)
        else:
            self._publish_render(job, slot)
        return job.ident

    def _free_render_slot(self, owner):
        preferred = self.render_affinity.get(owner)
        if (preferred is not None and preferred not in self.render_quarantined
                and self.render_slots[preferred] is None):
            return preferred
        for slot in range(2):
            if slot not in self.render_quarantined and self.render_slots[slot] is None:
                return slot
        return None

    def _publish_render(self, job, slot):
        job.slot = slot
        job.qids = (slot * 2, slot * 2 + 1) if self.options.qid_from_slot else (0, 1)
        job.published = True
        self.render_slots[slot] = job.ident
        self.render_affinity[job.queue] = slot
        self.check()

    def _dispatch_render_waiter(self):
        if not self.render_waiters:
            return
        for _ in range(len(self.render_waiters)):
            ident = self.render_waiters.popleft()
            job = self.jobs[ident]
            slot = self._free_render_slot(job.queue)
            if slot is None:
                self.render_waiters.appendleft(ident)
                return
            self._publish_render(job, slot)

    def submit_compute(self, index, token):
        client = self.client(index)
        job = Job((client.vm, token, "compute"), index, client.vm, client.queue,
                  client.context, token, "compute")
        if not self._mapped(job):
            raise InvariantError("compute was admitted without its lower/shadow bind pair")
        candidate = next((qid for qid, state in self.compute.items()
                          if state["owner"] == client.queue and state["job"] is None
                          and not state["quarantined"]), None)
        if candidate is None:
            candidate = next((qid for qid, state in self.compute.items()
                              if state["owner"] is None and not state["quarantined"]), None)
        if candidate is None:
            raise InvariantError("bounded compute pool exhausted in four-client test")
        state = self.compute[candidate]
        state["owner"] = client.queue
        state["job"] = job.ident
        job.qid = candidate
        job.published = True
        self.jobs[job.ident] = job
        self.check()
        return job.ident

    def complete(self, ident):
        job = self.jobs[ident]
        if not job.published or job.quarantined:
            raise InvariantError("completed an unpublished or quarantined job")
        if job.engine == "render":
            self.render_slots[job.slot] = None
        else:
            state = self.compute[job.qid]
            state["job"] = None
            client = self.client(job.client)
            if not client.open:
                state["owner"] = None
        job.fence.signalled = True
        del self.jobs[ident]
        self.completed += 1
        self._settle_client_close(job.client, job.vm)
        if job.engine == "render":
            self._dispatch_render_waiter()
        self.check()

    def close(self, index):
        client = self.client(index)
        client.open = False
        client.close_pending = True
        if not self.options.close_retains_jobs:
            client.mappings.clear()
        for state in self.compute.values():
            if state["owner"] == client.queue and state["job"] is None:
                state["owner"] = None
        self._settle_client_close(index, client.vm)
        self.check()

    def terminal_submit_failure(self, index):
        client = self.client(index)
        output = Fence()
        if self.options.quarantine_before_fence:
            self._mark_vm_lost(client)
            if self.options.sweep_failed_jobs:
                self._fail_jobs_for_vm(client.vm)
            self._signal_error(output, client, None)
        else:
            self._signal_error(output, client, None)
            self._mark_vm_lost(client)
            if self.options.sweep_failed_jobs:
                self._fail_jobs_for_vm(client.vm)
        if not self.options.sweep_failed_jobs and self._jobs_for_vm(client.vm):
            raise InvariantError("terminal submit left an earlier hardware fence unsignalled")
        self.check()
        return output

    def _mark_vm_lost(self, client):
        client.lost = True
        client.status = -5
        self.lost_epoch[client.vm] = self.map_epoch

    def _fail_jobs_for_vm(self, vm):
        for job in list(self.jobs.values()):
            if job.vm != vm or job.quarantined:
                continue
            job.quarantined = True
            if job.engine == "render":
                if job.slot is not None:
                    self.render_quarantined.add(job.slot)
            else:
                self.compute[job.qid]["quarantined"] = True
            self._signal_error(job.fence, self.client(job.client), job)

    def _signal_error(self, fence, client, job):
        fence.error = -5
        fence.status_at_signal = client.status
        fence.mapped_at_signal = True if job is None else self._mapped(job)
        fence.signalled = True
        if fence.status_at_signal == 0:
            raise InvariantError("error fence woke before sticky VM status")
        if not fence.mapped_at_signal:
            raise InvariantError("error fence woke after firmware-owned mappings were freed")

    def processor_stop(self):
        for ident, job in list(self.jobs.items()):
            if not job.quarantined:
                continue
            if job.engine == "render" and job.slot is not None:
                self.render_slots[job.slot] = None
            if job.engine == "compute":
                state = self.compute[job.qid]
                state["job"] = None
                state["owner"] = None
                state["quarantined"] = False
            del self.jobs[ident]
            self._settle_client_close(job.client, job.vm)
        self.render_quarantined.clear()
        for state in self.compute.values():
            state["quarantined"] = False
        for client in self.clients:
            for token in tuple(client.deferred_unmaps):
                client.mappings.discard((token, "low"))
                client.mappings.discard((token, "shadow"))
            client.deferred_unmaps.clear()
            self._settle_client_close(client.index, client.vm)
        self._dispatch_render_waiter()
        self.check()

    def _jobs_for_vm(self, vm):
        return [job for job in self.jobs.values() if job.vm == vm]

    def _jobs_for_token(self, vm, token):
        return [job for job in self.jobs.values() if job.vm == vm and job.token == token]

    def _settle_client_close(self, index, vm):
        client = self.client(index)
        if client.vm != vm or not client.close_pending or self._jobs_for_vm(vm):
            return
        client.mappings.clear()
        client.deferred_unmaps.clear()
        client.close_pending = False

    def check(self):
        seen_qids = set()
        for job in self.jobs.values():
            if job.published and not self._mapped(job):
                raise InvariantError("published job lost its VM_BIND backing")
            if not job.published:
                continue
            client = self.client(job.client)
            if job.vm != client.vm or job.context != client.context:
                raise InvariantError("job context was rebound to another VM incarnation")
            if job.engine == "render":
                expected = (job.slot * 2, job.slot * 2 + 1)
                if job.qids != expected:
                    raise InvariantError("render QID pair disagrees with its physical slot")
                if self.render_slots[job.slot] != job.ident:
                    raise InvariantError("render slot lost its exact completion owner")
                seen_qids.update(job.qids)
            else:
                if job.qid < 4 or job.qid in seen_qids:
                    raise InvariantError("compute QID collided with render or another compute")
                if self.compute[job.qid]["job"] != job.ident:
                    raise InvariantError("compute QID lost its exact packet")
                seen_qids.add(job.qid)
            if job.quarantined and not job.fence.signalled:
                raise InvariantError("quarantined job left a syncobj waiter behind")


def workload_process(index, rounds, output):
    plans = []
    for generation in range(rounds):
        token = index * 100_000 + generation
        plans.append((generation, token))
    output.put((index, os.getpid(), plans))


def collect_workloads(rounds=12):
    context = mp.get_context("spawn")
    output = context.Queue()
    processes = [context.Process(target=workload_process, args=(i, rounds, output))
                 for i in range(4)]
    for process in processes:
        process.start()
    records = [output.get(timeout=5) for _ in processes]
    for process in processes:
        process.join(5)
        if process.is_alive():
            process.terminate()
            process.join()
            raise AssertionError("workload producer did not terminate")
        assert process.exitcode == 0
    assert len({pid for _, pid, _ in records}) == 4
    return {index: plans for index, _, plans in records}


def run_stress(workloads):
    kernel = DesktopKernel()
    for generation in range(len(workloads[0])):
        tokens = [workloads[index][generation][1] for index in range(4)]
        for index, token in enumerate(tokens):
            kernel.map_pair(index, token)
        renders = [kernel.submit_render(index, token) for index, token in enumerate(tokens)]
        computes = [kernel.submit_compute(index, token) for index, token in enumerate(tokens)]

        # Two physical pairs run first, then their retirement admits the two
        # waiting clients without sharing a context or QID pair.
        kernel.complete(renders[0])
        kernel.complete(renders[1])
        if generation == 5:
            # A terminal failure in client 2 sweeps both its already-published
            # render and compute, then file close/unbind waits for processor stop.
            kernel.terminal_submit_failure(2)
            kernel.close(2)
            kernel.unmap_pair(2, tokens[2])
            kernel.complete(renders[3])
            for index in (3, 1, 0):
                kernel.complete(computes[index])
            kernel.processor_stop()
            kernel.reopen(2)
        else:
            kernel.complete(renders[2])
            kernel.complete(renders[3])
            # Queue/file destruction while compute owns QID/context must only
            # mark the graph released. Completion performs the actual release.
            victim = generation % 4
            kernel.close(victim)
            for index in (3, 2, 1, 0):
                kernel.complete(computes[index])
            kernel.reopen(victim)

        for index, token in enumerate(tokens):
            if kernel.client(index).open:
                kernel.unmap_pair(index, token)
        kernel.check()

    assert kernel.completed == 12 * 8 - 2
    assert not kernel.jobs and not kernel.render_waiters
    assert all(not client.mappings for client in kernel.clients)
    assert all(state["job"] is None for state in kernel.compute.values())


def expect_negative(option, scenario):
    options = Options()
    setattr(options, option, False)
    try:
        scenario(DesktopKernel(options))
    except InvariantError:
        return
    raise AssertionError(f"negative control {option} unexpectedly passed")


def negative_controls():
    def bind_race(kernel):
        kernel.map_pair(0, 1)
        kernel.submit_render(0, 1)
        kernel.terminal_submit_failure(0)
        kernel.map_pair(0, 2, prepared_before_loss=True)

    def fence_order(kernel):
        kernel.map_pair(0, 1)
        kernel.submit_compute(0, 1)
        kernel.terminal_submit_failure(0)

    def qid_identity(kernel):
        kernel.map_pair(0, 1)
        kernel.map_pair(1, 2)
        kernel.submit_render(0, 1)
        kernel.submit_render(1, 2)

    def close_lifetime(kernel):
        kernel.map_pair(0, 1)
        kernel.submit_compute(0, 1)
        kernel.close(0)

    def failed_sweep(kernel):
        kernel.map_pair(0, 1)
        kernel.submit_render(0, 1)
        kernel.submit_compute(0, 1)
        kernel.terminal_submit_failure(0)

    expect_negative("post_lock_recheck", bind_race)
    expect_negative("quarantine_before_fence", fence_order)
    expect_negative("qid_from_slot", qid_identity)
    expect_negative("close_retains_jobs", close_lifetime)
    expect_negative("sweep_failed_jobs", failed_sweep)


if __name__ == "__main__":
    run_stress(collect_workloads())
    negative_controls()
    print("PASS 4-process/12-generation desktop ownership stress: 94 retired jobs; "
          "VM_BIND quarantine, output fences, QID/context identity, close, and stop drain")
