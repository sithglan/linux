# G17 mixed-context ownership decision

This records native evidence and the implemented logical-owner correction.
The logical-owner, QoS-transition and process-identity modules all failed
the three-client hardware reproducer. The default-off split execution-ID probe
also failed at round zero and has been disabled on the test machine. No hardware
fix or elimination of silent corruption has been established.

## Native channels and global queue-ID leases

The pinned AGX image constructs **128** KSM queue objects in one accelerator
array at `+0x12f38`: `createKSMKickQueues`, `0xfffffe0008adb6b0`, stores each
entry at `0xfffffe0008adb760` and compares the index with `0x80` at
`0xfffffe0008adb768`. TA, 3D and CL select from this common pool using their
channel stamp index; it is not a pool of two render pairs. TA loads channel
`+0x18`, indexes the array, loads KSM queue `+0xc`, then writes ConfigUpdate
`+0x18` (`0xfffffe0008b91d54` through `0xfffffe0008b91dd0`).

Channel object lifetime is separate from that numerical lease. A native 3D
workqueue retains its TA and 3D channels at `+0x218/+0x220` (TA allocation
`0xfffffe0008ba329c`, 3D allocation `0xfffffe0008af4f78`). Their common init
retains the logical command queue and workqueue. Workqueue free releases both
channel objects (`0xfffffe0008ba1e08`).

The inspected `com.apple.iokit.IOGPUFamily` image has SHA256
`cd124ce0fb35e9af5fab2950e4d975e739b52a7e7754cdfcc1853e077c180e19`.
`IOGPUEventMachine::requestStampIndexForChannel` at `0xfffffe000a8688fc`
validates an existing channel index against the table's actual channel owner.
Otherwise it selects an empty entry or an entry with a released sequence and
a completed stamp. `releaseStampIndex` at `0xfffffe000a866c74` records a
release sequence at entry `+0x10`; it does not clear the owner or free channel
memory. `freeStampIndexForChannel` at `0xfffffe000a868cd0` checks owner equality
before clearing the entry. Thus a stable channel can lose its old ID and must
validate ownership on reacquisition. Native completion/release eligibility is
not evidence that every firmware reference to a destroyed scheduler object
has been detached.

The pre-migration Linux path conflated logical channel ownership, firmware QID
and compact-VA placement. Queue records used `qid * 0xc0` inside a graph whose
pointer blocks started at `0x1000`; extending the QID range in that layout would
overlap live metadata. The low queue-peer gap held four peers, and two compact
TVB/RT arenas and a two-entry VM registry restricted placement as well.

## Stable-channel migration: implemented, awaiting hardware qualification

- `g17_channel.rs` owns one 128-entry TA/3D/CL table. Bootstrap holds QIDs 0/1;
  compute prefers 4, then any empty entry. Every lease includes an immutable
  host identity and engine. First and repeated publication validate ownership.
  Unpublished allocation rollback checks that exact lease. Host identities do
  not reset when processors stop, preventing an old packet matching a new lease.
- The registry retains one render slot per logical queue without LRU eviction.
  Slot zero is bootstrap; 63 host slots can represent user render pairs, subject
  to the shared QID, root, memory and logical-queue limits. This is not a claim
  of 63 concurrent hardware execution lanes. A busy owner waits for its own
  graph; independent owners remain admissible. Both channels are allocated
  lazily, with rollback if the second ID cannot be obtained.
- QID record offsets remain native `qid * 0xc0`. Metadata now starts at
  `0x8000`, leaving room for all 128 records. Each render queue graph grows by
  32 KiB. Compact backing is allocated from the existing driver-reserved
  ranges, with actual addresses retained in TVB page inventories; no maximum
  TVB/RT arena is preallocated per owner. TVB guards remain 32 KiB. Queue-peer
  aliases use a free address shared by the client and firmware lower roots.
- The ordinary userspace AXFB reservation expands from
  `0x10002000000` to `0x10002400000` (4 MiB total). Kernel reservations, Mesa's
  allocator and DRM shim agree. The matching Mesa driver is required for
  testing; an old userspace allocator could place a BO inside the enlarged hole.
- Completion ownership uses a 64-bit host bitmap. Type-4 recovery routes through
  the current QID lease, not `qid / 2`. Compute packets accept QIDs 2/3 as well
  as 4..127; the channel table remains the ownership authority.
- Installed compute graphs are no longer rebound to a different owner. Queue
  close retains their bindings, roots and IDs. Render close similarly retains
  channel state; released registry identities are reclaimed only after a proven
  processor stop and after their outstanding leases have dropped.
- A definitely-unpublished, inactive compute graph can now return its exact
  QID lease on close and release its private storage. A sealed lease or any
  ambiguity still retains the graph. Merely creating a logical queue no longer
  adds its 16 KiB control page to session retention: first render handoff/begin
  or CL preparation retains it before pointer exposure, with one marker per
  control object. Create-only queues release their pages normally on close.

**Remaining load and security limitation:** this is bounded retention, not
native released-stamp recycling or acknowledged scheduler detach. Repeated
queue creation/closure can exhaust finite IDs or retained memory even after all
jobs finish. An untrusted client can therefore consume session capacity; clean
allocation failure is safer than aliasing firmware-owned state, but is not a
finished denial-of-service defense. Type-`0x13` detach, its acknowledgement,
released-stamp eligibility and exact-owner reacquisition remain required before
sustained churn can be qualified. Do not claim this migration fixes the r20 hang
until the unchanged reproducer passes on hardware.

Host regression coverage executes the production table, manager allocator,
admission, packet validation, completion and recovery routing. It covers mixed
engines, exhaustion, partial allocation rollback, stale-lease rejection, owner
63, repeated notifications, same-owner contention, close/stop lifetime and
scattered TVB page encoding. These mocks do not establish firmware acceptance,
GPU throughput or absence of memory corruption. Hardware gates remain exact
three-client r20, concurrent canary checking, queue churn, forced teardown and
recovery, followed by throughput/latency comparisons against the prior build.

## PID candidate result and teardown investigation

Module `afd4937f3d485b14170dd7943484bc9068b45af946b70afcbed0ee50fd313617`
failed unchanged r20 at round 0. The coordinator reports compute PID 566
DeviceLost, subgroup PID 569 downstream DeviceLost, ycbcr PID 568 Pass.
`diagnostics/owner-pid-r20-01/kernel.log` has SHA256
`0eaecf11e5abe13564ffd066090fbadc4772c2e5c9a70d6cd081a4f8d732ea32`.
QID 6/HBID 3/context 6/VM 8/submission 235 is one stamp short (0x31/0x30),
with a satisfied dependency and zero timestamps, no shown MMU fault, and
coherent QoS/B2 identity. The report now contains `command-word=0x236`, exactly
PID 566: the correction fixed attribution and did not fix the hang. The stale
PID hypothesis is rejected for this failure.

The coordinator reports a clean-boot two-client compute+ycbcr control passed
20 rounds (40/40), while three mixed clients fail at round 0. This strengthens
the render-pair handoff/context-cache hypothesis, without proving its cause.
The context-7 queue-close log immediately precedes recovery in the capture,
but its timestamp has only second resolution. `release_render_context_owner`
retains selected and parked graphs; `reclaim_released_superseded_render_graphs`
is currently a no-op. `release_async_compute_queue` likewise retains installed
bindings/contexts, except rollback of an unpublished replacement. The message
is not evidence that context 7's roots were freed at that point.

Further native evidence in the same pinned kext:

- `AGXArmFirmware::submitReleaseResource`, `8b37c74..8b37dbc`, encodes
  device-control type `0x13`, mapped scheduler address at `+0xc`, and identity
  bytes at `+8..+b`. After the gated submission succeeds with flag 2 still set,
  `8b37da4` calls `waitForDeviceControlCompletion`. The wait compares ring
  progress against the submitted interval and has timeout/fault exits. Thus
  scheduler detach has a completion protocol; host job completion or QoS ID
  release is not an equivalent witness. Firmware vtable `+0x288` contains the
  encoded target consistent with this function, matching the reclaimable-state
  free call's argument layout. Full original chained-fixup metadata is absent
  from this extracted image, so the encoded-pointer resolution is an inference.
- `AGXSecureGart::registerContextID`, `8b82f5c..8b82ff4`, removes the previous
  client ID, invokes accelerator virtual `+0x1210` for that old ID, then stores
  the replacement and calls `AGXUAT::setClientContextID`. The virtual target is
  consistent with `AGXAccelerator::invalidateContextIDLUT`, which walks a
  128-bit per-context membership set and calls `halSetContextIDLUT` with an
  invalid context before clearing each bit (`8ac6e80..8ac7060`). However,
  the G17X `halSetContextIDLUT` override at `8b5d678` is only `bti c; ret`
  (the base implementation is also empty). This generic LUT bookkeeping is
  **not evidence of a missing hardware LUT write on G17X**. Its feature bit
  must also be enabled for the generic routine to enter the walk.
- `AGXUATSingleStage::invalidateGPTBATEntry`, `8b995c0..8b99604`, clears both
  root valid bits, orders those stores, issues TLBI for the ID, then DSB.
  Linux root-lease drop similarly clears its exact root pair before ASID
  invalidation; the native LUT membership and scheduler detach domains are
  not established by that similarity.

No new live detach command or guessed LUT write is implemented. The precise
device firmware's consumers and acknowledgement semantics remain unverified.

## QoS candidate failed; process-identity correction

Module `46560ef49effddcff46b8967b88482967454ea6c23dcb9e149421e9e0dd27176`
failed at round 0. The coordinator reports compute Pass, subgroup DeviceLost,
ycbcr DeviceLost. Capture `diagnostics/native-qos-cache-r20-01/kernel.log` has
SHA256 `dcf47a806ee1f185ad9812e08963c0b8d3e2d1d0e3145cdf158ce5782a2f8459`.
The first fault names QIDs 1/6, reason 2/sources 2, no shown MMU fault. Compute
QID 6 is owner 2/VM 9/context 5/HBID 2; its target/completed stamp is 0x31/0x30
and timestamps are zero. B2 and active QoS identity are coherent. Healthy
render completion now shows retired `{ff,0}` rows, verifying that the new
retirement path runs. These corrections are not the sole cause of the fault.

Code inspection found two remaining process-identity errors:

- CL initial staging overwrote queue-record `+0x48` with captured PID `0x170`,
  mislabelled as a UUID; ConfigUpdate `+0x5a` emitted the same literal with a
  16-bit store. Native writes the full 32-bit channel process identity.
- Cached render retarget changed roots and scheduler owner while retaining
  the old `storage.owner_pid` and both queue-record PID fields.

The new patch captures the creator TGID once during logical queue allocation.
Both engines' queue records follow it during handoff; render optional records
use the updated retained PID, and every compute ConfigUpdate copies the full
u32 from its bound queue record. First-group staging preserves that value.
An unowned bootstrap compute graph starts with PID zero. Workqueue dispatch
and another process submitting through an inherited queue cannot substitute
the current worker/caller PID for the queue creator's identity.

Native evidence: CL `8aedc74..8aedcb4` invokes channel vtable `+0x1f8` and
stores `w0` at tag15 `+0x5a`. CL/TA/3D tables contain the same encoded target
there. `AGXChannel::getPid`, `8ae6a3c`, reads its channel-state PID. The existing
native capture-backed render ABI identifies runtime queue-record `+0x48` as
that same process field (the audited kext's internal state uses `+0x44`).

Both failures' recovery reports contain `command-word=0x170`; actual client
PIDs in the second run are 567/569/570. This matches the stale field but is
**not proof of causality**: that report word may itself encode a PID. If the
next failure reports the real PID, it establishes corrected attribution,
not that a scheduler hang has been fixed.

Tests now include full-width PID `0x12345678`, initial and repeat encoding,
same-VM owner changes, failed retarget preserving the old PID, and rollback
across compute/TA/3D. Historical descriptor hashes normalize only the known
PID and prior FList-index differences, preserving coverage of other bytes.

## r20 failure and QoS transition correction

`diagnostics/logical-owner-r20-01/kernel.log`, SHA256
`7437baec277098aca98d23dee8afb46fe01c90966532cbe2c140cbc580157beb`,
records reason 2, sources 3, blamed QIDs 1/4 (DM 1/2), with no shown MMU
fault. Three concurrent copies of the exact tessellation subgroup case ended
DeviceLost after successful internal submissions. Healthy render QIDs 0/1
reported requested-slot **4** and QoS rows `[4, 15, ...]`: byte 0 is HBID 4,
byte 1 is class 15. Compute QID 4 also used HBID 4 (logical owner 4, VM 8,
context 7, scheduler page `0xfffffc20c086c000`). This supports the same logical
owner; that render log does not independently print the page address. An
earlier interpretation of class 15 as its HBID was incorrect.

Further native inspection of the hash-pinned kext establishes:

- CL `prepare` at `8aef6d8..718` derives tag-15 `+0x52` from QoS cached ID
  `+0x38` versus live ID `+0x10`, cached QID bitmap **+0x28**, and dirty word
  `+0x68`. Its separate `+0x62` compares the GART generation `descriptor+0x158`
  against `channel+0x38`. `AGXContextIDManager::alloc` increments the allocation
  generation at `8b04ef0..f08` when rebinding a numeric slot.
- `willSubmit`/`complete` use a different host active bitmap, **+0x18**.
  Completion clears that bitmap and shared QoS row bytes 0/1 to `{ff,0}`
  (`8b7a5e8..644`). It does **not** clear the +0x28 configuration cache or
  directly edit the firmware scheduler-page QID bitmap.
- Completion counters at QoSManager `+0x648`, `+0xa48`, and `+0xc48` are
  **host** counters. Shared `+0x400`, `+0x600`, and `+0xe20` are monotonic
  submission totals. Their growth in r20 is not by itself evidence of debt.
- `willSubmit` compares shared DM submissions against host DM completions
  (`8b7a338`). Only an idle-to-busy edge updates shared `+0xe08[dm]` and
  increments `+0xe00`; overlapping work preserves the busy interval's start.
- Descriptor completion's boolean is the bit set after `willSubmit` (CL
  `8aee630` -> `8aef158`; TA `8b929dc` -> `8b9425c`; 3D `8ac4e44` ->
  `8ac5f58`). Cancellation after willSubmit still balances that reference.

The correction caches CL configuration only after successful group publication.
Same-owner repeats clear both update flags. A different unique logical page
reasserts them even if numeric context/QoS IDs were reused. Pages are immutable
root-lease identities while current; submission validates the root lease.
QoS identity and context-generation comparisons remain independent.

Resource ownership now tracks outstanding work per QID and DM. Exact executed
compute completion and paired render completion retire the expected HBID rows,
preserving other bytes and all submitted totals. Validation of a paired set is
atomic; duplicate, wrong-owner, wrong-DM and missing-active retirements fail
before stores. More than one outstanding reference remains supported, and its
row stays active until the last completion. Host DM accounting controls the
native busy-period timing behavior without polling or allocation.

Pre-publication failures balance QoS with a checked cancellation ticket: compute
tag-15 pointer failure, render failures before atomic outer publication, and
delayed-TA validation before prefix exposure. Shared totals never rewind.
Ambiguous errors after an inner producer is exposed retain/quarantine work;
they do not pretend the firmware missed it. Processor-stop reset clears shared
QoS and host accounting together. No live reset is added.

`test_g17_qos_transitions.py` runs the actual resource methods (only mappings
and clock mocked), encoder, stale-ticket/multi-engine/rollback cases and 4096
owner turnovers. `test_g17_async_retirement.py` verifies that QoS completion is
called once after the final executed stamp, never for pending or failed work.
The native audit now pins 13 witness groups. These checks prove host behavior,
not firmware correctness; repeat r20 and verify subgroup values on hardware.

## Implemented ownership correction

- `G17PLogicalQueueContext` owns the existing UAT root lease, one stable
  scheduler/channel-control page and a logical QoS lease. Compute, TA and 3D
  use that page in ConfigUpdate `+0x4a` and channel-state `+0x70`.
- QoS `+0x56`, queue QoS rows and kick QoS bytes use the same logical lease
  across engines. TA's descriptor/PB/PM ID and ConfigUpdate `+0x7e` remain
  separate. Physical compute graph creation no longer consumes a TA PB ID.
- Native QoS uses a 128-slot allocator (`AGXQOSManager::init`,
  `0xfffffe0008b79d18`). Linux reserves 0/1 for existing bootstrap owners and
  allocates logical IDs 2..127. IDs return when logical queues, installed graph
  bindings and old-binding retirement references all release their leases.
  This is a resource budget, not an execution-concurrency cap.
- Same-VM compute reuse retains aliases while adopting the incoming logical
  identity. Previous roots survive until the existing exact-completion/epoch
  witness. Unpublished cancellation restores the previous root and scheduler
  binding; failed restoration retains both owners and quarantines that graph.
- Cached render reactivation and owner rebinding request a QoS update through
  `+0x52`, independently of QID installation. Retained optional records are
  rebuilt from the logical owner, so subsequent submissions cannot silently
  restore the old graph-relative pointer. Render `+0x62` policy is unchanged;
  the subsequent CL transition correction is described above.
- B2 `+0x808` stays pinned to a dedicated bootstrap page. No logical switch
  rewrites it. Its exact firmware-private consumer remains unresolved; this
  separation is conservative, not a newly verified B2 ABI claim.

Referenced scheduler pages are retained until firmware processor stop,
separately from QoS leases and roots. This avoids freeing memory that an
unimplemented native scheduler detach may still reference. The later retention
correction above removes the former 16-KiB cost for create-only queues, but
retains one such mapping for each control that enters render or CL binding,
including ambiguous failures. **That cumulative memory growth remains a
lifecycle limitation**, not a finished heavy-churn
architecture or a reason to add automatic resets. Native detach/acknowledgement
must be implemented to make scheduler-page reclamation bounded.

No execution caps, engine exclusions, new sleeps, per-submit allocation, resets
or batching reductions were introduced. Throughput and latency impact remain
unmeasured until matched hardware testing.

## Evidence and the remaining variable

The hardware coordinator reports:

| Workload | Result |
| --- | --- |
| Three synchronized, prewarmed pure-compute queues | 192/192 |
| Two retained idle render contexts plus three pure-compute queues | 192/192 |
| Two continuously active simple render streams plus three pure-compute queues | 192/192 |
| Three exact mixed logical queues, each compute → render → compute | Canonical reason-2/sources-2 fault; victim compute one stamp short, dependency satisfied, rings consumed, no MMU fault |

One exact mixed client passes; earlier two-client mixed stress passed. A passing
bounded test is not proof of unlimited capacity. These controls nevertheless
make a generic three-compute burst, retained-render count, or two-render/three-
compute engine-count explanation inadequate for the observed reproducer.

The current mixed path combines two architectural features absent from the
independent-engine controls:

1. One logical queue's UAT execution-context ID is used by compute, TA and 3D.
2. More than two logical render owners are multiplexed onto two retained TA/3D
   QID pairs, requiring graph/context handoff while the outgoing logical owner
   may submit compute using that same UAT ID.

The current source proves these features exist; it does **not** yet prove which
firmware operation produces the one-stamp-short failure.

### Fault-only capture: retained sibling render, not simultaneous sibling work

`diagnostics/neo-dmesg-mixed-resource-faultonly-device-loss-r17.log` has verified
SHA256 `3de1aecc59afcbe005858cbb3b7136ed8f3771bc11005708f13130eb077286c3`.
The coordinator reports failure at round 17 with fault-only census enabled.
The captured sequence is:

| Boot time (seconds) | Observed transition |
| --- | --- |
| 34.555150 | Compute QID 4 rebound to logical owner 54, context 11. |
| 34.563058 | Render pair 0 handed off from VM 153 to VM 161. |
| 34.564964 | Compute QID 5 rebound to logical owner 55, context 12. |
| 34.566977 | Render pair 1 handed off from VM 152 to VM 163. |
| 34.726171 | Firmware report: reason 2, sources 2, blamed QID 4, data master 2. |
| 34.727875 | Pair 1: VM 163/context 12/HBID 7, host tracker present, job list linked, zero pass timestamps despite equal queue counters. |
| 34.729211 | Pair 0: VM 161/context 11/HBID 6, no host tracker, job list unlinked, completed pass timestamps. |

The victim compute ordinal is 1889, target stamp `0x761`, completed/dependency
stamp `0x760`. Its sibling render graph is retained and retired at the census;
the other client's render graph is outstanding. This weakens a requirement for
simultaneous execution of the victim's own render and compute work. It supports
investigating persistent scheduler state across rebinding. The census takes
about 1.43 ms and follows the fault; it is not an atomic view of fault-time
execution and does not establish the causal firmware store.

The compute QoS owner and B2 readback both contain control pointer
`0xfffffc20c07b8000`. No render control-pointer readback or firmware
context-table witness is present in this census. Therefore it does not prove
that a firmware context-indexed pointer was overwritten.

The earlier periodic-census capture
`diagnostics/neo-dmesg-mixed-resource-census-fail-r8.log` has verified SHA256
`b79da86dac38a14472451036f1390969b3b7e4ca099e64eba174e8aa345dd144`.
The coordinator reports a subgroup result mismatch (26/27 values correct),
without DeviceLost. Its kernel log alone cannot identify the failed result's
logical owner or correlate the first incorrect value with a particular handoff.
Value checking is consequently mandatory in stress validation; absence of
firmware recovery is not sufficient evidence of correctness.

## Decision: make each ownership domain explicit

Keep one logical queue timeline and stable firmware-visible ownership for its
engine queues. Treat UAT identity, firmware scheduler-context identity, QID,
QoS HardwareBufferID, PB/HWPB identity, FList pool identity and job stamp as
different namespaces. Never substitute one for another because their first
captured numerical values coincide.

The target is a logical queue owner containing an address-space owner, a stable
firmware scheduling-context owner, and lazy per-engine queue owners. TA and 3D
form a render pair but are not the only two physical engines the host can
represent. Firmware workqueue identities must not be conflated with concurrent
hardware execution slots. Scale installed workqueues against the verified QID,
root, descriptor and allocation budgets, with resource-specific backpressure.
Do not invent a global execution cap from the number of currently implemented
render-pair objects.

The native G17 host producer now establishes the sharing rule: one reclaimable
scheduler object per logical command queue, shared across its compute, TA and
3D channels. The direct binary evidence appears below. The firmware's internal
indexing and detach acknowledgement remain separate qualification items;
producer ownership does not establish every consumer operation.

| Object | Owner and lifetime invariant |
| --- | --- |
| UAT root/ID | A root lease names one VM root pair and stays stable while any job, installed queue, cached firmware reference or retained graph can use it. A host fence is not a universal firmware detach witness. Same-VM engine aliases may be used diagnostically; they are not a fix by themselves. |
| Firmware scheduling-context descriptor | A typed retained owner distinct from the UAT root. Pointer identity, initialization and firmware-written bytes survive ordinary engine alternation. Which engine queues share it must follow the exact native contract. |
| Compute/TA/3D firmware workqueues | Queue records, item rings, SKSM backing, producer stamps and registration identity live together. Ordinary logical-owner alternation must not silently reprogram an installed queue without the complete native handoff contract. |
| Logical queue timeline | Compute/render successors observe the actual predecessor completion/error fence. Cross-engine ordering is per logical queue; independent logical queues remain concurrent. |
| ConfigUpdate | Installation, context update, QoS update and work publication are separate transitions. Generation/dirty state follows the object that firmware actually caches. Do not use an ordinal or a physical-slot switch as an unproven substitute for native invalidation semantics. |
| QoS | QID row and HardwareBufferID-indexed scheduler-state/share rows point to their correct retained owners. Global accounting is serialized on the host; that does not prove firmware consumption or lifecycle ordering. |
| Render graph, PB/HWPB/TVB | The retained graph owns the PBDesc/HWPB identity, PM generation and VM mappings. A submission acquires a reference; retirement releases that reference, not necessarily the retained object or its firmware references. |
| FList/UMA | Pool identity and backing are separate from queue QoS identity. Pool generation/refcounts and growth/release acknowledgement must survive overlapping commands. Reinitialize only before firmware ownership, or at a proven safe lifecycle boundary. |
| Job list / completion | Ring acceptance, ring retirement, executed timestamp/stamp and EventControl unlink are different witnesses. Require the appropriate complete witness before releasing a job's mappings or reusing its descriptor. |

If a stable per-logical TA/3D queue graph is not currently representable, the
alternative is a **native-proven cross-engine-safe queue/context handoff**.
Checking that only the old render pair has retired is insufficient evidence that
every firmware object indexed by its context ID can be superseded while that
logical owner's compute is live. It may be valid, but its consumer and ack must
be demonstrated. Broad engine exclusion is not a substitute.

## Inspected implementation differences before the correction

- `g17_drm.rs::G17PQueue::new` allocates one `T8140ComputeExecutionContext`.
  `ensure_async_compute_queue` passes it to compute; render publication passes
  the same Arc. `mmu.rs` shows this object owns a VM and TTBAT root lease. It
  does not own a firmware scheduler-context descriptor.
- `g17_manager.rs::prepare_g17p_compute_queue` creates one manager-wide
  `G17PClChannelControl` and points every compute queue at it. Its cold contents
  match the render context-descriptor initializer, but identical initial bytes
  alone do not establish its sharing domain.
- `g17_submission.rs::apply_g17p_cl_b2_computed_state` also publishes that
  pointer in the device-global B2 computed object at `+0x808`, once per
  activation. A per-logical allocation change must establish whether this is
  a bootstrap reference, a persistent engine owner or a selected context.
  Updating this global pointer on every submission without that proof would
  introduce another cross-queue ownership hazard.
- `g17_resources.rs::new_user_render_storage` creates TA/3D channel-control
  storage at each graph's support `+0x2c000`. Both render ConfigUpdates use it.
  Thus a mixed logical queue shares its UAT ID across engines while using a
  different scheduling-context object for compute and render.
- `g17_compute.rs::encode_compute_config_update` sends the queue's declared
  context at `+0x32`, the compute control pointer at `+0x4a`, and **always 1**
  at `+0x62`, including repeats. The retained render encoder supplies `+0x62`
  from `context_update_pending`; completion clears that flag and cached graph
  reactivation sets it. This is an inspected asymmetry, not proof that clearing
  compute's flag would be safe.
- `G17PQueueLeaseState` offers two render slots with logical-owner affinity and
  eviction. `handoff_retained_render_vm` can cache/reactivate/rebind a graph
  while keeping the physical QID's SKSM backing and producers. The old render
  completion and FList ownership are checked; no inspected native proof in this
  workspace establishes how this transition interacts with another engine still
  using the outgoing context ID.
- M1/M2 `queue/mod.rs::Queue::new` creates one `workqueue::GpuContext`, then
  passes its same Arc to separately created vertex, fragment and compute
  workqueues. `fw/workqueue.rs::QueueInfo` retains that context; its drop routes
  through `gpu.free_context`. This is an actual cross-generation reference
  implementation, not a proof that every G17 offset or detach mechanism matches.

### Direct native producer evidence

The original Mac paths in `G17P_RE_HANDOFF.md` were unavailable, but the
coordinator supplied this local extracted native kext, inspected with
`llvm-objdump -d --no-show-raw-insn`:

`/home/dj/Developer/Asahi/research/kernelcache/kexts/com.apple.AGXG17X`

SHA256: `ccbe120a3a197038216426b507b5409873397b947308239b4ecc75d76759e87f`.
The findings apply to this binary; a filename alone does not establish that its
firmware payload/build matches the tested device. Addresses below are its
unslid virtual addresses.

| Native function / address | Inspected dataflow |
| --- | --- |
| `AGXCommandQueue::ensureReclaimableResources`, `0xfffffe0008af25e0`–`0xfffffe0008af266c` | Tests command queue `+0x618`; returns if present; otherwise allocates a reclaimable-state instance, initializes it with the command queue, and stores it at `+0x618`. |
| `AGXCommandQueueReclaimableState::init`, `0xfffffe0008b02d94` | Stores accelerator and command queue at reclaimable `+0x10/+0x18`. |
| Same init, `0xfffffe0008b03104`–`0xfffffe0008b0323c` | Selects a pool element and retains backing; stores GPU address at `+0xe0` and CPU address at `+0xc8`. |
| Same init, `0xfffffe0008b032ec`–`0xfffffe0008b0332c` | Clears the object, writes `0xffff` at `+0`, byte `+5=1`, byte `+0x33=0xff`, and derives byte `+0x26` from the command queue's task. |
| `AGXCLChannelSKSM::submitBuffer`, `0xfffffe0008aedc1c`–`0xfffffe0008aedc24` | Loads command queue `+0x618`, then reclaimable `+0xe0`, and writes tag-15 `+0x4a`. |
| `AGX3DChannelSKSM::submitBuffer`, `0xfffffe0008ac3c58`–`0xfffffe0008ac3c60` | Identical owner chain into tag-15 `+0x4a`. |
| `AGXTAChannelSKSM::submitBuffer`, `0xfffffe0008b91f2c`–`0xfffffe0008b91f34` | Identical owner chain into tag-15 `+0x4a`. |
| `AGXChannel::init`, `0xfffffe0008ae8684`–`0xfffffe0008ae868c` | Copies command queue `+0x618 → +0xe0` into channel `+0xf8`. |
| `AGXChannel::resetChannelState`, `0xfffffe0008ae6adc`–`0xfffffe0008ae6ae0` | Copies channel `+0xf8` into firmware channel state `+0x70`. |
| `AGXCLCommandDescriptor::prepare`, `0xfffffe0008aef5e8`–`0xfffffe0008aef618` | Compares descriptor `+0x158` with selected queue state `+0x38`; stores inequality in descriptor byte `+0x190`. |
| `AGXCLChannelSKSM::submitBuffer`, `0xfffffe0008aedb6c`–`0xfffffe0008aedb74` | Copies descriptor byte `+0x190 & 1` into tag-15 `+0x62`. It is not an unconditional one. |
| `AGXCLCommandDescriptor::prepare`, `0xfffffe0008aef924`–`0xfffffe0008aef928` | Updates selected queue's cached `+0x38` from descriptor `+0x158`. |
| `AGXCommandQueueReclaimableState::free`, `0xfffffe0008b03408`–`0xfffffe0008b0344c` | Passes GPU pointer and firmware-written bytes `+0/+1/+4/+0x26` through a firmware virtual call before later backing release. The call's exact completion contract still needs tracing. |

This proves the cross-engine producer ownership mismatch. It does not prove
that a firmware context-ID-indexed table causes the captured failure. In
particular, do not conflate hardware UAT TTBAT with firmware scheduler indices.
Linux's B2 computed `+0x808` belongs to a firmware-private page at
`0xfffffc20015d8000`. Its consumer must be traced in ASC firmware. No native
host store at a coincidentally equal C++ member offset proves that page's ABI.

The adjacent native QoS fields provide an additional ownership constraint:
CL `0xfffffe0008aedc28`–`0xfffffe0008aedc30`, 3D
`0xfffffe0008ac3c64`–`0xfffffe0008ac3c6c`, and TA
`0xfffffe0008b91f38`–`0xfffffe0008b91f40` all load command queue `+0x868`,
then QoS object `+0x10`, into ConfigUpdate `+0x56`. `AGXQOSQueue::prepare`
allocates through accelerator `+0x518 → QoS manager +0xc98`
(`0xfffffe0008b7a69c`–`0xfffffe0008b7a6b0`). TA PB allocation instead uses
accelerator `+0x14f50` (`0xfffffe0008b94900`–`0xfffffe0008b94914`); USC pool
allocation uses `+0x14f98` (`0xfffffe0008b9db74`–`0xfffffe0008b9db84`).
These are distinct allocator instances, despite sharing a C++ allocator class.
The coordinator additionally traced native QoS prepare/complete refcounts:
the numerical ID is an active-period lease, not a forever-immutable queue ID.
Linux conservatively keeps the lease through all retained graph bindings.

`audit_g17_native_scheduler_owner.py <binary>` reproduces the recorded groups of
instruction witnesses against the pinned hash. It does not validate firmware
consumer semantics or runtime correctness.

## Ranked fault hypotheses

1. **Cross-engine context-ID-indexed state is invalidated or rebound during
   render-pair handoff.** This explains why independent active render/compute
   owners pass but mixed owners plus render-pair eviction fail. It is supported
   by the asymmetric sharing/updates above, but the exact firmware entry/store
   remains unknown.
2. **Firmware scheduler-context pointer sharing/lifetime is wrong.** A session-
   singleton compute context object and graph-owned render objects may violate
   a logical-queue ownership contract. Pure compute passing does not validate
   mixed-engine mutation of an object indexed by the same logical context ID.
3. **Retained render graph supersession has another identity/lifetime hole.**
   With three render owners, PB/PM/queue-context/RCE state moves between cached
   graphs and physical pairs. A stale valid reference can avoid an MMU fault.
   Correct root Arc retention alone does not prove all firmware-owned queue
   pointers were switched or detached.
4. **Logical cross-engine dependency/error retirement defect.** Current code
   and host tests implement one shared timeline, but captures should retain
   exact predecessor/successor submission and stamp identities. Satisfied
   firmware dependency plus the one-stamp-short victim weakens a simple missing
   wait explanation without eliminating every ownership race.

## Diagnostic experiment: split execution IDs

The default-off `g17p_split_execution_id_probe=1` experiment is implemented.
Module `ab34d3315f12519e0c421fb93ca5f7af9ae048a81635aa937f60a27c75bd8cd2`
failed the unchanged three-client reproducer at round zero. The capture verified
distinct render/compute IDs for PIDs 742 (7/8), 739 (5/9) and 741 (6/10), with
shared scheduler/QoS identity. All three queues became DeviceLost; QIDs 4/5/6
were one stamp short, with zero timestamps and no reported MMU fault. The
capture `diagnostics/split-id-r20-01/kernel.log` has SHA256
`cad180a028163d11d778f7b21ab212d8ea685d567f7aed58e449f1b0dbc3fe5c`.
The coordinator disabled the probe and rebooted the production default.
This rejects splitting execution IDs as a sufficient fix for the reproducer;
it does not prove the precise firmware failure mechanism. Throughput cost
remains unmeasured. Shared execution IDs remain the production path.

The **split execution-ID** probe allocates an additional
`T8140ComputeExecutionContext` for the **same VM/root pair** on first compute
backend creation, and uses it only for that logical queue's compute descriptors,
ConfigUpdate and mappings. Render keeps the original context. It keeps QIDs,
scheduler-context pointers, PB/FList policy, all logical queue fences, kernels,
workload, and execution concurrency unchanged. It retains both root owners
through their existing job/graph lifetimes and allocates once per backend,
not on every engine switch.

The default retains the exact original logical-context Arc and allocates no
extra root. The probe shares its channel-control page, QoS lease and creator
PID with render, but creates a distinct root lease; compute packets retain
that returned context. Tests exercise owner sharing, drop lifetime, allocation
failure, stale roots, wrong VM and duplicate-ID rejection. Existing lazy
backend tests cover one setup per backend and reconstruction after recovery.

This is a diagnostic implementation, not a production fix. It requires one
additional context ID per mixed logical queue and therefore changes root-table
occupancy; record that cost and stay within the existing allocator's budget.
The existing graph rebind path installs the incoming split context; the probe
does not disable graph reuse. It rejects duplicate IDs/wrong VM and logs
distinct render and compute IDs with the same VM/root identity. Capture those
markers and verify every encoded context selector agrees with its retained
owner before interpreting hardware results.

- If the exact three-mixed failure persists, a collision solely caused by the
  shared execution ID is insufficient; prioritize graph/scheduler-object
  identity or lifetime, and obtain the firmware-table witness.
- If it disappears over matched repeated stress, context-ID-indexed interaction
  is implicated. That does **not** prove split IDs are the native architecture
  or distinguish a table overwrite from cache/maintenance identity. Do not ship
  the probe as the fix.
- The logical-owner correction already stabilizes one scheduler-control object
  per logical queue across its engines with the original shared UAT ID, and
  hardware still fails. The split probe therefore keeps that corrected pointer
  ownership unchanged; it does not combine a pointer-sharing change with the
  execution-ID experiment. Consumer semantics and retirement still need proof.

In parallel, the decisive read-only native/firmware audit is to follow tag-15
`+0x32`, `+0x4a`, `+0x52`, `+0x56`, and `+0x62` through the exact J700 handler,
identify every context-indexed table store and its engine domain, and trace the
native command queue's reclaimed-state pointer into compute, TA and 3D producers.
Capture the victim context's pointer/generation before compute, after render
publication/handoff, and at fault. A mismatched entry with exact owner identity
is stronger evidence than adding fences, flags or a global scheduling lock.

## Implementation design and remaining lifecycle work

Native producer evidence confirms one reclaimed scheduling-state object per
logical command queue shared by compute/TA/3D. The correction implements the
ownership changes below; the opening section distinguishes conservative
retention and unresolved firmware semantics from completed work.

1. Introduce a typed logical scheduling owner above the MMU layer. It retains
   the UAT execution-context Arc and the initialized channel-control mapping.
   Allocate the mapping once and keep its address and firmware-written bytes
   stable. Do not place scheduling policy inside `T8140ContextRootLease` or
   zero the page on an engine switch.
2. Pass the owner through compute preparation and render preparation. Encode
   its pointer consistently in each engine's ConfigUpdate `+0x4a` and queue
   channel-state pointer (currently graph `+0x9c`, corresponding to channel
   state `+0x70`). Update retained optional-address reconstruction as well as
   first-submit construction; otherwise later render submissions restore the
   old support-relative pointer.
3. Separate reusable VM render backing from the logical scheduling owner.
   Same-VM graph reuse currently changes descriptor/root context fields while
   retaining graph-owned channel control. It must also bind the correct new
   scheduling owner, preserving the previous owner until the native handoff
   acknowledgement. Do the equivalent for compute QID reuse, including the
   branch that returns an older same-VM execution context. VM equality alone
   must not merge distinct logical scheduling identities.
4. Resolve B2 `+0x808` independently. Retain a bootstrap owner if native B2
   requires one; do not turn a session field into a mutable per-submit selector
   by assumption. Every persistent firmware pointer must retain its mapping
   until proven detach or processor stop, even after queue close or failure.
5. Model `+0x62` from the verified native cache key and generation comparison.
   Keep installation, QoS update and context update distinct. An always-clear
   flag or a per-QID boolean is insufficient unless the firmware cache is
   proven to have that exact ownership domain. Preserve the current flags
   until this is understood rather than changing two independent variables.

This page-ownership change does not by itself validate the two-pair render
multiplexer. If firmware retains per-logical workqueue identity across engines,
provide stable lazy TA/3D/compute workqueues within the verified allocation
budgets, or implement the native acknowledged rebinding protocol. Installed
queue capacity and execution concurrency remain separate limits.

### Focused verification for that change

- Production encoder tests: same logical owner has the same control pointer
  in all three engine encodings; distinct owners have distinct pointers even
  in one VM; repeated submissions preserve the page's firmware-mutated bytes.
- Production reuse/lifetime tests: both render-pair orders, same-VM new owner,
  cross-VM cached graph, compute QID reuse, allocation failure, cancellation
  and close retain old mappings through the required acknowledgement. An old
  epoch's completion cannot retire a new binding.
- B2 tests: initialization names the intended retained owner and ordinary
  logical-queue switching does not rewrite session state without a verified
  transition. Default-off diagnostics perform no extra reads or logging.
- Hardware: repeat the exact three-mixed workload with result validation,
  followed by the established independent-engine and synchronized-compute
  controls. Include long-lived mixed owners and repeated owner churn as
  separate tests. Use distinct data patterns at matching GPU VAs in distinct
  VMs to detect valid-address cross-owner corruption.
- Measure completed work/second, CPU cost, allocation count and latency tails
  against the same uncapped workload and module settings. No performance cost
  or improvement has yet been measured for this proposed ownership change.

## Fix threshold

The native producer justifies correcting the ownership mismatch. B2 is kept
separate pending consumer verification; scheduler-page detach remains open.
Hardware still must establish whether this corrects the captured failure.
Preserve parallel execution. Required regressions include three
mixed owners, simultaneous independent streams, both render-pair orders, context
reuse/close/cancellation, same-VA distinct-VM canaries and long-lived churn.
Never treat successful capped, reset-heavy or globally excluded runs as proof
of correct architecture.
