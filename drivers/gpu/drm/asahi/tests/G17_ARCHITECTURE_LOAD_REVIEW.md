# G17 architecture and load review — 2026-09-19

This is the current review checkpoint. The corrected stable-channel candidate
builds and passes host regressions. The preceding candidate did not return to
the network after reboot; its compiled boot path exceeded the kernel stack.
Recovery and the unchanged three-client hardware reproducer are pending.
This is not a claim of hardware correctness, sustained throughput,
conformance, or freedom from security defects.

## Main finding

The pointer overview is useful for describing the boot objects and submission
graph, but it omits the lifetime and feedback paths that determine correctness
under load. The review found real fixed-pair assumptions in host ownership,
address placement, completion tracking, recovery routing, and packet validation.
The candidate removes those assumptions together across the kernel and Mesa.

The remaining architectural blocker is reclamation. A safe driver must distinguish
"the fence was signalled", "this job retired", "the QID lease is released", and
"firmware can no longer reference this channel/root". None implies all the others.

## Findings and disposition

| Priority | Finding | Evidence and disposition |
| --- | --- | --- |
| P1, corrected; hardware confirmation pending | Inline owner storage overflowed the boot stack. | The rejected candidate's inspected nested frames totalled 35,280 bytes against a 32,768-byte task stack, before outer callers. Lazy boxed render states, in-place boxed resource construction and preallocated runtime vectors reduce the largest inspected module-internal direct-call estimate to 20,864 bytes. Render switching drops from a 12,928-byte frame to 80 bytes and allocates nothing after an owner's first selection. The binary frame audit enforces budgets; it does not measure external/indirect/recursive calls. Network non-return is observed, but no serial failure PC was captured. |
| P1, corrected in host tests | Closing a bound, never-published compute graph stranded its QID and descriptor storage. | Close cleared the owner but retained the binding/context, which excluded the graph from the new reusable-empty filter. The manager now cancels only an exact unpublished lease, and close drops the graph only when inactive, unquarantined and without a previous binding. A production-method regression failed before the correction and passes 10,000 lifetimes afterward. Sealed leases are retained even if the hardware-state field has not advanced. |
| P1, corrected for create-only queues | Creating and closing unused logical queues retained a 16 KiB control page each. | Session retention moved from logical-context creation to the first render handoff/begin or CL preparation, before pointer exposure. Exact control-object markers make shared render/CL retention idempotent without a list scan. Tests cover 10,000 create/close cycles, reserve/push failure and processor-stop marker reset. Bound-but-unpublished controls remain conservatively retained; this does not implement native detach. |
| P1, open | Closed queues retain finite session resources. | `g17_channel.rs::cancel_unpublished` refuses published leases; `g17_async_compute.rs::release_async_compute_queue` retains installed mappings/contexts; render owner release marks a bit until processor stop. Repeated create/close can exhaust session capacity. This is both a workload limit and a potential denial-of-service avenue for an untrusted client. Native released-stamp eligibility and acknowledged detach are still needed. |
| P1, hardware qualification pending | Shared render-pair reassignment conflated logical ownership and channel state. | The registry now retains one slot per logical owner, while the manager lazily allocates TA/3D IDs from the same pool as CL. There is no idle-owner eviction or installed-CL reassignment to another owner. Host contention/exhaustion/lifetime tests pass. This does not yet establish that the r20 hang is fixed. |
| P1, addressed in candidate | Extending QIDs would overlap queue records with metadata. | Records retain `qid * 0xc0`; metadata was moved from the old low offsets to a prefix starting at `0x8000`. Tests check all 128 record positions against live graph regions. Cost: 32 KiB more per render queue graph. |
| P1, addressed in candidate | Completion/fault paths did not support the expanded ownership model. | `RenderFenceState` previously used an 8-bit mask; it now covers 64 owner slots. Type-4 fault routing previously inferred a fixed pair from the QID; it now resolves a validated current lease. Tests exercise owner 63, mixed-engine IDs and duplicate settlement. Compute packet construction now accepts nonbootstrap IDs 2/3 as well as 4..127. |
| P1, coordinated ABI requirement | Kernel and userspace must reserve the same AXFB range. | The shared reservation is now `[0x10002000000, 0x10002400000)`. The kernel, Mesa allocator and DRM shim were changed together. An old userspace allocator could place a BO in the expanded reservation. Both candidate artifacts must be deployed together. The ARM64 VA-layout regression passes under emulation. |
| P2, measure before redesign | One active hardware packet per compute owner requires retirement before the next publication. | `G17PAsyncComputeQueue.active` is one `Option`; `async_compute_dependency` exposes its fence, and publication rejects a second active packet. Independent owners can overlap. This may create host/GPU submission gaps for small jobs, but no throughput penalty is quantified yet. A deeper per-owner pipeline needs multiple exact completion owners, descriptor/stamp reuse guards and cancellation semantics; simply removing the check would be incorrect. |
| P2, measured allocation size; performance effect unknown | Compute descriptors reserve 4 MiB per created channel. | `G17P_COMPUTE_DESCRIPTOR_STORAGE_SIZE` is 256 slots times 16 KiB, allocated by `G17PComputeDescriptorStorage::new`. This excludes other queue objects and page tables. Coupled with retained closed channels it increases memory pressure. Reducing slots without matching the firmware geometry is not an acceptable optimization. Investigate lazy backing or a qualified ring-depth contract after lifecycle correctness. |
| P2, measure | First compute installation excludes existing render and compute leases. | `G17PQueueRegistry::acquire_compute(first_install)` preserves the existing transient installation barrier. Steady-state compute and independent renders overlap. Record installation latency separately from execution throughput; do not mistake an admission lease for a hardware execution lane. No new execution gate was added. |
| P2, corrected defensively | Fragment scratch staging could partially modify TA fields before rejecting a truncated fragment backing. | `stage_fragment_completion_scratch` now validates the fragment range before the first write. The production-method byte-preservation regression failed before this correction and passes afterward, including descriptor-slot wrap. Ordinary constructors allocate complete backing; no externally reachable truncation or r20 connection has been established. |

## Submission cost and synchronization review

`select_render_slot` lazily allocates one boxed owner state on first selection.
Repeated selection of another existing owner swaps fields using one reusable
empty scratch box. The host regression verifies every field participates and
6,400 switches allocate nothing. These swaps still copy host state; an 80-byte
stack frame is not a measured bandwidth or latency result. If owner alternation
is costly on hardware, keeping the active state behind a pointer is a separate
refactor to evaluate against measured CPU profiles.

The common QID table scans at most 128 entries on allocation; exact lease checks
are direct indexed lookups. Async owner lookup scans the retained queue vector,
and completion service makes two passes over eligible queues with one batched
completion-record scan between them. These are bounded by channel capacity,
but multiple scans per small submission may matter at high submit rates. Keep
the batch and completion ownership rules when considering a direct owner/QID
index; measure before adding a second mutable index and its recovery invariants.

The runtime mutex covers first graph construction, mappings, shared-ring writes
and completion servicing. Compute dependency waits and registry admission occur
outside the publication lock in `G17PAsyncQueueBackend::publish`. The async
completion poller returns `EAGAIN` when the required stamp has not arrived;
the legacy `await_compute_stamp` sleep loop is not that async retirement path.
First-use allocation can still delay other clients' host publication while
holding the shared mutex. Any future split should prepare private allocations
outside the lock, then revalidate session generation, owner and capacity before
publication; do not expose partial aliases or weaken shared-ring ordering.

Compute submission allocates completion/fence/packet ownership and two dependency
vectors before DRM scheduling. It does not allocate a new 4 MiB descriptor graph
per steady submission. Reusing pools could reduce small-job CPU overhead, but
recycling before exact retirement would trade correctness for apparent speed.

Optional submission accounting now leaves the production path before sampling
the host clock. With `g17p_perf_window=0`, render avoids both `Instant` reads and
compute avoids its ioctl-duration read as well as the already-skipped device-wide
accounting mutex. Mandatory control-ring backpressure adds one pending-state
check while the existing runtime mutex is held; the millisecond sleep and retry
bookkeeping run only after an observed full ring and after dropping that lock.

Measure CPU time, mutex wait/hold time, submits per second, p50/p99 completion
latency, GPU utilization and retained bytes separately for steady load, first
use, mixed owners and queue churn. No performance penalty or improvement has
yet been quantified on the corrected hardware candidate.

## Security boundaries requiring explicit verification

The inspected code enforces reserved-VA overlap checks, retains mappings behind
unresolved errors, validates exact channel lease identity, and protects against
stale leases after processor stop by keeping host identities monotonic. Host
tests cover these ownership properties. They are not a complete security proof.

Several client-root aliases intentionally permit GPU writes, including native
KSM entry peers and hardware-updated state. `pgtable.rs` distinguishes GPU and
firmware access and read/write permissions; `map_shared_render_range` creates
the matching lower-root views. A reserved VA prevents userspace VM bind/unbind
from replacing it, but that reservation alone does not establish what an
untrusted GPU shader can access. The reachable data and firmware's validation
of it need a dedicated adversarial test matrix. No cross-process write or
privilege escalation has been demonstrated by this review.

Required tests include same numerical VAs backed by different client BOs;
cross-client canaries during compute/render overlap; malformed command and
timestamp ranges; pending unmap/close; duplicate, stale and faulted completions;
and allocation failure at every prepublication ownership transition. All
unresolved hardware ownership must remain pinned even after an error fence
wakes userspace. Device recovery must invalidate stale frontend identities
before new channels can publish.

## Apple comparison and limits

The pinned AGX kext constructs a common 128-entry KSM pool and retains distinct
TA/3D channel objects on a workqueue. IOGPU validates the cached channel owner
and can reclaim a released stamp entry only after its completion criterion.
The numerical QID can change while the channel object persists. Native
`submitReleaseResource` emits device-control type `0x13` and conditionally waits
for the submitted control interval; timeout and fault exits matter.

The implemented table deliberately has no published-ID recycling yet. Reusing
an ID merely because a userspace fence signalled, or freeing a root immediately
after enqueueing a release command, would not reproduce the inspected contract.
See [native evidence and migration detail](G17_MIXED_CONTEXT_ARCHITECTURE.md)
for pinned hashes, instruction addresses and distinctions between observation
and inference. The exact test device's firmware consumer and detach success
conditions remain unverified. The native G17X context-LUT setter is a no-op;
this review does not infer a missing hardware LUT write.

### Published detach design boundary

The [stable-channel follow-up audit](G17_STABLE_CHANNEL_AUDIT.md) adds two
production-method fixes: preserve control-ring capacity before publication, and
avoid retaining a new logical control when compute QID/graph allocation fails.
Its 24 focused scripts and compiled stack audit pass. Full-ring deferred retry
and published-resource detach remain open; no hardware result is claimed.

The [native detach audit and implementation plan](G17_NATIVE_DETACH_IMPLEMENTATION_PLAN.md)
records the exact 64-byte layout, control-page identity sources, cursor predicate,
timeout/fault behavior, ring-credit integration gap and required firmware captures.
Its pinned-binary audit passes 52 instruction witnesses and 521,218 host predicate
cases; these do not establish firmware acknowledgement ordering.

This is a proposed lifecycle, not an implemented or firmware-qualified protocol.
Closing an owner must first prevent new publication and cancel definitely
unsubmitted work. Existing jobs then drain against their exact channel, stamp,
descriptor and firmware generation. Shared TA/3D/CL references must all be
accounted for before releasing the logical scheduler-control page.

The native producer's type-`0x13` record and wait routine are evidence that a
detach protocol exists, not sufficient evidence of the tested firmware's
successful-detach condition. Verify its consumer, identity fields and completion
ordering before implementing a release. In particular, establish whether a
device-control consumer cursor proves the handler completed or merely fetched
the record. Ring space, timeout, firmware fault and generation change must have
explicit outcomes. Waiting for that outcome should not hold the global runtime
mutex or prevent unrelated owners from publishing.

Only a validated completion in the original generation may permit dropping the
control and its remaining roots/mappings. Ambiguous errors retain those owners
until proven processor stop. QID released-stamp eligibility, scheduler-resource
detach, QoS-ID release and UAT root invalidation are separate transitions; a
signalled userspace fence is insufficient for any unproven transition. Native
released-stamp reuse may allow more logical channels than simultaneously leased
QIDs, but that requires its own owner-match and firmware-install qualification.
It must not be approximated by idle-LRU eviction or default global serialization.

## What to add to the overview

Keep the three structural panels, and add a separate lifecycle view containing:

1. A stable logical queue owner, its TA/3D/CL channels, and the separate shared
   QID lease table. Show QID, context/root ID, QoS ID and parameter-buffer ID as
   different identities.
2. Host, firmware and GPU readers/writers for each shared object, with the
   address-space and protection of each alias. An arrow is not proof of ownership.
3. Descriptor sealing, publication ordering, ring capacity/wrap, producer and
   consumer ownership, and backpressure.
4. The return path: type-1 events, completion records, timestamps, stamps and
   job-list unlink, through exact-once fence settlement.
5. Queue close, fault/quarantine, device-control detach acknowledgement and
   processor-stop cleanup, showing which references remain retained at each step.
6. Allocation/growth failure and rollback, including guard pages and the
   kernel/Mesa reserved address ranges.

## Qualification record

Corrected candidate kernel SHA256:
`b5993eab528b1cf4f5d2799988e92a10de4d751eab876270545306905f4089f9`.
Matching Mesa Vulkan library SHA256:
`20d5b1f74db5ecd251573a00edd926ed7fc7463df22604e8d7290142625b39c7`.

- Kernel and Mesa cross-builds succeeded. Kernel output still contains warnings;
  a successful build is not a warning-free audit.
- Before the stable-channel follow-up, the full `test_g17_*.py` sweep ran 117
  scripts: 116 passed. The remaining
  `test_g17_tsq_count.py` passed its three host Rust tests, then failed because
  `/tmp/g17p-re/fwb1/b1_raw.txt` is missing. Its firmware predicate was neither
  fabricated nor skipped. All 69 focused scripts passed. These scripts include
  compiled production-method Rust harnesses and source-contract checks; they
  do not constitute the full driver test suite or hardware testing.
- Nine broader failures were corrected stale fixtures/source-shape assumptions.
  Updating the scratch fixture exposed the partial-write defect listed above;
  its production fix restored the failure-atomic test. The async-retirement and
  B2-lifetime fixtures were updated for the new cancellation/retention helpers.
- The native audit verified 18 instruction/dataflow witness groups in the pinned
  AGX image. Mesa's VA-layout executable passed under ARM64 emulation.
- Kernel and Mesa `git diff --check` passed.
- The compiled ARM64 stack-frame audit passed: probe 4,816 bytes, runtime start
  1,728, boot session 3,568, manager construction 336, resource construction
  7,248 and render-slot selection 80. Its module-internal direct-call estimate
  is 20,864 bytes. This leaves 11,904 bytes of the configured stack outside that
  measured path; it is not a complete whole-kernel stack bound.
- Rejected kernel `dea85c30180177fd0d1384701b9a61c4c9e58569491c698134f76a1ebb12f109`
  did not return to the network after the coordinator's reboot. Its inspected
  probe/start/boot/manager/initializer/channel-table chain totals 35,280 bytes.
  The corrected artifact is awaiting physical recovery and hardware testing;
  no successful corrected boot or r20 result is claimed.
- Hardware qualification is owned by the Vulkan CTS task: unchanged exact r20
  first, then long-lived mixed load, canary integrity, queue churn, teardown and
  recovery, and throughput/latency/memory comparisons. No load guarantee is made
  before those results exist.

Earlier audit documents describe historical implementations and experiments.
In particular, current `G17PClRuntimeStorage` no longer owns 23 private FList
buffers; `cache_compute_vm_mappings` uses a shared resource pool and skips an
already matching VM mapping. The older two-unused-MiB and private-FList collision
findings must not be reported as current defects without new evidence.
