// SPDX-License-Identifier: GPL-2.0-only OR MIT

//! T8140 DRM frontend and userspace render queue.

use core::ops::Range;

use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

use kernel::{
    bindings, c_str,
    dma_fence::{Fence, FenceContexts, FenceObject, FenceOps, RawDmaFence, UserFence},
    macros::vtable,
    new_condvar, new_mutex,
    prelude::*,
    static_lock_class,
    sync::{Arc, CondVar, Mutex},
    time::{delay::fsleep, Delta, Instant, Monotonic},
    uapi, xarray,
};

use crate::{
    alloc,
    driver::{AsahiDevRef, AsahiDevice},
    drm_gpu::{DrmGpu, DrmGpuParams},
    file, g17_job, g17_adt_j700, g17_completion, g17_live_boot, g17_perf, g17_render,
    g17_resources, g17_uapi, gem, gpu, hw, mmu, module_parameters, queue,
};

const T8140_USER_START: u64 = mmu::UAT_PGSZ as u64;
const T8140_USER_TOP: u64 = 1 << 42;
const T8140_UNKNOWN_PAGE: u64 = T8140_USER_TOP - 2 * mmu::UAT_PGSZ as u64;
pub(crate) const T8140_TIMESTAMP_ALIAS_RANGE: Range<u64> =
    crate::g17_queue_limits::CLIENT_LOW_VA_START..0x71_0000_0000;

/// Bounded opt-in render timing trace. `g17p_completion_trace` enables it and
/// `g17p_trace_depth` caps the number of records for the whole module lifetime.
static G17P_RENDER_TIMING_RECORDS: AtomicU32 = AtomicU32::new(0);
static G17P_RENDER_POLL_RECORDS: AtomicU32 = AtomicU32::new(0);

pub(crate) type SharedG17PRuntime = Arc<Mutex<g17_live_boot::G17PLiveRuntime>>;

/// Owned runtime handle used by DRM callbacks and preparation transactions.
/// Platform unbind shuts down hardware explicitly and breaks the runtime's
/// device references. Existing handles then retain only the closed runtime;
/// dropping the platform Arc cannot invalidate an in-flight host callback.
#[derive(Clone)]
pub(crate) struct G17PRuntimeRef {
    runtime: SharedG17PRuntime,
    preparations: Arc<crate::g17_preparation::G17PPreparations>,
}

// SAFETY: all mutable runtime access remains protected by its mutex. The Arc
// now also owns the allocation for every use of this handle.
unsafe impl Send for G17PRuntimeRef {}
// SAFETY: see the Send implementation above.
unsafe impl Sync for G17PRuntimeRef {}

impl G17PRuntimeRef {
    pub(crate) fn new(runtime: &SharedG17PRuntime) -> Self {
        let preparations = runtime.lock().preparation_control();
        Self { runtime: runtime.clone(), preparations }
    }

    pub(crate) fn mutex(&self) -> &Mutex<g17_live_boot::G17PLiveRuntime> {
        &self.runtime
    }

    /// Drain and dispatch under the runtime mutex, then signal the
    /// completions that pass proved with the mutex released (see
    /// `G17PDeferredCompletion`). Drained whatever the pass returned: a
    /// render settled before a later step failed is still retired.
    pub(crate) fn service_primary_firmware_events(&self) -> Result {
        let e2e_entry = crate::g17_e2e::stamp();
        let mut e2e_locked = 0;
        // v87 OBS_SKIP: decided under this drain's Runtime hold. The timing
        // buffer's per-slot gate records come from the loop, so keep it then.
        let skip_enabled = crate::g17_host_path::enabled(crate::g17_host_path::OBS_SKIP)
            && !crate::g17_timing_buffer::enabled();
        let (result, completed, observable) = {
            let mut runtime = self.mutex().lock();
            if e2e_entry != 0 { e2e_locked = crate::g17_e2e::now(); }
            runtime.begin_deferred_completion_drain();
            let result = runtime.service_primary_firmware_events();
            // Outermost hold: replay what a nested witness left (iso4).
            runtime.service_compute_witness_top("event-worker");
            let observable = !skip_enabled || runtime.render_observation_possible();
            (result, runtime.take_deferred_completions(), observable)
        };
        let e2e_unlocked = crate::g17_e2e::stamp();
        let e2e_signalled = completed.len() as u64;
        for completion in completed {
            completion.signal();
        }
        if e2e_entry != 0 {
            crate::g17_e2e::record(crate::g17_e2e::K_WORKER, result.is_ok() as u32, 0,
                [e2e_entry, e2e_locked, e2e_unlocked, e2e_signalled, 0]);
        }
        result?;
        let observed = if observable {
            self.service_render_observations().inspect_err(|error| {
                self.mutex().lock().note_render_observation_error(*error);
            })
        } else {
            crate::g17_host_path::count(&crate::g17_host_path::OBS_SKIPPED);
            Ok(())
        };
        // Explicit one-shot only, requested during/after a nonempty timing window.
        // The normal worker supplies a lifetime-safe Runtime owner; no raw
        // global pointer, fabricated firmware kick or wait is introduced.
        if crate::g17_timing_buffer::claim_completion_snapshot() {
            let result = self.mutex().lock().capture_completion_ring_snapshot();
            crate::g17_timing_buffer::finish_completion_snapshot(result);
        }
        observed
    }

    /// Called only by the single TVB allocation work item. The firmware-event
    /// worker remains independent while this worker allocates/maps pages.
    pub(crate) fn service_render_grows(&self) -> Result {
        loop {
            let epoch = match self.preparations.enter() {
                Ok(epoch) => epoch,
                Err(error) if error == EAGAIN || error == ENODEV => return Ok(()),
                Err(error) => return Err(error),
            };
            let mut runtime = self.mutex().lock();
            if !epoch.is_current() { return Ok(()); }
            let Some((drm, mut grow)) = runtime.next_render_grow()? else { return Ok(()); };
            drop(runtime);
            let result = grow.grow(&drm);
            let mut runtime = self.mutex().lock();
            // Always return the installed graph before rejecting an old epoch.
            let returned = runtime.return_render_grow(grow, result, epoch.is_current());
            drop(runtime);
            drop(epoch);
            returned?;
        }
    }

    fn service_render_observations(&self) -> Result {
        let mut cursor = 0u8;
        let mut remaining = g17_resources::G17P_RENDER_MAX_IN_FLIGHT;
        let mut retries = 0usize;
        while cursor < g17_resources::G17PRenderQueuePair::SLOT_COUNT {
            // Recovery must be able to close admission and join us. Never
            // wait for its epoch from the event worker needed to service it.
            let epoch = match self.preparations.enter() {
                Ok(epoch) => epoch,
                Err(error) if error == EAGAIN || error == ENODEV => break,
                Err(error) => return Err(error),
            };
            let mut runtime = self.mutex().lock();
            let e2e_locked = crate::g17_e2e::stamp();
            if !epoch.is_current() { break; }
            let Some(mut observation) = runtime.next_render_observation(cursor)? else { break; };
            let slot = observation.pair_slot;
            if slot != cursor { remaining = g17_resources::G17P_RENDER_MAX_IN_FLIGHT; retries = 0; }
            drop(runtime);
            let e2e_taken = crate::g17_e2e::stamp();
            let observed = observation.observe();
            let e2e_observed = crate::g17_e2e::stamp();
            let mut runtime = self.mutex().lock();
            let e2e_relocked = crate::g17_e2e::stamp();
            runtime.return_render_observation(observation);
            // Even an invalid epoch or failed read cannot drop an installed
            // graph. Only the recovery owner decides its final lifetime.
            if !epoch.is_current() {
                // Recovery owns classification once its epoch closes; it
                // settles retained tokens with the other pending completions.
                break;
            }
            runtime.begin_deferred_completion_drain();
            let result = runtime.commit_render_observation(slot, observed);
            // Consume this observation before a timeout can advance the
            // tracker. A snapshot of the old front must not reach its successor.
            runtime.resolve_deferred_render_timeouts();
            let completed = runtime.take_deferred_completions();
            let e2e_committed = crate::g17_e2e::stamp();
            drop(runtime);
            drop(epoch);
            let e2e_signalled = completed.len() as u32;
            for completion in completed { completion.signal(); }
            if e2e_locked != 0 {
                crate::g17_e2e::record(crate::g17_e2e::K_OBS_LOOP,
                    e2e_signalled | u32::from(matches!(result, Ok(true))) << 16, u64::from(slot),
                    [e2e_locked, e2e_taken, e2e_observed, e2e_relocked, e2e_committed]);
            }
            if result? {
                remaining -= 1;
                retries = 0;
                cursor = if remaining == 0 { slot + 1 } else { slot };
            } else {
                retries += 1;
                cursor = if retries >= 2 { slot + 1 } else { slot };
            }
            if cursor != slot {
                remaining = g17_resources::G17P_RENDER_MAX_IN_FLIGHT;
                retries = 0;
            }
        }
        Ok(())
    }

    pub(crate) fn service_primary_recovery(&self) -> Result {
        let needs_join = {
            let runtime = self.mutex().lock();
            let pending = runtime.primary_recovery_pending();
            if pending { self.preparations.close(false); }
            pending
        };
        // No runtime guard survives this boundary. Cancelled builders can
        // reacquire it, reject their epoch and wake this joiner.
        if needs_join { self.preparations.wait_for_preparers(); }
        let (result, completed) = {
            let mut runtime = self.mutex().lock();
            runtime.begin_deferred_completion_drain();
            let result = runtime.service_primary_recovery();
            runtime.service_compute_witness_top("recovery-worker");
            (result, runtime.take_deferred_completions())
        };
        for completion in completed {
            completion.signal();
        }
        result
    }

    pub(crate) fn primary_recovery_pending(&self) -> bool {
        self.mutex().lock().primary_recovery_pending()
    }

    pub(crate) fn primary_firmware_event_pending(&self) -> bool {
        self.mutex().lock().primary_firmware_event_pending()
    }

    pub(crate) fn primary_device_control_backpressured(&self) -> bool {
        self.mutex().lock().primary_device_control_backpressured()
    }

    /// Publish due scheduler-state releases and collect acknowledged
    /// teardowns under the runtime mutex; the returned entries (a closed
    /// queue's graphs, VM root and scheduler page) are dropped by the caller
    /// with that mutex released.
    pub(crate) fn service_logical_queue_teardowns(
        &self,
    ) -> Result<KVec<crate::g17_manager::G17PLogicalQueueTeardown>> {
        self.mutex().lock().service_logical_queue_teardowns()
    }

    pub(crate) fn logical_queue_teardown_polling(&self) -> bool {
        self.mutex().lock().logical_queue_teardown_polling()
    }

    /// Give the hardware compute graph back when a client queue goes away.
    fn release_compute_owner(&self, owner: u64) {
        self.mutex().lock().release_compute_owner(owner);
    }

}

/// A completion proven retired under the runtime mutex whose fence is
/// signalled after that mutex is released.
///
/// `G17PRenderCompletion::complete` writes the UAPI timestamps, releases the
/// physical lease (registry lock, waiter wake), drops the VM-job guard
/// (deferred unmaps) and signals the dma_fence with its callbacks inline;
/// `G17PJobCompletion::complete` does the same for compute. None of that
/// needs the runtime mutex, and running it under the mutex made every
/// publisher wait behind fence callbacks and unmaps. The settle paths push
/// one of these instead and whoever releases the mutex calls `signal`: the
/// worker wrappers on `G17PRuntimeRef` drain the list after unlocking, and
/// any other settle site queues the event worker to do so. Failure paths
/// (`fail_while_owned`, quarantine, processor stop) are not deferred: they
/// rely on the registry quarantine being recorded before the mutex drops.
pub(crate) enum G17PDeferredCompletion {
    Render {
        completion: Arc<G17PRenderCompletion>,
        observed: Result<g17_completion::PairedRenderCompletion>,
    },
    Compute {
        packet: Arc<g17_job::G17PComputeJobPacket>,
        timestamps: [u64; 2],
    },
    /// v93r: a quarantined render whose pair the firmware provably drained.
    /// Its fence was already failed; only its retained guards are released.
    ReleaseAfterDrain {
        completion: Arc<G17PRenderCompletion>,
    },
}

impl G17PDeferredCompletion {
    /// Exact-once is `complete`'s own `signalled` CAS: a completion that
    /// was failed or cancelled in between is left alone.
    pub(crate) fn signal(self) {
        match self {
            Self::Render { completion, observed } => completion.complete(observed),
            Self::Compute { packet, timestamps } => {
                if let Ok(completion) = packet.completion() {
                    completion.complete(Ok(timestamps));
                }
            }
            Self::ReleaseAfterDrain { completion } => completion.release_after_firmware_drain(),
        }
    }

    /// Queue on `list` for signalling after the runtime mutex is released.
    /// Without memory to defer, signal here instead: exact-once still holds
    /// and the caller merely pays the pre-deferral mutex hold for one fence.
    pub(crate) fn defer_into(self, list: &mut KVec<Self>) {
        if list.reserve(1, GFP_KERNEL).is_err() {
            self.signal();
            return;
        }
        if let Err(kernel::alloc::kvec::PushError(completion)) = list.push_within_capacity(self) {
            completion.signal();
        }
    }
}

/// Ceiling on live DRM compute queue objects for this device.
///
/// Bound logical queue objects before allocating roots and scheduler state.
/// Engine channels lazily acquire QIDs from the shared 128-entry table;
/// this logical-object limit does not describe execution concurrency.
const G17P_MAX_COMPUTE_QUEUES: u32 = crate::g17_queue_limits::MAX_QUEUES;

/// Admission leases for independently owned queue graphs.
///
/// Compute submissions -- including a queue's first installation -- overlap
/// one another and every independent render, exactly as render pairs do:
/// a queue's first tag-15 install is a per-queue firmware transaction, not
/// a device-wide one (AGXG17P registers command queues concurrently).
/// Each submission retains its lease through retirement or quarantine. The
/// runtime mutex serializes host publication, not asynchronous GPU execution.
/// This policy does not establish the capacity of a hardware execution resource:
/// a recovery source mask or a passing client count is not such a contract.
#[pin_data]
pub(crate) struct G17PQueueRegistry {
    /// Stable logical render ownership and transient compute installation.
    /// Steady-state compute readers do not occupy render slots.
    #[pin]
    lease: Mutex<G17PQueueLeaseState>,
    #[pin]
    lease_free: CondVar,
    /// `g17p_tile_backstop` (A): every render holding a pair lease, with its
    /// attachment footprint. Taken only nested inside `lease`.
    #[pin]
    tile_inflight: Mutex<KVec<G17PTileInflight>>,
    live_queues: AtomicU32,
    compute_readers: AtomicU32,
    compute_faulted: AtomicU32,
    /// Device-wide throughput accounting.
    ///
    /// Each update holds this mutex for integer accounting only. Disabled
    /// entirely when `g17p_perf_window` is zero.
    #[pin]
    perf: Mutex<G17PPerfState>,
}

/// The open reporting window and the instant it opened.
///
/// The kernel's `Instant` keeps its absolute nanoseconds `pub(crate)` to the
/// `kernel` crate, so the opening instant is held here and only the elapsed
/// span reaches `g17_perf`.
struct G17PPerfState {
    window: g17_perf::SubmitWindow,
    opened: Instant<Monotonic>,
}

const G17P_RENDER_SLOTS: usize =
    g17_resources::G17PRenderQueuePair::SLOT_COUNT as usize;
/// Host owner slots are independent of firmware QIDs. Slot zero retains the
/// bootstrap channels; other owners lazily lease IDs from the shared pool.
const G17P_ACTIVE_RENDER_SLOTS: usize = G17P_RENDER_SLOTS;
const _: () = assert!(G17P_ACTIVE_RENDER_SLOTS > 0);
const _: () = assert!(G17P_ACTIVE_RENDER_SLOTS <= G17P_RENDER_SLOTS);

/// `g17p_tile_backstop` (A): one render holding a physical pair lease, or
/// (slot `TILE_PENDING`) waiting for an overlapping render with `ticket`.
struct G17PTileInflight {
    owner: u64,
    sequence: u64,
    slot: u8,
    ticket: u64,
    footprint: Arc<crate::g17_tile_backstop::Footprint>,
}

const TILE_PENDING: u8 = u8::MAX;

/// Removes a waiting render's pending record on every early exit of
/// `acquire_lease`. Declared after the lease guard it is used under, so it
/// drops first, with `lease` still held (the `lease` -> `tile_inflight` order).
struct G17PTilePending<'a> {
    registry: &'a G17PQueueRegistry,
    owner: u64,
    ticket: u64,
}

impl Drop for G17PTilePending<'_> {
    fn drop(&mut self) {
        let mut inflight = self.registry.tile_inflight.lock();
        if let Some(index) = inflight.iter().position(|entry|
            entry.slot == TILE_PENDING && entry.owner == self.owner && entry.ticket == self.ticket)
        {
            drop(inflight.swap_remove(index));
        }
    }
}

/// `g17p_tile_backstop`: what a render brings to its pair lease.
#[derive(Copy, Clone)]
pub(crate) struct G17PTileRequest<'a> {
    footprint: &'a Arc<crate::g17_tile_backstop::Footprint>,
    sequence: u64,
    wait_through: Option<u64>,
}

impl<'a> G17PTileRequest<'a> {
    fn of(packet: &'a g17_job::G17PRenderJobPacket) -> Option<Self> {
        let order = packet.command().render_order;
        packet.footprint().map(|footprint| Self {
            footprint, sequence: order.sequence, wait_through: order.wait_through,
        })
    }
}

struct G17PQueueLeaseState {
    render_slots: u64,
    render_refs: [u8; G17P_RENDER_SLOTS],
    /// Logical queue that owns each render slot's retained storage, or 0 for a
    /// slot no client has claimed yet.
    ///
    /// Storage, descriptors, parameter buffers and the application root remain
    /// attached to that logical owner even while it has no active submission.
    render_owner: [u64; G17P_RENDER_SLOTS],
    /// Submission sequence for diagnostics. Zero means never acquired.
    render_epoch: [u64; G17P_RENDER_SLOTS],
    /// Slots whose retained graph is known to be unusable: a render abandoned
    /// after a completion timeout leaves firmware potentially still running the
    /// group, and its host-side tracker set, so every later submission on that
    /// slot fails. Retiring the slot costs concurrency; continuing to hand it
    /// out costs the session, because each client that lands on it is told
    /// EBUSY and reports VK_ERROR_DEVICE_LOST.
    render_quarantined: u64,
    render_released: u64,
    /// `g17p_exit_kill`: closed owners' pairs named by a published
    /// process-exit kill (device control 0x29). Held from just before the
    /// record is published, so the kill cannot reach a successor's QIDs,
    /// and kept for the rest of the session: the firmware kill task writes
    /// per-QID state (fw 0x26ae0..0x26b2c, words at +0x21088/+0x21090 of the
    /// base at fw 0x178010 -- likely KSMFE registers, not confirmed) whose
    /// effect on a later rebind (+0x1a=0, no host KSMFE write) is not
    /// established. Cleared at processor stop, like `render_quarantined`.
    render_kill_hold: u64,
    epoch: u64,
}

impl Default for G17PQueueLeaseState {
    fn default() -> Self {
        Self { render_slots: 0, render_refs: [0; G17P_RENDER_SLOTS],
            render_owner: [0; G17P_RENDER_SLOTS],
            render_epoch: [0; G17P_RENDER_SLOTS], render_quarantined: 0,
            render_released: 0, render_kill_hold: 0, epoch: 0 }
    }
}



impl G17PQueueLeaseState {
    const fn render_mask() -> u64 {
        // Slot zero is the boot channel owner, not a userspace lease.
        (u64::MAX >> (64 - G17P_ACTIVE_RENDER_SLOTS)) & !1
    }

    /// True once no render slot can ever be handed out again.
    fn render_exhausted(&self) -> bool {
        self.render_quarantined & Self::render_mask() == Self::render_mask()
    }

    fn take_render_slot(&mut self, slot: u8, owner: u64) -> u8 {
        let bit = 1u64 << slot;
        // A B1 physical TA/3D queue pair admits one render at a time. Logical
        // multi-flight is provided by selecting another retained pair, matching
        // AGXCommandQueue's array of AGX3DWorkQueues, rather than by publishing
        // a second command onto the same QIDs.
        debug_assert!(self.render_refs[slot as usize] == 0);
        self.render_refs[slot as usize] += 1;
        self.render_slots |= bit;
        self.render_released &= !bit;
        self.render_owner[slot as usize] = owner;
        self.epoch = self.epoch.wrapping_add(1);
        self.render_epoch[slot as usize] = self.epoch;
        slot
    }

    /// Keep live owners stable while spreading one logical queue's bounded
    /// multi-flight window across distinct physical queue pairs. A different
    /// owner may take only a graph whose exact prior owner has closed and whose
    /// last completion released the physical slot. The graph and QID pair stay
    /// allocated; the manager retains outgoing mappings through handoff.
    fn acquire_render_slot(&mut self, owner: u64) -> Option<u8> {
        if owner == 0 { return None; }

        // Prefer an idle retained work queue. If both retained pairs are
        // active, the bounded stacking path below admits further commands;
        // this preference does not impose a host retirement barrier.
        for slot in 0..G17P_RENDER_SLOTS {
            let bit = 1u64 << slot;
            if self.render_owner[slot] == owner
                && self.render_refs[slot] == 0
                && self.render_quarantined & bit == 0
            {
                // An owner left with one pair (its sibling was quarantined or
                // the namespace was full at first bind) gets the sibling back
                // at an idle moment -- the only time it can be built warm.
                let live = (0..G17P_RENDER_SLOTS)
                    .any(|other| self.render_owner[other] == owner && self.render_refs[other] != 0);
                if !live {
                    self.claim_sibling_render_slot(owner);
                }
                return Some(self.take_render_slot(slot as u8, owner));
            }
        }

        // Bound each logical queue to the native two physical pairs. Once
        // they are busy, queue the next kick on the owner's least-loaded pair
        // up to the per-pair command depth, as AGXG17P does on its work
        // queues: firmware orders consecutive kicks on one queue through the
        // implicit previous-kick parent and cross-pair work through explicit
        // parents, so the host does not wait for a retirement first.
        //
        // An owner that already holds a pair never takes ANOTHER one here,
        // under load. Native AGXCommandQueue creates both of its
        // AGX3DWorkQueues at command-queue creation, so a work queue is never
        // constructed while the sibling has live kicks. This driver used to
        // hand out the second pair lazily, exactly when the first was busy,
        // and build it cold there: measured on Neo 2026-09-26 (07:48, 07:54,
        // 08:12, 08:29, 08:54) that second pair's TA never launched and the
        // firmware requested a stamp-blamed recovery. Both pairs are now
        // claimed together at the owner's first acquisition (below) and
        // warmed by the publish path before the owner's first kick, so the
        // only pairs an owner can stack onto are ones it already warmed.
        let owned = self.render_owner.iter().filter(|id| **id == owner).count();
        if owned >= 1 {
            let depth = crate::g17_queue_limits::RENDER_KICKS_PER_PAIR as u8;
            let slot = (0..G17P_RENDER_SLOTS)
                .filter(|slot| {
                    self.render_owner[*slot] == owner
                        && self.render_quarantined & (1u64 << *slot) == 0
                        && self.render_slots & (1u64 << *slot) != 0
                        && self.render_refs[*slot] < depth
                })
                .min_by_key(|slot| (self.render_refs[*slot], u64::MAX - self.render_epoch[*slot]))?;
            self.render_refs[slot] += 1;
            self.epoch = self.epoch.wrapping_add(1);
            self.render_epoch[slot] = self.epoch;
            return Some(slot as u8);
        }

        let free = (!self.render_slots) & Self::render_mask() & !self.render_quarantined
            & !self.render_kill_hold;
        if free == 0 {
            return None;
        }
        let usable = |slot: usize| free & (1u64 << slot) != 0;
        // Reuse an exactly retired, closed owner before consuming another
        // graph/root lease. Fresh-first allocation pins all 58 app contexts
        // before the 63-slot render namespace fills, so QUEUE_CREATE fails
        // before the old fallback reuse branch can ever be reached.
        // Idle live owners and closed-but-active slots remain ineligible.
        let first = if let Some(slot) = (0..G17P_RENDER_SLOTS)
            .filter(|slot| usable(*slot) && self.render_released & (1u64 << slot) != 0)
            .min_by_key(|slot| self.render_epoch[*slot])
        {
            self.take_render_slot(slot as u8, owner)
        } else {
            let slot = (0..G17P_RENDER_SLOTS)
                .find(|slot| usable(*slot) && self.render_owner[*slot] == 0)?;
            self.take_render_slot(slot as u8, owner)
        };
        // Native: two work queues per command queue, both from creation.
        // Claim the sibling now, while this owner has no live kick, as an
        // idle owned pair; `warmable_sibling_render_slot` hands it to the
        // publish path to construct before the owner's first doorbell.
        self.claim_sibling_render_slot(owner);
        Some(first)
    }

    /// Give `owner` its second physical pair as an idle owned slot (owner
    /// set, no reference, not active). Nothing firmware-visible happens here;
    /// the pair is constructed by `G17PLiveRuntime::warm_render_pair` at the
    /// next publication that finds the owner without live kicks. A namespace
    /// with no free slot leaves the owner on one pair, stacking kicks on it,
    /// which is the same depth the busy path already allows.
    fn claim_sibling_render_slot(&mut self, owner: u64) {
        // Opt-in `g17p_render_single_pair=1` (default 0) leaves every render
        // queue on one physical pair, stacking its kicks there. Measured on
        // Neo 2026-10-01, both ways:
        // - it avoided the hard hang (watchdog reset) when 8..16 clients
        //   construct render pairs at once: 8x20 and 16x10 passed (boots
        //   79600be8, d5dedb62) where two pairs hung in round 1, and it was
        //   about 3.5% faster at depth 2 (2.3% in Minecraft);
        // - it raised the per-pass tabench 3D-stall hazard about 1.5x
        //   (9.3e-3..1.22e-2 vs 6.2e-3..6.5e-3), and it is not native: the
        //   2026-09-28 macOS capture alternates two pairs per process (3D
        //   qids 16/18 switch on nearly every command buffer).
        // A mitigation, not parity; the cause of the two-pair hang is open.
        if crate::module_parameters::g17p_render_single_pair.value() != 0 { return; }
        let owned = self.render_owner.iter().filter(|id| **id == owner).count();
        if owned >= crate::g17_queue_limits::MAX_IN_FLIGHT_PER_QUEUE as usize {
            return;
        }
        let free = (!self.render_slots) & Self::render_mask() & !self.render_quarantined
            & !self.render_kill_hold;
        let usable = |slot: usize| free & (1u64 << slot) != 0;
        let sibling = (0..G17P_RENDER_SLOTS)
            .filter(|slot| {
                usable(*slot)
                    && self.render_owner[*slot] != owner
                    && self.render_released & (1u64 << slot) != 0
            })
            .min_by_key(|slot| self.render_epoch[*slot])
            .or_else(|| {
                (0..G17P_RENDER_SLOTS).find(|slot| usable(*slot) && self.render_owner[*slot] == 0)
            });
        let Some(slot) = sibling else { return };
        let bit = 1u64 << slot;
        debug_assert!(self.render_refs[slot] == 0);
        self.render_released &= !bit;
        self.render_owner[slot] = owner;
        self.epoch = self.epoch.wrapping_add(1);
        self.render_epoch[slot] = self.epoch;
    }

    /// The owner's other pair, if this owner has no live kick anywhere:
    /// `publishing` holds exactly the one unpublished reference of the job
    /// being published and the sibling holds none. Only then may the sibling
    /// be constructed (a cold pair is never built beside live kicks).
    fn warmable_sibling_render_slot(&self, owner: u64, publishing: u8) -> Option<u8> {
        if self.render_refs[usize::from(publishing)] != 1 {
            return None;
        }
        (0..G17P_RENDER_SLOTS)
            .find(|slot| {
                *slot != usize::from(publishing)
                    && self.render_owner[*slot] == owner
                    && self.render_refs[*slot] == 0
                    && self.render_quarantined & (1u64 << slot) == 0
            })
            .map(|slot| slot as u8)
    }

    /// `g17p_exit_kill`: hold `slots` if every one is still the departed
    /// `owner`'s: released by it, idle, and taken by nobody since. All or
    /// nothing.
    fn hold_for_exit_kill(&mut self, owner: u64, slots: u64) -> bool {
        if owner == 0 || slots & !Self::render_mask() != 0 { return false; }
        for slot in 0..G17P_RENDER_SLOTS {
            let bit = 1u64 << slot;
            if slots & bit == 0 { continue; }
            if self.render_owner[slot] != owner
                || self.render_refs[slot] != 0
                || self.render_slots & bit != 0
                || self.render_released & bit == 0
            {
                return false;
            }
        }
        self.render_kill_hold |= slots;
        true
    }

    fn release_render_slot(&mut self, slot: u8) {
        debug_assert!(slot < g17_resources::G17PRenderQueuePair::SLOT_COUNT);
        let refs = &mut self.render_refs[slot as usize];
        debug_assert!(*refs != 0);
        if *refs != 0 {
            *refs -= 1;
        }
        if *refs == 0 {
            self.render_slots &= !(1u64 << slot);
        }
    }

}

// ---- g17p_tile_backstop (B) ------------------------------------------------
impl G17PQueueLeaseState {
    /// Take `slot` for `owner` behind the owner's overlapping in-flight render
    /// there: only a pair the owner holds, not quarantined or held, with depth
    /// left (the same bound as the stacking path).
    fn acquire_render_slot_on(&mut self, owner: u64, slot: u8) -> Option<u8> {
        let index = usize::from(slot);
        if owner == 0 || index >= G17P_RENDER_SLOTS || self.render_owner[index] != owner {
            return None;
        }
        let bit = 1u64 << slot;
        if (self.render_quarantined | self.render_kill_hold) & bit != 0 {
            return None;
        }
        if self.render_refs[index] == 0 {
            return Some(self.take_render_slot(slot, owner));
        }
        if u32::from(self.render_refs[index]) >= crate::g17_queue_limits::RENDER_KICKS_PER_PAIR {
            return None;
        }
        self.render_refs[index] += 1;
        self.epoch = self.epoch.wrapping_add(1);
        self.render_epoch[index] = self.epoch;
        Some(slot)
    }
}

impl G17PQueueRegistry {
    fn new() -> Result<Arc<Self>> {
        Arc::pin_init(
            pin_init!(G17PQueueRegistry {
                lease <- new_mutex!(G17PQueueLeaseState::default(), "G17PQueueRegistry::lease"),
                lease_free <- new_condvar!("G17PQueueRegistry::lease_free"),
                tile_inflight <- new_mutex!(KVec::new(), "G17PQueueRegistry::tile_inflight"),
                live_queues: AtomicU32::new(0),
                compute_readers: AtomicU32::new(0),
                compute_faulted: AtomicU32::new(0),
                perf <- new_mutex!(
                    G17PPerfState {
                        window: g17_perf::SubmitWindow::default(),
                        opened: Instant::<Monotonic>::now(),
                    },
                    "G17PQueueRegistry::perf"
                ),
            }),
            GFP_KERNEL,
        )
    }


    /// Every compute job, first install included, takes a shared reader lease.
    /// Nothing drains: a queue's first tag-15 install is published exactly like
    /// a render pair's first install, concurrently with everything else. The
    /// guard lives through exact retirement and does not own graph lifetime.
    ///
    /// The admission poison refuses a compute job only when quarantine is
    /// device-wide (`QUARANTINE_SCOPE` bit 0 clear). With the scope set, a
    /// quarantined queue already refuses its own client
    /// (`check_async_compute_submission`, `async_compute_queue_matches`) and is
    /// never handed to another owner, so the latch has nothing left to
    /// protect. It used to be read unscoped here to wake installers parked
    /// on the first-install drain; that drain is gone, so the unscoped read
    /// only failed every OTHER client's compute publication with EIO after
    /// one client's timeout -- and, through `fail_outstanding_jobs_for_vm`,
    /// poisoned each fresh VM on its first compute job (Neo 2026-09-26
    /// 19:56:58: one qid-13 timeout, then 179 `POISONED err=-5` and 1,410
    /// failed submissions, no recovery). A fresh VM must never inherit another
    /// client's compute failure.
    fn acquire_compute(self: &Arc<Self>) -> Result<G17PComputeLeaseGuard> {
        if self.compute_faulted.load(Ordering::Acquire) != 0
            && self.compute_poison_is_device_wide()
        {
            return Err(EIO);
        }
        // Compute has no exclusion relationship with render admission. This
        // count is diagnostic only; no waiter uses it as a capacity predicate.
        // Publication revalidates queue/context/device state under runtime.
        self.compute_readers.fetch_add(1, Ordering::AcqRel);
        Ok(G17PComputeLeaseGuard { registry: self.clone(), released: false })
    }


    /// Fold one finished render into the open window.
    ///
    /// Returns a report only on the submission that fills the window; the
    /// caller prints it with this lock released.
    fn record_render(&self, sample: g17_perf::RenderSample) -> Option<g17_perf::SubmitReport> {
        let size = module_parameters::g17p_perf_window.value();
        if size == 0 {
            return None;
        }
        let mut state = self.perf.lock();
        if !state.window.is_open() {
            state.window.open();
            state.opened = Instant::<Monotonic>::now();
        }
        state.window.record_render(sample);
        Self::close_full_window(&mut state, size)
    }

    /// Fold one queued asynchronous compute into the open window.
    ///
    /// Only the ioctl cost is recorded: the job has not run when the ioctl
    /// returns, by design, so there is no GPU interval to attribute to it here.
    fn record_compute(&self, ioctl_ns: u64) -> Option<g17_perf::SubmitReport> {
        let size = module_parameters::g17p_perf_window.value();
        if size == 0 {
            return None;
        }
        let mut state = self.perf.lock();
        if !state.window.is_open() {
            state.window.open();
            state.opened = Instant::<Monotonic>::now();
        }
        state.window.record_compute(ioctl_ns);
        Self::close_full_window(&mut state, size)
    }

    fn close_full_window(
        state: &mut G17PPerfState,
        size: u32,
    ) -> Option<g17_perf::SubmitReport> {
        if state.window.submissions() < size {
            return None;
        }
        let wall_ns = state.opened.elapsed().as_nanos().max(0) as u64;
        Some(state.window.close(wall_ns))
    }

    /// Reserve one queue slot, or report a full device.
    fn reserve_queue(&self) -> Result {
        let mut live = self.live_queues.load(Ordering::Acquire);
        loop {
            if live >= G17P_MAX_COMPUTE_QUEUES {
                return Err(ENOSPC);
            }
            match self.live_queues.compare_exchange(
                live,
                live + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(observed) => live = observed,
            }
        }
    }

    fn release_queue(&self) {
        self.live_queues.fetch_sub(1, Ordering::AcqRel);
    }

    fn release_render_owner(&self, owner: u64) {
        let mut held = self.lease.lock();
        for slot in 0..G17P_RENDER_SLOTS {
            if held.render_owner[slot] == owner {
                held.render_released |= 1u64 << slot;
            }
        }
        drop(held);
        self.lease_free.notify_all();
    }

    pub(crate) fn release_retained_owners_after_processor_stop(&self) {
        let mut held = self.lease.lock();
        let released = held.render_released & !held.render_slots;
        for slot in 1..G17P_RENDER_SLOTS {
            if released & (1u64 << slot) != 0 {
                held.render_owner[slot] = 0;
                held.render_epoch[slot] = 0;
            }
        }
        held.render_released &= !released;
        held.render_quarantined &= !released;
        // No firmware kill survives a processor stop.
        held.render_kill_hold = 0;
        self.lease_free.notify_all();
    }

    /// `g17p_exit_kill`: hold `slots` -- render pairs a process-exit kill is
    /// about to name -- out of reuse until firmware acknowledges the kill.
    /// Every pair must still belong to the departed `owner` (closed, idle,
    /// not yet taken by anyone else); otherwise nothing is held and false
    /// is returned, and the caller does not publish. Runtime -> registry is
    /// the established nesting (`admission_snapshot`, lease drop).
    pub(crate) fn hold_render_slots_for_exit_kill(&self, owner: u64, slots: u64) -> bool {
        self.lease.lock().hold_for_exit_kill(owner, slots)
    }

    /// Undo a hold whose kill record could not be published.
    pub(crate) fn release_exit_kill_hold(&self, slots: u64) {
        if slots == 0 { return; }
        let mut held = self.lease.lock();
        held.render_kill_hold &= !slots;
        drop(held);
        self.lease_free.notify_all();
    }

    /// Select one idle retained work queue from this logical queue's bounded
    /// set, allocating another physical pair when its other render is active.
    ///
    /// `owner` is the logical queue id. Retained descriptors name that queue's
    /// independently allocated application-GART descriptor context, so two
    /// queues in one VM cannot share one graph. Waiting here is interruptible
    /// for temporary contention. Exhaustion of retained owners
    /// returns ENOSPC because waiting cannot reclaim an installed channel.
    /// Whether the compute admission poison reaches clients other than the
    /// quarantined queue's owner. A quarantined queue is retired permanently
    /// and owns nothing that can be reused, so with the quarantine scoped the
    /// latch must not fail unrelated render or compute submissions or status
    /// queries.
    fn compute_poison_is_device_wide(&self) -> bool {
        crate::g17_config::QUARANTINE_SCOPE & 1 == 0
    }

    /// Release the compute admission poison at a proven rehabilitation
    /// boundary: a firmware recovery that acknowledged its final clear.
    pub(crate) fn clear_compute_poison(&self) -> bool {
        let held = self.lease.lock();
        let was = self.compute_faulted.swap(0, Ordering::AcqRel) != 0;
        drop(held);
        self.lease_free.notify_all();
        was
    }

    /// See `G17PQueueLeaseState::warmable_sibling_render_slot`.
    fn warmable_sibling_render_slot(&self, owner: u64, publishing: u8) -> Option<u8> {
        self.lease.lock().warmable_sibling_render_slot(owner, publishing)
    }

    fn acquire_lease(self: &Arc<Self>, owner: u64, tile: Option<G17PTileRequest<'_>>)
        -> Result<G17PLeaseGuard>
    {
        use crate::g17_tile_backstop::{self as backstop, Plan};
        if owner == 0 { return Err(EINVAL); }
        let mut held = self.lease.lock();
        // g17p_tile_backstop (C): a bounded wait for an overlapping render
        // of another queue; after it expires the ordinary selection applies.
        let mut tile_budget = kernel::time::msecs_to_jiffies(backstop::WAIT_CAP_MS);
        let mut tile_expired = false;
        let mut tile_pending: Option<G17PTilePending<'_>> = None;
        // Recovery isolation ADMISSION_WAIT: bounded wait for the reclaim of
        // an owner's quarantined pair (boot d324fcdd 05:38:44: spared VM
        // 2559's next render hit EIO here and `fail_unpublished` poisoned it,
        // 2.7 ms before the reclaim returned the pair).
        let isolate = crate::g17_recovery_isolation::enabled(
            crate::g17_recovery_isolation::ADMISSION_WAIT);
        let mut quarantine_budget = kernel::time::msecs_to_jiffies(
            crate::g17_recovery_isolation::ADMISSION_WAIT_MS);
        let mut quarantine_waited = false;
        let slot = loop {
            if self.compute_faulted.load(Ordering::Acquire) != 0
                && self.compute_poison_is_device_wide()
            {
                return Err(EIO);
            }
            // Every slot retired: no wait can ever succeed, so report the dead
            // device instead of sleeping forever.
            if held.render_exhausted() {
                return Err(EIO);
            }
            match held.render_owner.iter().position(|id| *id == owner) {
                Some(slot) if held.render_quarantined & (1u64 << slot) != 0 => {
                    // Y refuses when the owner's first pair is quarantined.
                    // With isolation the owner waits a bounded time for the
                    // reclaim that returns that pair, then refuses as Y does.
                    // (iso3.1: no fall-through onto a healthy sibling pair --
                    // a sibling kick may name the abandoned group as its
                    // cross-pair parent.)
                    if !isolate { return Err(EIO); }
                    {
                        if quarantine_budget == 0 { return Err(EIO); }
                        if !quarantine_waited {
                            quarantine_waited = true;
                            crate::g17_recovery_isolation::count(
                                &crate::g17_recovery_isolation::ADMISSION_WAITS);
                            pr_info!("G17P render lease waiting for quarantine release: owner={} slot={}\n",
                                owner, slot);
                        }
                        match self.lease_free.wait_interruptible_timeout(&mut held, quarantine_budget) {
                            kernel::sync::CondVarTimeoutResult::Signal { .. } => return Err(ERESTARTSYS),
                            kernel::sync::CondVarTimeoutResult::Timeout => {
                                quarantine_budget = 0;
                                crate::g17_recovery_isolation::count(
                                    &crate::g17_recovery_isolation::ADMISSION_TIMEOUTS);
                                pr_warn!("G17P render lease quarantine wait timed out: owner={} slot={}\n",
                                    owner, slot);
                            }
                            kernel::sync::CondVarTimeoutResult::Woken { jiffies } => quarantine_budget = jiffies,
                        }
                        continue;
                    }
                }
                None if !held.render_owner[1..].contains(&0)
                    && (held.render_released
                        & !held.render_slots
                        & !held.render_quarantined
                        & !held.render_kill_hold
                        & G17PQueueLeaseState::render_mask()) == 0
                    => return Err(ENOSPC),
                _ => {},
            }
            let ticket = tile_pending.as_ref().map(|pending| pending.ticket);
            let plan = match tile {
                Some(tile) if !tile_expired => self.tile_plan(&held, owner, &tile, ticket),
                _ => Plan::Free,
            };
            let picked = match plan {
                Plan::Free => held.acquire_render_slot(owner),
                Plan::Stack(slot) => held.acquire_render_slot_on(owner, slot),
                Plan::Wait => None,
            };
            if let Some(slot) = picked {
                if let Plan::Stack(_) = plan {
                    G17PQueueRegistry::note_tile("stacked behind its overlapping render", &backstop::STACKED,
                        owner, slot, tile);
                }
                // A waiting render's pending record becomes its in-flight one.
                drop(tile_pending.take());
                let recorded = tile.and_then(|tile| self.tile_record(owner, slot, &tile));
                break (slot, recorded);
            }
            if plan != Plan::Free {
                if tile_pending.is_none() {
                    G17PQueueRegistry::note_tile("waiting for an overlapping render", &backstop::WAITS,
                        owner, u8::MAX, tile);
                    if let Some(tile) = tile {
                        tile_pending = self.tile_wait(owner, &tile);
                    }
                }
                match self.lease_free.wait_interruptible_timeout(&mut held, tile_budget) {
                    kernel::sync::CondVarTimeoutResult::Signal { .. } => return Err(ERESTARTSYS),
                    kernel::sync::CondVarTimeoutResult::Timeout => {
                        tile_budget = 0;
                        tile_expired = true;
                        G17PQueueRegistry::note_tile("wait expired; publishing unordered",
                            &backstop::WAIT_TIMEOUTS, owner, u8::MAX, tile);
                    }
                    kernel::sync::CondVarTimeoutResult::Woken { jiffies } => tile_budget = jiffies,
                }
                continue;
            }
            if self.lease_free.wait_interruptible(&mut held) {
                return Err(ERESTARTSYS);
            }
        };
        core::mem::drop(held);
        let (slot, tile) = slot;
        Ok(G17PLeaseGuard {
            registry: self.clone(),
            slot,
            tile,
        })
    }

    /// v84: `acquire_lease` without waiting. `Ok(None)` when a wait would be
    /// needed or any refusal applies; the caller then takes the scheduler path,
    /// which reproduces the exact refusal (or waits) as before.
    fn try_acquire_lease(self: &Arc<Self>, owner: u64, tile: Option<G17PTileRequest<'_>>)
        -> Result<Option<G17PLeaseGuard>>
    {
        use crate::g17_tile_backstop::Plan;
        if owner == 0 { return Ok(None); }
        let mut held = self.lease.lock();
        if self.compute_faulted.load(Ordering::Acquire) != 0 && self.compute_poison_is_device_wide() {
            return Ok(None);
        }
        if held.render_exhausted() { return Ok(None); }
        match held.render_owner.iter().position(|id| *id == owner) {
            Some(slot) if held.render_quarantined & (1u64 << slot) != 0 => return Ok(None),
            // A queue that owns no pair yet takes its first graph (and sibling
            // claim) on the scheduler path only.
            None => return Ok(None),
            _ => {}
        }
        // g17p_tile_backstop: never wait here; a conflict takes the
        // scheduler path, which plans (and may wait) again.
        let plan = tile.map_or(Plan::Free, |tile| self.tile_plan(&held, owner, &tile, None));
        let picked = match plan {
            Plan::Free => held.acquire_render_slot(owner),
            Plan::Stack(slot) => held.acquire_render_slot_on(owner, slot),
            Plan::Wait => None,
        };
        let Some(slot) = picked else { return Ok(None); };
        if let Plan::Stack(_) = plan {
            G17PQueueRegistry::note_tile("stacked behind its overlapping render",
                &crate::g17_tile_backstop::STACKED, owner, slot, tile);
        }
        let recorded = tile.and_then(|tile| self.tile_record(owner, slot, &tile));
        core::mem::drop(held);
        Ok(Some(G17PLeaseGuard { registry: self.clone(), slot, tile: recorded }))
    }

    /// `g17p_tile_backstop`: the plan for a render of `owner` under the lease
    /// mutex (B, C, D). `tile_inflight` nests inside `lease` only.
    fn tile_plan(&self, held: &G17PQueueLeaseState, owner: u64, tile: &G17PTileRequest<'_>,
        ticket: Option<u64>) -> crate::g17_tile_backstop::Plan
    {
        use crate::g17_tile_backstop as backstop;
        let inflight = self.tile_inflight.lock();
        if inflight.is_empty() { return backstop::Plan::Free; }
        backstop::plan(backstop::mask(), owner, tile.wait_through, ticket, tile.footprint,
            inflight.iter().map(|entry| backstop::Inflight {
                owner: entry.owner,
                sequence: entry.sequence,
                slot: entry.slot,
                footprint: &entry.footprint,
                quarantined: entry.slot < 64 && held.render_quarantined & (1u64 << entry.slot) != 0,
                pending: entry.slot == TILE_PENDING,
                ticket: entry.ticket,
            }))
    }

    /// `g17p_tile_backstop` (C): record a render that starts waiting, so a
    /// newer overlapping render of another queue waits behind it. Called
    /// with `lease` held; the returned guard removes the record.
    fn tile_wait<'a>(&'a self, owner: u64, tile: &G17PTileRequest<'_>) -> Option<G17PTilePending<'a>> {
        let ticket = crate::g17_tile_backstop::next_ticket();
        let entry = G17PTileInflight {
            owner, sequence: tile.sequence, slot: TILE_PENDING, ticket, footprint: tile.footprint.clone(),
        };
        self.tile_inflight.lock().push(entry, GFP_KERNEL).ok()?;
        Some(G17PTilePending { registry: self, owner, ticket })
    }

    /// `g17p_tile_backstop` (A): record a render that now holds `slot`. The
    /// guard forgets it when the lease drops (completion releases the lease
    /// before its fence signals). Called with `lease` held.
    fn tile_record(&self, owner: u64, slot: u8, tile: &G17PTileRequest<'_>) -> Option<(u64, u64)> {
        let mut inflight = self.tile_inflight.lock();
        let entry = G17PTileInflight {
            owner, sequence: tile.sequence, slot, ticket: 0, footprint: tile.footprint.clone(),
        };
        match inflight.push(entry, GFP_KERNEL) {
            Ok(()) => Some((owner, tile.sequence)),
            Err(_) => {
                crate::g17_tile_backstop::count(&crate::g17_tile_backstop::UNRECORDED);
                None
            }
        }
    }

    fn tile_forget(&self, slot: u8, owner: u64, sequence: u64) {
        let mut inflight = self.tile_inflight.lock();
        if let Some(index) = inflight.iter().position(|entry|
            entry.slot == slot && entry.owner == owner && entry.sequence == sequence)
        {
            drop(inflight.swap_remove(index));
        }
    }

    /// The first few backstop decisions per boot, for hardware verification.
    #[cold]
    #[inline(never)]
    fn note_tile(what: &str, counter: &core::sync::atomic::AtomicU64, owner: u64, slot: u8,
        tile: Option<G17PTileRequest<'_>>)
    {
        let events = crate::g17_tile_backstop::count(counter);
        if events <= 8 {
            pr_info!("G17P tile backstop: owner={} sequence={} {} (slot {}; #{}; totals(stacked,waits,timeouts,object,truncated,unrecorded)={:?})\n",
                owner, tile.map_or(0, |tile| tile.sequence), what, slot, events,
                crate::g17_tile_backstop::totals());
        }
    }

    /// Retire one render slot whose retained graph can no longer be trusted.
    ///
    /// Called after a completion timeout has abandoned a submission. Nothing is
    /// freed -- the firmware may still own the abandoned group -- but no client
    /// is handed this slot again for the rest of the session.
    fn quarantine_render_slot(&self, slot: u8) {
        let mut held = self.lease.lock();
        if held.render_quarantined & (1u64 << slot) != 0 {
            return;
        }
        held.render_quarantined |= 1u64 << slot;
        let remaining = (G17PQueueLeaseState::render_mask() & !held.render_quarantined)
            .count_ones();
        drop(held);
        pr_warn!(
            "G17P render: retiring slot {} after an abandoned submission; {} render slot(s) left\n",
            slot,
            remaining,
        );
        self.lease_free.notify_all();
    }

    /// Make one quarantined slot eligible after both firmware CPUs stop.
    ///
    /// The completion still owns its lease while this bit is cleared, so no
    /// waiter can acquire the slot until that guard drops immediately after
    /// this call. Both processors have relinquished the abandoned group, so it
    /// can no longer justify a permanent capacity loss in the next session.
    fn rehabilitate_render_slot_after_processor_stop(&self, slot: u8) {
        debug_assert!(slot < g17_resources::G17PRenderQueuePair::SLOT_COUNT);
        let mut held = self.lease.lock();
        held.render_quarantined &= !(1u64 << slot);
    }
}

pub(crate) struct G17PComputeLeaseGuard {
    registry: Arc<G17PQueueRegistry>,
    released: bool,
}
impl G17PComputeLeaseGuard {
    /// Retire only the diagnostic admission share. A quarantined queue still
    /// retains its packet, mappings and roots until firmware ownership ends.
    pub(crate) fn release_retired_admission(&mut self) {
        if self.released { return; }
        self.registry.compute_readers.fetch_sub(1, Ordering::AcqRel);
        self.released = true;
    }

    /// Recovery isolation REPLAY: a spared never-started kick is published
    /// again under the guard whose share `release_retired_admission` returned
    /// at the zero-duration retire. Take the share back for that publication.
    pub(crate) fn rearm_admission(&mut self) {
        if !self.released { return; }
        self.registry.compute_readers.fetch_add(1, Ordering::AcqRel);
        self.released = false;
    }

    /// Fault-only host admission snapshot. The render fields are coherent;
    /// the compute count is an independent atomic observation. These are leases, not a
    /// hardware execution-slot count: admitted work may still be queued, and
    /// quarantined work retains its lease after userspace sees an error.
    pub(crate) fn admission_snapshot(&self) -> (u32, u64, u64) {
        let held = self.registry.lease.lock();
        (
            self.registry.compute_readers.load(Ordering::Acquire),
            held.render_slots,
            held.render_quarantined,
        )
    }

    pub(crate) fn quarantine(&self) {
        let held = self.registry.lease.lock();
        self.registry.compute_faulted.store(1, Ordering::Release);
        drop(held);
        self.registry.lease_free.notify_all();
    }

    /// Clear the device-wide admission poison only after both firmware
    /// processors stop. The runtime still owns this guard at that boundary,
    /// so the registry cannot lose the recovery transition when the active
    /// queue graph is dropped immediately afterward.
    pub(crate) fn rehabilitate_after_processor_stop(&self) {
        let held = self.registry.lease.lock();
        self.registry.compute_faulted.store(0, Ordering::Release);
        drop(held);
        self.registry.lease_free.notify_all();
    }
}


impl Drop for G17PComputeLeaseGuard {
    fn drop(&mut self) {
        if self.released { return; }
        self.registry.compute_readers.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Releases the compute lease and wakes the next waiting client.
struct G17PLeaseGuard {
    registry: Arc<G17PQueueRegistry>,
    slot: u8,
    /// `g17p_tile_backstop`: (owner, render sequence) recorded in flight.
    tile: Option<(u64, u64)>,
}

/// iso4 `g17_recovery_isolation::inherited_errno`: a consumer that fails
/// because its dependency failed is not charged with that dependency's
/// device loss (with INNOCENT set). Counted when the code changes.
pub(crate) fn inherited_dependency_error(error: Error) -> Error {
    let errno = error.to_errno();
    let inherited = crate::g17_recovery_isolation::inherited_errno(
        crate::g17_recovery_isolation::mask(), errno);
    if inherited == errno { return error; }
    crate::g17_recovery_isolation::count(&crate::g17_recovery_isolation::INHERITED_NOT_POISONED);
    Error::from_errno(inherited)
}

fn append_render_prefix_dependencies(
    frontier: &[(u64, g17_job::G17PCommandFences)], prefix: Option<u64>,
    dependencies: &mut KVec<Fence>, checked_dependencies: &mut KVec<Fence>,
) -> Result {
    for (sequence, fence) in frontier {
        if crate::g17_submission::g17p_render_prefix_contains(prefix, *sequence) {
            // Native tokens order GPU execution, but user timestamps are
            // copied by the host after retirement. Such producers must finish
            // that copy before a consumer can publish. Keep hardware status
            // separately so publication cannot hide a failed run_job.
            checked_dependencies.push(fence.completed.clone(), GFP_KERNEL)?;
            dependencies.push(fence.ready.clone(), GFP_KERNEL)?;
        }
    }
    Ok(())
}

/// Same-QID firmware parents cover ordinary execution, but not the host's
/// post-retirement timestamp copies. Add host waits only for timestamp
/// publishers inside this command's explicit logical compute prefix.
fn append_compute_timestamp_dependencies(
    frontier: &[(u64, g17_job::G17PCommandFences)], prefix: Option<u64>,
    dependencies: &mut KVec<Fence>, checked_dependencies: &mut KVec<Fence>,
) -> Result {
    if prefix.is_none() { return Ok(()); }
    for (sequence, fence) in frontier {
        if crate::g17_submission::g17p_render_prefix_contains(prefix, *sequence)
            && fence.ready.raw() == fence.completed.raw()
        {
            // ready aliases completed only for host-published timestamps.
            // Retain status as well as readiness: an error fence is signalled.
            checked_dependencies.push(fence.completed.clone(), GFP_KERNEL)?;
            dependencies.push(fence.ready.clone(), GFP_KERNEL)?;
        }
    }
    Ok(())
}

/// Callback-driven completion of every hardware command in one ioctl. The
/// initial member is held by the output-publication guard until enqueue ends.
#[pin_data]
pub(crate) struct G17PSubmitCompletion {
    fence: UserFence<G17PSubmissionFence>,
    /// v81 e2e diagnostic identity (the ioctl submission id).
    e2e_id: u64,
    /// All engines in this ioctl share one native command-buffer state.
    /// Successful aggregate retirement detaches this bookkeeping owner before
    /// signaling. Failed/replayable work keeps its original lifetime. Runtime
    /// preparation and firmware owners retain their independent Arcs.
    #[pin]
    work_state: Mutex<Option<Arc<g17_resources::G17PWorkStateLease>>>,
    remaining: AtomicU32,
    error: AtomicI32,
}

impl G17PSubmitCompletion {
    fn new(fence: UserFence<G17PSubmissionFence>, context: Arc<g17_resources::G17PLogicalQueueContext>, e2e_id: u64) -> Result<Arc<Self>> {
        let work_state = Arc::new(g17_resources::G17PWorkStateLease::deferred(context), GFP_KERNEL)?;
        Arc::pin_init(pin_init!(Self {
            fence, e2e_id,
            work_state <- new_mutex!(Some(work_state), "G17PSubmitCompletion::work_state"),
            remaining: AtomicU32::new(1),
            error: AtomicI32::new(0),
        }), GFP_KERNEL)
    }

    fn fence(&self) -> Fence { Fence::from_fence(&self.fence) }

    fn member_done(&self, result: Result) {
        if let Err(error) = result {
            let _ = self.error.compare_exchange(0, error.to_errno(), Ordering::AcqRel, Ordering::Acquire);
        }
        if self.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
            let error = self.error.load(Ordering::Acquire);
            if error != 0 {
                self.fence.set_error(Error::from_errno(error));
            } else {
                // Every command has retired successfully and enqueue is closed.
                // Scheduler free_job can lag hardware completion or wait behind
                // run_job on its ordered worker. Do not let those bookkeeping
                // references retain all native command-buffer state slots.
                // Error signals are deliberately excluded: quarantine/replay
                // may still require the exact original allocation.
                let retired = self.work_state.lock().take();
                drop(retired);
            }
            let e2e_before = crate::g17_e2e::stamp();
            self.fence.signal();
            if e2e_before != 0 {
                crate::g17_e2e::record(crate::g17_e2e::K_AGG_SIGNAL, error as u32, self.e2e_id,
                    [e2e_before, 0, 0, 0, 0]);
            }
        }
    }

    fn finish_enqueue(&self, result: Result) {
        // Preserve the actual ioctl failure over a dropped unsubmitted member's
        // ECANCELED status. Published members still own their completion pins.
        if let Err(error) = result { self.error.store(error.to_errno(), Ordering::Release); }
        self.member_done(result);
    }
}

/// One counted command, including construction/enqueue failure. Dropping an
/// uncompleted member contributes cancellation instead of stranding the fence.
pub(crate) struct G17PSubmitCompletionMember {
    aggregate: Arc<G17PSubmitCompletion>,
    completed: AtomicU32,
}

impl G17PSubmitCompletionMember {
    pub(crate) fn work_state(&self) -> Result<Arc<g17_resources::G17PWorkStateLease>> {
        self.aggregate.work_state.lock().as_ref().cloned().ok_or(ECANCELED)
    }

    fn new(aggregate: Arc<G17PSubmitCompletion>) -> Self {
        aggregate.remaining.fetch_add(1, Ordering::Relaxed);
        Self { aggregate, completed: AtomicU32::new(0) }
    }

    pub(crate) fn complete(&self, result: Result) {
        if self.completed.swap(1, Ordering::AcqRel) == 0 {
            self.aggregate.member_done(result);
        }
    }
}

impl Drop for G17PSubmitCompletionMember {
    fn drop(&mut self) { self.complete(Err(ECANCELED)); }
}

/// Exact hardware completion for one published render.  The lease is owned by
/// the completion packet, not the submit ioctl, so neither file close nor a
/// fast return can hand the physical slot to another client before retirement.
pub(crate) struct G17PRenderCompletion {
    fence: UserFence<G17PSubmissionFence>,
    /// Immutable host identity for fault-only correlation.  Keeping this on
    /// the already allocated completion avoids timing-distorting per-submit
    /// printk traffic while still joining a recovery to the exact UAPI work.
    submission_id: u64,
    vdm_base: u64,
    owner: u64,
    /// 0xff until the scheduler worker acquires the exact physical slot.
    render_slot: AtomicU32,
    render_order: crate::g17_submission::G17PRenderOrder,
    /// Immutable native QID/stamp, assigned before this completion is installed.
    fragment_dependency: AtomicU64,
    aggregate_member: G17PSubmitCompletionMember,
    /// Vertex start/end and fragment start/end UAPI destinations.
    timestamps: [Option<TimestampDestination>; 4],
    status: Arc<crate::g17_status::VmStatus>,
    lease: Arc<Mutex<Option<G17PLeaseGuard>>>,
    vm_job: Arc<Mutex<Option<mmu::T8140VmJobGuard>>>,
    created: Instant<Monotonic>,
    trace_created_ns: u64,
    trace_run_ns: AtomicU64,
    trace_attach_ns: AtomicU64,
    /// CNTPCT samples of the service pass that proved this render retired,
    /// stored under the runtime mutex for the completion record: last
    /// notification ingress on the primary and secondary RTKit roles, entry
    /// of the service pass, and the observation itself.
    trace_service_counters: [AtomicU64; 4],
    attached_ns: AtomicU64,
    published_ns: AtomicU64,
    signalled: AtomicU32,
    /// An already expired deadline awaiting return of this exact host-borrowed
    /// graph. This is not a renewed GPU timeout; Runtime resolves it on return.
    deferred_timeout_error: AtomicU32,
    /// `g17p_recovery_isolation`: class assigned while servicing a recovery
    /// (`g17_recovery_isolation::CLASS_*`). Only SPARED changes behaviour.
    recovery_class: core::sync::atomic::AtomicU8,
}

impl G17PRenderCompletion {
    /// Recovery isolation: record this render's class for the recovery being
    /// serviced. A later recovery may reclassify it.
    pub(crate) fn set_recovery_class(&self, class: u8) {
        self.recovery_class.store(class, Ordering::Release);
    }

    /// Cancelled by another client's recovery (or graced): its failure fails
    /// this fence only and never writes the VM's sticky status.
    pub(crate) fn recovery_spared(&self) -> bool {
        self.recovery_class.load(Ordering::Acquire) == crate::g17_recovery_isolation::CLASS_SPARED
    }

    pub(crate) fn submission_id(&self) -> u64 { self.submission_id }

    pub(crate) fn defer_timeout(&self, error: Error) {
        // The first classification request owns the error. Duplicate scheduler
        // callbacks/cancellation cannot replace it or extend its deadline.
        let _ = self.deferred_timeout_error.compare_exchange(
            0, error.to_errno() as u32, Ordering::AcqRel, Ordering::Acquire);
    }

    pub(crate) fn deferred_timeout(&self) -> Option<Error> {
        let error = self.deferred_timeout_error.load(Ordering::Acquire) as i32;
        (error != 0).then(|| Error::from_errno(error))
    }

    pub(crate) fn take_deferred_timeout(&self) -> Option<Error> {
        let error = self.deferred_timeout_error.swap(0, Ordering::AcqRel) as i32;
        (error != 0).then(|| Error::from_errno(error))
    }

    pub(crate) fn work_state(&self) -> Result<Arc<g17_resources::G17PWorkStateLease>> {
        self.aggregate_member.work_state()
    }

    fn new(
        fence: UserFence<G17PSubmissionFence>,
        timestamps: [Option<TimestampDestination>; 4],
        status: Arc<crate::g17_status::VmStatus>,
        vm_job: mmu::T8140VmJobGuard,
        submission_id: u64,
        vdm_base: u64,
        owner: u64,
        render_order: crate::g17_submission::G17PRenderOrder,
        aggregate_member: G17PSubmitCompletionMember,
    ) -> Result<Arc<Self>> {
        let lease = Arc::pin_init(
            new_mutex!(None, "G17PRenderCompletion::lease"),
            GFP_KERNEL,
        )?;
        let vm_job = Arc::pin_init(
            new_mutex!(Some(vm_job), "G17PRenderCompletion::vm_job"),
            GFP_KERNEL,
        )?;
        Ok(Arc::new(Self {
            fence,
            submission_id,
            vdm_base,
            owner,
            render_slot: AtomicU32::new(u8::MAX as u32),
            render_order,
            fragment_dependency: AtomicU64::new(u64::MAX),
            aggregate_member,
            timestamps,
            status,
            lease,
            vm_job,
            created: Instant::now(),
            trace_created_ns: if crate::g17_timing_buffer::enabled() { crate::g17_timing_buffer::now() } else { 0 },
            trace_run_ns: AtomicU64::new(0),
            trace_attach_ns: AtomicU64::new(0),
            trace_service_counters: [const { AtomicU64::new(0) }; 4],
            attached_ns: AtomicU64::new(0),
            published_ns: AtomicU64::new(0),
            signalled: AtomicU32::new(0),
            deferred_timeout_error: AtomicU32::new(0),
            recovery_class: core::sync::atomic::AtomicU8::new(0),
        }, GFP_KERNEL)?)
    }

    pub(crate) fn fence(&self) -> Fence { Fence::from_fence(&self.fence) }

    pub(crate) fn set_fragment_dependency(&self, word: u64) {
        self.fragment_dependency.store(word, Ordering::Release);
    }

    pub(crate) fn owner(&self) -> u64 { self.owner }

    pub(crate) fn compute_predecessor_dependency(
        &self, owner: u64, status: &Arc<crate::g17_status::VmStatus>,
        prefix: Option<u64>, last: Option<u64>,
    ) -> Result<Option<crate::g17_submission::G17PClBarrierDependency>> {
        if self.owner != owner || !Arc::ptr_eq(&self.status, status) {
            return Ok(None);
        }
        let sequence = self.render_order.sequence;
        if !crate::g17_submission::g17p_render_prefix_contains(prefix, sequence)
            && last != Some(sequence)
        {
            return Ok(None);
        }
        // SAFETY: this completion owns the queried hardware fence.
        let status = unsafe { bindings::dma_fence_get_status(self.fence().raw()) };
        let word = self.fragment_dependency.load(Ordering::Acquire);
        Self::counted_dependency(crate::g17_submission::g17p_live_render_dependency(
            crate::g17_submission::G17PRenderOrder { sequence: 0, wait_through: Some(sequence) },
            sequence, status, word,
        ), status, word)
    }

    /// Count a resolver outcome (`g17p_render_dep_stats`) and map its error.
    #[inline]
    fn counted_dependency(
        resolved: core::result::Result<Option<crate::g17_submission::G17PClBarrierDependency>,
            crate::g17_submission::SubmissionError>,
        status: i32, word: u64,
    ) -> Result<Option<crate::g17_submission::G17PClBarrierDependency>> {
        match resolved {
            Ok(Some(dependency)) => {
                crate::g17_dep_stats::note_resolved(word);
                Ok(Some(dependency))
            }
            Ok(None) => Ok(None),
            Err(error) => {
                crate::g17_dep_stats::note_error(error, status, word);
                Err(if error
                    == crate::g17_submission::SubmissionError::RenderBarrierPredecessorFailed
                {
                    // iso4: the predecessor failed; that is not this render's own
                    // device loss (see `inherited_dependency_error`).
                    inherited_dependency_error(EIO)
                } else { EIO })
            }
        }
    }

    /// Runtime publication and retirement share one mutex. Every preceding
    /// render has either retired or is in the pending table with its native
    /// token. Scheduler FIFO alone does not order different physical pairs.
    pub(crate) fn predecessor_dependency(
        &self, predecessor: &Self,
    ) -> Result<Option<crate::g17_submission::G17PClBarrierDependency>> {
        if self.owner != predecessor.owner || !Arc::ptr_eq(&self.status, &predecessor.status) {
            return Ok(None);
        }
        // No dependency means no fence-status lock, reference churn or GPU
        // token read. A predecessor outside the captured prefix is unrelated.
        if !crate::g17_submission::g17p_render_prefix_contains(
            self.render_order.wait_through, predecessor.render_order.sequence,
        ) { return Ok(None); }
        // SAFETY: predecessor owns its fence throughout this read.
        let status = unsafe { bindings::dma_fence_get_status(predecessor.fence().raw()) };
        let word = predecessor.fragment_dependency.load(Ordering::Acquire);
        Self::counted_dependency(crate::g17_submission::g17p_live_render_dependency(
            self.render_order, predecessor.render_order.sequence, status, word,
        ), status, word)
    }

    pub(crate) fn render_sequence(&self) -> u64 {
        self.render_order.sequence
    }

    /// Attach the physical lease only when the scheduler has resolved every
    /// dependency and is ready to publish. The VM job was retained at ioctl
    /// time, so queued work remains pinned without consuming hardware state.
    fn attach_lease(&self, lease: G17PLeaseGuard) -> Result<u8> {
        if self.signalled.load(Ordering::Acquire) != 0 { return Err(ECANCELED); }
        let slot = lease.slot();
        let mut held = self.lease.lock();
        if held.is_some() || self.signalled.load(Ordering::Acquire) != 0 {
            return Err(ECANCELED);
        }
        *held = Some(lease);
        self.render_slot.store(slot as u32, Ordering::Release);
        if self.trace_created_ns != 0 {
            self.trace_attach_ns.store(crate::g17_timing_buffer::now(), Ordering::Relaxed);
        }
        self.attached_ns.store(
            self.created.elapsed().as_nanos().max(0) as u64,
            Ordering::Release,
        );
        Ok(slot)
    }

    fn mark_published(&self) {
        self.published_ns.store(
            self.created.elapsed().as_nanos().max(0) as u64,
            Ordering::Release,
        );
    }

    /// Record where the retirement service pass stood, under the runtime
    /// mutex, before the fence is signalled. Only called while the timing
    /// buffer is armed; a render admitted before arming carries no record.
    pub(crate) fn note_service_trace(&self, ingress: [u64; 2], entry: u64, observed: u64) {
        if self.trace_created_ns == 0 {
            return;
        }
        let samples = [ingress[0], ingress[1], entry, observed];
        for (slot, value) in self.trace_service_counters.iter().zip(samples) {
            slot.store(value, Ordering::Relaxed);
        }
    }

    pub(crate) fn render_slot(&self) -> Option<u8> {
        let slot = self.render_slot.load(Ordering::Acquire);
        (slot != u8::MAX as u32).then_some(slot as u8)
    }

    pub(crate) fn diagnostic_identity(&self) -> (u64, u64, u8, u32) {
        (
            self.submission_id,
            self.vdm_base,
            self.render_slot.load(Ordering::Acquire) as u8,
            self.signalled.load(Ordering::Acquire),
        )
    }

    pub(crate) fn diagnostic_fragment_dependency(&self) -> u64 {
        self.fragment_dependency.load(Ordering::Acquire)
    }

    pub(crate) fn belongs_to_vm(
        &self,
        status: &Arc<crate::g17_status::VmStatus>,
    ) -> bool {
        Arc::ptr_eq(&self.status, status)
    }

    pub(crate) fn vm_status(&self) -> Arc<crate::g17_status::VmStatus> {
        self.status.clone()
    }

    /// Finish a packet which never became firmware-visible. No quarantine or
    /// processor-stop boundary is needed because the scheduler still owns the
    /// only publication right and the physical lease has already been dropped.
    ///
    /// An unpublished packet touched no firmware state, so its failure reason
    /// must obey the same VM-poison policy as `note_submission_failure`: only
    /// EIO/ETIMEDOUT/ENODEV mean the device is gone for this VM. A pre-
    /// publication EBUSY -- e.g. a second render pair whose cold, new-root
    /// graph could not be constructed over addresses this VM's other live pair
    /// still holds (`client_rebind_addresses_available` refuses the cached
    /// graph, then construction returns EBUSY) -- leaves a perfectly healthy
    /// VM. Recording it here poisoned the client's sticky status, so its very
    /// next submission refused at `stage=vm-status` and the session was then
    /// declared crashed for EVERY client (measured on Neo 2026-09-26 08:12:52:
    /// a Chromium launch killed the whole desktop's GPU this way). Use the
    /// device-loss filter the rest of the driver already uses; the fence still
    /// reports the exact error to this one client.
    pub(crate) fn fail_unpublished(&self, error: Error) -> bool {
        let error = mmu::context_refusal_to_uapi(error);
        if self.signalled.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return false;
        }
        self.status.record_if_device_loss(error.to_errno());
        self.fence.set_error(error);
        drop(self.lease.lock().take());
        drop(self.vm_job.lock().take());
        self.fence.signal();
        self.aggregate_member.complete(Err(error));
        true
    }

    /// Report failure without claiming that firmware stopped using this job.
    /// The runtime must retain this completion through queue retirement or
    /// processor stop. In particular, an error fence is not an unmap witness.
    pub(crate) fn fail_while_owned(&self, error: Error) -> bool {
        if self.signalled.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return false;
        }
        if let Some(vm_job) = self.vm_job.lock().as_ref() {
            vm_job.quarantine();
        }
        // Publish the registry quarantine before waking userspace or dropping
        // the runtime mutex. Recovery may stop the processors immediately
        // after this failure; recording the bit here ensures its post-stop
        // rehabilitation cannot race an older caller which marks it later.
        if let Some(lease) = self.lease.lock().as_ref() {
            lease.registry.quarantine_render_slot(lease.slot);
        }
        // Recovery isolation: another client's recovery cancelled this render.
        // Its guards and the pair quarantine above are retained exactly as
        // before; only the VM-wide sticky status is not written.
        if self.recovery_spared() {
            crate::g17_recovery_isolation::count(&crate::g17_recovery_isolation::POISON_SKIPPED);
        } else {
            self.status.record(error.to_errno());
        }
        self.fence.set_error(error);
        // Retain BOTH guards: dropping the lease here also lets a waiter
        // reacquire this physical slot before quarantine is recorded.
        self.fence.signal();
        self.aggregate_member.complete(Err(error));
        true
    }

    /// Signal at most once even when both RTKit roles report the same event.
    /// The caller must prove retirement, processor stop, or no publication.
    /// Unresolved hardware failures must use `fail_while_owned` instead.
    pub(crate) fn complete(
        &self,
        result: Result<g17_completion::PairedRenderCompletion>,
    ) {
        if self.signalled.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return;
        }
        let e2e_entry = crate::g17_e2e::stamp();
        if let Ok(completion) = result.as_ref() {
            // v84 feed-perf detector: atomics only; no-op unless enabled.
            crate::g17_host_path::note_render_pass(
                completion.execution.fragment_timestamps[0],
                completion.execution.fragment_timestamps[1]);
        }
        let gpu_durations = result.as_ref().ok().map(|completion| [
            completion.execution.tiling_timestamps[1]
                .wrapping_sub(completion.execution.tiling_timestamps[0]),
            completion.execution.fragment_timestamps[1]
                .wrapping_sub(completion.execution.fragment_timestamps[0]),
        ]);
        let completed_ns = self.created.elapsed().as_nanos().max(0) as u64;
        if self.trace_created_ns != 0 {
            let mut words = [0; crate::g17_timing_buffer::WORDS];
            let absolute = result.as_ref().ok().map_or([0; 4], |completion| [
                completion.execution.tiling_timestamps[0],
                completion.execution.tiling_timestamps[1],
                completion.execution.fragment_timestamps[0],
                completion.execution.fragment_timestamps[1],
            ]);
            // Words 10..19 (layout `RENDER_COMPLETION_LAYOUT`): absolute TA
            // start/end and 3D start/end GPU timestamps (10..13); the
            // architectural counter when this fence is signalled (14); the
            // layout version (15); then the service pass that proved
            // retirement: last notification ingress on the primary and
            // secondary RTKit roles, i.e. the IRQ (16, 17), entry of the
            // service pass that observed it (18) and the observation under
            // the runtime mutex (19). All on CNTPCT, the clock the compute
            // records and the publication record carry, so one capture
            // splits doorbell->start, execution, native end->ingress->
            // service->observed->signalled without assuming a host/GPU epoch.
            let service = &self.trace_service_counters;
            words[..20].copy_from_slice(&[crate::g17_timing_buffer::KIND_RENDER_COMPLETION,
                self.owner, self.submission_id, self.vdm_base,
                self.trace_created_ns, crate::g17_timing_buffer::now(),
                self.render_slot.load(Ordering::Relaxed) as u64,
                gpu_durations.unwrap_or([0; 2])[0], gpu_durations.unwrap_or([0; 2])[1],
                result.as_ref().map(|r| r.complete).unwrap_or(false) as u64,
                absolute[0], absolute[1], absolute[2], absolute[3],
                crate::g17_timing_buffer::arch_counter(),
                crate::g17_timing_buffer::RENDER_COMPLETION_LAYOUT,
                service[0].load(Ordering::Relaxed), service[1].load(Ordering::Relaxed),
                service[2].load(Ordering::Relaxed), service[3].load(Ordering::Relaxed)]);
            crate::g17_timing_buffer::record(words);
        }
        let result = result.and_then(|completion| {
            if !completion.complete {
                return Err(EIO);
            }
            // The tiler stamps the pass start and the fragment stage its end;
            // publish them before the fence lets userspace read the results.
            let execution = completion.execution;
            let observed = [
                execution.tiling_timestamps[0],
                execution.tiling_timestamps[1],
                execution.fragment_timestamps[0],
                execution.fragment_timestamps[1],
            ];
            for (destination, timestamp) in self.timestamps.iter().zip(observed) {
                if let Some(destination) = destination {
                    destination.write(timestamp)?;
                }
            }
            Ok(())
        });
        let trace_limit = module_parameters::g17p_trace_depth.value();
        if module_parameters::g17p_completion_trace.value() != 0
            && trace_limit != 0
            && G17P_RENDER_TIMING_RECORDS.fetch_add(1, Ordering::Relaxed) < trace_limit
        {
            let attached_ns = self.attached_ns.load(Ordering::Acquire);
            let published_ns = self.published_ns.load(Ordering::Acquire);
            let durations = gpu_durations.unwrap_or([0; 2]);
            pr_info!(
                "G17P render timing: owner={} submission={} vdm={:#x} slot={} attach={}ns publish={}ns complete={}ns dependency={}ns publication={}ns retirement={}ns gpu-ta={} gpu-3d={} ok={}\n",
                self.owner,
                self.submission_id,
                self.vdm_base,
                self.render_slot.load(Ordering::Acquire),
                attached_ns,
                published_ns,
                completed_ns,
                attached_ns,
                published_ns.saturating_sub(attached_ns),
                completed_ns.saturating_sub(published_ns),
                durations[0],
                durations[1],
                result.is_ok(),
            );
        }
        if let Err(error) = result {
            // Recovery isolation: a spared render's failure is reported on
            // its own fence only (firmware retirement is already proven here).
            if self.recovery_spared() {
                crate::g17_recovery_isolation::count(&crate::g17_recovery_isolation::POISON_SKIPPED);
            } else {
                self.status.record(error.to_errno());
            }
            self.fence.set_error(error);
        }
        let e2e_written = crate::g17_e2e::stamp();
        // Release the physical slot before waking userspace. The scheduler
        // intentionally retains this completion packet until its finished
        // fence retires; tying lease Drop to packet Drop lets the woken client
        // race ahead, take the other slot and collide with this VM's fixed
        // render-state aliases.
        drop(self.lease.lock().take());
        let e2e_lease = crate::g17_e2e::stamp();
        // A signalled output fence is userspace's permission to recycle BOs
        // and unbind transient VDM aliases. Retire the VM-job guard first;
        // leaving it in the scheduler-owned completion packet made those
        // post-fence unbinds return EBUSY until an unrelated worker freed the
        // packet, after which the already-recycled VA collided with the stale
        // mapping.
        drop(self.vm_job.lock().take());
        let e2e_vm = crate::g17_e2e::stamp();
        // v93 AGG_FIRST: wake userspace (the ioctl aggregate) before the
        // per-command hardware fence, whose inline callbacks are drm_sched
        // job-done bookkeeping nobody outside this driver waits on. Every
        // guarantee userspace relies on (timestamps, sticky status, slot and
        // VM-job release) is already established above. The submit path's
        // depth count treats a render as retired once its aggregate signals.
        let agg_first = crate::g17_host_path::enabled(crate::g17_host_path::AGG_FIRST);
        let e2e_hw = if agg_first {
            crate::g17_host_path::count(&crate::g17_host_path::AGG_FIRST_TAKEN);
            self.aggregate_member.complete(result);
            let e2e_agg = crate::g17_e2e::stamp();
            self.fence.signal();
            e2e_agg
        } else {
            self.fence.signal();
            let e2e_hw = crate::g17_e2e::stamp();
            self.aggregate_member.complete(result);
            e2e_hw
        };
        if e2e_entry != 0 {
            // a=1: AGG_FIRST order; word 4 is then the aggregate stamp.
            crate::g17_e2e::record(crate::g17_e2e::K_COMPLETE, agg_first as u32, self.submission_id,
                [e2e_entry, e2e_written, e2e_lease, e2e_vm, e2e_hw]);
        }
    }

    /// Release guards retained by `fail_while_owned` after both ASCs stop.
    ///
    /// The output fence may already be signalled, so `complete` deliberately
    /// returns early and cannot serve as this ownership boundary. Retire the VM
    /// job before restoring the physical slot, then clear its quarantine while
    /// the lease still excludes waiters; dropping the lease publishes both
    /// changes and wakes them.
    pub(crate) fn release_after_processor_stop(&self) {
        drop(self.vm_job.lock().take());
        let lease = self.lease.lock().take();
        if let Some(lease) = lease.as_ref() {
            lease
                .registry
                .rehabilitate_render_slot_after_processor_stop(lease.slot);
        }
        drop(lease);
    }

    /// v93r `g17p_recovery_path` bit0: the same release as
    /// `release_after_processor_stop`, at the other boundary that ends
    /// firmware ownership of this job: the runtime has proven, under the
    /// runtime mutex with the recovery closed, that the firmware drained the
    /// job's physical pair and has retired the pair's trackers
    /// (`reclaim_drained_quarantined_render_slots`). The VM stays poisoned.
    pub(crate) fn release_after_firmware_drain(&self) {
        self.release_after_processor_stop();
    }
}

impl G17PLeaseGuard {
    fn slot(&self) -> u8 { self.slot }
}

impl Drop for G17PLeaseGuard {
    fn drop(&mut self) {
        let mut held = self.registry.lease.lock();
        held.release_render_slot(self.slot);
        if let Some((owner, sequence)) = self.tile {
            self.registry.tile_forget(self.slot, owner, sequence);
        }
        drop(held);
        self.registry.lease_free.notify_all();
    }
}

const _: () = assert!(core::mem::needs_drop::<G17PRuntimeRef>());

/// DRM-visible T8140 manager. Hardware lifetime stays in the retained runtime.
pub(crate) struct G17PDrmGpu {
    runtime: G17PRuntimeRef,
    soc: &'static hw::agx3::SocConfig,
    observed: hw::GpuIdConfig,
    ids: gpu::SequenceIDs,
    registry: Arc<G17PQueueRegistry>,
}

impl G17PDrmGpu {
    pub(crate) fn new(
        runtime: G17PRuntimeRef,
        soc: &'static hw::agx3::SocConfig,
        observed: hw::GpuIdConfig,
    ) -> Result<Self> {
        let registry = G17PQueueRegistry::new()?;
        runtime.mutex().lock().attach_queue_registry(registry.clone())?;
        Ok(Self {
            runtime,
            soc,
            observed,
            ids: Default::default(),
            registry,
        })
    }
}

impl DrmGpu for G17PDrmGpu {
    fn init(&self) -> Result {
        Ok(())
    }

    fn ids(&self) -> &gpu::SequenceIDs {
        &self.ids
    }

    fn is_crashed(&self) -> bool {
        // `file.rs::get_params` turns this into ENODEV, and it runs before any
        // ioctl that could reach the submit path's own recovery -- so a crashed
        // session used to lock every LATER process out of the device until the
        // machine was rebooted. Try once (bounded inside) to rebuild instead,
        // then answer with the state that leaves.
        self.runtime.mutex().lock().recover_crashed_session_once()
    }

    fn supports_vm_status(&self) -> bool { true }
    /// `submission_error` is non-zero only for device-wide compute poison,
    /// which QUARANTINE_SCOPE bit 0 (compile time) disables.
    fn supports_vm_status_mirror(&self) -> bool {
        !self.registry.compute_poison_is_device_wide()
    }
    fn supports_per_command_input_syncs(&self) -> bool { true }
    fn supports_exact_prior_barriers(&self) -> bool { true }
    fn independent_queue_limits(&self) -> Option<[u8; 16]> {
        Some(crate::g17_queue_limits::encode_limits())
    }

    fn submission_error(&self) -> i32 {
        if self.registry.compute_faulted.load(Ordering::Acquire) != 0
            && self.registry.compute_poison_is_device_wide()
        {
            EIO.to_errno()
        } else {
            0
        }
    }

    fn update_globals(&self) {}

    fn params(&self) -> Result<DrmGpuParams> {
        let mut core_masks = [0u32; uapi::DRM_ASAHI_MAX_CLUSTERS as usize];
        for (index, mask) in self.observed.core_masks.iter().enumerate() {
            *core_masks.get_mut(index).ok_or(EIO)? = *mask;
        }
        Ok(DrmGpuParams {
            gpu_generation: self.observed.gpu_gen as u32,
            gpu_variant: self.observed.gpu_variant as u32,
            gpu_revision: self.observed.gpu_rev as u32,
            chip_id: self.soc.chip_id,
            num_dies: self.observed.num_dies,
            num_clusters_total: self.observed.num_clusters,
            num_cores_per_cluster: self.observed.num_cores,
            core_masks,
            max_frequency_khz: g17_adt_j700::J700_PERF_STATES
                [g17_adt_j700::J700_GPU_NUM_PERF_STATES as usize]
                .freq_hz
                / 1_000,
            usc_generation: self.observed.usc_generation,
            gpu_hal_generation: self.observed.gpu_hal_generation as u32,
            max_commands_per_submission: file::MAX_COMMANDS_PER_SUBMISSION,
        })
    }

    fn user_range(&self) -> Result<Range<u64>> {
        Ok(T8140_USER_START..T8140_UNKNOWN_PAGE)
    }

    fn unknown_page(&self) -> Result<u64> {
        Ok(T8140_UNKNOWN_PAGE)
    }

    fn base_clock_hz(&self) -> u32 {
        // `DRM_IOCTL_ASAHI_GET_TIME` scales CNTPCT_EL0 by this rate. T8140
        // runs the architectural counter at its own CNTFRQ_EL0, not at the
        // GPU's 24 MHz command timestamp rate.
        let frequency: u64;
        // SAFETY: CNTFRQ_EL0 is a read-only system register.
        unsafe {
            core::arch::asm!(
                "mrs {x}, CNTFRQ_EL0",
                x = out(reg) frequency
            );
        }
        frequency as u32
    }

    fn new_vm(&self, kernel_range: Range<u64>) -> Result<mmu::Vm> {
        self.runtime
            .mutex()
            .lock()
            .new_user_vm(self.ids.vm.next(), kernel_range)
    }

    fn new_queue(
        &self,
        vm: mmu::Vm,
        _ualloc: Arc<Mutex<alloc::DefaultAllocator>>,
        _ualloc_priv: Arc<Mutex<alloc::DefaultAllocator>>,
        priority: u32,
        usc_exec_base: u64,
    ) -> Result<KBox<dyn queue::Queue>> {
        let dev = self.runtime.mutex().lock().drm_ref()?;
        // Reserved before the queue object exists so the failure is a clean
        // ENOSPC from QUEUE_CREATE. `G17PQueue::drop` gives the slot back.
        self.registry.reserve_queue()?;
        let queue = G17PQueue::new(
            &dev,
            self.runtime.clone(),
            self.registry.clone(),
            self.observed.gpc_perf_state_map,
            self.observed.gpc_perf_state_map_low,
            self.observed.gpc_perf_state_control,
            vm,
            self.ids.queue.next(),
            priority,
            usc_exec_base,
        )
        .inspect_err(|_| self.registry.release_queue())?;
        match KBox::new(queue, GFP_KERNEL) {
            Ok(boxed) => Ok(boxed),
            Err(error) => Err(error.into()),
        }
    }

    fn map_timestamp_buffer(
        &self,
        bo: gem::ObjectRef,
        range: Range<usize>,
    ) -> Result<mmu::KernelMapping> {
        self.runtime
            .mutex()
            .lock()
            .map_timestamp_buffer(bo, range)
    }
}

struct UserAddressSpace<'a> {
    vm: &'a mmu::Vm,
    range: Range<u64>,
}

impl g17_uapi::GpuAddressSpace for UserAddressSpace<'_> {
    fn covers(&self, address: u64, size: u64, access: g17_uapi::GpuAccess) -> bool {
        let Some(end) = address.checked_add(size) else {
            return false;
        };
        let (need_read, need_write) = match access {
            g17_uapi::GpuAccess::Read => (true, false),
            g17_uapi::GpuAccess::Write => (false, true),
            g17_uapi::GpuAccess::ReadWrite => (true, true),
        };
        size != 0
            && self.range.start <= address
            && end <= self.range.end
            && self.vm.covers_range(address, size, need_read, need_write)
    }
}

/// v91 VALIDATE_PROOF: request tuples of one proof. A render validates about
/// ten ranges (VDM, scissor, depth bias, occlusion, two ZLS, samplers, four
/// USC programs) plus its attachments (at most 2 x 16).
const G17P_PROOF_RANGES: usize = 48;

fn g17p_access_code(access: g17_uapi::GpuAccess) -> u8 {
    match access {
        g17_uapi::GpuAccess::Read => 1,
        g17_uapi::GpuAccess::Write => 2,
        g17_uapi::GpuAccess::ReadWrite => 3,
    }
}

/// Address-coverage requests of one translation, in first-request order and
/// without duplicates. `overflow` means the set is incomplete.
#[derive(Copy, Clone)]
pub(crate) struct G17PRangeRequests {
    ranges: [(u64, u64, u8); G17P_PROOF_RANGES],
    count: usize,
    overflow: bool,
}

impl G17PRangeRequests {
    pub(crate) const fn new() -> Self {
        Self { ranges: [(0, 0, 0); G17P_PROOF_RANGES], count: 0, overflow: false }
    }

    pub(crate) fn as_slice(&self) -> &[(u64, u64, u8)] { &self.ranges[..self.count] }

    fn push(&mut self, request: (u64, u64, u8)) {
        if self.as_slice().contains(&request) { return; }
        if self.count == G17P_PROOF_RANGES { self.overflow = true; return; }
        self.ranges[self.count] = request;
        self.count += 1;
    }
}

/// VALIDATE_PROOF: request tuples proven covered by this queue's VM at
/// one mapping-validation epoch. The epoch advances on every attempted leaf
/// or permission mutation (under the GPUVM exec lock) and reads `None` while
/// one is in progress, the same rule the retained-graph reachability cache
/// uses, so an unchanged epoch means the proven page-table state is intact.
pub(crate) struct G17PValidationProof {
    epoch: Option<u64>,
    requests: G17PRangeRequests,
}

impl G17PValidationProof {
    pub(crate) const fn new() -> Self { Self { epoch: None, requests: G17PRangeRequests::new() } }

    /// True when every request was proven at `current`, which must be the
    /// epoch the proof was recorded at.
    pub(crate) fn covers(&self, current: Option<u64>, requests: &[(u64, u64, u8)]) -> bool {
        self.epoch.is_some() && current == self.epoch
            && requests.iter().all(|request| self.requests.as_slice().contains(request))
    }

    pub(crate) fn record(&mut self, epoch: Option<u64>, requests: &G17PRangeRequests) {
        // Keep proofs for alternating in-flight buffers. Replacing the set on
        // every miss makes a depth-two stream evict its own previous buffer.
        // Only union exact tuples proved under the SAME stable mapping epoch;
        // any mutation, unavailable epoch, or incomplete set discards reuse.
        if epoch.is_some() && self.epoch == epoch && !requests.overflow {
            for &request in requests.as_slice() {
                self.requests.push(request);
            }
            if !self.requests.overflow { return; }
        }
        // Bounded storage: on union overflow retain the latest complete set.
        // Never leave an incomplete union marked reusable.
        self.epoch = if requests.overflow { None } else { epoch };
        self.requests = *requests;
    }

    pub(crate) fn invalidate(&mut self) { self.epoch = None; }
}

/// Translation address space for v91 VALIDATE_PROOF. `Record` applies the
/// static half of `UserAddressSpace::covers` exactly (size, overflow, user
/// range) and records the page-table half for one later proof, answering it
/// optimistically; `Checked` is the unchanged per-range locked test.
enum G17PProofSpace<'a> {
    Record { range: Range<u64>, requests: core::cell::RefCell<G17PRangeRequests> },
    Checked(UserAddressSpace<'a>),
}

impl g17_uapi::GpuAddressSpace for G17PProofSpace<'_> {
    fn covers(&self, address: u64, size: u64, access: g17_uapi::GpuAccess) -> bool {
        match self {
            Self::Checked(space) => space.covers(address, size, access),
            Self::Record { range, requests } => {
                let Some(end) = address.checked_add(size) else {
                    return false;
                };
                if !(size != 0 && range.start <= address && end <= range.end) {
                    return false;
                }
                requests.borrow_mut().push((address, size, g17p_access_code(access)));
                true
            }
        }
    }
}

/// Outcome of `g17p_translate_with_proof` for the fine laps and counters.
#[derive(Copy, Clone, Default)]
pub(crate) struct G17PProofOutcome {
    pub(crate) requests: u8,
    pub(crate) hit: bool,
    pub(crate) batched: bool,
    pub(crate) fallback: bool,
}

/// v91 VALIDATE_PROOF translation. With the knob clear this is exactly the
/// v90 call. Otherwise the pure translation runs once against the recording
/// space; its requests are then proven either by the queue's previous proof
/// at the VM's current epoch (no lock) or by one batched exec-lock walk. A
/// translation error, an incomplete request set or a failed proof reruns
/// the unchanged per-range translation and returns its result, so every
/// error (and every outcome of a concurrent mapping change) is one v90 could
/// produce.
///
/// Never inlined: its request sets (about 1.2 KB each) must live in this
/// frame, which is popped before the submit ioctl's early publication
/// descends into the deep publish chain on the same kernel stack.
#[inline(never)]
fn g17p_translate_with_proof<T>(
    vm: &mmu::Vm,
    range: Range<u64>,
    proof: &mut G17PValidationProof,
    outcome: &mut G17PProofOutcome,
    translate: impl Fn(&G17PProofSpace<'_>) -> core::result::Result<T, g17_uapi::UapiTranslateError>,
) -> core::result::Result<T, g17_uapi::UapiTranslateError> {
    let checked = |range: Range<u64>| G17PProofSpace::Checked(UserAddressSpace { vm, range });
    if !crate::g17_host_path::enabled(crate::g17_host_path::VALIDATE_PROOF) {
        return translate(&checked(range));
    }
    let space = G17PProofSpace::Record {
        range: range.clone(), requests: core::cell::RefCell::new(G17PRangeRequests::new()),
    };
    let optimistic = translate(&space);
    let G17PProofSpace::Record { requests, .. } = space else { unreachable!() };
    let requests = requests.into_inner();
    outcome.requests = requests.count as u8;
    if optimistic.is_err() || requests.overflow {
        outcome.fallback = true;
        crate::g17_host_path::count(&crate::g17_host_path::PROOF_FALLBACKS);
        return translate(&checked(range));
    }
    if proof.covers(vm.mapping_validation_epoch(), requests.as_slice()) {
        outcome.hit = true;
        crate::g17_host_path::count(&crate::g17_host_path::PROOF_HITS);
        return optimistic;
    }
    outcome.batched = true;
    crate::g17_host_path::count(&crate::g17_host_path::PROOF_BATCHES);
    let (covered, epoch) = vm.covers_ranges_batch(requests.as_slice());
    if covered {
        proof.record(epoch, &requests);
        return optimistic;
    }
    proof.invalidate();
    outcome.fallback = true;
    crate::g17_host_path::count(&crate::g17_host_path::PROOF_FALLBACKS);
    translate(&checked(range))
}

#[derive(Default)]
pub(crate) struct G17PSubmissionFence;

#[vtable]
impl FenceOps for G17PSubmissionFence {
    fn get_driver_name<'a>(self: &'a FenceObject<Self>) -> &'a CStr {
        c_str!("asahi")
    }

    fn get_timeline_name<'a>(self: &'a FenceObject<Self>) -> &'a CStr {
        c_str!("g17p-submit")
    }
}

static QUEUE_NAME: &CStr = c_str!("g17p_queue");
static QUEUE_CLASS_KEY: core::pin::Pin<&'static kernel::sync::LockClassKey> = static_lock_class!();

/// Hardware commands and ioctl aggregates can finish out of order. Sharing
/// a dma_fence context would let fence merging discard a still-live command
/// in favor of a later completed one. Give each independently signalled fence
/// a context of its own without imposing GPU execution order.
fn new_independent_submission_fence(
    contexts: &FenceContexts,
) -> Result<UserFence<G17PSubmissionFence>> {
    let fence = contexts.new_fence(0, G17PSubmissionFence)?;
    // SAFETY: `fence` is still a UniqueFence with no published references,
    // callbacks or waiters. The allocator returns a globally unique context.
    // The Rust wrapper initializes a fixed-context fence, so replace that
    // identity before converting it to the shareable UserFence.
    unsafe { (*fence.raw()).context = bindings::dma_fence_context_alloc(1); }
    Ok(fence.into())
}

/// One userspace queue bound to the retained G17P runtime.
///
/// One of these exists per DRM `queue_create`, i.e. per client, and its `id`
/// is what the runtime records as the owner of the hardware compute graph.
/// Dropping it -- `queue_destroy`, or the file close the kernel performs for a
/// client that died mid-submission -- releases that ownership.
struct G17PQueue {
    dev: AsahiDevRef,
    runtime: G17PRuntimeRef,
    registry: Arc<G17PQueueRegistry>,
    /// Cached during the safe SGX GPU-ID probe; never sampled at submission.
    gpc_perf_state_map: u32,
    gpc_perf_state_map_low: u32,
    gpc_perf_state_control: u32,
    vm: mmu::Vm,
    user_range: Range<u64>,
    usc_exec_base: u64,
    fence_contexts: FenceContexts,
    /// Per-logical-queue VM context. Render needs the context immediately,
    /// but the hardware compute graph is deliberately allocated only if this
    /// logical queue actually submits compute.
    context: Arc<crate::g17_resources::G17PLogicalQueueContext>,
    async_backend: Option<Arc<G17PAsyncQueueBackend>>,
    async_scheduler: Option<g17_job::G17PJobScheduler<G17PAsyncQueueBackend>>,
    render_scheduler: Option<g17_job::G17PRenderScheduler<G17PRenderQueueBackend>>,
    /// Monotonic logical render index; independent of physical pair rotation.
    render_sequence: u64,
    compute_sequence: u64,
    /// Only accepted renders which have not yet signalled. Queued work counts
    /// until its real completion; physical-pair admission remains unchanged.
    render_frontier: KVec<(u64, g17_job::G17PCommandFences)>,
    compute_frontier: KVec<(u64, g17_job::G17PCommandFences)>,
    /// v91 VALIDATE_PROOF: this queue's last address-validation proof.
    validation_proof: G17PValidationProof,
    /// v91 fine laps (e2e option bit4) of the submission in progress; the
    /// queue mutex serialises its submissions.
    e2e_fine: [u64; 12],
    context_unbound_refusal: Option<u64>,
    /// v93 FW_DEPS/AGG_FIRST: this queue's most recent fully enqueued ioctls
    /// (aggregate reference, per-command hardware fences, logical command
    /// ranges), ring order. Slots and their vectors are reused.
    recent: [G17PRecentSubmission; crate::g17_host_path::FW_DEP_RECENT],
    recent_next: usize,
    /// v94 `g17p_mem_path` bit2: this queue's retired compute timestamp pages.
    ts_pool: Arc<g17_job::G17PTimestampPool>,
    /// v93: hardware fences of the ioctl being enqueued (swapped into its
    /// ring slot when the whole ioctl is queued); `None` after a failed push.
    ioctl_commands: Option<KVec<Fence>>,
    id: u64,
}

/// v93: one fully enqueued ioctl of this queue. Holding the aggregate
/// reference keeps its address unique while it is compared against inputs.
struct G17PRecentSubmission {
    aggregate: Option<Fence>,
    /// Every command's hardware fence: an unpublished failure signals it with
    /// an error and may leave the VM clean, so a converted dependent must
    /// status-check each one at publication, not only the aggregate.
    commands: KVec<Fence>,
    /// Inclusive logical ranges of its commands; (0, 0) when it had none.
    render: (u64, u64),
    compute: (u64, u64),
}

impl G17PRecentSubmission {
    const EMPTY: Self = Self { aggregate: None, commands: KVec::new(), render: (0, 0), compute: (0, 0) };
}

/// v93 FW_DEPS: the ioctl's input syncs split into unchanged host waits and
/// firmware-expressed same-queue prefixes. Applied to every command.
struct G17PSyncPlan {
    /// Kept exactly as before: DRM scheduler dependency plus checked input.
    host: KVec<Fence>,
    /// Matched same-queue aggregates: status-checked at publication, never
    /// waited (their commands are ordered by the raised prefixes).
    firmware: KVec<Fence>,
    render_through: u64,
    compute_through: u64,
}

impl Drop for G17PQueue {
    fn drop(&mut self) {
        // Entity teardown handles pending scheduler jobs before the frontend
        // reference goes away. Active hardware packets are retained by runtime.
        //
        // Do not leave async ownership release to G17PAsyncQueueBackend::drop:
        // DRM scheduler teardown may retain that Arc past the file/queue close
        // boundary.  An idle queue would then pin its QID and execution context
        // even though no DRM file still owns the render node, forcing the next
        // client onto a needlessly fresh hardware queue.  Release explicitly
        // after the entity is torn down; the backend's later Drop is idempotent.
        drop(self.render_scheduler.take());
        drop(self.async_scheduler.take());
        // Do not publish the physical pairs as reusable before the manager has
        // retired this queue's descriptor-context ownership.  A fresh client
        // can acquire a released registry slot immediately; exposing it first
        // lets that client enter VM handoff while the retained QIDs still name
        // this context.  Completion already establishes runtime -> registry
        // as the nested lock order when it drops the physical lease, so retire
        // under the runtime mutex first and wake registry waiters only after.
        self.runtime
            .mutex()
            .lock()
            .release_render_owner(self.id, &self.context);
        self.registry.release_render_owner(self.id);
        self.runtime
            .mutex()
            .lock()
            .release_async_compute_queue(self.id);
        drop(self.async_backend.take());
        self.runtime.release_compute_owner(self.id);
        self.registry.release_queue();
        // The teardown armed above publishes its firmware release and frees
        // the queue's graphs from the event worker; wake it now rather than
        // waiting for the next firmware event.
        self.runtime.mutex().lock().kick_logical_queue_teardown();
    }
}

pub(crate) struct G17PRenderQueueBackend {
    runtime: G17PRuntimeRef,
    registry: Arc<G17PQueueRegistry>,
    owner: u64,
    vm: mmu::Vm,
    context: Arc<crate::g17_resources::G17PLogicalQueueContext>,
    /// v84: packets pushed to the DRM scheduler whose run_job publication has
    /// not finished. Early publication requires zero, so it can never
    /// overtake (or run concurrently with) an earlier scheduler publication
    /// of this queue; submits of one queue are serialised by its file mutex.
    sched_backlog: AtomicU32,
}

/// Scheduler teardown is not evidence that a published GPU job failed.
/// Keep its owner alive and let the RTKit worker retire the hardware fence.
/// This runs only at queue destruction, with no runtime mutex held, and is
/// uninterruptible because the exiting task already has a fatal signal.
/// Use the same finite bound as the render scheduler's watchdog; a genuinely
/// stuck job still reaches the existing final-observation/quarantine path.
fn wait_for_render_teardown(fence: &Fence) -> bool {
    // SAFETY: The retained Fence reference lives across the entire wait.
    unsafe {
        bindings::dma_fence_wait_timeout(
            fence.raw(),
            false,
            kernel::time::msecs_to_jiffies(2000) as _,
        ) > 0
    }
}

impl g17_job::G17PRenderQueueBackend for G17PRenderQueueBackend {
    fn defer_timeout_if_borrowed(&self, packet: &g17_job::G17PRenderJobPacket) -> bool {
        if !packet.is_published() { return false; }
        let Some(slot) = packet.completion().render_slot() else { return false; };
        self.runtime.mutex().lock().defer_render_timeout_if_borrowed(
            slot, packet.completion(), ETIMEDOUT)
    }

    fn timeout_progress(&self) -> Option<(u64, u32)> {
        self.runtime.mutex().lock().timeout_progress_snapshot()
    }

    fn prepare(&self, _packet: &g17_job::G17PRenderJobPacket) -> Option<Fence> {
        None
    }

    fn publish(&self, packet: Arc<g17_job::G17PRenderJobPacket>) -> Result<Fence> {
        // v84: the submit ioctl already owned this packet's publication. The
        // scheduler only takes over timeout/teardown ownership of its fence.
        if let Some(outcome) = packet.early_outcome() {
            crate::g17_e2e::record(crate::g17_e2e::K_RUN_JOB, 2, packet.submission_id(),
                [0, 0, 0, outcome.is_ok() as u64, 0]);
            return outcome.map(|()| packet.fence()).map_err(mmu::context_refusal_to_uapi);
        }
        let result = self.publish_with(packet.clone(), None);
        self.release_backlog(&packet);
        result.map_err(mmu::context_refusal_to_uapi)
    }

    fn try_publish_early(&self, packet: &Arc<g17_job::G17PRenderJobPacket>) -> bool {
        // Never wait in the submit ioctl for a physical lease or behind an
        // earlier scheduler publication of this queue: decline instead.
        if self.sched_backlog.load(Ordering::Acquire) != 0 { return false; }
        // v93 EARLY_DEPS: signalled inputs (consumed without sleeping by
        // publish_with) and firmware status checks may publish early too.
        let with_deps = !packet.input_fences().is_empty() || !packet.firmware_fences().is_empty();
        if with_deps && (!crate::g17_host_path::enabled(crate::g17_host_path::EARLY_DEPS)
            || packet.input_fences().iter().any(|fence|
                // SAFETY: the packet owns each input fence for this read.
                unsafe { bindings::dma_fence_get_status(fence.raw()) } == 0))
        {
            return false;
        }
        let lease = match self.registry.try_acquire_lease(self.owner, G17PTileRequest::of(packet)) {
            Ok(Some(lease)) => lease,
            _ => return false,
        };
        let result = self.publish_with(packet.clone(), Some(lease));
        packet.set_early_outcome(result.map(|_| ()));
        crate::g17_host_path::count(&crate::g17_host_path::EARLY_TAKEN);
        if with_deps { crate::g17_host_path::count(&crate::g17_host_path::EARLY_WITH_DEPS); }
        true
    }

    fn count_backlog(&self, packet: &g17_job::G17PRenderJobPacket) {
        packet.mark_backlog_counted();
        self.sched_backlog.fetch_add(1, Ordering::AcqRel);
    }

    fn timed_out(
        &self,
        packet: Arc<g17_job::G17PRenderJobPacket>,
    ) -> kernel::drm::sched::Status {
        self.release_backlog(&packet);
        self.timed_out_inner(packet)
    }

    fn cancel(&self, packet: Arc<g17_job::G17PRenderJobPacket>) {
        self.release_backlog(&packet);
        self.cancel_inner(packet)
    }
}

impl G17PRenderQueueBackend {
    fn release_backlog(&self, packet: &g17_job::G17PRenderJobPacket) {
        if packet.take_backlog_counted() {
            self.sched_backlog.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Publish one packet. `early_lease` is Some only for the v84 submit-ioctl
    /// path, which acquired it without waiting; the scheduler path acquires
    /// (and may wait for) the lease here exactly as before.
    fn publish_with(&self, packet: Arc<g17_job::G17PRenderJobPacket>,
        early_lease: Option<G17PLeaseGuard>) -> Result<Fence> {
        let early = early_lease.is_some();
        let mut early_lease = early_lease;
        // v81 e2e diagnostic (default off): scheduler run_job entry.
        let e2e_run = crate::g17_e2e::stamp();
        let e2e = e2e_run != 0;
        let completion = packet.completion();
        let run_cntvct = if completion.trace_created_ns != 0 {
            completion.trace_run_ns.store(crate::g17_timing_buffer::now(), Ordering::Relaxed);
            crate::g17_timing_buffer::arch_counter()
        } else { 0 };
        let submission_id = packet.submission_id();
        if e2e {
            let signalled = packet.input_fences().iter().filter(|fence| {
                // SAFETY: the packet owns each input fence for this read.
                unsafe { bindings::dma_fence_get_status(fence.raw()) != 0 }
            }).count() as u64;
            crate::g17_e2e::record_at(e2e_run, crate::g17_e2e::K_RUN_JOB, early as u32, submission_id,
                [packet.input_fences().len() as u64, packet.firmware_fences().len() as u64,
                 signalled, 0, 0]);
        }
        if !packet.begin_publication() {
            completion.fail_unpublished(ECANCELED);
            return Err(ECANCELED);
        }
        // g17p_stack_watch (diagnostic, default off): stack high-water mark
        // of the whole publication, including every callee.
        let stack_watch = crate::g17_stackwatch::begin();
        let mut failure_stage = "render-vm-status";
        let result = (|| -> Result<Fence> {
            if completion.vm_status().get() != 0 { return Err(EIO); }
            failure_stage = "render-input-fence";
            for fence in packet.input_fences() {
                G17PQueue::wait_fence(fence).map_err(inherited_dependency_error)?;
            }
            for fence in packet.firmware_fences() {
                G17PQueue::check_firmware_fence(fence).map_err(inherited_dependency_error)?;
            }
            if packet.cancel_requested() { return Err(ECANCELED); }
            if e2e { crate::g17_e2e::record(crate::g17_e2e::K_DEPS_DONE, 0, submission_id, [0; 5]); }

            // Dependencies are retired before this potentially sleeping slot
            // acquisition. A stable owner normally reacquires its idle slot
            // immediately; only first-use namespace contention waits here.
            failure_stage = "render-acquire-physical-lease";
            let lease = match early_lease.take() {
                Some(lease) => lease,
                None => self.registry.acquire_lease(self.owner, G17PTileRequest::of(&packet))?,
            };
            if packet.cancel_requested() { return Err(ECANCELED); }
            failure_stage = "render-attach-physical-lease";
            let render_slot = completion.attach_lease(lease)?;
            if e2e {
                crate::g17_e2e::record(crate::g17_e2e::K_LEASE, u32::from(render_slot), submission_id,
                    [u64::from(render_slot), 0, 0, 0, 0]);
            }
            if crate::g17_timing_buffer::diag_knob(
                crate::g17_timing_buffer::DIAG_EARLY_RENDER_DOORBELL
                    | crate::g17_timing_buffer::DIAG_IDLE_RENDER_DOORBELL)
            {
                failure_stage = "render-early-work-doorbell";
                self.runtime.mutex().lock().wake_pending_render_submission()?;
            }
            if crate::g17_timing_buffer::diag_knob(
                crate::g17_timing_buffer::DIAG_EARLY_RENDER_KICK)
            {
                // Input/firmware dependencies and physical-queue ownership
                // have been checked. Only wake for actual publishable work;
                // do not speculate at completion or while awaiting a fence.
                failure_stage = "render-early-kick";
                self.runtime.mutex().lock().kick_pending_render_submission()?;
            }
            let trace_limit = module_parameters::g17p_trace_depth.value();
            let mut stages = crate::g17_perf::RenderStageTimes::default();
            stages.enabled = e2e || completion.trace_created_ns != 0 ||
                (module_parameters::g17p_completion_trace.value() != 0
                && trace_limit != 0
                && G17P_RENDER_POLL_RECORDS.load(Ordering::Relaxed) < trace_limit);
            // Register states belong to this execution context and are shared
            // with its compute commands. Another client's states cannot hold
            // this publication back. Nothing firmware-ordered
            // is waited for here: the retained FList release (opcode 0x2e) is
            // fire-and-forget as in AGXG17P's releaseResources, and
            // control-ring backpressure is handled where a control record is
            // actually published.
            let mut state_polls = 0usize;
            // v92c FList takeover guard polls (200 us each; 1 s bound).
            let mut takeover_polls = 0usize;
            // v81 e2e: work state, lock request/acquire, prepare begin/end,
            // relock (CNTPCT). Last loop iteration wins.
            let mut e2e_t = [0u64; 6];
            let submit = (|| {
                let mut state_poll = crate::g17_perf::PollSchedule::new(0);
                loop {
                    if packet.cancel_requested() { return Err(ECANCELED); }
                    if completion.vm_status().get() != 0 { return Err(EIO); }
                    // Allocation belongs to the logical context: its atomic
                    // work-state pool and TSQ mutex provide synchronization.
                    // Do not exclude independent queues or their retirement
                    // worker while initializing a node or awaiting capacity.
                    failure_stage = "render-command-buffer-states";
                    // The submit ioctl must not turn a signal during recovery
                    // into a failed render; the worker path cannot see signals.
                    let preparation = if early {
                        self.runtime.preparations.enter_wait_uninterruptible()?
                    } else {
                        self.runtime.preparations.enter_wait()?
                    };
                    match completion.work_state().and_then(|state| state.acquire()) {
                        Ok(_) => {}
                        Err(error) if error == EBUSY => {
                            drop(preparation);
                            if state_polls >= 1000 { return Err(ETIMEDOUT); }
                            state_polls += 1;
                            fsleep(Delta::from_micros(i64::from(state_poll.next_delay_us())));
                            continue;
                        }
                        Err(error) => return Err(error),
                    }
                    if e2e { e2e_t[0] = crate::g17_e2e::now(); }
                    let lock_started = stages.enabled.then(Instant::<Monotonic>::now);
                    if e2e { e2e_t[1] = crate::g17_e2e::now(); }
                    let mut runtime = self.runtime.mutex().lock();
                    // v87 SETTLE_ONCE: true once the open-recovery gate passed
                    // during the current Runtime hold; cleared on every relock.
                    let mut settled = false;
                    if e2e { e2e_t[2] = crate::g17_e2e::now(); }
                    if let Some(started) = lock_started {
                        stages.runtime_lock_ns = stages.runtime_lock_ns.saturating_add(
                            started.elapsed().as_nanos().max(0) as u64);
                    }
                    if !preparation.is_current() {
                        // No descriptor was installed or exposed. A recovery
                        // raced this context-local allocation; retry admission
                        // after dropping runtime rather than failing this job.
                        drop(runtime);
                        continue;
                    }
                    // Cancellation, VM loss or a context replacement may
                    // have occurred while acquiring the runtime for commit.
                    if packet.cancel_requested() { return Err(ECANCELED); }
                    if completion.vm_status().get() != 0 { return Err(EIO); }
                    if !self.context.is_current() { return Err(EFAULT); }
                    // Build the selected graph and an eligible idle sibling
                    // before publication. Only ownership capture and shared
                    // table/route commit hold Runtime; page allocation and
                    // private mappings do not exclude independent clients.
                    // v92c: never take a pair over from another owner while
                    // firmware may still process that owner's 0x2e release
                    // for the same FList ID (fault at the predecessor's pool
                    // page list, boot fc063e77). Steady state is one counter
                    // snapshot; the wait happens only at an owner change.
                    failure_stage = "render-flist-takeover-guard";
                    {
                        let sibling = self.registry.warmable_sibling_render_slot(self.owner, render_slot);
                        if !runtime.render_flist_takeover_ready(render_slot, sibling, &self.context)? {
                            drop(runtime);
                            if takeover_polls >= 5000 {
                                pr_err!(
                                    "G17P FList takeover guard: pair {} still carries an unconsumed 0x2e release after 1 s; failing this submission rather than exposing a stale pool\n",
                                    render_slot,
                                );
                                return Err(ETIMEDOUT);
                            }
                            takeover_polls += 1;
                            fsleep(Delta::from_micros(200));
                            continue;
                        }
                    }
                    failure_stage = "render-warm-pairs";
                    let sibling = self.registry.warmable_sibling_render_slot(self.owner, render_slot);
                    let mut retry_warm = false;
                    for slot in [Some(render_slot), sibling].into_iter().flatten() {
                        let captured = match runtime.take_warm_render_graph(
                            &self.vm, slot, self.context.clone(), &completion, packet.command(),
                            &mut settled,
                        ) {
                            Ok(request) => request,
                            Err(error) if error == EBUSY || error == EAGAIN => {
                                if slot == render_slot { retry_warm = true; break; }
                                continue;
                            }
                            Err(error) => return Err(error),
                        };
                        if let Some((drm, mut request)) = captured {
                            drop(runtime);
                            let built = request.build(&drm);
                            runtime = self.runtime.mutex().lock();
                            settled = false;
                            let current = preparation.is_current();
                            let commit = built.is_ok() && current && !packet.cancel_requested()
                                && completion.vm_status().get() == 0 && self.context.is_current();
                            // No fallible exit before returning the reserved
                            // pair and its retained queue backing. Keep the
                            // epoch until the rejected graph has dropped too.
                            let returned = runtime.return_warm_render_graph(&mut request, commit);
                            drop(runtime);
                            drop(request);
                            returned?;
                            // The outer loop restarts (and clears `settled`) on retry.
                            if !current { retry_warm = true; runtime = self.runtime.mutex().lock(); break; }
                            built?;
                            if packet.cancel_requested() { return Err(ECANCELED); }
                            if completion.vm_status().get() != 0 { return Err(EIO); }
                            if !self.context.is_current() { return Err(EFAULT); }
                            runtime = self.runtime.mutex().lock();
                            settled = false;
                        }
                    }
                    if retry_warm || !preparation.is_current() {
                        drop(runtime);
                        continue;
                    }
                    // The 0x2e consumption witness is carried by the successor
                    // lifecycle and settled here and on every event-worker
                    // turn without sleeping.
                    failure_stage = "render-settle-freelist-witness";
                    runtime.settle_render_usc_freelist_release_before_submission()?;
                    failure_stage = "render-submit-translated";
                    let restage = match runtime.take_render_restage(
                        &self.vm, render_slot, self.context.clone(),
                        &completion, packet.command(), &mut settled,
                    ) {
                        Ok(prepared) => prepared,
                        Err(error) if error == EAGAIN => {
                            drop(runtime);
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    // A desynchronized idle graph can be recycled during
                    // reservation. Rebuild it through the detached path;
                    // never fall through to cold allocation under Runtime.
                    if restage.is_none() {
                        drop(runtime);
                        continue;
                    }
                    if let Some((drm, mut prepared)) = restage {
                        drop(runtime);
                        if e2e { e2e_t[3] = crate::g17_e2e::now(); }
                        // No fallible exit between borrowing and returning an
                        // installed graph: errors are carried back with it.
                        let prepared_result = prepared.prepare(&drm, &self.vm, packet.command(), &mut stages);
                        if e2e { e2e_t[4] = crate::g17_e2e::now(); }
                        let lock_started = stages.enabled.then(Instant::<Monotonic>::now);
                        runtime = self.runtime.mutex().lock();
                        if e2e { e2e_t[5] = crate::g17_e2e::now(); }
                        if let Some(started) = lock_started {
                            stages.runtime_lock_ns = stages.runtime_lock_ns.saturating_add(
                                started.elapsed().as_nanos().max(0) as u64);
                        }
                        let returned = runtime.return_render_restage(prepared);
                        if let Err(error) = returned {
                            runtime.cancel_render_restage(render_slot)?;
                            return Err(error);
                        }
                        if !preparation.is_current() {
                            runtime.cancel_render_restage(render_slot)?;
                            drop(runtime);
                            continue;
                        }
                        if let Err(error) = prepared_result {
                            runtime.cancel_render_restage(render_slot)?;
                            return Err(error);
                        }
                        if packet.cancel_requested() || completion.vm_status().get() != 0
                            || !self.context.is_current() {
                            runtime.cancel_render_restage(render_slot)?;
                            return Err(ECANCELED);
                        }
                    }
                    let result = runtime.submit_translated_render(
                        &self.vm,
                        render_slot,
                        true,
                        self.context.clone(),
                        packet.command(),
                        completion.clone(),
                        &mut stages,
                    );
                    if e2e {
                        let pair = |low: u64, high: u64| (low & 0xffff_ffff) | (high << 32);
                        crate::g17_e2e::record(crate::g17_e2e::K_PUB_LOCKS, u32::from(render_slot),
                            submission_id, [e2e_t[0], e2e_t[1], e2e_t[2], e2e_t[3], e2e_t[4]]);
                        crate::g17_e2e::record(crate::g17_e2e::K_PUB_STAGES,
                            u32::from(render_slot) | u32::from(stages.retained) << 8
                                | u32::from(stages.reachability_cache_hit) << 9
                                | u32::from(result.is_ok()) << 10
                                | u32::from(stages.config_present[0]) << 11
                                | u32::from(stages.config_present[1]) << 12,
                            submission_id,
                            [e2e_t[5], pair(stages.runtime_lock_ns, state_polls as u64),
                             pair(stages.prepare_ns, stages.restage_ns),
                             pair(stages.handoff_ns, stages.reachability_ns),
                             stages.doorbell_cntvct]);
                        let sub = |index: usize| u64::from(stages.sub_ns[index]);
                        crate::g17_e2e::record(crate::g17_e2e::K_PUB_SUB, stages.sub_ns[10],
                            submission_id, [pair(sub(0), sub(1)), pair(sub(2), sub(3)),
                            pair(sub(4), sub(5)), pair(sub(6), sub(7)), pair(sub(8), sub(9))]);
                    }
                    if result.is_err() { runtime.cancel_render_restage(render_slot)?; }
                    // Keep the epoch through publication so inline recovery
                    // cannot reset a successfully prepared but unpublished graph.
                    drop(preparation);
                    return result;
                }
            })();
            if e2e {
                crate::g17_e2e::record(crate::g17_e2e::K_PUB_EXIT, 0, submission_id,
                    [submit.is_ok() as u64, 0, 0, 0, 0]);
            }
            // Capture before optional legacy formatting; no completion-side copy.
            // Completion may already be recorded: userspace joins immutable IDs.
            if completion.trace_created_ns != 0 {
                let mut words = [0; crate::g17_timing_buffer::WORDS];
                words.copy_from_slice(&[crate::g17_timing_buffer::KIND_RENDER_ADMISSION,
                    self.owner, submission_id, completion.vdm_base,
                    completion.trace_created_ns, crate::g17_timing_buffer::now(), render_slot as u64,
                    completion.trace_run_ns.load(Ordering::Relaxed),
                    completion.trace_attach_ns.load(Ordering::Relaxed),
                    stages.runtime_lock_ns, stages.handoff_ns, stages.prepare_ns,
                    stages.restage_ns, stages.reachability_ns, stages.flist_request_ns,
                    // Word 16 (FList-release polls) is always 0 now; word 17
                    // counts command-buffer-state back-pressure polls.
                    0, state_polls as u64,
                    stages.tvb_blocks as u64, stages.retained as u64,
                    stages.reachability_cache_hit as u64, submit.is_ok() as u64,
                    // Word 21 was reserved zero: the architectural counter at
                    // submit return, on the compute records' clock.
                    crate::g17_timing_buffer::arch_counter(), 0, 0]);
                crate::g17_timing_buffer::record(words);
                // Publication sub-stages and the host-side 24 MHz timeline
                // (run, doorbell, return) for joining with the firmware's
                // pass-start/pass-end ticks in the completion record.
                let mut sub = [0; crate::g17_timing_buffer::WORDS];
                sub[0] = crate::g17_timing_buffer::KIND_RENDER_PUBLISH_STAGES;
                sub[1] = self.owner;
                sub[2] = submission_id;
                sub[3] = completion.vdm_base;
                sub[4] = run_cntvct;
                sub[5] = stages.doorbell_cntvct;
                sub[6] = crate::g17_timing_buffer::arch_counter();
                sub[7] = crate::g17_resources::G17P_RENDER_MAX_IN_FLIGHT as u64;
                for (index, value) in stages.sub_ns.iter().enumerate() {
                    sub[8 + index] = u64::from(*value);
                }
                sub[19] = u64::from(stages.config_present[0]);
                sub[20] = u64::from(stages.config_present[1]);
                sub[21] = u64::from(stages.item_targets[0]);
                sub[22] = u64::from(stages.item_targets[1]);
                sub[23] = u64::from(stages.item_plan_valid);
                crate::g17_timing_buffer::record(sub);
            }
            if module_parameters::g17p_completion_trace.value() != 0
                && trace_limit != 0
                && G17P_RENDER_POLL_RECORDS.fetch_add(1, Ordering::Relaxed) < trace_limit
            {
                pr_info!(
                    "G17P render admission: owner={} submission={} vdm={:#x} slot={} lock={}ns handoff={}ns prepare={}ns restage={}ns reachability={}ns cache-hit={} flist-request={}ns tvb-blocks={} retained={}\n",
                    self.owner, submission_id, completion.vdm_base, render_slot,
                    stages.runtime_lock_ns, stages.handoff_ns, stages.prepare_ns,
                    stages.restage_ns, stages.reachability_ns,
                    stages.reachability_cache_hit,
                    stages.flist_request_ns, stages.tvb_blocks, stages.retained,
                );
            }
            match submit {
                Ok(()) => {
                    completion.mark_published();
                    packet.mark_published();
                    Ok(packet.fence())
                }
                Err(error) => {
                    // submit_translated_render may fail after the first
                    // firmware-visible write. Let its exact tracker decide
                    // whether this is an unpublished release or quarantine.
                    self.runtime.mutex().lock().timeout_deferred_render(
                        render_slot, submission_id, completion, error,
                    );
                    Err(error)
                }
            }
        })();
        if let Err(error) = result.as_ref() {
            completion.fail_unpublished(*error);
            G17PQueue::stage_fail(submission_id, failure_stage, *error);
        }
        crate::g17_stackwatch::end(stack_watch, early);
        result
    }

    fn timed_out_inner(
        &self,
        packet: Arc<g17_job::G17PRenderJobPacket>,
    ) -> kernel::drm::sched::Status {
        if packet.request_cancel() {
            packet.completion().fail_unpublished(ETIMEDOUT);
        } else if packet.is_published() {
            if let Some(render_slot) = packet.completion().render_slot() {
                self.runtime.mutex().lock().timeout_deferred_render(
                    render_slot, packet.submission_id(), packet.completion(), ETIMEDOUT,
                );
            }
        }
        kernel::drm::sched::Status::Nominal
    }

    fn cancel_inner(&self, packet: Arc<g17_job::G17PRenderJobPacket>) {
        if packet.request_cancel() {
            packet.completion().fail_unpublished(ECANCELED);
            return;
        }
        if wait_for_render_teardown(&packet.fence()) {
            return;
        }
        if packet.is_published() {
            if let Some(render_slot) = packet.completion().render_slot() {
                self.runtime.mutex().lock().timeout_deferred_render(
                    render_slot, packet.submission_id(), packet.completion(), ETIMEDOUT,
                );
            }
        } else {
            packet.completion().fail_unpublished(ECANCELED);
        }
    }
}

enum G17PPendingCommand {
    Render(g17_uapi::TranslatedRenderCommand),
    Compute(g17_uapi::TranslatedComputeCommand),
}

type TimestampDestination = g17_job::TimestampDestination;

/// One boot-log line naming the compute publication model, once per load.
static G17P_COMPUTE_PIPELINE_ANNOUNCED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

pub(crate) struct G17PAsyncQueueBackend {
    runtime: G17PRuntimeRef,
    registry: Arc<G17PQueueRegistry>,
    owner: u64,
    hardware_queue: u8,
    context: Arc<crate::g17_resources::G17PLogicalQueueContext>,
}

impl Drop for G17PAsyncQueueBackend {
    fn drop(&mut self) { self.runtime.mutex().lock().release_async_compute_queue(self.owner); }
}

impl g17_job::G17PQueueBackend for G17PAsyncQueueBackend {
    fn timeout_progress(&self) -> Option<(u64, u32)> {
        self.runtime.mutex().lock().timeout_progress_snapshot()
    }

    fn victim_timeout(&self, packet: &g17_job::G17PComputeJobPacket, renewals: u8)
        -> crate::g17_recovery_isolation::VictimTimeout
    {
        self.runtime.mutex().lock().classify_compute_timeout(self.owner, packet.submission_id(), renewals)
    }

    /// No host dependency on the previous compute job. Its CL kick is this
    /// kick's implicit firmware parent, so publication never waits for a
    /// host retirement; scheduler credits bound only host bookkeeping.
    fn prepare(&self, _packet: &g17_job::G17PComputeJobPacket) -> Option<Fence> {
        None
    }
    fn publish(&self, packet: Arc<g17_job::G17PComputeJobPacket>) -> Result<Fence> {
        let status = packet.completion()?.status.clone();
        let result = (|| -> Result<Fence> {
            let completion = packet.completion()?;
            // A predecessor that already failed recorded its sticky VM status
            // before signalling. Refuse the queued successor before acquiring a
            // lease or publishing it.
            if completion.status.get() != 0 {
                completion.complete(Err(EIO));
                return Err(EIO);
            }
            // A dma_fence carrying an error is still signalled, so DRM sched has
            // legitimately made this job runnable. It does not mean the GPU
            // dependency succeeded. Consume every retained dependency status
            // before taking a hardware lease or writing queue state.
            for fence in packet.input_fences() {
                G17PQueue::wait_fence(fence).map_err(inherited_dependency_error)?;
            }
            for fence in packet.firmware_fences() {
                G17PQueue::check_firmware_fence(fence).map_err(inherited_dependency_error)?;
            }
            let hardware_fence = completion.fence();
            // iso4 REPLAY: while this queue awaits the retirement witness of a
            // spared recovery, its older never-started kicks are about to be
            // republished. Wait (bounded) so this newer job neither overtakes
            // them nor meets the quarantine and fails. Costs one owner scan
            // under the lock already taken; waits only after a recovery.
            let mut replay_wait_ms = if crate::g17_recovery_isolation::enabled(
                crate::g17_recovery_isolation::REPLAY,
            ) { crate::g17_recovery_isolation::SUBMIT_WAIT_MS } else { 0 };
            // The same context-local allocator serves TA, 3D and CL. Its
            // initialization must not take place under the device mutex.
            loop {
                if completion.status.get() != 0 { return Err(EIO); }
                let preparation = self.runtime.preparations.enter_wait()?;
                completion.work_state()?.acquire()?;
                let lease = self.registry.acquire_compute()?;
                let mut runtime = self.runtime.mutex().lock();
                if preparation.finish().is_err() {
                    drop(runtime);
                    continue;
                }
                if replay_wait_ms != 0 && runtime.spared_compute_pending(self.owner) {
                    drop(runtime);
                    drop(lease);
                    if replay_wait_ms == crate::g17_recovery_isolation::SUBMIT_WAIT_MS {
                        crate::g17_recovery_isolation::count(
                            &crate::g17_recovery_isolation::REPLAY_ORDER_WAITS);
                    }
                    replay_wait_ms -= 1;
                    if replay_wait_ms == 0 {
                        crate::g17_recovery_isolation::count(
                            &crate::g17_recovery_isolation::REPLAY_ORDER_TIMEOUTS);
                    }
                    fsleep(Delta::from_millis(1));
                    continue;
                }
                runtime.publish_async_compute(self.owner, packet, lease)?;
                break;
            }
            Ok(hardware_fence)
        })();
        if let Err(error) = result.as_ref() {
            let lost = *error == EIO
                || *error == ETIMEDOUT
                || *error == ENODEV
                || status.get() != 0;
            if lost {
                self.runtime
                    .mutex()
                    .lock()
                    .fail_outstanding_jobs_for_vm(&status, *error);
            }
        }
        result
    }
    fn timed_out(&self, packet: Arc<g17_job::G17PComputeJobPacket>) -> kernel::drm::sched::Status {
        self.runtime.mutex().lock().quarantine_async_compute(self.owner, packet.submission_id(), ETIMEDOUT);
        kernel::drm::sched::Status::Nominal
    }
    fn cancel(&self, packet: Arc<g17_job::G17PComputeJobPacket>) {
        let active = self.runtime.mutex().lock()
            .cancel_async_compute(self.owner, packet.submission_id());
        if !active {
            if let Ok(completion) = packet.completion() { completion.complete(Err(ECANCELED)); }
        }
    }
}

impl G17PQueue {
    fn new(
        dev: &AsahiDevice,
        runtime: G17PRuntimeRef,
        registry: Arc<G17PQueueRegistry>,
        gpc_perf_state_map: u32,
        gpc_perf_state_map_low: u32,
        gpc_perf_state_control: u32,
        vm: mmu::Vm,
        id: u64,
        priority: u32,
        usc_exec_base: u64,
    ) -> Result<Self> {
        if priority > 3 {
            return Err(EINVAL);
        }
        let user_range = T8140_USER_START..T8140_UNKNOWN_PAGE;
        g17_uapi::QueueUscWindow {
            base: usc_exec_base,
            user_start: user_range.start,
            user_end: user_range.end,
        }
        .validate()
        .map_err(|_| EINVAL)?;
        let fence_contexts = FenceContexts::new(1, QUEUE_NAME, QUEUE_CLASS_KEY)?;
        // Every logical queue owns a VM execution context, since render also
        // names it. Do not allocate a hardware compute graph here: desktop
        // stacks routinely create idle/render-only logical queues, and eager
        // allocation made those queues consume QID4 before the first real
        // compute submit. The graph and scheduler are created atomically by
        // ensure_async_compute_queue on first compute use.
        let context = runtime.mutex().lock().prepare_queue_execution_context(&vm)?;
        let render_backend = Arc::new(G17PRenderQueueBackend {
            runtime: runtime.clone(),
            registry: registry.clone(),
            owner: id,
            vm: vm.clone(),
            context: context.clone(),
            sched_backlog: AtomicU32::new(0),
        }, GFP_KERNEL)?;
        let render_scheduler = g17_job::G17PRenderScheduler::new(
            dev, render_backend.clone(),
            crate::g17_queue_limits::MAX_IN_FLIGHT_PER_QUEUE * crate::g17_queue_limits::RENDER_KICKS_PER_PAIR,
        )?;
        Ok(Self {
            dev: dev.into(),
            runtime,
            registry,
            gpc_perf_state_map,
            gpc_perf_state_map_low,
            gpc_perf_state_control,
            vm,
            user_range,
            usc_exec_base,
            fence_contexts,
            context,
            async_backend: None,
            async_scheduler: None,
            render_scheduler: Some(render_scheduler),
            render_sequence: 0,
            compute_sequence: 0,
            compute_frontier: KVec::new(),
            render_frontier: KVec::new(),
            validation_proof: G17PValidationProof::new(),
            e2e_fine: [0; 12],
            context_unbound_refusal: None,
            recent: [const { G17PRecentSubmission::EMPTY }; crate::g17_host_path::FW_DEP_RECENT],
            recent_next: 0,
            ts_pool: g17_job::G17PTimestampPool::new()?,
            ioctl_commands: Some(KVec::new()),
            id,
        })
    }

    // Out of line: queue (re)creation builds a whole compute graph; keep its
    // frame off submit_compute_async (ioctl stack depth).
    #[inline(never)]
    fn ensure_async_compute_queue(&mut self) -> Result {
        // Recovery isolation: a spared queue is returned to service by its
        // firmware-retirement witness, normally within the recovery's own
        // event pass. Give that witness a bounded chance before the queue
        // check below would refuse (and so poison) this healthy client.
        if crate::g17_recovery_isolation::enabled(crate::g17_recovery_isolation::ADMISSION_WAIT) {
            let mut waited = false;
            let mut pending = false;
            for _ in 0..crate::g17_recovery_isolation::SUBMIT_WAIT_MS {
                pending = self.runtime.mutex().lock().spared_compute_pending(self.id);
                if !pending { break; }
                if !waited {
                    waited = true;
                    crate::g17_recovery_isolation::count(&crate::g17_recovery_isolation::SUBMIT_WAITS);
                }
                fsleep(Delta::from_millis(1));
            }
            if pending {
                crate::g17_recovery_isolation::count(&crate::g17_recovery_isolation::SUBMIT_TIMEOUTS);
            }
        }
        match (&self.async_backend, &self.async_scheduler) {
            (Some(backend), Some(_)) => {
                if self.runtime.mutex().lock().async_compute_queue_matches(
                    self.id, backend.hardware_queue, &backend.context,
                )? {
                    return Ok(());
                }
            }
            (None, None) => {}
            _ => return Err(EIO),
        }

        // Processor-stop recovery removes every runtime graph after settling
        // its active fence. Drop the now-stale entity first so any jobs queued
        // behind that fence are cancelled before a new graph is published.
        // Backend Drop is harmless here: the runtime just proved this owner is
        // absent. Keep the same teardown ordering as G17PQueue::drop.
        drop(self.async_scheduler.take());
        drop(self.async_backend.take());

        // Default returns the original Arc. The opt-in probe allocates once
        // per backend creation, never on each render/compute engine switch.
        let context = self.runtime.mutex().lock()
            .prepare_compute_execution_context(&self.context)?;
        let (hardware_queue, context) = self.runtime.mutex().lock()
            .prepare_async_compute_queue(self.id, &self.vm, context)?;
        let backend = Arc::new(G17PAsyncQueueBackend {
            runtime: self.runtime.clone(),
            registry: self.registry.clone(),
            owner: self.id,
            hardware_queue,
            context,
        }, GFP_KERNEL).inspect_err(|_| {
            self.runtime.mutex().lock().release_async_compute_queue(self.id);
        })?;
        let scheduler = g17_job::G17PJobScheduler::new(
            &self.dev, backend.clone(),
            crate::g17_queue_limits::MAX_COMPUTE_IN_FLIGHT_PER_QUEUE,
        )?;
        if !G17P_COMPUTE_PIPELINE_ANNOUNCED.swap(true, Ordering::Relaxed) {
            pr_info!("G17P async compute: native-ordered pipeline, {} kicks per queue, no host retirement between kicks\n",
                crate::g17_queue_limits::MAX_COMPUTE_IN_FLIGHT_PER_QUEUE);
        }
        self.async_backend = Some(backend);
        self.async_scheduler = Some(scheduler);
        Ok(())
    }

    /// Print one throughput window.
    ///
    /// This is the only line the submit path emits at steady state, and it
    /// carries every number the driver can honestly report:
    ///
    /// * `renders`/`computes`/`gpu-intervals` -- what the window contained,
    ///   and how many renders published a native pass envelope.
    /// * `renders/s` -- end-to-end submission throughput.
    /// * `ioctl`/`publish`/`wait` -- host microseconds per render, and the
    ///   split between building and publishing the work and waiting for it.
    /// * `gpu` -- the firmware's own pass envelope, converted with the 24 MHz
    ///   rate this driver already reports through `DRM_IOCTL_ASAHI_GET_TIME`.
    /// * `occupancy` -- GPU busy ticks over the tick span the window covered.
    ///   Both terms are GPU-written, so no timebase enters this ratio.
    /// * `ioctl-share` -- how much of wall time the client spent inside the
    ///   ioctl. With a synchronous render wait this is the number that says
    ///   how much throughput a client is losing to the driver.
    /// * `tick-khz` -- the tick rate MEASURED from this window. It exists so a
    ///   wrong timebase assumption is visible instead of silently scaling
    ///   every microsecond above.
    fn report_perf(&self, report: g17_perf::SubmitReport) {
        // Async publication records host costs before the job has completed.
        // Missing completion samples do not establish zero GPU utilisation.
        if report.intervals == 0 {
            dev_info!(
                self.dev.as_ref(),
                "G17P perf[{}]: renders={} computes={} gpu-intervals=0 wall={}us renders/s={}.{:02} ioctl={}.{}us(max {}.{}us) publish={}.{}us compute-ioctl={}.{}us ioctl-share={}.{}% gpu=unmeasured\n",
                report.window_index,
                report.renders,
                report.computes,
                report.wall_us,
                report.renders_per_s_x100 / 100,
                report.renders_per_s_x100 % 100,
                report.ioctl_avg_us_x10 / 10,
                report.ioctl_avg_us_x10 % 10,
                report.ioctl_max_us_x10 / 10,
                report.ioctl_max_us_x10 % 10,
                report.publish_avg_us_x10 / 10,
                report.publish_avg_us_x10 % 10,
                report.compute_avg_us_x10 / 10,
                report.compute_avg_us_x10 % 10,
                report.ioctl_pct_x10 / 10,
                report.ioctl_pct_x10 % 10,
            );
            return;
        }
        dev_info!(
            self.dev.as_ref(),
            "G17P perf[{}]: renders={} computes={} gpu-intervals={} wall={}us renders/s={}.{:02} ioctl={}.{}us(max {}.{}us) publish={}.{}us wait={}.{}us polls={}.{} compute-ioctl={}.{}us gpu={}.{}us(max {}.{}us) occupancy={}.{}% ioctl-share={}.{}% tick-khz={}\n",
            report.window_index,
            report.renders,
            report.computes,
            report.intervals,
            report.wall_us,
            report.renders_per_s_x100 / 100,
            report.renders_per_s_x100 % 100,
            report.ioctl_avg_us_x10 / 10,
            report.ioctl_avg_us_x10 % 10,
            report.ioctl_max_us_x10 / 10,
            report.ioctl_max_us_x10 % 10,
            report.publish_avg_us_x10 / 10,
            report.publish_avg_us_x10 % 10,
            report.wait_avg_us_x10 / 10,
            report.wait_avg_us_x10 % 10,
            report.polls_avg_x10 / 10,
            report.polls_avg_x10 % 10,
            report.compute_avg_us_x10 / 10,
            report.compute_avg_us_x10 % 10,
            report.gpu_avg_us_x10 / 10,
            report.gpu_avg_us_x10 % 10,
            report.gpu_max_us_x10 / 10,
            report.gpu_max_us_x10 % 10,
            report.occupancy_pct_x10 / 10,
            report.occupancy_pct_x10 % 10,
            report.ioctl_pct_x10 / 10,
            report.ioctl_pct_x10 % 10,
            report.tick_khz,
        );
    }

    /// Name the UAPI check that refused a submission. Without this the ioctl
    /// only reports a bare EINVAL, and every validation in `g17_uapi` looks
    /// alike from userspace.
    fn reject_uapi(
        submission_id: u64,
        stage: &str,
        error: g17_uapi::UapiTranslateError,
    ) -> kernel::error::Error {
        pr_info!(
            "G17P submit: UAPI REJECT submission {} at {} check={}\n",
            submission_id,
            stage,
            error.name()
        );
        EINVAL
    }

    /// Log and return any error raised on the submit path outside UAPI
    /// translation. `reject_uapi` only covers parse/translate; everything from
    /// the sync-count check to the manager publish used to fail silently, which
    /// is why a submission could die with a bare EINVAL and no marker at all.
    fn stage_fail(submission_id: u64, stage: &str, error: kernel::error::Error) -> kernel::error::Error {
        if mmu::is_context_unbound(error) {
            pr_info!("G17P submit: context-unbound refusal submission {} stage={} errno=-2\n", submission_id, stage);
            return error;
        }
        if error == ENOSPC {
            crate::g17_queue_slots::note_submit_enospc(stage);
        }
        pr_info!(
            "G17P submit: FAIL submission {} stage={} errno={}\n",
            submission_id,
            stage,
            error.to_errno()
        );
        error
    }

    /// Walk the raw command buffer and report every command header, without
    /// validating anything. This says what a submission actually CONTAINS --
    /// render vs compute vs attachment-set, sizes and barriers -- so a
    /// submission that dies before the parse loop is still identifiable.
    fn log_cmdbuf_headers(submission_id: u64, raw: &[u8]) {
        // Every line inside is already gated, but the walk itself ran on every
        // submission to produce nothing. Leave the whole thing out of the
        // steady-state path.
        if !crate::util::g17p_verbose() {
            return;
        }
        let mut offset = 0usize;
        let mut index = 0u32;
        while offset + g17_uapi::UAPI_COMMAND_HEADER_SIZE <= raw.len() {
            let header = &raw[offset..offset + g17_uapi::UAPI_COMMAND_HEADER_SIZE];
            let command_type = u16::from_le_bytes([header[0], header[1]]);
            let size = u16::from_le_bytes([header[2], header[3]]);
            let vdm_barrier = u16::from_le_bytes([header[4], header[5]]);
            let cdm_barrier = u16::from_le_bytes([header[6], header[7]]);
            if crate::util::g17p_verbose() {
                pr_info!(
                    "G17P submit: submission {} cmd[{}] type={} size={} vdm_barrier={:#x} cdm_barrier={:#x} at offset {}\n",
                    submission_id,
                    index,
                    command_type,
                    size,
                    vdm_barrier,
                    cdm_barrier,
                    offset,
                );
            }
            offset += g17_uapi::UAPI_COMMAND_HEADER_SIZE + size as usize;
            index += 1;
            if index > 8 {
                if crate::util::g17p_verbose() {
                    pr_info!("G17P submit: submission {} cmd list truncated\n", submission_id);
                }
                return;
            }
        }
        if offset != raw.len() {
            if crate::util::g17p_verbose() {
                pr_info!(
                    "G17P submit: submission {} cmd walk ended at {} of {} bytes (ragged)\n",
                    submission_id,
                    offset,
                    raw.len(),
                );
            }
        }
    }

    /// v93: remember a fully enqueued ioctl for FW_DEPS matching and the
    /// AGG_FIRST depth count. Only called after every command was queued, so
    /// an ioctl that failed part-way (whose aggregate may still carry that
    /// failure) is never matched and keeps the host wait.
    fn note_recent_submission(&mut self, aggregate: Fence, render_before: u64, compute_before: u64) {
        let range = |before: u64, after: u64| if after > before { (before + 1, after) } else { (0, 0) };
        let (render, compute) = (range(render_before, self.render_sequence),
            range(compute_before, self.compute_sequence));
        let index = self.recent_next % crate::g17_host_path::FW_DEP_RECENT;
        self.recent_next = (index + 1) % crate::g17_host_path::FW_DEP_RECENT;
        let slot = &mut self.recent[index];
        slot.aggregate = None;
        slot.commands.clear();
        // An incomplete command list (allocation failure) leaves the slot
        // empty: that ioctl is never matched and keeps the host wait.
        let Some(commands) = self.ioctl_commands.as_mut() else {
            self.ioctl_commands = Some(KVec::new());
            return;
        };
        core::mem::swap(&mut slot.commands, commands);
        commands.clear();
        slot.aggregate = Some(aggregate);
        slot.render = render;
        slot.compute = compute;
    }

    /// v93: remember one enqueued command's hardware fence for the ring.
    /// A command with host-copied user timestamps leaves the ioctl's list
    /// incomplete, so FW_DEPS never converts a wait on it: firmware tokens
    /// cannot order the CPU timestamp copy that precedes its completion.
    fn note_ioctl_command(&mut self, fence: &Fence, host_timestamps: bool) {
        if host_timestamps {
            self.ioctl_commands = None;
            return;
        }
        if let Some(commands) = self.ioctl_commands.as_mut() {
            if commands.push(fence.clone(), GFP_KERNEL).is_err() {
                self.ioctl_commands = None;
            }
        }
    }

    fn recent_identities(&self) -> [Option<crate::g17_host_path::FwDepRecent>; crate::g17_host_path::FW_DEP_RECENT] {
        let mut out = [None; crate::g17_host_path::FW_DEP_RECENT];
        for (index, entry) in self.recent.iter().enumerate() {
            if let Some(aggregate) = &entry.aggregate {
                out[index] = Some(crate::g17_host_path::FwDepRecent {
                    identity: aggregate.raw() as usize,
                    render_last: entry.render.1,
                    compute_last: entry.compute.1,
                });
            }
        }
        out
    }

    /// v93 AGG_FIRST: a render whose recorded aggregate has signalled is
    /// retired even if its hardware fence (signalled just after) is not yet.
    fn render_retired_by_aggregate(&self, sequence: u64) -> bool {
        self.recent.iter().any(|entry| {
            entry.render.0 != 0 && entry.render.0 <= sequence && sequence <= entry.render.1
                && entry.aggregate.as_ref().is_some_and(|aggregate|
                    // SAFETY: the ring owns this aggregate reference.
                    unsafe { bindings::dma_fence_get_status(aggregate.raw()) } != 0)
        })
    }

    /// Renders counted by EARLY_SHALLOW. Without AGG_FIRST this is exactly
    /// the pruned frontier length (v87..v92d).
    fn outstanding_renders(&self) -> usize {
        if !crate::g17_host_path::enabled(crate::g17_host_path::AGG_FIRST) {
            return self.render_frontier.len();
        }
        self.render_frontier.iter()
            .filter(|(sequence, _)| !self.render_retired_by_aggregate(*sequence))
            .count()
    }

    /// v93 FW_DEPS: unwrap one input fence (timeline chain links, each
    /// holding one leaf) and fold every leaf into `resolution`. Returns false
    /// as soon as any leaf is not provably this queue's own recent work.
    /// Arrays and nested containers are never unwrapped: no field binding
    /// exists for them, so they keep the host wait.
    fn fw_dep_walk(
        fence: &Fence, recent: &[Option<crate::g17_host_path::FwDepRecent>],
        resolution: &mut crate::g17_host_path::FwDepResolution,
    ) -> bool {
        use crate::g17_host_path::{fw_dep_step, FwDepLeaf, FW_DEP_LINKS_MAX};
        // Exported, immutable ops tables; only their addresses are compared.
        let chain_ops = core::ptr::addr_of!(bindings::dma_fence_chain_ops);
        let array_ops = core::ptr::addr_of!(bindings::dma_fence_array_ops);
        let mut node = fence.raw();
        // SAFETY: `fence` owns a reference; this walk owns one more, which
        // dma_fence_chain_walk consumes (returning the next link's reference
        // or NULL) and which an early exit drops explicitly.
        unsafe { bindings::dma_fence_get(node) };
        let mut links = 0u32;
        while !node.is_null() {
            links += 1;
            // SAFETY: `node` is referenced; a chain node's `fence` is fixed
            // for the node's lifetime and referenced by it.
            let leaf_ptr = unsafe {
                if (*node).ops == chain_ops {
                    (*node.cast::<bindings::dma_fence_chain>()).fence
                } else {
                    node
                }
            };
            let leaf = if links > FW_DEP_LINKS_MAX || leaf_ptr.is_null() {
                FwDepLeaf::Unsupported
            } else {
                // SAFETY: `leaf_ptr` is kept alive by the referenced node.
                let ops = unsafe { (*leaf_ptr).ops };
                if ops == chain_ops || ops == array_ops {
                    FwDepLeaf::Unsupported
                } else {
                    // SAFETY: as above.
                    match unsafe { bindings::dma_fence_get_status(leaf_ptr) } {
                        status if status > 0 => FwDepLeaf::Signalled,
                        status if status < 0 => FwDepLeaf::Failed,
                        _ => FwDepLeaf::Pending(leaf_ptr as usize),
                    }
                }
            };
            if !fw_dep_step(resolution, leaf, recent) {
                // SAFETY: drop the walk's reference to the current link.
                unsafe { bindings::dma_fence_put(node) };
                return false;
            }
            // SAFETY: consumes `node`'s reference; returns the previous link
            // referenced, or NULL at the end (also for a non-chain fence).
            node = unsafe { bindings::dma_fence_chain_walk(node) };
        }
        true
    }

    /// v93 FW_DEPS: split this ioctl's input syncs. A sync is converted only
    /// if every leaf is signalled-OK or a pending aggregate of one of this
    /// queue's recent fully enqueued ioctls; otherwise it stays a host wait
    /// exactly as before. Same queue implies same owner, VM and fault domain.
    fn plan_input_syncs(&self, syncs: &[file::SyncItem]) -> Result<G17PSyncPlan> {
        let recent = self.recent_identities();
        let mut plan = G17PSyncPlan {
            host: KVec::with_capacity(syncs.len(), GFP_KERNEL)?,
            firmware: KVec::new(),
            render_through: 0,
            compute_through: 0,
        };
        for sync in syncs {
            let Some(fence) = &sync.fence else { continue };
            let mut resolution = crate::g17_host_path::FwDepResolution::default();
            if !Self::fw_dep_walk(fence, &recent, &mut resolution) {
                crate::g17_host_path::count(&crate::g17_host_path::FW_DEP_HOST);
                plan.host.push(fence.clone(), GFP_KERNEL)?;
                continue;
            }
            if resolution.used == 0 {
                crate::g17_host_path::count(&crate::g17_host_path::FW_DEP_SATISFIED);
                continue;
            }
            crate::g17_host_path::count(&crate::g17_host_path::FW_DEP_CONVERTED);
            plan.render_through = plan.render_through.max(resolution.render_through);
            plan.compute_through = plan.compute_through.max(resolution.compute_through);
            for (index, entry) in self.recent.iter().enumerate() {
                if resolution.used & (1u32 << index) == 0 { continue; }
                // The aggregate and every command's hardware fence: each is a
                // status check at publication (a failure before execution
                // must fail the dependent exactly as the host wait did).
                for fence in Iterator::chain(entry.aggregate.iter(), entry.commands.iter()) {
                    if !plan.firmware.iter().any(|known| known.raw() == fence.raw()) {
                        plan.firmware.push(fence.clone(), GFP_KERNEL)?;
                    }
                }
            }
        }
        Ok(plan)
    }

    /// A failed timestamp publisher has no valid query value to expose. Keep
    /// the earliest failed timestamp fence as a bounded prefix-error witness,
    /// even for cancellation/recovery-spared errors which do not poison the VM.
    /// Entries are appended in increasing logical sequence: every prefix that
    /// covers any later failed publisher also covers this first one. Holding
    /// one independent fence does not retain the producer's packet or mapping.
    fn prune_compute_frontier(&mut self) {
        let mut retained_timestamp_failure = false;
        self.compute_frontier.retain(|(_, fence)| {
            // SAFETY: the retained Fence owns this status read. Inspect once:
            // a pending fence that fails afterward stays for the next pass.
            let status = unsafe { bindings::dma_fence_get_status(fence.completed.raw()) };
            if status == 0 { return true; }
            if status < 0 && fence.ready.raw() == fence.completed.raw()
                && !retained_timestamp_failure
            {
                retained_timestamp_failure = true;
                return true;
            }
            false
        });
    }

    fn prune_render_frontier(&mut self) {
        // All command failure paths publish sticky VM status before signalling;
        // an errored render may be dropped here without treating it as success.
        self.render_frontier.retain(|(_, fence)| {
            // SAFETY: the retained Fence owns the pointer for this status read.
            unsafe { bindings::dma_fence_get_status(fence.completed.raw()) == 0 }
        });
    }

    /// Queue a compute command with its required render prefix and an owned
    /// member of the ioctl output aggregate. Input fence scope is unchanged.
    // Never inlined: keeps this command's frame off the other command
    // kind's publication path (stack depth, see g17p_stack_watch).
    #[inline(never)]
    fn submit_compute_async(
        &mut self, submission_id: u64, translated: &g17_uapi::TranslatedComputeCommand,
        syncs: &[file::SyncItem], plan: Option<&G17PSyncPlan>, record: bool,
        objects: core::pin::Pin<&xarray::XArray<KBox<file::Object>>>,
        aggregate_member: G17PSubmitCompletionMember,
        vm_job: mmu::T8140VmJobGuard,
        entered: Option<Instant<Monotonic>>,
    ) -> Result {
        let mut translated = *translated;
        // Native parents retain ordinary same-engine execution order. Explicit
        // prefixes additionally cover host-published timestamp destinations.
        if let Some(plan) = plan {
            translated.render_wait_through = crate::g17_host_path::fw_dep_raise(
                translated.render_wait_through, plan.render_through);
            translated.compute_wait_through = crate::g17_host_path::fw_dep_raise(
                translated.compute_wait_through, plan.compute_through);
        }
        self.prune_compute_frontier();
        self.compute_frontier.reserve(1, GFP_KERNEL)?;
        self.ensure_async_compute_queue()?;
        self.runtime.mutex().lock().check_async_compute_submission(self.id)?;
        let destinations = [
            Self::timestamp_destination(objects, translated.timestamps.start)?,
            Self::timestamp_destination(objects, translated.timestamps.end)?,
        ];
        let (timestamps, ts_pool) = self.compute_timestamps()?;
        let output = new_independent_submission_fence(&self.fence_contexts)?;
        self.prune_render_frontier();
        let backend = self.async_backend.as_ref().ok_or(EIO)?;
        // Resolve cross-engine publication before taking native token snapshots.
        // Only publication fences order the host: a stamp must exist before a
        // kick can name it. Explicit user syncs remain completion
        // dependencies; native CL dependencies (render barriers plus the
        // implicit previous-kick parent) carry the execution order.
        let prefix_count = self.render_frontier.iter().filter(|(sequence, _)|
            crate::g17_submission::g17p_render_prefix_contains(translated.render_wait_through, *sequence)
        ).count();
        let timestamp_prefix_count = if translated.compute_wait_through.is_some() {
            self.compute_frontier.iter().filter(|(sequence, fence)|
                crate::g17_submission::g17p_render_prefix_contains(translated.compute_wait_through, *sequence)
                    && fence.ready.raw() == fence.completed.raw()
            ).count()
        } else { 0 };
        let mut dependencies = KVec::with_capacity(syncs.len() + prefix_count + timestamp_prefix_count, GFP_KERNEL)?;
        let mut checked_dependencies = KVec::with_capacity(syncs.len() + timestamp_prefix_count, GFP_KERNEL)?;
        let mut firmware_fences = KVec::with_capacity(prefix_count, GFP_KERNEL)?;
        append_compute_timestamp_dependencies(
            &self.compute_frontier, translated.compute_wait_through,
            &mut dependencies, &mut checked_dependencies,
        )?;
        append_render_prefix_dependencies(
            &self.render_frontier, translated.render_wait_through,
            &mut dependencies, &mut firmware_fences,
        )?;
        match plan {
            None => for sync in syncs {
                if let Some(fence) = &sync.fence {
                    checked_dependencies.push(fence.clone(), GFP_KERNEL)?;
                    dependencies.push(fence.clone(), GFP_KERNEL)?;
                }
            },
            Some(plan) => {
                for fence in plan.host.iter() {
                    checked_dependencies.push(fence.clone(), GFP_KERNEL)?;
                    dependencies.push(fence.clone(), GFP_KERNEL)?;
                }
                for fence in plan.firmware.iter() {
                    firmware_fences.push(fence.clone(), GFP_KERNEL)?;
                }
            }
        }
        let host_timestamps = destinations.iter().any(Option::is_some);
        let completion = g17_job::G17PJobCompletion::new(
            output,
            timestamps,
            ts_pool,
            destinations,
            self.vm.status().clone(),
            vm_job,
            aggregate_member,
        )?;
        let compute_sequence = translated.compute_sequence;
        let packet = g17_job::G17PComputeJobPacket::new(
            submission_id, backend.hardware_queue, backend.context.clone(), translated,
            KVec::new(), KVec::new(), checked_dependencies, firmware_fences,
            Some(completion),
        )?;
        let mut finished = self.async_scheduler.as_mut().ok_or(EIO)?.enqueue(packet, dependencies)?;
        // Firmware dependency tokens precede the CPU timestamp copy. A
        // consumer must not copy the query sentinel before that write lands.
        if host_timestamps {
            finished.ready = finished.completed.clone();
        }
        let recorded = record.then(|| finished.completed.clone());
        // The job is queued and owns its completion from here: give userspace
        // the fence that signals when the GPU has actually run it.
        self.compute_sequence = compute_sequence;
        self.compute_frontier.push_within_capacity((compute_sequence, finished))
            .map_err(|_| ENOMEM)?;
        if crate::util::g17p_verbose() {
            pr_info!("G17P async submit: QUEUED submission {} owner={} qid={} context={}\n",
                submission_id, self.id, backend.hardware_queue, backend.context.context_id());
        }
        if let Some(entered) = entered {
            if let Some(report) = self
                .registry
                .record_compute(entered.elapsed().as_nanos().max(0) as u64)
            {
                self.report_perf(report);
            }
        }
        if let Some(fence) = recorded { self.note_ioctl_command(&fence, host_timestamps); }
        Ok(())
    }

    /// Queue render behind user completion syncs and its explicit compute
    /// prefix's publication. Independent engines can publish in either order;
    /// the runtime resolves only tokens inside the captured logical prefix.
    // Never inlined: keeps this command's frame off the other command
    // kind's publication path (stack depth, see g17p_stack_watch).
    #[inline(never)]
    fn submit_render_async(
        &mut self, submission_id: u64, translated: &g17_uapi::TranslatedRenderCommand,
        syncs: &[file::SyncItem], plan: Option<&G17PSyncPlan>, record: bool,
        objects: core::pin::Pin<&xarray::XArray<KBox<file::Object>>>,
        aggregate_member: G17PSubmitCompletionMember,
        vm_job: mmu::T8140VmJobGuard,
        entered: Option<Instant<Monotonic>>,
    ) -> Result {
        // g17p_tile_backstop: from the caller's command, before the local copy.
        let footprint = self.tile_footprint(&translated.fragment_attachments)?;
        let mut translated = *translated;
        // v93 FW_DEPS: same-queue input syncs become render/compute prefixes,
        // resolved exactly like an explicit barrier: render predecessors by
        // native tokens at publication (scheduler FIFO publishes them first),
        // compute predecessors by publication fences plus native tokens.
        if let Some(plan) = plan {
            translated.render_order.wait_through = crate::g17_host_path::fw_dep_raise(
                translated.render_order.wait_through, plan.render_through);
            translated.compute_wait_through = crate::g17_host_path::fw_dep_raise(
                translated.compute_wait_through, plan.compute_through);
        }
        self.prune_render_frontier();
        self.render_frontier.reserve(1, GFP_KERNEL)?;
        let timestamps = [
            Self::timestamp_destination(objects, translated.vertex_timestamps.start)
                .map_err(|error| Self::stage_fail(submission_id, "ts-vtx-start", error))?,
            Self::timestamp_destination(objects, translated.vertex_timestamps.end)
                .map_err(|error| Self::stage_fail(submission_id, "ts-vtx-end", error))?,
            Self::timestamp_destination(objects, translated.fragment_timestamps.start)
                .map_err(|error| Self::stage_fail(submission_id, "ts-frag-start", error))?,
            Self::timestamp_destination(objects, translated.fragment_timestamps.end)
                .map_err(|error| Self::stage_fail(submission_id, "ts-frag-end", error))?,
        ];
        self.e2e_fine[7] = crate::g17_e2e::fine();
        let hardware_fence = new_independent_submission_fence(&self.fence_contexts)
            .map_err(|error| Self::stage_fail(submission_id, "new-render-fence", error))?;
        let host_timestamps = timestamps.iter().any(Option::is_some);
        let completion = G17PRenderCompletion::new(
            hardware_fence,
            timestamps,
            self.vm.status().clone(),
            vm_job,
            submission_id,
            translated.vdm_base,
            self.id,
            translated.render_order,
            aggregate_member,
        )?;
        let prefix_count = self.compute_frontier.iter().filter(|(sequence, _)|
            crate::g17_submission::g17p_render_prefix_contains(translated.compute_wait_through, *sequence)
        ).count();
        let mut dependencies = KVec::with_capacity(syncs.len() + prefix_count, GFP_KERNEL)?;
        let mut checked_dependencies = KVec::with_capacity(syncs.len(), GFP_KERNEL)?;
        let mut firmware_fences = KVec::with_capacity(prefix_count, GFP_KERNEL)?;
        append_render_prefix_dependencies(
            &self.compute_frontier, translated.compute_wait_through,
            &mut dependencies, &mut firmware_fences,
        )?;
        match plan {
            None => for sync in syncs {
                if let Some(fence) = &sync.fence {
                    checked_dependencies.push(fence.clone(), GFP_KERNEL)?;
                    dependencies.push(fence.clone(), GFP_KERNEL)?;
                }
            },
            Some(plan) => {
                for fence in plan.host.iter() {
                    checked_dependencies.push(fence.clone(), GFP_KERNEL)?;
                    dependencies.push(fence.clone(), GFP_KERNEL)?;
                }
                for fence in plan.firmware.iter() {
                    firmware_fences.push(fence.clone(), GFP_KERNEL)?;
                }
            }
        }
        let e2e_counts = crate::g17_e2e::armed().then(|| [dependencies.len() as u64,
            checked_dependencies.len() as u64, firmware_fences.len() as u64]);
        self.e2e_fine[8] = crate::g17_e2e::fine();
        let packet = g17_job::G17PRenderJobPacket::new(
            submission_id, translated, checked_dependencies, firmware_fences, completion,
            footprint,
        )?;
        self.e2e_fine[10] = crate::g17_e2e::fine();
        // v84: publish from this ioctl only for fully independent work.
        // v87 EARLY_SHALLOW: and only while this queue is shallow. The pruned
        // frontier (start of this function) holds exactly the queue's
        // unsignalled earlier renders; a deeper queue takes the unchanged
        // scheduler path, identical to v84 with bit0 clear.
        // v93 EARLY_DEPS: only an unsignalled host dependency declines; a
        // signalled one (e.g. an already published compute prefix) cannot
        // make the ioctl wait. v93 AGG_FIRST: see `outstanding_renders`.
        let host_blocked = if crate::g17_host_path::enabled(crate::g17_host_path::EARLY_DEPS) {
            // SAFETY: each owned Fence keeps its pointer alive for the read.
            dependencies.iter().any(|fence| unsafe { bindings::dma_fence_get_status(fence.raw()) } == 0)
        } else {
            !dependencies.is_empty()
        };
        let early_allowed = crate::g17_host_path::early_publish_allowed(
            self.outstanding_renders(), host_blocked);
        let mut finished = self.render_scheduler.as_mut().ok_or(ENODEV)?
            .enqueue(packet, dependencies, early_allowed)?;
        // Firmware dependency tokens precede the CPU timestamp copy. A
        // consumer must not copy the query sentinel before that write lands.
        if host_timestamps {
            finished.ready = finished.completed.clone();
        }
        if record { self.note_ioctl_command(&finished.completed, host_timestamps); }
        if let Some(counts) = e2e_counts {
            crate::g17_e2e::record(crate::g17_e2e::K_SCHED_PUSHED, 0, submission_id,
                [counts[0], counts[1], counts[2], translated.render_order.sequence, 0]);
            if crate::g17_e2e::option(crate::g17_e2e::OPT_FINE) {
                let f = self.e2e_fine;
                crate::g17_e2e::record(crate::g17_e2e::K_FINE_IOCTL, f[9] as u32, submission_id,
                    [f[0], f[1], f[2], f[3], f[4]]);
                crate::g17_e2e::record(crate::g17_e2e::K_FINE_QUEUE, 0, submission_id,
                    [f[5], f[6], f[7], f[8], f[10]]);
            }
        }
        self.render_sequence = translated.render_order.sequence;
        self.render_frontier.push_within_capacity((self.render_sequence, finished))
            .map_err(|_| ENOMEM)?;
        if crate::util::g17p_verbose() {
            pr_info!("G17P render submit: QUEUED submission {} owner={} context={}\n",
                submission_id, self.id, self.context.context_id());
        }
        if let Some(entered) = entered {
            let mut sample = g17_perf::RenderSample::default();
            sample.ioctl_ns = entered.elapsed().as_nanos().max(0) as u64;
            if let Some(report) = self.registry.record_render(sample) {
                self.report_perf(report);
            }
        }
        Ok(())
    }

    /// Dump the render payload exactly as userspace supplied it, before any
    /// validation runs. A rejected submit never reaches the GPU, so this is
    /// the only place the offending field is visible -- and printing it
    /// unconditionally means one hardware run diagnoses any check, not just
    /// the one that happens to fire first.
    fn log_render_payload(submission_id: u64, payload: &g17_uapi::UapiRenderCommand) {
        if crate::util::g17p_verbose() {
            pr_info!(
                "G17P render uapi[{}]: flags={:#x} {}x{} layers={} utile={}x{} samples={} sample_size={} tib_cfg={:#x}\n",
                submission_id,
                payload.flags,
                payload.width,
                payload.height,
                payload.layers,
                payload.utile_width,
                payload.utile_height,
                payload.samples,
                payload.sample_size,
                payload.ppp_control,
            );
        }
        if crate::util::g17p_verbose() {
            pr_info!(
                "G17P render uapi[{}]: vdm={:#x} scissor={:#x} dbias={:#x} oclqry={:#x} sampler={:#x}/{} bgobjdepth={:#x} bgobjvals={:#x}\n",
                submission_id,
                payload.vdm_base,
                payload.scissor_base,
                payload.depth_bias_base,
                payload.occlusion_query_base,
                payload.sampler_heap,
                payload.sampler_count,
                payload.depth_clear,
                payload.stencil_clear,
            );
        }
        if crate::util::g17p_verbose() {
            pr_info!(
                "G17P render uapi[{}]: depth={:#x}/{:#x} stride={:#x}/{:#x} stencil={:#x}/{:#x} stride={:#x}/{:#x} zls={:#x}\n",
                submission_id,
                payload.depth.base,
                payload.depth.compression_base,
                payload.depth.stride,
                payload.depth.compression_stride,
                payload.stencil.base,
                payload.stencil.compression_base,
                payload.stencil.stride,
                payload.stencil.compression_stride,
                payload.zls_control,
            );
        }
        if crate::util::g17p_verbose() {
            pr_info!(
                "G17P render uapi[{}]: bg={:#x}/{:#x} eot={:#x}/{:#x} pbg={:#x}/{:#x} peot={:#x}/{:#x}\n",
                submission_id,
                payload.background.usc,
                payload.background.resource_spec,
                payload.end_of_tile.usc,
                payload.end_of_tile.resource_spec,
                payload.partial_background.usc,
                payload.partial_background.resource_spec,
                payload.partial_end_of_tile.usc,
                payload.partial_end_of_tile.resource_spec,
            );
        }
        if crate::util::g17p_verbose() {
            pr_info!(
                "G17P render uapi[{}]: vhelper={:#x}/{:#x}/{:#x} fhelper={:#x}/{:#x}/{:#x}\n",
                submission_id,
                payload.vertex_helper.binary,
                payload.vertex_helper.config,
                payload.vertex_helper.data,
                payload.fragment_helper.binary,
                payload.fragment_helper.config,
                payload.fragment_helper.data,
            );
        }
    }

    fn check_firmware_fence(fence: &Fence) -> Result {
        // Publication readiness is supplied by the scheduler fence. A pending
        // hardware fence is valid: the native dependency carries that wait.
        // SAFETY: the owned Fence keeps this pointer alive for the status read.
        let status = unsafe { bindings::dma_fence_get_status(fence.raw()) };
        if status < 0 { return Err(Error::from_errno(status)); }
        Ok(())
    }

    fn wait_fence(fence: &Fence) -> Result {
        // SAFETY: `fence.raw()` remains valid for both calls. The completed
        // fence status must be consumed as well as its signal bit: an error
        // fence counts as signalled, but it does not satisfy a GPU dependency.
        let ret = unsafe {
            bindings::dma_fence_wait_timeout(
                fence.raw(),
                true,
                kernel::task::MAX_SCHEDULE_TIMEOUT,
            )
        };
        if ret < 0 {
            return Err(Error::from_errno(ret as i32));
        }
        if ret == 0 {
            return Err(ETIMEDOUT);
        }
        let status = unsafe { bindings::dma_fence_get_status(fence.raw()) };
        if status < 0 {
            return Err(Error::from_errno(status));
        }
        Ok(())
    }

    fn timestamp_destination(
        objects: core::pin::Pin<&xarray::XArray<KBox<file::Object>>>,
        timestamp: g17_uapi::UapiTimestamp,
    ) -> Result<Option<TimestampDestination>> {
        if timestamp.handle == 0 {
            return Ok(None);
        }
        let guard = objects.lock();
        let object = guard
            .get(timestamp.handle.try_into()?)
            .ok_or(ENOENT)?
            .clone();
        core::mem::drop(guard);
        match object {
            file::Object::TimestampBuffer(mapping) => {
                let offset: usize = timestamp.offset.try_into()?;
                let end = offset.checked_add(8).ok_or(EINVAL)?;
                if end > mapping.size() {
                    return Err(ERANGE);
                }
                Ok(Some(TimestampDestination { mapping, offset }))
            }
        }
    }

    /// v94 `g17p_mem_path` bit2: reuse a retired page of this queue when one
    /// is pooled; otherwise allocate exactly as before. The returned pool is
    /// where the completion gives the page back (None: dropped as before).
    fn compute_timestamps(&self) -> Result<(mmu::KernelMapping, Option<Arc<g17_job::G17PTimestampPool>>)> {
        if !crate::g17_mem_path::enabled(crate::g17_mem_path::COMPUTE_TS_POOL) {
            return Ok((self.owned_compute_timestamps()?, None));
        }
        if let Some(page) = self.ts_pool.take() {
            crate::g17_mem_path::count(&crate::g17_mem_path::TS_REUSED, 1);
            return Ok((page, Some(self.ts_pool.clone())));
        }
        Ok((self.owned_compute_timestamps()?, Some(self.ts_pool.clone())))
    }

    /// `g17p_tile_backstop`: the render's attachment footprint -- its
    /// validated fragment attachment spans in this VM, plus the object ranges
    /// they reach through shareable-object bindings (D). Out of line: the
    /// footprint is built in this frame, not the submit path's.
    #[inline(never)]
    fn tile_footprint(&self, attachments: &g17_uapi::UapiAttachmentList)
        -> Result<Option<Arc<crate::g17_tile_backstop::Footprint>>>
    {
        use crate::g17_tile_backstop as backstop;
        if backstop::mask() & backstop::DEFAULT_MASK == 0 || attachments.as_slice().is_empty() {
            return Ok(None);
        }
        let mut footprint = backstop::Footprint::new(self.vm.id());
        for attachment in attachments.as_slice() {
            footprint.push_vm_span(attachment.address, attachment.address.saturating_add(attachment.size));
            self.vm.shared_footprint_spans(attachment.address, attachment.size, &mut footprint);
        }
        if footprint.object_spans() != 0 {
            backstop::count(&backstop::OBJECT_FOOTPRINTS);
        }
        if footprint.truncated() {
            backstop::count(&backstop::TRUNCATED);
        }
        Ok(Some(Arc::new(footprint, GFP_KERNEL)?))
    }

    fn owned_compute_timestamps(&self) -> Result<mmu::KernelMapping> {
        // Firmware writes both completion words. A WB CPU alias can keep the
        // polling core on stale zeroes even after the GPU has completed.
        crate::g17_mem_path::count(&crate::g17_mem_path::TS_ALLOCATED, 1);
        let mut object = gem::new_kernel_object_wc(&self.dev, mmu::UAT_PGSZ)?;
        object.vmap()?.memset(0);
        object.map_into_range(
            &self.vm,
            T8140_TIMESTAMP_ALIAS_RANGE.clone(),
            mmu::UAT_PGSZ as u64,
            mmu::PROT_GPU_FW_SHARED_RW,
            false,
        )
    }

    fn install_output_fences(mut syncs: impl Iterator<Item = file::SyncItem>, fence: &Fence) {
        for mut sync in syncs.by_ref() {
            if let Some(chain) = sync.chain_fence.take() {
                sync.syncobj.add_point(chain, fence, sync.timeline_value);
            } else {
                sync.syncobj.replace_fence(Some(fence));
            }
        }
    }
}

/// Diagnostic byte dumps for submitted command streams.
impl G17PQueue {
    /// Compare a TTBAT entry against a page-table root.
    ///
    /// TTB0 carries the root physical address plus a valid bit and the
    /// context's ASID, so only the address field is comparable.  The mask is
    /// the firmware's own declared level mask (`g17_initdata::UAT_LEVEL_PHYS_MASK`).
    const TTB_ADDR_MASK: u64 = 0x0000_03ff_ffff_c000;

    fn log_hex(label: &str, base: u64, bytes: &[u8]) {
        for (index, chunk) in bytes.chunks(16).enumerate() {
            pr_info!(
                "G17P vm-audit: {} {:#x} {:02x?}\n",
                label,
                base.wrapping_add((index * 16) as u64),
                chunk,
            );
        }
    }

}

/// Publish the scheduled job's fence, or a signalled error fence on failure.
/// Once output syncobjs are resolved, every exit must give waiters a fence.
struct G17POutputFencePublication<'a> {
    fence: &'a UserFence<G17PSubmissionFence>,
    outputs: KVec<file::SyncItem>,
    failure: Error,
    succeeded: bool,
    aggregate: Option<Arc<G17PSubmitCompletion>>,
}

impl G17POutputFencePublication<'_> {
    /// Preserve the actual submit error in the fence userspace will wait on.
    fn failed(&mut self, error: Error) {
        self.failure = mmu::context_refusal_to_uapi(error);
    }

    /// Hand the output syncobjs to an asynchronous completion fence.
    ///
    /// An asynchronously scheduled submission has *not* run when the ioctl
    /// returns, so neither of this guard's own answers is correct for it:
    /// signalling success would tell userspace the GPU had finished work it
    /// has not started, and the ECANCELED failure fence would claim work that
    /// is queued and will run was thrown away. Both read as "signalled" to a
    /// DRM syncobj wait, so either one lets the client observe the destination
    /// buffer before the job that fills it has executed.
    ///
    /// Install the job's real completion fence instead, and disarm the guard:
    /// from here the async job owns the promise made to userspace.
    fn publish_async(&mut self, fence: &Fence) {
        G17PQueue::install_output_fences(self.outputs.drain(..), fence);
        self.succeeded = true;
    }
}

impl Drop for G17POutputFencePublication<'_> {
    fn drop(&mut self) {
        if let Some(aggregate) = self.aggregate.take() {
            aggregate.finish_enqueue(if self.succeeded { Ok(()) } else { Err(self.failure) });
        }
        if !self.succeeded {
            self.fence.set_error(self.failure);
        }
        let fence = Fence::from_fence(self.fence);
        G17PQueue::install_output_fences(self.outputs.drain(..), &fence);
        self.fence.signal();
    }
}

impl queue::Queue for G17PQueue {
    fn context_unbound_refusal(&self, submission_id: u64) -> bool {
        self.context_unbound_refusal == Some(submission_id)
    }

    /// Every synchronous submission funnels through here so that no exit path
    /// can leave the client's own outstanding work unaccounted for.
    ///
    /// `submit_guarded` already promises an output fence for THIS ioctl on
    /// every path. What it cannot promise is anything about work this client
    /// handed the hardware earlier: once a failure is terminal the client's
    /// device is lost, Mesa tears its context down, and the jobs already in
    /// flight will never complete or signal. Settle them here.
    fn submit(
        &mut self,
        submission_id: u64,
        syncs: KVec<file::SyncItem>,
        in_sync_count: usize,
        cmdbuf_raw: &[u8],
        objects: core::pin::Pin<&xarray::XArray<KBox<file::Object>>>,
    ) -> Result {
        self.context_unbound_refusal = None;
        match self.submit_guarded(submission_id, syncs, in_sync_count, cmdbuf_raw, objects) {
            Err(error) if mmu::is_context_unbound(error) => {
                self.context_unbound_refusal = Some(submission_id);
                Err(ENOENT)
            }
            result => result,
        }
    }
}

impl G17PQueue {
    /// Settle a client whose submission failed terminally.
    ///
    /// Only failures that mean "this work will never run on this device" count.
    /// Contention and rejection do not: an EBUSY from the render lease, an
    /// interrupted wait, or a malformed command buffer all leave a perfectly
    /// healthy VM, and recording a sticky error for one of those would retire a
    /// client that merely lost a frame.
    fn note_submission_failure(&mut self, submission_id: u64, error: Error) {
        let lost = error == EIO
            || error == ETIMEDOUT
            || error == ENODEV
            || self.vm.status().get() != 0;
        if !lost {
            return;
        }
        // Sticky first: a syncobj waiter woken by an error fence asks the VM
        // status why, and must not find a clean one.
        self.vm.status().record(error.to_errno());
        let failed = self
            .runtime
            .mutex()
            .lock()
            .fail_outstanding_jobs_for_vm(self.vm.status(), error);
        if failed != 0 {
            dev_err!(
                self.dev.as_ref(),
                "G17P submit: submission {} lost the device ({:?}); failed {} outstanding job(s) on vm {}\n",
                submission_id,
                error,
                failed,
                self.vm.id(),
            );
        }
    }

    /// Phase one of `submit_guarded`: VM status, mapping pin, parse and
    /// translate every hardware command, and one VM job guard per command.
    /// Kept out of line (never inlined) so its large temporaries are not on
    /// the stack while the commands are published.
    #[inline(never)]
    fn translate_submission(
        &mut self,
        submission_id: u64,
        cmdbuf_raw: &[u8],
    ) -> Result<(KVec<G17PPendingCommand>, KVec<mmu::T8140VmJobGuard>)> {
        // A VM that has already faulted rejects every later submission. That
        // check used to run before any of the above, so a client whose VM died
        // got EIO with no fence published at all and then waited forever on the
        // syncobj it had just handed us. Keep the rejection, but make it
        // publish like every other failure.
        if self.vm.status().get() != 0 {
            return Err(Self::stage_fail(submission_id, "vm-status", EIO));
        }
        // Pin every userspace mapping before translating raw GPU addresses.
        // GEM-handle close and VM teardown defer their unmaps behind this
        // guard; a successfully published render/compute packet inherits it.
        let parse_guard = self.vm.retain_first_t8140_job()?;
        if self.vm.t8140_job_context_view0_unbound() {
            self.context_unbound_refusal = Some(submission_id);
            pr_info!("G17P submit: context-unbound refusal submission {} vm={} view=0 errno=-2\n",
                submission_id, self.vm.id());
            return Err(ENOENT);
        }
        let window = g17_uapi::QueueUscWindow {
            base: self.usc_exec_base,
            user_start: self.user_range.start,
            user_end: self.user_range.end,
        };
        let mut parser = g17_uapi::UapiCommandParser::new(cmdbuf_raw);
        // Freeze logical bases for every historical-prefix check in this
        // ioctl. Translation completes before the first job is published.
        let render_base = self.render_sequence;
        let compute_base = self.compute_sequence;
        let mut render_preceding = 0u64;
        let mut compute_preceding = 0u64;
        // v91 PENDING_EXACT: 64 x ~1.2 KB preallocated is a kmalloc_large
        // (page allocator) round trip on every submission. Grow to the
        // commands present; the 64-command bound below is unchanged.
        let pending_exact = crate::g17_host_path::enabled(crate::g17_host_path::PENDING_EXACT);
        let pending_limit = file::MAX_COMMANDS_PER_SUBMISSION as usize;
        let mut pending = KVec::with_capacity(
            if pending_exact { 1 } else { pending_limit },
            GFP_KERNEL,
        )?;
        let mut proof_outcome = G17PProofOutcome::default();
        self.e2e_fine[2] = crate::g17_e2e::fine();
        while let Some(command) = parser
            .next_hardware()
            .map_err(|error| Self::reject_uapi(submission_id, "parse", error))?
        {
            let command = match command {
                g17_uapi::ParsedHardwareCommand::Render {
                    header,
                    payload,
                    vertex_attachments,
                    fragment_attachments,
                    ..
                } => {
                    Self::log_render_payload(submission_id, &payload);
                    // Dump Mesa's VDM control stream. The tiler provably
                    // FETCHES it -- pipe-vdm-stream reads back the encoded
                    // pointer on pipes 0 and 2 -- and then bins 60 non-zero
                    // bytes into a 1.28 MB tile buffer, whose content is an
                    // empty bounding box (0x7fc00000 NaN and 0x7fffffff
                    // extremes). So the stream is reached and executed and
                    // yields zero primitives. Its first bytes say whether it
                    // contains a draw at all.
                    if module_parameters::g17p_vdm_dump.value() != 0 {
                        // 1 KiB, not 128 bytes: the G17P native bind tail
                        // (hk_g17p_emit_native_bind_tail) emits its bind0
                        // structure INLINE into this stream and reaches past
                        // offset 0x340, so a 128-byte window stops before the
                        // PPP/graphics-core state that decides whether
                        // primitives survive.
                        let mut head = [0u8; 1024];
                        match self.vm.read_bytes(payload.vdm_base, &mut head) {
                            Ok(()) => Self::log_hex("vdm", payload.vdm_base, &head),
                            Err(error) => pr_info!(
                                "G17P vdm dump: read {:#x} failed ({:?})\n",
                                payload.vdm_base,
                                error
                            ),
                        }
                        // g17p_vm_probe_va was compute-only. The render path
                        // needs it more: the VDM stream embeds the vertex
                        // buffer address, and NaN bounds in the tile buffer
                        // point at what the shader READS rather than at the
                        // stream itself.
                        let probe = module_parameters::g17p_vm_probe_va.value();
                        if probe != 0 {
                            let mut buf = [0u8; 64];
                            match self.vm.read_bytes(probe, &mut buf) {
                                Ok(()) => Self::log_hex("vdm-probe", probe, &buf),
                                Err(error) => pr_info!(
                                    "G17P vdm probe: read {:#x} failed ({:?})\n",
                                    probe,
                                    error
                                ),
                            }
                        }
                    }
                    self.e2e_fine[3] = crate::g17_e2e::fine();
                    let internal = g17_uapi::RenderInternalState {
                                parameters: {
                                    let mut p =
                                        g17_render::G17pRenderParameters::source_rsrc8();
                                    if crate::g17_config::RENDER_BG_PREFIX != 0 {
                                        p.bg_resource_prefix = 0xffff_8000_0000_0000;
                                    }
                                    p.native_pm_bytes =
                                        crate::g17_resources::g17p_render_native_pm_bytes();
                                    // Diagnostic only: isolate the fragment PTC
                                    // high-word comparison already encoded by
                                    // g17_render. The captured count is not a
                                    // general workload-sizing policy.
                                    p.native_ta_registers =
                                        crate::g17_config::RENDER_NATIVE_REGS
                                        | if cfg!(g17p_fragment_ptc_probe) { 8 } else { 0 }
                                        | if crate::g17_timing_buffer::diag_knob(
                                            crate::g17_timing_buffer::DIAG_TA_PTC_NATIVE) { 2 } else { 0 }
                                        | if crate::g17_timing_buffer::diag_knob(
                                            crate::g17_timing_buffer::DIAG_FRG_PTC_NATIVE) { 8 } else { 0 };
                                    p.gpc_perf_state_map = self.gpc_perf_state_map;
                                    p.gpc_perf_state_map_low = self.gpc_perf_state_map_low;
                                    p.gpc_perf_state_control = self.gpc_perf_state_control;
                                    p
                                },
                            };
                    let mut translated = g17p_translate_with_proof(
                        &self.vm, self.user_range.clone(), &mut self.validation_proof,
                        &mut proof_outcome,
                        |space| g17_uapi::translate_render_command(
                            payload,
                            vertex_attachments,
                            fragment_attachments,
                            window,
                            space,
                            internal,
                        ),
                    )
                        .map_err(|error| Self::reject_uapi(submission_id, "render", error))?;
                    self.e2e_fine[4] = crate::g17_e2e::fine();
                    translated.render_barrier = header.render_barrier;
                    translated.compute_barrier = header.compute_barrier;
                    translated.compute_wait_through = crate::g17_submission::g17p_render_order_with_prior(
                        compute_base, compute_preceding, header.compute_barrier, payload.prior_cdm,
                    ).map_err(|error| Self::stage_fail(submission_id, "render-compute-index",
                        if error == crate::g17_submission::SubmissionError::PriorBarrierFuture {
                            EINVAL
                        } else { EOVERFLOW }))?
                        .wait_through;
                    translated.render_order = crate::g17_submission::g17p_render_order_with_prior(
                        render_base, render_preceding, header.render_barrier, payload.prior_vdm,
                    ).map_err(|error| Self::stage_fail(submission_id, "render-index",
                        if error == crate::g17_submission::SubmissionError::PriorBarrierFuture {
                            EINVAL
                        } else { EOVERFLOW }))?;
                    if crate::g17_e2e::armed() {
                        crate::g17_e2e::record(crate::g17_e2e::K_IOCTL_PARSED, 0, submission_id,
                            [render_preceding | compute_preceding << 32,
                             u64::from(header.render_barrier) | u64::from(header.compute_barrier) << 32,
                             translated.render_order.wait_through.unwrap_or(u64::MAX),
                             translated.compute_wait_through.unwrap_or(u64::MAX),
                             translated.render_order.sequence]);
                    }
                    render_preceding += 1;
                    G17PPendingCommand::Render(translated)
                }
                g17_uapi::ParsedHardwareCommand::Compute {
                    payload, attachments, header, ..
                } => {
                    let mut translated = g17p_translate_with_proof(
                        &self.vm, self.user_range.clone(), &mut self.validation_proof,
                        &mut proof_outcome,
                        |space| g17_uapi::translate_compute_command(
                            payload, attachments, window, space,
                        ),
                    ).map_err(|error| Self::reject_uapi(submission_id, "compute", error))?;
                    let compute_order = crate::g17_submission::g17p_render_order_with_prior(
                        compute_base, compute_preceding, header.compute_barrier, payload.prior_cdm,
                    ).map_err(|error| Self::stage_fail(submission_id, "compute-index",
                        if error == crate::g17_submission::SubmissionError::PriorBarrierFuture {
                            EINVAL
                        } else { EOVERFLOW }))?;
                    translated.compute_sequence = compute_order.sequence;
                    translated.compute_wait_through = compute_order.wait_through;
                    compute_preceding += 1;
                    translated.render_wait_through = crate::g17_submission::g17p_render_order_with_prior(
                        render_base, render_preceding, header.render_barrier, payload.prior_vdm,
                    ).map_err(|error| Self::stage_fail(submission_id, "compute-render-index",
                        if error == crate::g17_submission::SubmissionError::PriorBarrierFuture {
                            EINVAL
                        } else { EOVERFLOW }))?
                        .wait_through;
                    G17PPendingCommand::Compute(translated)
                },
            };
            if pending_exact && pending.len() == pending.capacity() && pending.len() < pending_limit {
                pending.reserve(1, GFP_KERNEL).map_err(|_| ENOMEM)?;
            }
            pending.push_within_capacity(command).map_err(|_| ENOMEM)?;
        }
        parser
            .finish()
            .map_err(|error| Self::reject_uapi(submission_id, "finish", error))?;
        if pending.is_empty() {
            return Err(Self::stage_fail(
                submission_id,
                "no-hardware-command",
                EINVAL,
            ));
        }
        if crate::util::g17p_verbose() {
            let kind = if pending.len() == 1 { "single" } else { "batch" };
            pr_info!(
                "G17P submit: submission {} translated as {}\n",
                submission_id,
                kind
            );
        }

        // Keep every mapping pinned before publishing the first job.  The
        // final completion would be sufficient for a totally ordered queue,
        // but one guard per command also preserves exact failure quarantine
        // ownership if a later command cannot be published.
        let command_count = pending.len();
        let mut vm_jobs = KVec::with_capacity(command_count, GFP_KERNEL)?;
        vm_jobs
            .push_within_capacity(parse_guard)
            .map_err(|_| ENOMEM)?;
        while vm_jobs.len() < command_count {
            let guard = self.vm.retain_t8140_job()?;
            vm_jobs.push_within_capacity(guard).map_err(|_| ENOMEM)?;
        }
        self.e2e_fine[5] = crate::g17_e2e::fine();
        self.e2e_fine[9] = u64::from(proof_outcome.requests)
            | u64::from(proof_outcome.hit) << 8 | u64::from(proof_outcome.batched) << 9
            | u64::from(proof_outcome.fallback) << 10 | (pending.capacity() as u64) << 16;
        Ok((pending, vm_jobs))
    }

    fn submit_guarded(
        &mut self,
        submission_id: u64,
        mut syncs: KVec<file::SyncItem>,
        in_sync_count: usize,
        cmdbuf_raw: &[u8],
        objects: core::pin::Pin<&xarray::XArray<KBox<file::Object>>>,
    ) -> Result {
        // Keep diagnostic timing entirely out of the production hot path.
        // The parameter has no runtime permissions, so this branch is stable
        // for the lifetime of the module.
        let perf_enabled = module_parameters::g17p_perf_window.value() != 0;
        let mut entered = if perf_enabled {
            Some(Instant::<Monotonic>::now())
        } else {
            None
        };
        if crate::util::g17p_verbose() {
            pr_info!(
                "G17P submit: ENTER submission {} syncs={} in_sync={} cmdbuf_bytes={}\n",
                submission_id,
                syncs.len(),
                in_sync_count,
                cmdbuf_raw.len()
            );
        }
        self.e2e_fine = [0; 12];
        self.e2e_fine[0] = crate::g17_e2e::fine();
        Self::log_cmdbuf_headers(submission_id, cmdbuf_raw);
        if in_sync_count > syncs.len() {
            return Err(Self::stage_fail(submission_id, "sync-count", EINVAL));
        }
        // Split the output syncobjs off immediately and hand them to a
        // publication guard, so every failure from here on still publishes a
        // fence. Arming this only after the queue lease was acquired left the
        // whole class of "another client owns the render slot" failures
        // publishing nothing, and a compositor that hit one waited on an
        // unsignalled syncobj forever instead of losing a frame.
        let early_output_fence = new_independent_submission_fence(&self.fence_contexts)
            .map_err(|error| Self::stage_fail(submission_id, "new-fence", error))?;
        let output_count = syncs.len() - in_sync_count;
        let deferred_outputs: KVec<file::SyncItem> =
            match KVec::with_capacity(output_count, GFP_KERNEL) {
                Ok(outputs) => outputs,
                Err(_) => {
                    // Output syncobjs are already resolved and userspace may
                    // wait on them even when this bookkeeping allocation
                    // fails. Publish the fence directly from the original
                    // vector before returning the allocation error.
                    early_output_fence.set_error(ENOMEM);
                    let fence = Fence::from_fence(&early_output_fence);
                    Self::install_output_fences(syncs.drain(in_sync_count..), &fence);
                    early_output_fence.signal();
                    return Err(Self::stage_fail(
                        submission_id, "output-sync-list", ENOMEM,
                    ));
                }
            };
        let mut publication = G17POutputFencePublication {
            fence: &early_output_fence,
            outputs: deferred_outputs,
            failure: ECANCELED,
            succeeded: false,
            aggregate: None,
        };
        while syncs.len() > in_sync_count {
            // Removing the first output keeps timeline points in userspace
            // order and, unlike a Drain iterator, leaves every later output
            // reachable if the supposedly infallible capacity invariant is
            // ever violated.
            let item = match syncs.remove(in_sync_count) {
                Ok(item) => item,
                Err(_) => {
                    let failure = Self::stage_fail(submission_id, "output-sync-index", EIO);
                    publication.failed(failure);
                    let fence = Fence::from_fence(&early_output_fence);
                    Self::install_output_fences(syncs.drain(in_sync_count..), &fence);
                    self.note_submission_failure(submission_id, failure);
                    return Err(failure);
                }
            };
            if let Err(error) = publication.outputs.push_within_capacity(item) {
                let failure = Self::stage_fail(submission_id, "output-sync-capacity", EIO);
                publication.failed(failure);
                let fence = Fence::from_fence(&early_output_fence);
                Self::install_output_fences(core::iter::once(error.0), &fence);
                Self::install_output_fences(syncs.drain(in_sync_count..), &fence);
                self.note_submission_failure(submission_id, failure);
                return Err(failure);
            }
        }
        self.e2e_fine[1] = crate::g17_e2e::fine();
        // Keep the publication guard alive until the exact result is known.
        // Terminal VM state and outstanding jobs must be settled before an
        // error fence wakes userspace; wrapping the body also makes every `?`
        // and explicit return obey that ordering without duplicating cleanup.
        let result = (|| -> Result {
        // Parse and translate in a separate, never-inlined frame: its parser,
        // parsed command and translated temporaries (~8 KiB) are released
        // before publication, which can run the whole render publication
        // (early path) on this ioctl stack.
        let (pending, mut vm_jobs) = self.translate_submission(submission_id, cmdbuf_raw)?;
        let command_count = pending.len();

        let aggregate = G17PSubmitCompletion::new(
            new_independent_submission_fence(&self.fence_contexts)?,
            self.context.clone(),
            submission_id,
        )?;
        // Export the aggregate before enqueue: partial failure still leaves a
        // fence which covers every already queued command plus the enqueue gate.
        Self::install_output_fences(publication.outputs.drain(..), &aggregate.fence());
        publication.aggregate = Some(aggregate.clone());
        // v93 FW_DEPS: resolve input syncs against this queue's recent ioctls
        // once; every command of the ioctl applies the same plan.
        let plan = if crate::g17_host_path::enabled(crate::g17_host_path::FW_DEPS) {
            Some(self.plan_input_syncs(&syncs)?)
        } else {
            None
        };
        let (render_before, compute_before) = (self.render_sequence, self.compute_sequence);
        let record = crate::g17_host_path::enabled(
            crate::g17_host_path::FW_DEPS | crate::g17_host_path::AGG_FIRST);
        if record {
            match self.ioctl_commands.as_mut() {
                Some(commands) => commands.clear(),
                None => self.ioctl_commands = Some(KVec::new()),
            }
        }
        self.e2e_fine[6] = crate::g17_e2e::fine();

        // Index the commands in place: moving each ~1.1 KiB command out of
        // the vector staged it (and the iterator's Option) on this frame,
        // which is the base of the early-publication stack.
        for index in 0..command_count {
            let last = index + 1 == command_count;
            // Input syncs gate the whole ioctl. Independent commands may now
            // publish in either engine order, so each owns the input fences
            // and checks their errors before acquiring a hardware lease.
            let vm_job = vm_jobs.remove(0).map_err(|_| EIO)?;
            let command_entered = if last { entered.take() } else { None };

            match pending.get(index).ok_or(EIO)? {
                G17PPendingCommand::Render(translated) => self.submit_render_async(
                    submission_id,
                    translated,
                    &syncs,
                    plan.as_ref(),
                    record,
                    objects,
                    G17PSubmitCompletionMember::new(aggregate.clone()),
                    vm_job,
                    command_entered,
                )?,
                G17PPendingCommand::Compute(translated) => self.submit_compute_async(
                    submission_id,
                    translated,
                    &syncs,
                    plan.as_ref(),
                    record,
                    objects,
                    G17PSubmitCompletionMember::new(aggregate.clone()),
                    vm_job,
                    command_entered,
                )?,
            }
        }
        if record {
            self.note_recent_submission(aggregate.fence(), render_before, compute_before);
        }
        publication.publish_async(&aggregate.fence());
        Ok(())
        })();
        if let Err(error) = result {
            self.note_submission_failure(submission_id, error);
            publication.failed(error);
        }
        result
    }
}
