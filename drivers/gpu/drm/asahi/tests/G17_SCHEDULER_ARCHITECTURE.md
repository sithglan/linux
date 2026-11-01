# G17 scheduling and ownership decision

Historical decisions and experiments. The current stable-channel implementation,
remaining lifecycle work and qualification status are recorded in
[the architecture and load review](G17_ARCHITECTURE_LOAD_REVIEW.md).

Status: architecture review, 2026-09-19. No new execution cap or firmware ABI
operation is implemented here. Hardware results below were supplied by the
coordinating Vulkan CTS task; model tests do not establish firmware semantics.

## Decision

Do not ship a two-compute limit, global compute/render exclusion, or periodic
whole-device reset as the solution. None follows from an identified hardware
resource capacity. Keep graph ownership separate from submission admission.
The source does not yet establish either a complete installed-graph switch
contract or a live detach/reclaim contract. These are production blockers,
not gaps that a host mutex or an ownership type can resolve by itself.

The preferred *candidate to qualify* is a bounded, session-owned physical
frontend pool whose driver graph root is immutable, with application VM selection
carried only by documented per-work payload fields. This is not yet an established
ABI: ConfigUpdate context +0x32 and descriptor context programming are both
present, and their distinct translation responsibilities must be demonstrated.
Do not assume that a client context can be removed from ConfigUpdate merely
because the descriptor also contains a context ID.

If that translation split is unsupported, use native per-VM graph creation and
the native acknowledged detach operation. That operation has not been verified
for this driver. Immutable per-VM graphs reclaimed only after processor stop
are a bounded diagnostic, not the required churn-capable production design.

## Evidence and limits

- Shared compute FList and allocator-issued render QoS HBIDs pass the exact
  two-client mixed workload for 200/200 executions.
- Retaining old aliases/root through a superseding command did not solve
  installed compute graph switches. A command completion is not independently
  established as an acknowledgment of all firmware reference replacement.
- Fresh-QID immutable graphs still fail under three-client mixed overlap.
  Therefore cross-VM rebind is not necessary for the remaining fault.
- Short pure compute and render-oriented runs reached 57 successful executions
  before diagnostic context exhaustion. Queue count alone is not sufficient
  to explain the failure; neither is the recovery source mask a capacity field.
- Later sustained pure-compute isolation passed 192/192 cases in three parallel
  long-lived processes, with QIDs 4/5/6 installed concurrently (later 7..12 as
  queues were recreated). One measured window had 256 compute submissions in
  355 ms. A two-wide compute gate is contradicted as a required remedy.
- One mixed tessellation client plus two compute clients passed 30/30; two mixed
  clients plus one compute client faulted. Four-versus-five engine streams is a
  hypothesis, not a measured active-resource count: logical client activity,
  admitted leases, queue ingestion and simultaneous GPU execution differ.
- Consumed outer/item rings establish ingestion, not hardware execution.
  A satisfied dependency word does not establish that every dispatch prerequisite
  (or its firmware-cached representation) is ready.
- Completion dump display length 24 is not ring capacity. The completion scanner
  in `g17_resources::scan_compute_completion_batch` uses the advertised capacity.

## Inspected ownership and ABI

| Resource | Current owner / access | Consequence |
| --- | --- | --- |
| Compute channel control | `G17PClChannelControl`, one session allocation; manager retains it through stop | No evidence it aliases a render control page. |
| Render channel control | Each render storage's support BO + `0x2c000`; TA/3D partners share it | Sharing is within the retained render graph, not proof of a global render/compute hazard. |
| FList backing / UMA | Compute pool slot 1; render pool slot 0; separately allocated backing | A gate keyed only on "some render is active" does not represent these resources. Growth/pressure needs its actual allocator contract. |
| QoS | One table; rows keyed by QID/HBID; totals and per-DM counters updated during host publication | Host writes are under runtime ownership. Their sharing alone does not require exclusion through GPU retirement. |
| Primary PBDesc/UMA views | `stage_primary_region_views` validates immutable pointer values | This function does not retarget them on each engine submission. |
| Compute graph/root | `bind_independent_compute_user_vm` creates aliases; repeat ConfigUpdate writes context +0x32 and +0x62=1, install +0x1a=0 | No verified complete firmware graph-switch witness. |
| Compute queue context | `refresh_repeat_queue_context` rewrites item 1 descriptor pointer only | Other installed state survives; whether firmware caches additional context state is unknown. |
| Render execution owner | `poll_translated_render_completion` uses `observe_paired_render_window`, including job-list unlink | done/read/write equality with job-list-empty=false is not complete retirement. |
| Application roots | `g17_queue_limits`: contexts 5..62 | At most 58 roots, shared with applicable render users; retaining historical clients exhausts capacity. |

`unregister_compute_sksm_queue` is not a reliable detach witness: its MMIO disable
helper is suppressed under the usable default mode, yet the caller advances host
state. Do not recycle firmware-owned memory based on that host state alone.

There is a concrete admission asymmetry worth isolating: `acquire_compute(true)`
waits for both compute and render leases, while render's `acquire_lease` waits
only for compute installation and a free render pair. Thus render first QID
installation or first storage/PB/HWPB/context publication can overlap ordinary
compute. `stage_render_groups_with_sksm` detects first TA/fragment installation
but only logs around it. This does not prove the hardware needs exclusion; it
identifies a lifecycle transition to test separately from steady engine overlap.

M1/M2 reference patterns in this repository provide useful structure, not G17
ABI proof. `workqueue::Job::can_submit` accounts actual ring/event space and
returns an existing work fence when full. Submission is sequenced and supports
rollback. `buffer::Buffer::auto_grow` / `ensure_blocks` use observed allocation
pressure and prepare new allocations before committing. Neither establishes a
G17 two-job engine limit, nor supplies the missing G17 detach acknowledgment.

## Production components and rules

1. **InstalledGraph owner.** Own graph BOs, QID, HBID reservation, firmware root,
   control references and session generation together. States are Prepared,
   Installed, Quarantined, and Stopped. Close releases a logical claim, not
   installed ownership. No Installed -> different-root transition without a
   verified native transaction. No QID/HBID reuse while firmware references survive.
2. **Immutable Job packet.** Own application context, mapping-generation guards,
   validated payload, fences and the submission generation. Firmware descriptor
   slots may be rewritten only after their own exact retirement and ring-credit
   return. Graph ownership is not transferred to an execution lease.
3. **Admission ledger.** Reserve known physical queue/descriptor/event capacity
   and any *verified* resource allocations atomically. Existing per-QID active
   ownership and two physical render pairs are identifiable resources. A numeric
   compute execution width remains unknown. Do not introduce "2" as a resource
   budget from source masks or observed passing client counts.
4. **Resource-scoped transitions.** A mutation lease must name the actual graph,
   root generation, pool generation or other identified resource. Readiness of
   unrelated resources must not be blocked. A pool operation that firmware can
   perform concurrently needs its native protocol, not an invented host lock.
5. **Admission and wakeup.** Wait on the conflicting resource's completion/credit
   fence without holding the runtime mutex. Revalidate session and generation
   after wakeup, reserve all required credits before publication, and return
   them only on the matching retirement. Wake after releasing bookkeeping locks.
   Do not let a waiter on one resource block unrelated ready jobs.
6. **Failures.** Before visibility, unwind only unpublished allocations/credits.
   After possible visibility, retain graph, packet and credits until exact
   retirement or proven stop. An error fence wakes userspace; it does not return
   hardware ownership. Session reset invalidates all old completion identities.
7. **Teardown.** Live per-graph reclamation requires the missing native detach
   acknowledgment. Without it, processor stop is the only unconditional release
   boundary. Never conceal cumulative exhaustion with automatic global resets.

## Minimal implementation sequence

- Keep the current identity fixes and complete owner-retention checks. The prior
  transactional-rebind tests prove host bookkeeping only; do not label them a
  verified graph-switch implementation after the hardware failure.
- Qualify graph-root versus per-work application-root selection first. If it is
  supported, add separate typed `graph_context` and `execution_context` fields;
  `encode_compute_config_update` consumes only the immutable graph context,
  descriptor construction consumes the job context, and all graph aliases live
  in the session graph root. Add translation and physical-backing assertions
  before publication. Never switch the session root to the client's page tables.
- Install the physical graph pool under the existing install exclusion. Keep
  per-graph ring state and installed identity for the session. The pool size
  must come from a verified allocator/register/native contract; root-table or
  QID addressability is not simultaneous execution capacity.
- Release job contexts at exact job retirement only after the qualified ABI
  proves firmware cannot retain them outside that job. This avoids historical
  client roots consuming the session pool. If that proof fails, implement native
  detach instead; there is no equivalent host-only substitution.
- Add resource-specific admission only after identifying the exhausted/conflicting
  hardware object. Preserve all independent render/compute overlap.

## Experiments that decide the missing contracts

1. **Translation split, one frontend.** Two live client VMs map the same numerical
   input/output VAs to different physical canary buffers. Keep installed QID,
   graph/control pointers and ConfigUpdate graph context fixed. Alternate only
   the proposed per-job execution-context fields. Verify each output and untouched
   peer canaries, with private-memory/preemption use as well as a trivial kernel.
   Repeat while retiring/reallocating client context IDs. This is an explicitly
   isolated diagnostic until the native decoder/root selection is confirmed.
2. **Width versus aggregate pressure.** Precreate long-lived VMs/graphs to remove
   allocation and handoff. Compare two large jobs with three smaller jobs at
   matched aggregate occupancy, private-memory demand and work. Then hold demand
   per job fixed and vary overlap. Record actual allocation/dispatch state and
   completion intervals; a third queue merely existing must be tested separately
   from a third queue executing. Do not derive width from source masks.
3. **Cross-engine identity.** Run the heavy compute kernel with no rendering,
   then pair it with a short compute trigger and separately a short render trigger.
   Precreate resources and reuse the same identities. Record channel-control VAs,
   QID/HBID rows, root generations, FList state/pressure and the exact resource
   transition around the first one-stamp-short job. Native matched captures must
   include simultaneous owners, not repeated samples from one queue.
4. **Publication versus retirement.** With pool creation excluded, timestamp the
   third submission's admission, ConfigUpdate ingestion, hardware start/end,
   dependency progress and render job-list unlink. Test whether failure follows
   a publication/state update even when the trigger has already retired.
   Preinstall both render pairs and every compute graph while retaining all
   VMs/storages; then run two heavy mixed clients plus one compute client without
   graph/PB/context creation or handoff. Compare against triggering only first
   render installation, only storage handoff, and only first PB/HWPB allocation.
   If a specific mutation is responsible, qualify its resource/ack boundary;
   do not hold a new exclusion lease across every render/compute execution.
5. **Native lifecycle.** Trace first installation, normal repeat, context change,
   logical close and final hardware-queue destruction. Identify the writer/ack
   clearing the installed slot and any cached SKSM context references. A logged
   host no-op, completed command, or error fence cannot substitute for that ack.
6. **Acceptance, after the contract is implemented.** More than 58 distinct client
   lifetimes without processor resets or growing retained roots; sustained mixed
   overlap; close/cancel/failure injection; context-ID reuse; allocation pressure;
   independent-output canaries; fairness; throughput and tail latency versus the
   uncapped path. A passing capped test alone is not architectural validation.

A boot-time stress test can detect a failing configuration, but finite successful
tests cannot establish physical capacity or ABI semantics. Keep qualification
explicit and report its workload and measured performance; do not silently use
its largest passing client count as a new production concurrency cap.

## Changes made in this review

Misleading ownership comments were corrected in `g17_drm.rs` and
`g17_queue_limits.rs`. A fault-only `G17PComputeLeaseGuard::admission_snapshot`
reads compute lease count, render slot mask, install exclusion, render transition
and render quarantine mask under the existing registry mutex. The first compute
quarantine and retirement-error paths log it, including zero-duration recovery
which can bypass quarantine. These are host leases, not a hardware slot claim or
peak-overlap trace. Production admission, concurrency, pool size, binding and
reclamation behavior remain unchanged. `test_g17_compute_firmware_ownership_model.py`
has six passing proof-obligation tests, including a counterexample to deriving
detach from work completion and stop-only cumulative context exhaustion.

## Opt-in render transition qualification

`asahi.g17p_render_transition_probe=1` now enables **observation only**, default
**0**. It logs `submission`, `slot`, `reasons`, `class`, `selected` and `held` for
nonzero observations. The separate `asahi.g17p_render_transition_class` defaults
to **0**, which never drains compute. Select exactly one value:

| Bit / selector | Predicted mutation |
| --- | --- |
| 1 | Queue registration |
| 2 | VM/context handoff or pending context update |
| 4 | New PB/HWPB storage graph |
| 8 | FList initialization |
| 16 | TVB capacity growth |
| 32 | Desync rebuild (normally also has bit 4) |

Bits 64 (unreachable mapping), 128 (session recycle) and 256 (query error) are
observation-only. A guard requires `reasons == class`, with exactly one valid
selector bit. Compound observations, invalid selectors and query errors never
drain compute. This intentionally cannot qualify a cold registration that also
creates PB/FList state, or a desync rebuild that also allocates PB storage. Those
require a future hook at the particular mutation phase; guarding the whole
submission cannot honestly isolate them.

The query inspects the graph that normal handoff will select, including cached
graphs, before predicting new PB allocation or TVB growth. Cached reactivation
rebases its cursors; its outgoing ordinal is not classified as desync. A completed
same-owner render with registered queues and sufficient TVB/FList state reports
zero. The query does not call the recycle predicate before handoff.

The runtime mutex is released before acquiring the typed transition guard.
The registry publishes transition intent before draining existing compute
leases, blocking new compute admission while retirement can continue. A waiting
first-compute installer cannot claim installation while this render slot is
held. Competing render transitions wait; stable render work retains its existing
overlap. Interruption or a fault rolls back intent and wakes waiters.
The reasons are rechecked after draining. If the selected class is no longer
isolated, the guard is dropped before publication rather than broadening it.

The guard ends when the host `submit_translated_render` call returns, on success
or failure. It does **not** wait for GPU render retirement or establish a native
firmware detach/ack contract. Nondefault split-launch modes may already wait for
TA inside that call. `drain-ns` measures acquisition through compute drain;
`guard-ns` measures the full interval from acquisition attempt through release,
including contention and drain. Console logging itself adds diagnostic cost.
The revised probe's throughput/latency cost and hardware behavior are
**unmeasured**; this must not become a production workaround by default.

First run observation only. If it confirms isolated `reasons=0x2` cached
handoffs, compare identical fresh-boot workloads with class 0 versus class 2,
retaining completion, admission, graph/context and elapsed-throughput evidence.
Class 2 may intentionally drain frequently when three clients alternate over two
render slots; that is a handoff experiment, not first-install exclusion. A passing
probe would implicate the excluded interval, not prove a native ordering contract.

`test_g17_render_transition_probe.py` compiles the actual registry methods,
manager/runtime query and render publication loop against host adapters. Its
15 tests cover default-off and observation-only bypass, steady overlap, writer intent, wakeups,
first-install ordering, competing writers, interruption/fault rollback, query
conditions, unlocked draining, and release on host-publication success/failure.
They also exercise actual context-update completion, incoming cached graph
selection, exact single-class filtering, query errors and changed reasons after
drain. These are host proofs, not firmware validation.

### Rejected broad-probe hardware result

`diagnostics/neo-dmesg-render-transition-probe-r1-fail.log` records 44 guard
acquisitions/releases and 42 `session recycle requested ... same-owner=false`
messages. The old query called `retained_render_needs_recycle` **before**
`handoff_retained_render_vm`; actual submission calls it **after** handoff.
Thus cached owner alternation was treated as a broad transition and emitted
misleading recycle messages. The normal paired-completion path does call
`complete_context_update`, which clears pending state; this trace does not show
that pending state was stuck.

Recorded guard intervals have median 0.500790 ms, maximum 5.759530 ms, and sum
44.328551 ms. That sum includes contention/drain and potentially overlapping
acquisition attempts; it is **not** a measured throughput loss. The coordinator
reported round-0 triple-mixed failure (0/3), then a fresh-boot 3/3 pass with the
same module and the parameter removed. The failure trace has recovery reason 0,
sources 2, no blamed QID, and three ENODATA retirement snapshots with compute
leases=3 and no render/transition lease. This rejects the old probe for the
experiment; it does not establish a hardware execution-width limit.

The causal path from the probe to reason 0 remains unknown. Code inspection
shows that repeated drains block newcomers, then notify all compute waiters at
host publication return, allowing compute to bunch up again while GPU render
execution continues. Logging also changes timing. Burst synchronization and
timing perturbation are hypotheses to test, not demonstrated firmware causes.

## Compute-only burst qualification after the class-2 failure

The coordinator's class-2 test failed round 0 while probe-off and observation-only
controls each passed 3/3 on the same module. The saved
`diagnostics/neo-dmesg-render-transition-class2-fail.log` reports reason 2,
sources 2, blamed QID 5, ordinal 51, target 0x33 versus completed 0x32, and no MMU
fault. Its retirement snapshot is `(1, 2, false, false, 0)`: one compute lease,
render slot 1 occupied, no install/transition exclusion. The render counters were
equal but its job list was still linked. Excluding host cached-handoff overlap
therefore did not solve the original failure. It did not exclude concurrent GPU
render execution; the handoff guard ends at host publication, not retirement.

### Inspected admission and publication ordering

`G17PRenderTransitionGuard::drop` clears the flag under the registry mutex,
unlocks, then calls `notify_all`. Every awakened steady compute worker separately
checks admission and increments the compute lease count. Each then competes for
the runtime mutex; that mutex covers `publish_async_compute`, including its
RTKit activation/direct-kick sends. There is no inspected host path in which
another such publisher interleaves its ring/QoS writes between those sends.
Compute lease Drop also notifies all, so wakeups are not unique to handoffs.

The repeat path publishes command pointer, QoS state, ConfigUpdate, SKSM entry,
AddKicks, EntrySignal, then the shared CL_2 outer slot and its producer. Store
fences separate the entry/producer transitions. The outer producer advances
modulo 256 and refuses reuse when the next slot equals either consumer. QID
rings/storage are separate. QoS queue rows are QID-indexed, scheduler-state and
share rows are HardwareBufferID-indexed, and aggregate counters are shared.
HardwareBufferIDs come from the shared allocator rather than a constant.

All compute QIDs intentionally reference one manager-owned
`G17PClChannelControl` page for physical CL_2. It is initialized once; inspected
repeat publication does not reinitialize it. Its firmware concurrency contract
still needs matched native multi-QID evidence. Repeat notifications send the
direct kick; activation is omitted when the per-queue cached channel identity
is unchanged. The runtime mutex establishes host order, not proof that firmware
has consumed the preceding queue's update before the next publication.

Removing `notify_all` is not a safe control experiment. Kernel condvar wakes
are non-sticky, and another event is not guaranteed after the predicate clears;
already-waiting jobs could sleep indefinitely. Replacing it with `notify_one`
would also change admission fairness and concurrency rather than isolate the
state mutation.

### Ranked candidates (hypotheses, not diagnosed causes)

1. **Firmware admission/notification or shared CL_2 state under clustered
   publication.** The guard bunches eligible compute wakeups; the same reason-2
   one-stamp-short shape then returns. A shared channel-control page and shared
   outer channel remain firmware-visible across serialized host publications.
   Neither a consumed item ring nor a successful RTKit send proves dispatch of
   the final SKSM entry. This motivates the compute-only burst experiment; it is
   not evidence that sharing the channel page is wrong.
2. **Concurrent execution resource pressure or retirement ownership.** The
   failed mixed run still had a live render lease/job list. Guarding handoff
   leaves that overlap intact. Previously passing long-lived pure-compute runs
   weaken a simple three-queue capacity theory but do not exclude pressure that
   depends on render, private memory, or FList occupancy. Match aggregate work
   and allocation demand before inferring a hardware width limit.
3. **Retained context/QoS ownership or visibility.** No MMU fault rules out the
   observed unmapped-root failure, not a stale valid reference or a firmware
   sampling/ownership mismatch. The per-QID/HBID rows and barriers are present;
   native simultaneous-QID captures are needed before changing indexing or
   inventing additional fences/activation messages.
4. **A plain host outer-ring producer race or lost host wakeup** is less
   supported: the runtime mutex covers writes and sends, outer full checks
   exist, and the failed work had passed admission and reached firmware. Tests
   establish host wakeup/rollback behavior. They cannot prove the firmware
   memory-visibility or doorbell protocol.

### Bounded diagnostic and interpretation

`asahi.g17p_compute_burst_probe` defaults to **0**. Mode **1** logs up to 192
steady publications without waiting. Mode **2** gathers three distinct logical
owners before compute lease acquisition, then wakes the cohort together. It
does not hold the runtime mutex or a GPU lease while gathering, does not drain
other compute work, and does not cap or serialize GPU execution. First installs,
active/quarantined/released queues, stale/missing contexts and pending graph
rebinds are ineligible. Runtime validation still runs at actual publication.

Collection has a 20 ms deadline (subject to kernel scheduling/timer granularity)
and at most 64 cohorts, including aborted cohorts. Timeout releases a partial
group into normal admission; it does not fail its jobs. Signals cancel only the
interrupted participant and release peers. Render acquisition changes the
recorded epoch; render/install transitions abort collection even if they retire
before waiters resume. Faults release the cohort without clearing device poison.
Duplicate owners and arrivals while an older cohort departs bypass gathering.
The normal admission and publication path follows every release.

One line per eligible participant is emitted **after** the runtime publication
lock is dropped. It records cohort, parties, outcome, logical owner, QID,
submission ID, gather duration, release-to-publication-return duration, render
epoch, compute readers at that participant's departure, and publication errno.
Outcome 0 means a full three-owner release; 1 timeout, 2 signal, 3 render/install
transition, 4 fault, and 5 observation-only. Only outcome-0, three-party cohorts
with three successful publication records qualify as the intended burst.
Release times are common within a cohort, so the spread of the three
`release-to-published-ns` values measures host publication-return spread. These
are not GPU start times or proof of simultaneous hardware dispatch. Readers at
departure may differ because other released participants can already be admitted.

Run three **long-lived, prewarmed compute-only queues**, with render transition
probes disabled, comparing modes 0, 1 and 2 on fresh boots with identical kernels
and aggregate workload. Verify no render acquisitions during the measured
window, enough outcome-0 cohorts, three distinct QIDs per cohort, and their
publication timing spread. A failure confined to mode 2 would show that render
mutation is not necessary for that reproduction and strengthen a burst-sensitive
admission/shared-state hypothesis. It would not identify the faulty field or
justify serializing compute. If mode 2 passes, it only rejects this particular
burst shape/workload; continue the matched mixed-execution pressure experiment.
If mode 1 fails too, logging/timing is a confounder. Aborted or partial cohorts
are inconclusive, not passes. The diagnostic's hardware throughput/latency cost
is unmeasured; report the logged gather cost and wall throughput in qualification.

`test_g17_compute_burst_probe.py` compiles the actual rendezvous, eligibility and
admission methods. Its 13 tests cover off/observe modes, distinct owners, full
cohorts with concurrent admission afterward, timeouts, signals, spurious wakes,
faults, transient render/install activity, graph-rebind exclusion, and the cohort
limit. The existing 15 render-probe tests and 35 retirement/dependency/rebind/
output-fence tests also pass. No kernel build or hardware deployment was performed
for this patch.

## Next discriminator: retained render resources versus live render passes

The coordinator reports a fresh-boot mode-2 pure-compute result of **192/192**,
including **60 full cohorts / 180 successful publications** from three distinct
prewarmed QIDs. Publication-return spread was **40.9–182.9 us**, mean **93.5 us**,
with no fault-like lines. The following exact triple-mixed round failed (two
DeviceLost, one Pass) after the 64-cohort diagnostic limit was exhausted. Its
reported signature was reason 2 / sources 2, QID 14 ordinal 49, target 0x31 versus
completed 0x30, valid satisfied dependency, zero stamp, no MMU fault, and admission
`(1, 1, false, false, 0)`. The mixed run formed no new burst cohorts.

This rejects clustered compute publication as a sufficient reproducer **for
that tested workload**, and reduces the priority of a compute-only CL_2/burst
hypothesis. It does not prove that live render execution is necessary: retained
render graphs, PB/HWPB reservations, FList lifetime, and actual execution must
still be separated. Nor do the passing/failing client combinations establish a
hardware limit of two render plus three compute queues.

### Smallest controlled workload matrix

On each fresh boot, prewarm **the same two render queues and three compute
queues**, retaining all logical queues, VMs, buffers and pipeline identities.
Render warmup must actually use both physical pairs; serial affinity can keep
reusing one pair. Use the census below to verify both retained storages exist.
Complete warmup, observe successful paired retirement, job-list unlink and
FList release acknowledgement, then retain both idle render graphs. Keep render
handoff and compute burst probes disabled. Compare:

| Case | Retained render graphs | Submitted render passes during compute | Purpose |
| --- | --- | --- | --- |
| A | Same two | None | Does retained resource ownership suffice? |
| B | Same two | Slot 0 only, fixed owner | One live pass, other graph retained |
| C | Same two | Slot 1 only, fixed owner | Pair/identity asymmetry control |
| D | Same two | Both, fixed owners, overlapping passes | Concurrent render resource demand |

Run the identical three-queue compute workload and submission count in every
case. Do not close/reopen render queues between cases or allow client handoff;
that changes the retained graph/cache/HWPB population. Prewarm TVB capacities
for the largest tested render so a later growth transition is not accidentally
the independent variable. Measure output correctness, GPU pass intervals where
available, throughput, tail latency, raw pool state and sampled ownership.

- If A reproduces, prioritize retained PB/HWPB/UMA/FList ownership or namespace
  interactions; a live render execution threshold is unnecessary for that case.
- If A passes and B/C differ, investigate pair-specific identity/configuration
  before aggregate capacity.
- If only D reproduces, prioritize shared resources held by concurrent passes:
  USC private-memory/FList usage, PB/TVB demand and firmware scheduler ownership.
  Two admitted host trackers alone do not establish simultaneous GPU execution.
- If all fixed-owner cases pass while triple-mixed churn still fails, return to
  graph/context/FList lifetime transitions. Do not add a concurrency cap.

Passing cells are evidence limited to their duration/workload, not proofs of
absence. Keep the numerical matrix, identities and measured overlap in the
hardware report rather than compressing it into a purported engine-width rule.

### Default-off DRAM resource census

`asahi.g17p_mixed_resource_probe=0` is the default and performs no snapshot reads
or logging. Mode **1** samples on the existing compute fault/timeout diagnostic
path only. Mode **2** additionally samples before compute publication at queue
ordinals 0, 1 and each multiple of 16. It changes no admission, publication,
retirement or resource-allocation decisions and propagates no snapshot errors.
It does not select/swap a render slot: both selected and parked owners are read
in place under the existing runtime mutex. Each census reports:

- For each render pair: host ordinal, tracker presence, active HWPB lease,
  publication flag and VM; separately retained descriptor context, HWPB ID,
  render pool, PM generation, TVB capacity and pending context update.
- Raw TA/3D done/read/write indices, raw two-word job-list links and expected
  head address, unlink predicate, and native pass timestamps. Missing storage
  and failed reads remain explicit `None`/`Err`, not zero/idle observations.
- Allocator free/reserved masks, cached graph count and retained PM count,
  global render FList lifecycle/refcounts/pending release state, UMA slots 0/1,
  the compute queue's FList and QoS owner, and render UMA growth words.
- Total `census-ns`, including sequential reads and logging. Host locking does
  not stop firmware memory updates, so this is not an atomic hardware snapshot.

Use mode 2 to verify the matrix setup and identity stability, mode 1 for a lower
perturbation fault witness, and mode 0 as the timing control. Snapshot/vmap/logging
cost is diagnostic overhead, not measured production slowdown; no hardware cost
has yet been measured for this patch. The periodic samples can miss short phases
or a transition between samples. Absence of a sampled active pass does not prove
absence of overlap.

The masks intentionally distinguish **reserved retained storage** from an
**active host HWPB lease**. A free bit can coexist with a reservation: the
submission's reference has retired but the retained graph still reserves its
identity. Likewise `tracker=None` plus retained storage does not by itself prove
firmware has dropped every graph reference. Successful completion, unlink and
FList acknowledgement are separate witnesses.

### TA versus 3D: what can and cannot be isolated

`G17PUserRenderStorage::snapshot` reads just two native words: pass start written
by TA and pass end written by 3D. It exposes them as `[start,start]` and
`[start,end]`. They are **not** a TA duration plus a fragment duration. TA queue
retirement does not supply the missing TA hardware-end timestamp, and equal
queue counters do not prove execution finished while the job list is linked.
The census therefore emits raw witnesses and does not label “TA-active” or
“3D-active”. A start without end establishes an unfinished started pass, which
may also be stalled or preempted; it does not identify the currently running DM.

After case D reproduces, vary one demand at a time with the same owner graph:
geometry/tiling-heavy work with tiny fragment coverage, versus simple geometry
with fragment-heavy work. Preserve compute shaders/private-memory requirements
and preallocated TVB capacity. Separately vary attachment footprint and compute
private-memory demand to distinguish PB/TVB pressure from USC/FList demand.
These are **stage-emphasis** experiments, not pure TA-only/3D-only execution.
Do not use split-launch waits, omitted work stages or serialized execution as
evidence of the normal scheduler contract. Precise phase attribution requires
an independently verified native/firmware TA-end or 3D-start witness, with QID,
stamp and timestamp identity; the current pass-wide pair cannot provide it.

### M1/M2 reference constraints

The inspected reference `workqueue.rs::can_submit` checks ring-space and event
credits, returning the oldest pending fence when capacity is occupied. It does
not establish a G17 global engine count. `buffer.rs::auto_grow` uses observed
`max_pages` pressure; `ensure_blocks` allocates before committing and updates
owner-only count fields only when `active_scenes == 0`. `new_scene` retains scene
resources and an active-scene count through lifetime ownership. These support
measuring the particular exhausted pool and respecting its ownership boundary,
not translating a passing client count into a permanent G17 concurrency cap.
This inspection neither challenges established M1/M2 conformance nor proves the
same firmware structures/limits apply to G17.

Six production-extracted census tests pass: default-off/fault-only/periodic
sampling, both physical slots without selection or owner changes, explicit
absence/read failures, and unchanged raw incomplete-pass/job-list evidence.
The existing timeout-reader 5, FList-reader 3, retirement 19, compute-burst 13,
and render-transition 15 tests also pass (61 total). No kernel build or hardware
deployment was performed for this census patch.
