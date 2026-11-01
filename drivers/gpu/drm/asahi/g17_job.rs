// SPDX-License-Identifier: GPL-2.0-only OR MIT

//! Owned command packets and DRM scheduler boundary for independent G17P queues.
//!
//! This component is deliberately separate from the qualified synchronous
//! frontend. Enabling it requires a backend with one retained hardware queue
//! owner per DRM queue and completion-driven retirement; the current shared
//! QID0/1/4 runtime does not satisfy that contract.

use core::sync::atomic::{fence, AtomicU32, Ordering};
use kernel::{
    c_str,
    dma_fence::{Fence, RawDmaFence, UserFence},
    drm::sched,
    new_mutex,
    prelude::*,
    sync::{Arc, Mutex},
};
use crate::{driver::AsahiDevice, g17_uapi, mmu};

// Fault-injection build only; this deliberately makes deep heavy rendering
// exceed the watchdog. It is never a performance setting or default policy.
const SCHEDULER_TIMEOUT_MS: kernel::time::Msecs =
    if cfg!(g17p_short_timeout_probe) { 100 } else { 2000 };

/// Host-sealed bytes in a job-private command allocation. The builder consumes
/// its owning mapping, copies the encoded command, and exposes only its GPU
/// address afterward. Another job cannot restage this wrapper's allocation.
/// Additional address-space aliases of the same job-private GEM belong in the
/// packet's retained mappings, never in another queue's mutable command state.
pub(crate) struct G17PSealedCommand {
    mapping: mmu::KernelMapping,
    encoded_size: usize,
}

impl G17PSealedCommand {
    pub(crate) fn new(mapping: mmu::KernelMapping, encoded: &[u8]) -> Result<Self> {
        if encoded.is_empty() || encoded.len() > mapping.size() { return Err(EINVAL); }
        mapping.with_cpu_bytes(|bytes| {
            bytes.fill(0);
            bytes[..encoded.len()].copy_from_slice(encoded);
            Ok(())
        })?;
        fence(Ordering::Release);
        Ok(Self { mapping, encoded_size: encoded.len() })
    }

    pub(crate) fn gpu_address(&self) -> u64 { self.mapping.iova() }
    pub(crate) fn encoded_size(&self) -> usize { self.encoded_size }
}

pub(crate) struct TimestampDestination {
    pub(crate) mapping: Arc<mmu::KernelMapping>,
    pub(crate) offset: usize,
}

/// Rate of the command completion timestamps firmware records: its 24 MHz
/// command-time counter, the architectural counter scaled to 24 MHz.
pub(crate) const G17P_COMMAND_TIMESTAMP_HZ: u64 = 24_000_000;

impl TimestampDestination {
    /// Publish one firmware completion timestamp. UAPI timestamps are
    /// nanoseconds, the unit `DRM_IOCTL_ASAHI_GET_TIME` reports.
    pub(crate) fn write(&self, ticks: u64) -> Result {
        const NSEC_PER_SEC: u64 = 1_000_000_000;
        let timestamp = ticks / G17P_COMMAND_TIMESTAMP_HZ * NSEC_PER_SEC
            + ticks % G17P_COMMAND_TIMESTAMP_HZ * NSEC_PER_SEC / G17P_COMMAND_TIMESTAMP_HZ;
        self.mapping.with_cpu_bytes(|bytes| {
            let end = self.offset.checked_add(8).ok_or(EINVAL)?;
            bytes.get_mut(self.offset..end).ok_or(EINVAL)?
                .copy_from_slice(&timestamp.to_le_bytes());
            Ok(())
        })?;
        fence(Ordering::Release);
        Ok(())
    }
}

/// v94 `g17p_mem_path` bit2: retired compute completion-timestamp pages of
/// one queue. Only the two completion words are ever written by firmware;
/// they are cleared when a page is returned, so a reused page is exactly as
/// the fresh allocation left it (zeroed words, same VM, driver-owned alias).
#[pin_data]
pub(crate) struct G17PTimestampPool {
    #[pin]
    pages: Mutex<KVec<mmu::KernelMapping>>,
}

/// More than the per-queue compute in-flight bound; the rest are dropped.
const G17P_TIMESTAMP_POOL_MAX: usize =
    2 * crate::g17_queue_limits::MAX_COMPUTE_IN_FLIGHT_PER_QUEUE as usize;

impl G17PTimestampPool {
    pub(crate) fn new() -> Result<Arc<Self>> {
        Arc::pin_init(pin_init!(Self {
            pages <- new_mutex!(KVec::new(), "G17PTimestampPool::pages"),
        }), GFP_KERNEL)
    }

    pub(crate) fn take(&self) -> Option<mmu::KernelMapping> {
        self.pages.lock().pop()
    }

    fn put(&self, mapping: mmu::KernelMapping) {
        let cleared = mapping.with_cpu_bytes(|bytes| {
            bytes.get_mut(..16).ok_or(EINVAL)?.fill(0);
            Ok(())
        });
        if cleared.is_err() { return; }
        fence(Ordering::Release);
        // Unmap a rejected page outside the pool lock.
        let rejected = {
            let mut pages = self.pages.lock();
            if pages.len() >= G17P_TIMESTAMP_POOL_MAX {
                Some(mapping)
            } else {
                // A failed push drops (unmaps) the page, as without the pool.
                let _ = pages.push(mapping, GFP_KERNEL);
                None
            }
        };
        drop(rejected);
    }
}

pub(crate) struct G17PJobCompletion {
    pub(crate) status: Arc<crate::g17_status::VmStatus>,
    fence: UserFence<crate::g17_drm::G17PSubmissionFence>,
    /// Taken exactly once, by `Drop`, to return it to `ts_pool`.
    timestamps: core::mem::ManuallyDrop<mmu::KernelMapping>,
    /// v94: set when the page came from (and returns to) the queue's pool.
    ts_pool: Option<Arc<G17PTimestampPool>>,
    destinations: [Option<TimestampDestination>; 2],
    vm_job: Arc<Mutex<Option<mmu::T8140VmJobGuard>>>,
    signalled: AtomicU32,
    aggregate_member: crate::g17_drm::G17PSubmitCompletionMember,
    /// `g17p_recovery_isolation`: class assigned while servicing a recovery.
    recovery_class: core::sync::atomic::AtomicU8,
    /// iso4 REPLAY: publications beyond the first, and whether the scheduler
    /// job may renew one timeout for the latest replay.
    replays: core::sync::atomic::AtomicU8,
    replay_timeout_credit: core::sync::atomic::AtomicU8,
    /// iso4: a render took this kick's native token as its parent.
    native_consumer: core::sync::atomic::AtomicBool,
}

impl G17PJobCompletion {
    /// Recovery isolation: record this command's class for the recovery
    /// being serviced. A later recovery may reclassify it.
    pub(crate) fn set_recovery_class(&self, class: u8) {
        self.recovery_class.store(class, Ordering::Release);
    }

    /// Cancelled by another client's recovery (or graced).
    pub(crate) fn recovery_spared(&self) -> bool {
        self.recovery_class.load(Ordering::Acquire) == crate::g17_recovery_isolation::CLASS_SPARED
    }

    pub(crate) fn work_state(&self) -> Result<Arc<crate::g17_resources::G17PWorkStateLease>> {
        self.aggregate_member.work_state()
    }

    pub(crate) fn new(
        fence: UserFence<crate::g17_drm::G17PSubmissionFence>,
        timestamps: mmu::KernelMapping,
        ts_pool: Option<Arc<G17PTimestampPool>>,
        destinations: [Option<TimestampDestination>; 2],
        status: Arc<crate::g17_status::VmStatus>,
        vm_job: mmu::T8140VmJobGuard,
        aggregate_member: crate::g17_drm::G17PSubmitCompletionMember,
    ) -> Result<Self> {
        let vm_job = Arc::pin_init(
            new_mutex!(Some(vm_job), "G17PJobCompletion::vm_job"),
            GFP_KERNEL,
        )?;
        Ok(Self {
            status,
            fence,
            timestamps: core::mem::ManuallyDrop::new(timestamps),
            ts_pool,
            destinations,
            vm_job,
            signalled: AtomicU32::new(0),
            aggregate_member,
            recovery_class: core::sync::atomic::AtomicU8::new(0),
            replays: core::sync::atomic::AtomicU8::new(0),
            replay_timeout_credit: core::sync::atomic::AtomicU8::new(0),
            native_consumer: core::sync::atomic::AtomicBool::new(false),
        })
    }

    pub(crate) fn gpu_address(&self) -> u64 { self.timestamps.iova() }
    pub(crate) fn fence(&self) -> Fence { Fence::from_fence(&self.fence) }

    /// Call only while firmware is halted: these words may otherwise be
    /// changing as the command starts or retires.
    pub(crate) fn halted_timestamp_snapshot(&self) -> Result<[u64; 2]> {
        self.timestamps.with_cpu_bytes(|bytes| {
            let raw = bytes.get(..16).ok_or(EINVAL)?;
            Ok([
                u64::from_le_bytes(raw[..8].try_into().map_err(|_| EINVAL)?),
                u64::from_le_bytes(raw[8..].try_into().map_err(|_| EINVAL)?),
            ])
        })
    }

    /// Recovery isolation REPLAY: this command's own completion words, the
    /// evidence of whether firmware retired it without executing. Firmware
    /// retires a cancelled kick by stamping start == end (the zero-duration
    /// shape `g17p_zero_duration` documents), and a kick it never reached
    /// leaves both at the zeroed allocation value. Any start != end means the
    /// shader ran, and running it again could repeat side effects. Call only
    /// after the retirement witness: the words can no longer change.
    /// Returns the words for `g17_recovery_isolation::compute_replay_decision`,
    /// or None when the fence is already signalled or the page is unreadable.
    pub(crate) fn replay_witness_words(&self) -> Option<[u64; 2]> {
        if self.signalled.load(Ordering::Acquire) != 0 { return None; }
        self.halted_timestamp_snapshot().ok()
    }

    /// Recovery isolation REPLAY: account one more publication of this
    /// command. False once MAX_REPLAYS are used (the caller then fails it).
    /// Each accepted replay grants one scheduler timeout renewal.
    pub(crate) fn begin_replay(&self) -> bool {
        let accepted = self.replays.fetch_update(Ordering::AcqRel, Ordering::Acquire, |replays|
            crate::g17_recovery_isolation::replay_budget_left(replays).then_some(replays + 1)).is_ok();
        if accepted {
            self.replay_timeout_credit.store(1, Ordering::Release);
        }
        accepted
    }

    pub(crate) fn replay_count(&self) -> u8 { self.replays.load(Ordering::Acquire) }

    /// iso4: record that a render was published with this kick's native
    /// token as its parent. Such a command is never replayed.
    pub(crate) fn note_native_consumer(&self) { self.native_consumer.store(true, Ordering::Release); }
    pub(crate) fn has_native_consumer(&self) -> bool { self.native_consumer.load(Ordering::Acquire) }

    /// Consume the timeout renewal the latest replay granted. Only an
    /// unsignalled command can use it.
    pub(crate) fn take_replay_timeout_credit(&self) -> bool {
        self.signalled.load(Ordering::Acquire) == 0
            && self.replay_timeout_credit.swap(0, Ordering::AcqRel) != 0
    }

    /// Recovery isolation REPLAY: zero the completion words before the same
    /// command is published again, so the replayed kick's timestamps (and the
    /// zero-duration test on it) start exactly as a fresh allocation does.
    pub(crate) fn reset_for_replay(&self) -> Result {
        if self.signalled.load(Ordering::Acquire) != 0 { return Err(ECANCELED); }
        self.timestamps.with_cpu_bytes(|bytes| {
            bytes.get_mut(..16).ok_or(EINVAL)?.fill(0);
            Ok(())
        })?;
        fence(Ordering::SeqCst);
        Ok(())
    }

    /// Wake error waiters while retaining the VM-job pin in the quarantined
    /// packet. Neither an error fence nor retaining an otherwise emptied Arc
    /// proves that firmware has finished dereferencing userspace mappings.
    pub(crate) fn fail_while_owned(&self, error: Error) {
        if self.signalled.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return;
        }
        if let Some(vm_job) = self.vm_job.lock().as_ref() {
            vm_job.quarantine();
        }
        // Recovery isolation: a command another client's recovery cancelled
        // keeps its VM pin (above) until the retirement witness, but does not
        // retire its whole VM.
        if self.recovery_spared() {
            crate::g17_recovery_isolation::count(&crate::g17_recovery_isolation::POISON_SKIPPED);
        } else {
            self.status.record(error.to_errno());
        }
        self.fence.set_error(error);
        self.fence.signal();
        self.aggregate_member.complete(Err(error));
    }

    /// Scheduler teardown cancels the waiter, not firmware ownership. Keep
    /// mappings pinned and reject/defer their mutation until exact retirement,
    /// but do not turn a destroyed queue into a hardware failure of its VM.
    pub(crate) fn cancel_while_owned(&self) {
        if self.signalled.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return;
        }
        if let Some(vm_job) = self.vm_job.lock().as_ref() {
            vm_job.quarantine();
        }
        self.fence.set_error(ECANCELED);
        self.fence.signal();
        self.aggregate_member.complete(Err(ECANCELED));
    }

    /// The exact descriptor, fresh completion, final stamp, ring cursors and
    /// publication epoch have all passed. Cancellation may already have
    /// signalled the fence, so `complete` cannot release this pin itself.
    pub(crate) fn release_after_retirement(&self) {
        drop(self.vm_job.lock().take());
    }

    /// Only for retired work, proven processor stop, or unsubmitted packets.
    pub(crate) fn complete(&self, result: Result<[u64; 2]>) {
        let context_unbound = result.as_ref().err()
            .is_some_and(|error| mmu::is_context_unbound(*error));
        let result = result.map_err(mmu::context_refusal_to_uapi);
        if self.signalled.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return;
        }
        let result = result.and_then(|timestamps| {
            for (destination, timestamp) in self.destinations.iter().zip(timestamps) {
                if let Some(destination) = destination {
                    destination.write(timestamp)?;
                }
            }
            Ok(())
        });
        if let Err(error) = result {
            // Publish sticky failure BEFORE the fence wakes userspace. A DRM
            // syncobj considers an error fence signaled, so waits query this.
            // An unsubmitted job cancelled during scheduler destruction has
            // no hardware failure to publish to sibling queues in this VM.
            if error != ECANCELED && !context_unbound && !self.recovery_spared() {
                self.status.record(error.to_errno());
            }
            self.fence.set_error(error);
        }
        // Match render completion: once this fence wakes userspace, every
        // mapping named by the completed job must already be releasable.
        drop(self.vm_job.lock().take());
        // v93 AGG_FIRST: as for render, the user-visible aggregate first.
        if crate::g17_host_path::enabled(crate::g17_host_path::AGG_FIRST) {
            crate::g17_host_path::count(&crate::g17_host_path::AGG_FIRST_TAKEN);
            self.aggregate_member.complete(result);
            self.fence.signal();
        } else {
            self.fence.signal();
            self.aggregate_member.complete(result);
        }
    }

    /// Release the VM pin of an already-signalled error completion after both
    /// firmware processors stop. `complete` deliberately returns early after
    /// `fail_while_owned`, while DRM sched may retain the packet Arc beyond
    /// that fence; processor stop is the independent ownership witness.
    pub(crate) fn release_after_processor_stop(&self) {
        drop(self.vm_job.lock().take());
    }
}

impl Drop for G17PJobCompletion {
    fn drop(&mut self) {
        // SAFETY: the only take of `timestamps`; `self` is being dropped.
        // The packet owning this completion is released only after exact
        // retirement or a proven processor stop, so firmware no longer writes
        // the page.
        let mapping = unsafe { core::mem::ManuallyDrop::take(&mut self.timestamps) };
        match self.ts_pool.take() {
            Some(pool) => pool.put(mapping),
            None => drop(mapping),
        }
    }
}

/// Everything a prepared compute submission needs remains owned until its
/// hardware fence retires, including when its userspace queue is destroyed.
/// The context can be shared by jobs of one VM; different VMs own different
/// independently rooted leases. Command images are immutable after sealing.
pub(crate) struct G17PComputeJobPacket {
    submission_id: u64,
    hardware_queue_id: u8,
    context: Arc<crate::g17_resources::G17PLogicalQueueContext>,
    command: g17_uapi::TranslatedComputeCommand,
    command_images: KVec<G17PSealedCommand>,
    _retained_mappings: KVec<mmu::KernelMapping>,
    /// DRM sched treats a signalled error fence as dependency-ready. Retain
    /// the same fences until run_job so the backend can consume their status
    /// before publishing any hardware-visible queue state.
    input_fences: KVec<Fence>,
    /// Published predecessors whose native tokens carry the GPU wait. These
    /// retained hardware fences are inspected for failure without waiting.
    firmware_fences: KVec<Fence>,
    completion: Option<G17PJobCompletion>,
}

impl G17PComputeJobPacket {
    pub(crate) fn new(
        submission_id: u64,
        hardware_queue_id: u8,
        context: Arc<crate::g17_resources::G17PLogicalQueueContext>,
        command: g17_uapi::TranslatedComputeCommand,
        command_images: KVec<G17PSealedCommand>,
        retained_mappings: KVec<mmu::KernelMapping>,
        input_fences: KVec<Fence>,
        firmware_fences: KVec<Fence>,
        completion: Option<G17PJobCompletion>,
    ) -> Result<Arc<Self>> {
        // QIDs 0/1 belong to bootstrap render. All other IDs are shared by
        // logical TA/3D/CL channels; compute is not partitioned at QID 4.
        if submission_id == 0 || !(2..128).contains(&hardware_queue_id)
        { return Err(EINVAL); }
        Ok(Arc::new(Self {
            submission_id, hardware_queue_id, context, command, command_images,
            _retained_mappings: retained_mappings, input_fences, firmware_fences, completion,
        }, GFP_KERNEL)?)
    }

    pub(crate) fn submission_id(&self) -> u64 { self.submission_id }
    pub(crate) fn hardware_queue_id(&self) -> u8 { self.hardware_queue_id }
    pub(crate) fn context(&self) -> &Arc<crate::g17_resources::G17PLogicalQueueContext> { &self.context }
    pub(crate) fn command(&self) -> &g17_uapi::TranslatedComputeCommand { &self.command }
    pub(crate) fn command_images(&self) -> &[G17PSealedCommand] { &self.command_images }
    pub(crate) fn input_fences(&self) -> &[Fence] { &self.input_fences }
    pub(crate) fn firmware_fences(&self) -> &[Fence] { &self.firmware_fences }
    pub(crate) fn completion(&self) -> Result<&G17PJobCompletion> { self.completion.as_ref().ok_or(EINVAL) }
}

/// The hardware owner supplies queue-capacity dependencies and nonblocking
/// publication. It must retain `packet` before the first doorbell and until
/// exact completion, or quarantine it through recovery if completion is lost.
/// Publication errors are permitted only before firmware can see the packet.
/// Timeout/cancel must never signal completion and release an active packet
/// until that queue has retired or a proven device-stop boundary is reached.
/// This follows the legacy driver's per-job VmBind/command/fence contract;
/// it is not implemented by the single-graph synchronous G17P runtime.
// Opt-in diagnostic only. Keep renewal bounded so unrelated busy queues
// cannot hide a permanently stuck fragment indefinitely. At most three extra
// scheduler intervals; no claim this cap or interval matches Apple's policy.
fn renew_progress_timeout(previous: &mut Option<(u64, u32)>, renewals: &mut u8,
                          current: Option<(u64, u32)>) -> bool {
    let old = *previous;
    *previous = current;
    match (old, current) {
        (Some((old_epoch, old_count)), Some((epoch, count)))
            if old_epoch == epoch && old_count != count && *renewals < 3 => {
                *renewals += 1;
                true
            }
        _ => false,
    }
}

pub(crate) trait G17PQueueBackend: Send + Sync {
    fn timeout_progress(&self) -> Option<(u64, u32)> { None }
    /// `g17p_recovery_isolation` bit7: classify an expired timeout before
    /// `timed_out`. `renewals` periods were already granted to this job.
    fn victim_timeout(&self, _packet: &G17PComputeJobPacket, _renewals: u8)
        -> crate::g17_recovery_isolation::VictimTimeout
    {
        crate::g17_recovery_isolation::VictimTimeout::Own
    }
    fn prepare(&self, packet: &G17PComputeJobPacket) -> Option<Fence>;
    fn publish(&self, packet: Arc<G17PComputeJobPacket>) -> Result<Fence>;
    fn timed_out(&self, packet: Arc<G17PComputeJobPacket>) -> sched::Status;
    fn cancel(&self, packet: Arc<G17PComputeJobPacket>);
}

pub(crate) struct G17PScheduledJob<B: G17PQueueBackend> {
    backend: Arc<B>,
    packet: Arc<G17PComputeJobPacket>,
    timeout_progress: Option<(u64, u32)>,
    timeout_renewals: u8,
    victim_renewals: u8,
}

impl<B: G17PQueueBackend> sched::JobImpl for G17PScheduledJob<B> {
    const MANAGED_TIMEOUT_RECOVERY: bool = true;
    fn false_timeout(job: &mut sched::Job<Self>) -> bool {
        // iso4 REPLAY: a replayed command was published again after the
        // recovery consumed most of its timeout; give it one fresh period.
        if job.packet.completion().is_ok_and(|completion| completion.take_replay_timeout_credit()) {
            crate::g17_recovery_isolation::count(&crate::g17_recovery_isolation::REPLAY_TIMEOUT_RENEWED);
            pr_info!("G17P recovery isolation: replayed compute submission={} timeout renewed once\n",
                job.packet.submission_id());
            return true;
        }
        // bit7 VICTIM_DEFER: head-of-line blocked behind another VM's older
        // unfinished kick -- wait (bounded) for it instead of taking the blame.
        if job.backend.victim_timeout(&job.packet, job.victim_renewals)
            == crate::g17_recovery_isolation::VictimTimeout::Defer
        {
            job.victim_renewals += 1;
            return true;
        }
        if !cfg!(g17p_progress_timeout_probe) { return false; }
        let current = job.backend.timeout_progress();
        let mut previous = job.timeout_progress;
        let mut renewals = job.timeout_renewals;
        let renew = renew_progress_timeout(&mut previous, &mut renewals, current);
        job.timeout_progress = previous;
        job.timeout_renewals = renewals;
        if renew {
            pr_info!("G17P timeout progress probe: renewal={} snapshot={:?}\n", renewals, current);
        }
        renew
    }

    fn prepare(job: &mut sched::Job<Self>) -> Option<Fence> {
        job.backend.prepare(&job.packet)
    }

    fn run(job: &mut sched::Job<Self>) -> Result<Option<Fence>> {
        if cfg!(g17p_progress_timeout_probe) {
            job.timeout_progress = job.backend.timeout_progress();
        }
        // Return the unsignalled hardware fence immediately. DRM retains the
        // job's immutable packet while other independent queues can publish.
        job.backend.publish(job.packet.clone()).inspect_err(|error| {
            // DRM also signals its finished fence when run_job fails before
            // publication; that failure must be visible through the same API.
            // A publish that never became firmware-visible obeys the same
            // VM-poison policy as `note_submission_failure`: only EIO /
            // ETIMEDOUT / ENODEV mean the device is gone for this VM. EBUSY
            // (contention), ECANCELED and EINVAL leave a healthy VM, and
            // recording them here made the client's very next submission
            // refuse at `stage=vm-status` (measured on Neo 2026-09-26: a
            // Chromium launch was poisoned this way). The fence still
            // carries the exact error to this one submission.
            // A second measured poison at this site: a transient EBUSY from a
            // TVB pre-grow while the render tracker was busy was recorded
            // here and retired the whole client.
            if let Ok(completion) = job.packet.completion() {
                completion.status.record_if_device_loss(error.to_errno());
            }
        }).map(Some)
    }

    fn timed_out(job: &mut sched::Job<Self>) -> sched::Status {
        job.backend.timed_out(job.packet.clone())
    }

    fn cancel(job: &mut sched::Job<Self>) {
        job.backend.cancel(job.packet.clone());
    }
}

/// One scheduler/entity per DRM queue, as on M1/M2. Dependencies are handed to
/// DRM instead of synchronously waited under a hardware lease. Queue hardware
/// capacity is a credit limit plus backend prepare fences, not a global lock.
pub(crate) struct G17PJobScheduler<B: G17PQueueBackend> {
    entity: sched::Entity<G17PScheduledJob<B>>,
    _scheduler: sched::Scheduler<G17PScheduledJob<B>>,
    backend: Arc<B>,
}

/// Everything needed to publish one render after its input and queue-order
/// dependencies retire.  Keeping the immutable command and exact completion
/// in this packet lets the submit ioctl return before a physical render slot
/// is available without giving userspace an early completion fence.
pub(crate) struct G17PRenderJobPacket {
    submission_id: u64,
    command: g17_uapi::TranslatedRenderCommand,
    /// DRM sched treats a signalled error fence as dependency-ready. Retain
    /// the fences so the backend can consume their status before publication.
    input_fences: KVec<Fence>,
    /// As above, distinct from user completion dependencies.
    firmware_fences: KVec<Fence>,
    completion: Arc<crate::g17_drm::G17PRenderCompletion>,
    /// `g17p_tile_backstop`: the attachment footprint recorded while this
    /// render holds its pair lease (None: backstop off or no attachments).
    footprint: Option<Arc<crate::g17_tile_backstop::Footprint>>,
    publication_state: AtomicU32,
    cancel_requested: AtomicU32,
    /// v84 early publication from the submit ioctl: 0 not attempted/declined,
    /// 1 published, 2 failed with `early_errno`. Written once, before the DRM
    /// scheduler job is pushed, so run_job always observes it.
    early_state: AtomicU32,
    early_errno: core::sync::atomic::AtomicI32,
    /// This packet occupies one unit of its backend's scheduler backlog.
    backlog_counted: core::sync::atomic::AtomicBool,
}

impl G17PRenderJobPacket {
    pub(crate) fn new(
        submission_id: u64,
        command: g17_uapi::TranslatedRenderCommand,
        input_fences: KVec<Fence>,
        firmware_fences: KVec<Fence>,
        completion: Arc<crate::g17_drm::G17PRenderCompletion>,
        footprint: Option<Arc<crate::g17_tile_backstop::Footprint>>,
    ) -> Result<Arc<Self>> {
        if submission_id == 0 { return Err(EINVAL); }
        Ok(Arc::new(Self {
            submission_id, command, input_fences, firmware_fences, completion, footprint,
            publication_state: AtomicU32::new(0),
            cancel_requested: AtomicU32::new(0),
            early_state: AtomicU32::new(0),
            early_errno: core::sync::atomic::AtomicI32::new(0),
            backlog_counted: core::sync::atomic::AtomicBool::new(false),
        }, GFP_KERNEL)?)
    }

    /// Record the result of an early (submit-ioctl) publication attempt that
    /// reached `begin_publication`. Declined attempts record nothing.
    pub(crate) fn set_early_outcome(&self, result: Result) {
        match result {
            Ok(()) => self.early_state.store(1, Ordering::Release),
            Err(error) => {
                self.early_errno.store(error.to_errno(), Ordering::Relaxed);
                self.early_state.store(2, Ordering::Release);
            }
        }
    }

    /// `Some` when the submit ioctl already owned this packet's publication.
    pub(crate) fn early_outcome(&self) -> Option<Result> {
        match self.early_state.load(Ordering::Acquire) {
            1 => Some(Ok(())),
            2 => Some(Err(Error::from_errno(self.early_errno.load(Ordering::Relaxed)))),
            _ => None,
        }
    }

    pub(crate) fn mark_backlog_counted(&self) { self.backlog_counted.store(true, Ordering::Release); }
    /// True exactly once for a counted packet.
    pub(crate) fn take_backlog_counted(&self) -> bool { self.backlog_counted.swap(false, Ordering::AcqRel) }

    pub(crate) fn submission_id(&self) -> u64 { self.submission_id }
    pub(crate) fn command(&self) -> &g17_uapi::TranslatedRenderCommand { &self.command }
    pub(crate) fn input_fences(&self) -> &[Fence] { &self.input_fences }
    pub(crate) fn firmware_fences(&self) -> &[Fence] { &self.firmware_fences }
    pub(crate) fn completion(&self) -> &Arc<crate::g17_drm::G17PRenderCompletion> {
        &self.completion
    }
    pub(crate) fn fence(&self) -> Fence { self.completion.fence() }
    pub(crate) fn footprint(&self) -> Option<&Arc<crate::g17_tile_backstop::Footprint>> {
        self.footprint.as_ref()
    }

    /// Claim the only transition from queued to hardware publication.
    pub(crate) fn begin_publication(&self) -> bool {
        self.publication_state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn mark_published(&self) {
        self.publication_state.store(2, Ordering::Release);
    }

    pub(crate) fn is_published(&self) -> bool {
        self.publication_state.load(Ordering::Acquire) == 2
    }

    /// Prevent an unstarted packet from publishing, or ask an in-progress
    /// publisher to stop at its next pre-doorbell cancellation point.
    pub(crate) fn request_cancel(&self) -> bool {
        self.cancel_requested.store(1, Ordering::Release);
        self.publication_state
            .compare_exchange(0, 3, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn cancel_requested(&self) -> bool {
        self.cancel_requested.load(Ordering::Acquire) != 0
    }
}

pub(crate) trait G17PRenderQueueBackend: Send + Sync {
    /// Transfer an expired timeout to the exact host transaction currently
    /// owning its graph. It must classify on return, not at another timer tick.
    fn defer_timeout_if_borrowed(&self, _packet: &G17PRenderJobPacket) -> bool { false }
    fn timeout_progress(&self) -> Option<(u64, u32)> { None }
    fn prepare(&self, packet: &G17PRenderJobPacket) -> Option<Fence>;
    fn publish(&self, packet: Arc<G17PRenderJobPacket>) -> Result<Fence>;
    fn timed_out(&self, packet: Arc<G17PRenderJobPacket>) -> sched::Status;
    fn cancel(&self, packet: Arc<G17PRenderJobPacket>);
    /// v84: publish from the submit ioctl before the job is pushed. Returns
    /// true when the packet's publication was attempted (success or failure
    /// is then recorded in the packet); false leaves it for run_job.
    fn try_publish_early(&self, _packet: &Arc<G17PRenderJobPacket>) -> bool { false }
    /// v84: account a packet that run_job will publish.
    fn count_backlog(&self, _packet: &G17PRenderJobPacket) {}
}

pub(crate) struct G17PRenderScheduledJob<B: G17PRenderQueueBackend> {
    backend: Arc<B>,
    packet: Arc<G17PRenderJobPacket>,
    timeout_progress: Option<(u64, u32)>,
    timeout_renewals: u8,
}

impl<B: G17PRenderQueueBackend> sched::JobImpl for G17PRenderScheduledJob<B> {
    const MANAGED_TIMEOUT_RECOVERY: bool = true;
    fn false_timeout(job: &mut sched::Job<Self>) -> bool {
        // This hook precedes drm_sched_stop. The C core reinserts the job even
        // if graph return settles its fence before this callback returns.
        if job.backend.defer_timeout_if_borrowed(&job.packet) { return true; }
        if !cfg!(g17p_progress_timeout_probe) { return false; }
        let current = job.backend.timeout_progress();
        let mut previous = job.timeout_progress;
        let mut renewals = job.timeout_renewals;
        let renew = renew_progress_timeout(&mut previous, &mut renewals, current);
        job.timeout_progress = previous;
        job.timeout_renewals = renewals;
        if renew {
            pr_info!("G17P timeout progress probe: renewal={} snapshot={:?}\n", renewals, current);
        }
        renew
    }

    fn prepare(job: &mut sched::Job<Self>) -> Option<Fence> {
        job.backend.prepare(&job.packet)
    }

    fn run(job: &mut sched::Job<Self>) -> Result<Option<Fence>> {
        if cfg!(g17p_progress_timeout_probe) {
            job.timeout_progress = job.backend.timeout_progress();
        }
        job.backend.publish(job.packet.clone()).map(Some)
    }

    fn timed_out(job: &mut sched::Job<Self>) -> sched::Status {
        job.backend.timed_out(job.packet.clone())
    }

    fn cancel(job: &mut sched::Job<Self>) {
        job.backend.cancel(job.packet.clone());
    }
}

pub(crate) struct G17PRenderScheduler<B: G17PRenderQueueBackend> {
    entity: sched::Entity<G17PRenderScheduledJob<B>>,
    _scheduler: sched::Scheduler<G17PRenderScheduledJob<B>>,
    backend: Arc<B>,
}

impl<B: G17PRenderQueueBackend> G17PRenderScheduler<B> {
    pub(crate) fn new(dev: &AsahiDevice, backend: Arc<B>, credits: u32) -> Result<Self> {
        if credits == 0 { return Err(EINVAL); }
        let scheduler = sched::Scheduler::new(
            dev.as_ref(), 1, credits, 0, SCHEDULER_TIMEOUT_MS, c_str!("asahi_g17p_render"),
        )?;
        let entity = sched::Entity::new(&scheduler, sched::Priority::Kernel)?;
        Ok(Self { entity, _scheduler: scheduler, backend })
    }

    pub(crate) fn enqueue(
        &mut self, packet: Arc<G17PRenderJobPacket>, dependencies: KVec<Fence>,
        early_allowed: bool,
    ) -> Result<G17PCommandFences> {
        // Userspace completion must follow the exact hardware retirement
        // fence, not drm_sched's secondary finished wrapper. The scheduler
        // wrapper is signalled only after its worker observes this parent
        // fence and retires the job, which added a full scheduling interval to
        // every synchronous Vulkan submit even though firmware had already
        // completed. Every pre-publication cancellation and failure path
        // signals the packet fence as well, so it is complete on all paths.
        let hardware = packet.fence();
        let early_packet = early_allowed.then(|| packet.clone());
        let counted_packet = packet.clone();
        let mut job = self.entity.new_job(1, G17PRenderScheduledJob {
            backend: self.backend.clone(), packet, timeout_progress: None, timeout_renewals: 0,
        })?;
        for dependency in dependencies { job.add_dependency(dependency)?; }
        let mut job = job.arm();
        let published = job.fences().scheduled();
        // v84: every fallible scheduler step is done; publish from the ioctl
        // before push when the backend accepts the packet without waiting.
        // run_job then returns the recorded outcome and the scheduler keeps
        // timeout, credit and teardown ownership of the job.
        let early = early_packet.is_some_and(|packet| self.backend.try_publish_early(&packet));
        if !early { self.backend.count_backlog(&counted_packet); }
        job.push();
        Ok(G17PCommandFences { ready: published, completed: hardware })
    }
}

impl<B: G17PQueueBackend> G17PJobScheduler<B> {
    pub(crate) fn new(dev: &AsahiDevice, backend: Arc<B>, credits: u32) -> Result<Self> {
        if credits == 0 { return Err(EINVAL); }
        let scheduler = sched::Scheduler::new(
            dev.as_ref(), 1, credits, 0, SCHEDULER_TIMEOUT_MS, c_str!("asahi_g17p_sched"),
        )?;
        let entity = sched::Entity::new(&scheduler, sched::Priority::Kernel)?;
        Ok(Self { entity, _scheduler: scheduler, backend })
    }

    pub(crate) fn enqueue(
        &mut self, packet: Arc<G17PComputeJobPacket>, dependencies: KVec<Fence>,
    ) -> Result<G17PCommandFences> {
        let hardware = packet.completion()?.fence();
        let mut job = self.entity.new_job(1, G17PScheduledJob {
            backend: self.backend.clone(), packet, timeout_progress: None, timeout_renewals: 0,
            victim_renewals: 0,
        })?;
        for dependency in dependencies { job.add_dependency(dependency)?; }
        let mut job = job.arm();
        let published = job.fences().scheduled();
        job.push();
        Ok(G17PCommandFences { ready: published, completed: hardware })
    }
}

/// Consumer readiness normally follows scheduler publication. Commands with
/// user timestamp destinations instead use `completed` for `ready`: retirement
/// copies the timestamps before signalling that fence. Firmware tokens alone
/// cannot order those CPU writes. Only `completed` may satisfy user output.
#[derive(Clone)]
pub(crate) struct G17PCommandFences {
    pub(crate) ready: Fence,
    pub(crate) completed: Fence,
}
