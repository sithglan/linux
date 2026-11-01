#!/usr/bin/env python3
"""Guard the kernel-stack depth of render publication (module frames only).

Run against the built asahi.ko (or partially linked asahi.o). Neo's kernel
stack is 32 KiB (arm64 MIN_THREAD_SHIFT 15, 16K pages, VMAP_STACK); an
overflow there is a silent watchdog-reset hang. Render publication can run
on the submit ioctl's own stack (early publication) and, on a pair's first
use, builds and first-installs that pair inside the same call chain.

Frames are the immediate prologue allocations; chains follow direct calls
through named waypoints and take the deepest module-internal continuation
below the last one. Kernel frames above (syscall entry, drm_ioctl) and below
(printk, scheduling, allocation) are not measured here; the budgets leave
at least 8 KiB of the 32 KiB for them plus margin. `g17p_stack_watch`
measures the real high-water mark on hardware.
"""
import argparse
import re
import subprocess
import sys
from pathlib import Path

sys.setrecursionlimit(100000)

IOCTL = [r'G17PQueueNtNtB7_5queue5Queue6submit$', r'G17PQueue19submit_render_async$',
         r'17try_publish_early$', r'G17PRenderQueueBackend12publish_with$',
         r'G17PRenderQueueBackend12publish_withs_00Bb_$']
WORKER = [r'G17PRenderQueueBackend12publish_with$', r'G17PRenderQueueBackend12publish_withs_00Bb_$']
INSTALL = [r'G17PLiveRuntime24submit_translated_render$', r'23begin_translated_renderNC']
# (name, waypoints, module-frame budget in bytes)
CHAINS = [
    ('ioctl first-install with power bracket',
     IOCTL + INSTALL + [r'ensure_g17p_render_registration_powers0_0'], 17 * 1024),
    ('ioctl first-install KSMFE configure',
     IOCTL + INSTALL + [r'configure_fresh_nonbootstrap_render_pair$'], 17 * 1024),
    ('ioctl sibling warm build', IOCTL + [r'G17PWarmRenderGraph5build$'], 21 * 1024),
    ('worker warm build', WORKER + [r'G17PWarmRenderGraph5build$'], 18 * 1024),
    ('worker first-install with power bracket',
     WORKER + INSTALL + [r'ensure_g17p_render_registration_powers0_0'], 14 * 1024),
]


def analyze(path):
    output = subprocess.check_output(
        ['llvm-objdump', '-dr', '--no-show-raw-insn', str(path)], text=True)
    frame, calls, current, prologue = {}, {}, None, False
    for line in output.splitlines():
        header = re.match(r'^[0-9a-f]+ <(.+)>:$', line)
        if header:
            current = header.group(1)
            frame[current], calls[current], prologue = 0, set(), True
            continue
        if current is None:
            continue
        if 'R_AARCH64_CALL26' in line or 'R_AARCH64_JUMP26' in line:
            calls[current].add(re.sub(r'\+0x[0-9a-f]+$', '', line.split()[-1]))
            continue
        direct = re.search(r'\bbl\s+0x[0-9a-f]+ <([^+>]+)(\+0x[0-9a-f]+)?>', line)
        if direct:
            calls[current].add(direct.group(1))
        tail = re.search(r'\bb\s+0x[0-9a-f]+ <([^+>]+)>$', line)
        if tail and tail.group(1) != current:
            calls[current].add(tail.group(1))
        if prologue:
            sub = re.search(r'\bsub\s+sp, sp, #(0x[0-9a-f]+|\d+)', line)
            push = re.search(r'\b(?:stp\s+x\d+, x\d+|str\s+x\d+), \[sp, #-(0x[0-9a-f]+|\d+)\]!', line)
            if sub:
                frame[current] += int(sub.group(1), 0) << (12 if 'lsl #12' in line else 0)
            elif push:
                frame[current] += int(push.group(1), 0)
            elif re.search(r'\b(bl|b|ret|retaa|cbz|cbnz|tbz|tbnz|b\.\w+)\b', line):
                prologue = False
    return frame, calls


def deepest_below(frame, calls):
    memo, active = {}, set()
    def walk(node):
        if node in memo:
            return memo[node]
        if node not in frame or node in active:
            return 0
        active.add(node)
        value = frame[node] + max((walk(c) for c in calls.get(node, ())), default=0)
        active.discard(node)
        memo[node] = value
        return value
    return walk


def longest_to(frame, calls, entry, pattern):
    memo, active, target = {}, set(), re.compile(pattern)
    def walk(node):
        if node in memo:
            return memo[node]
        if node not in frame or node in active:
            return None
        if target.search(node):
            memo[node] = (frame[node], [node])
            return memo[node]
        active.add(node)
        best = None
        for callee in calls.get(node, ()):
            found = walk(callee)
            if found and (best is None or found[0] > best[0]):
                best = found
        active.discard(node)
        memo[node] = (frame[node] + best[0], [node] + best[1]) if best else None
        return memo[node]
    return walk(entry)


def chain(frame, calls, below, waypoints):
    starts = [name for name in frame if re.search(waypoints[0], name)]
    assert len(starts) == 1, (waypoints[0], len(starts))
    node, total, path = starts[0], frame[starts[0]], [starts[0]]
    for pattern in waypoints[1:]:
        found = longest_to(frame, calls, node, pattern)
        assert found, (node, pattern)
        total += found[0] - frame[node]
        path += found[1][1:]
        node = path[-1]
    return total + below(node) - frame[node], path


# Worker chain named in the 2026-10-02 review: the event worker publishing an
# iso4 REPLAY kick whose publication services firmware events inline and runs
# a recovery (whose diagnostics used to nest ~10 KiB of logging frames).
WORKER_CHAINS = [
    ('event worker replay publication -> inline recovery',
     [r'WorkItemPointerKy1_E3run', r'G17PLiveRuntime21publish_async_compute$',
      r'settle_open_recovery_before_publication$', r'service_primary_recovery(_body)?$'], 18 * 1024),
]

# Every entry into the module, measured exactly (below). Workqueue items and
# DRM scheduler callbacks run on worker stacks: 18 KiB. Ioctls and the rest:
# 20 KiB, except the submit ioctl, whose early sibling warm build is the
# 'ioctl sibling warm build' chain above (21 KiB budget; this adds the 560-byte
# ioctl wrapper frame that the waypoint chains start after).
WORKER, OTHER = 18 * 1024, 20 * 1024
ENTRIES = [('ioctl ' + name, r'IOCTLS\d+ASAHI_' + name + '$', 21 * 1024 if name == 'SUBMIT' else OTHER)
           for name in ['SUBMIT', 'VM_BIND', 'GEM_CREATE', 'GEM_BIND_OBJECT', 'GEM_MMAP_OFFSET',
                        'VM_CREATE', 'VM_DESTROY', 'QUEUE_CREATE', 'QUEUE_DESTROY', 'GET_PARAMS', 'GET_TIME']]
ENTRIES += [(f'workqueue item {item}', rf'AsahiDriverEEINtB5_15WorkItemPointerKy{item}_E3run', WORKER)
            for item in range(1, 8)]
ENTRIES += [
    ('closed-object drain worker', r'T8140ClosedObjectDrainEINtB5_15WorkItemPointerKy0_E3run', WORKER),
    ('compute run_job', r'run_job_cb.*G17PScheduledJob', WORKER),
    ('render run_job', r'run_job_cb.*G17PRenderScheduledJob', WORKER),
    ('compute free_job', r'free_job_cb.*G17PScheduledJob', WORKER),
    ('render free_job', r'free_job_cb.*G17PRenderScheduledJob', WORKER),
    ('compute timedout_job', r'timedout_job_cb.*G17PScheduledJob', WORKER),
    ('render timedout_job', r'timedout_job_cb.*G17PRenderScheduledJob', WORKER),
    ('compute cancel_job', r'cancel_job_cb.*G17PScheduledJob', WORKER),
    ('render cancel_job', r'cancel_job_cb.*G17PRenderScheduledJob', WORKER),
    ('rtkit recv_message', r'recv_message_callback.*G17PRtkitOps', OTHER),
    ('rtkit crashed', r'crashed_callback.*G17PRtkitOps', OTHER),
    ('gem close', r'gem14close_callback.*AsahiObject', OTHER),
    ('gem free', r'shmem.*AsahiObjectE13free_callback', OTHER),
    ('gpuvm vm_free', r'vm_free_callback', OTHER),
    ('file postclose', r'postclose_callback', OTHER),
    ('DrmGpu free_context', r'G17PDrmGpuNtNtB6_7drm_gpu6DrmGpu12free_context', OTHER),
]

# Calls through trait objects (`blr`) are invisible to the direct-call graph.
# The ioctls reach the G17P implementations of `DrmGpu` and `Queue` this way
# (file.rs); add exactly the methods each ioctl invokes.
_GPU = r'G17PDrmGpuNtNtB\w+DrmGpu\d+'
DYNAMIC_CALLS = [
    (r'IOCTLS16ASAHI_GET_PARAMS$', _GPU + r'(ids|independent_queue_limits|is_crashed|params|user_range|'
     r'supports_vm_status|supports_vm_status_mirror|supports_per_command_input_syncs|submission_error)$'),
    (r'IOCTLS15ASAHI_VM_CREATE$', _GPU + r'(user_range|new_vm|params|unknown_page)$'),
    (r'IOCTLS21ASAHI_GEM_BIND_OBJECT$', _GPU + r'map_timestamp_buffer$'),
    (r'IOCTLS18ASAHI_QUEUE_CREATE$', _GPU + r'new_queue$'),
    (r'IOCTLS12ASAHI_SUBMIT$', _GPU + r'(update_globals|ids)$'),
    (r'IOCTLS14ASAHI_GET_TIME$', _GPU + r'base_clock_hz$'),
    (r'IOCTLS12ASAHI_SUBMIT$', r'G17PQueueNtNtB7_5queue5Queue\w+$'),
    (r'IOCTLS19ASAHI_QUEUE_DESTROY$|postclose_callback', r'G17PQueueNtNtNtCs\w+Drop4drop$'),
]


def exact_depth(frame, calls):
    """Deepest module stack from each function: memoized over the acyclic
    condensation of the call graph, exhaustive over simple paths inside each
    recursive component (each function at most once per path; recursion
    deeper than that is not bounded statically and must be broken in code).
    Unlike `deepest_below`, the result does not depend on traversal order."""
    index, low, stack, on, comps, counter = {}, {}, [], set(), [], [0]
    def strong(v):
        index[v] = low[v] = counter[0]; counter[0] += 1
        stack.append(v); on.add(v)
        for w in calls.get(v, ()):
            if w not in frame or w == v:
                continue
            if w not in index:
                strong(w); low[v] = min(low[v], low[w])
            elif w in on:
                low[v] = min(low[v], index[w])
        if low[v] == index[v]:
            comp = []
            while True:
                w = stack.pop(); on.discard(w); comp.append(w)
                if w == v:
                    break
            comps.append(comp)
    for v in frame:
        if v not in index:
            strong(v)
    component = {v: i for i, comp in enumerate(comps) for v in comp}
    memo = {}
    def depth(v, visited):
        single = len(comps[component[v]]) == 1
        if single and v in memo:
            return memo[v]
        best = (0, [])
        for w in calls.get(v, ()):
            if w not in frame or w == v:
                continue
            if component[w] == component[v]:
                if w in visited:
                    continue
                found = depth(w, visited | {w})
            else:
                found = depth(w, frozenset([w]))
            if found[0] > best[0]:
                best = found
        result = (frame[v] + best[0], [v] + best[1])
        if single:
            memo[v] = result
        return result
    return lambda v: depth(v, frozenset([v]))


def short(node):
    return re.sub(r'_R[A-Za-z0-9]*?5asahi', '', node)[:110]


def audit(path):
    frame, calls = analyze(path)
    below = deepest_below(frame, calls)
    violations = []
    for name, waypoints, budget in CHAINS + WORKER_CHAINS:
        total, path_ = chain(frame, calls, below, waypoints)
        print(f'{name}: {total} bytes (budget {budget})')
        for node in path_:
            print(f'  {frame[node]:6d} {short(node)}')
        if total > budget:
            violations.append((name, total, budget))
    for source, target in DYNAMIC_CALLS:
        targets = {name for name in frame if re.search(target, name)}
        sources = [name for name in frame if re.search(source, name)]
        assert sources and targets, (source, target)
        for name in sources:
            calls.setdefault(name, set()).update(targets)
    exact = exact_depth(frame, calls)
    print('entry points (exact deepest module chain):')
    for name, pattern, budget in ENTRIES:
        nodes = [node for node in frame if re.search(pattern, node)]
        assert nodes, (name, pattern)
        total, path_ = max((exact(node) for node in nodes), key=lambda found: found[0])
        print(f'  {name}: {total} bytes (budget {budget})')
        if total > budget:
            for node in path_:
                print(f'    {frame[node]:6d} {short(node)}')
            violations.append((name, total, budget))
    assert not violations, violations
    print('PASS publication and entry-point stack budgets (module frames; kernel frames unmeasured)')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('object', type=Path)
    audit(parser.parse_args().object)
