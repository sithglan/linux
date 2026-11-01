# A18 Pro stable-channel follow-up audit

Local Rust-driver audit, 2026-09-19. No hardware execution, deployment, reboot,
serial access, DCP change, or detach implementation occurred in this pass.

## Corrected defects

### Device-control producer could erase the occupied-ring distinction

`G17ResourceOwner::publish_primary_device_control_raw` previously read both
cursors but unconditionally advanced the producer modulo 256. With 255 pending
records and a stalled consumer, the next append made producer equal consumer.
That represents an empty ring; another append can then overwrite outstanding
work. Invalid cursor values were also masked into valid producer slots.

The production-method harness reproduced four failing cases before the change.
The publisher now rejects out-of-range cursors with `EIO` and a full ring with
`EAGAIN`, before acquiring writable views or changing any record, power tally or
producer. It keeps the normal publication order and 255 usable credits.
It adds no allocation, sleeping, execution gate or batching limit.

Architectural evidence agrees: the existing M1/M2 driver transport in
`channel.rs::TxChannel::put` checks next-producer against consumer before writing;
the pinned G17X producer checks the same full condition at `0xfffffe0008b10cc8`.
The M1/M2 path sleeps on fullness. Copying that sleep into G17 event service
under the runtime mutex would require a separate liveness analysis.

`test_g17_device_control_capacity.py` extracts the complete production publisher
and power-opcode classification, replacing GEM mappings/counter reads with
fallible host-backed storage. Five Rust tests pass, including all 65,536 valid
cursor pairs, both power-accounting opcode classes at every full-ring position,
invalid cursors, writable-view failure atomicity, and 1,000 rejected appends
followed by exactly one restored credit. These are host results, not evidence
that a captured CTS failure reached ring fullness.

The mandatory TVB reply path now handles that integration boundary. If the
event has already grown storage and the control ring is full, the manager keeps
the exact encoded 64-byte reply in one fixed pending slot and stops both role
event-ring consumers. It publishes that record before reading another event,
clears the slot before the RTKit announcement so a send failure cannot replay
the grow, and rejects a reply captured in an older recovery generation. Other
control producers yield the next returned credit to this pending reply.

The same backpressure path now preserves a completion-triggered FList release.
If the release cannot be published, the Ready lifecycle and zero reference
count remain intact and the exact release is retried after the mandatory TVB
reply. An explicit continuation covers deferral outside the normal event
worker, and render admission waits outside the runtime mutex while either
mandatory record is pending. This prevents a new render from reusing lifecycle
state before the old release has reached firmware without adding work to the
ordinary submission path beyond a pending-state check under the existing lock.

The event worker retries after releasing the runtime mutex and sleeping one
millisecond; ordinary publication does not sleep or take a new lock. Retry is
bounded to 1,000 worker passes, and timeout enters the existing crashed-session
path. Requeue is performed while holding the same runtime mutex used by
disarm, so unbind either cancels the queued pass or prevents it from being
queued. Inline event service can publish the pending reply even when the IRQ
count was already consumed, then continues the ring in that pass.

The production-method host harness covers full-ring deferral, repeated blocked
retries, one returned credit, exactly one grow/record, consecutive TVB events,
cross-recovery rejection, retry bounds, TVB-before-FList ordering, the explicit
continuation, render-admission exclusion, and both pending wake predicates.
These remain host results; hardware full-ring stress and CTS qualification are
still required.

### Failed compute allocation retained an unused logical control

`prepare_async_compute_queue` used to session-retain the incoming control before
reserving runtime storage or obtaining a QID/graph. At QID exhaustion, a new
logical queue could therefore leave its 16 KiB control retained until processor
stop despite never binding or publishing it. Idempotence protects repeated
attempts using the same control, but not churn through distinct logical queues.

Retention now occurs after graph allocation and binding/context consistency
validation, immediately before either alias binding or context retargeting.
All potentially exposed controls remain retained. A retention allocation failure
leaves an empty unpublished graph available for retry.

The extended `test_g17_async_compute_rebind.py` executes production prepare,
publish and close methods with fallible allocation/retention/binding actuators.
Before correction, 10,000 failed prepares invoked retention 10,000 times; after
correction they invoke it zero times, with no binding or owner transition.
The retry and existing ambiguous-publication lifetime tests pass. The separate
production control-retention tests continue to verify actual page lifetime and
idempotent session-list membership.

## Ownership, races and hot-path findings

- `ChannelTable` validates exact stable identity and engine; cancellation refuses
  published leases. Constructor rollback returns only the matching unpublished
  lease. The render pair's failed second allocation rolls back the first.
- Runtime ownership mutation is serialized by the existing runtime mutex.
  Different logical render owners keep distinct retained graphs; compute close
  cannot make a published graph eligible for a different owner. Same-owner
  contention waits, while new-owner capacity exhaustion returns `ENOSPC`.
- Completion dispatch preserves the 128-bit QID mask. Compute completion uses
  two bounded owner passes around one shared completion scan; it does not scan
  the entire completion ring separately for each owner. Broad observation remains
  for lost-edge/watchdog paths. No replacement index is justified without profiling.
- First selection allocates parked render storage; repeat selection swaps retained
  state without per-switch allocation. Logical control session retention is O(1)
  after its first successful retention. Channel acquisition and owner lookup scan
  bounded arrays; no measured throughput cost is claimed by this inspection.
- The shared M1/M2 `event.rs`/`workqueue.rs` lifetime model separates stamp ownership
  from pending-command retirement. Pinned G17X/IOGPU evidence likewise separates
  released-stamp eligibility, scheduler resource detach and root invalidation.
  Neither reference licenses early reclamation for an unverified G17 protocol.
- Published QIDs, controls and mappings still remain retained until a safe stop.
  Finite-session queue churn exhaustion and its potential denial-of-service impact
  remain open. The native opcode-`0x13` acknowledgement ordering is still unproved;
  [the detach plan](G17_NATIVE_DETACH_IMPLEMENTATION_PLAN.md) records the missing facts.

These observations and host regressions narrow specific failure paths. They do
not prove absence of races or establish the cause of any hardware CTS device loss.

## Exact verification

`./scripts/build-asahi-ko.sh` succeeded; log
`/tmp/g17-qid-capacity-audit-build.log` reports 81 warnings. Module SHA256:
`b5993eab528b1cf4f5d2799988e92a10de4d751eab876270545306905f4089f9`.
Vermagic: `7.1.6-neo-unified SMP preempt mod_unload aarch64`.

All 24 focused scripts passed: `channel_table`, `async_compute_rebind`,
`logical_control_retention`, `unpublished_compute_close`, `device_control_capacity`,
`owned_scheduler`, `completion_ownership`, `recovery_blamed_settle`,
`render_pair_identity_stress`, `render_slot_storage`, `render_queue_layout`,
`async_retirement`, `async_release_retains_firmware_state`, `compute_queue_reuse`,
`render_slot_affinity`, `render_vm_handoff`, `render_retired_root_lifetime`,
`lost_device_teardown`, `shutdown_ownership`, `lazy_compute`,
`queue_create_reservation`, `ready_lease`, `tvb_grow_event`, `parameter_buffer_lease`.
Names abbreviate `test_g17_<name>.py`. Their output reports 194 successful Rust
test executions, including nested/shared suites; this is not a unique-test count.

The compiled stack audit passed: probe 4,816 bytes; runtime start 1,728; boot
3,568; manager construction 336; resource construction 7,248; slot selection 80.
The module-internal direct-call estimate remains 20,864 bytes; external, indirect
and recursive paths are unmeasured. `git diff --check` passed.

Pinned G17X scheduler audit: 18 witnesses pass. Native detach audit: 52 instruction
witnesses, routing table, five encoded virtual targets and 521,218 predicate
boundary/wrap cases pass. No new full regression sweep or hardware results are
claimed for this artifact.
