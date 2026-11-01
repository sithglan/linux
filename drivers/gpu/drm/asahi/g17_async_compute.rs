// SPDX-License-Identifier: GPL-2.0-only OR MIT
// Included in g17_live_boot so the runtime remains the sole firmware owner.

struct G17PAsyncComputeActive {
    packet: Arc<crate::g17_job::G17PComputeJobPacket>,
    _lease: crate::g17_drm::G17PComputeLeaseGuard,
    _work_state: Arc<crate::g17_resources::G17PWorkStateLease>,
    retirement: Option<G17PAsyncComputeRetirementTicket>,
    /// This kick's KSM completion record, latched the first time any scan saw
    /// it. The ring slot can be reused before the kick is retireable (its
    /// stamp store may still be pending), so retirement must never depend on
    /// finding the record in the ring a second time.
    observed: Option<crate::g17_resources::G17PComputeCompletionCandidate>,
    /// Global publication order (`g17_recovery_isolation::COMPUTE_PUBLICATION`).
    published: u64,
}

/// `g17p_recovery_isolation` REPLAY: one spared kick that the retirement
/// witness proved firmware retired without executing, held (with its VM pin
/// and its admission guard) from the witness until it is published again.
struct G17PAsyncComputeReplay {
    packet: Arc<crate::g17_job::G17PComputeJobPacket>,
    lease: crate::g17_drm::G17PComputeLeaseGuard,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct G17PAsyncComputeRetirementTicket {
    owner: u64,
    submission: u64,
    context: u32,
    recovery_epoch: u64,
    firmware: g17_manager::G17PComputeRetirementTicket,
}

/// A prepared alias set does not change the context retained by firmware.
/// Keep the superseded root and aliases until work published under the new
/// context has actually executed, or both firmware processors have stopped.
struct G17PAsyncComputePreviousBinding {
    _binding: Option<g17_manager::G17PComputeUserBinding>,
    _context: Arc<crate::g17_resources::G17PLogicalQueueContext>,
    /// Exact first publication using the replacement binding, and its
    /// firmware recovery epoch. An error/zero-duration fence is not a witness.
    publication: Option<(u64, u64)>,
}

/// Published, unretired compute jobs of one queue, oldest first.
///
/// Firmware orders them: each CL kick names the previous kick of its QID as
/// its implicit parent, so they execute and retire in publication order and
/// the host never waits for one to retire before publishing the next.
type G17PAsyncComputeInflight = g17_completion::PairFifo<
    G17PAsyncComputeActive,
    { crate::g17_queue_limits::MAX_COMPUTE_IN_FLIGHT_PER_QUEUE as usize },
>;

/// Admission guards kept after a quarantined queue's packets were released
/// on the retirement witness (`release_firmware_retired_quarantined_compute`).
type G17PAsyncComputeRetiredLeases = g17_completion::PairFifo<
    crate::g17_drm::G17PComputeLeaseGuard,
    { crate::g17_queue_limits::MAX_COMPUTE_IN_FLIGHT_PER_QUEUE as usize },
>;

/// Installed queue graphs survive logical queue destruction. Firmware retains
/// their addresses; exact command retirement permits rebinding, not freeing.
struct G17PAsyncComputeQueue {
    owner: Option<u64>,
    queue: g17_manager::G17PSksmQueue,
    binding: Option<g17_manager::G17PComputeUserBinding>,
    context: Option<Arc<crate::g17_resources::G17PLogicalQueueContext>>,
    previous_binding: Option<G17PAsyncComputePreviousBinding>,
    inflight: G17PAsyncComputeInflight,
    released: bool,
    /// The most recent publication passed every queue-local retirement gate.
    /// Consumed when ownership changes or another command is published.
    retirement_ready: bool,
    quarantined: bool,
    /// The client cancelled its pending waiter. Keep observing exact command
    /// retirement; installed backing remains retained afterward. Also marks a
    /// failed unpublished-owner restore, which remains quarantined separately.
    retired_by_teardown: bool,
    terminal_fanout: Option<(Arc<crate::g17_status::VmStatus>, Error)>,
    /// The oldest in-flight kick has finished on the GPU (record latched or
    /// firmware counters complete) but was not retireable at the last scan,
    /// typically because its stamp store was not visible yet. Such a queue is
    /// re-evaluated on every later completion event, not only on events that
    /// name its QID, so a late stamp costs one event interval instead of the
    /// scheduler's 2 s timeout.
    retire_pending: bool,
    /// Quarantined with a published, ticketed command whose exact firmware
    /// retirement has not been observed yet (zero-duration retire or
    /// scheduler timeout or another job losing this VM). While set, the
    /// queue-local terminal words are
    /// re-read at recovery completion, owner release, the next client's
    /// preparation and completion-event batches. Only that command's packet
    /// and VM pin are released on the witness; the installed graph (QID,
    /// SKSM entry aliases, context root) stays quarantined as before.
    awaiting_retirement_witness: bool,
    /// Admission guards of quarantined commands whose packets were released
    /// on that witness, one per released kick. Kept so admission accounting
    /// and the processor-stop rehabilitation are exactly what they were while
    /// the packets were held. A quarantined queue admits nothing new, so the
    /// in-flight bound also bounds this.
    retired_leases: G17PAsyncComputeRetiredLeases,
    /// `g17p_recovery_isolation`: every command this queue was quarantined
    /// for was spared by a recovery (cancelled for another client). Once the
    /// retirement witness releases them the queue returns to service.
    spared_quarantine: bool,
    /// Log and count the first retirement after a return to service.
    first_kick_watch: bool,
    /// Spared zero-duration kicks whose fences are NOT yet signalled: they
    /// complete (with an error, no VM status) at the retirement witness, so
    /// no VM-job quarantine ever blocks the innocent VM's admission.
    spared_deferred: bool,
    /// `g17p_exit_kill`: the process-exit kill published for this queue's
    /// context after its owner left with kicks outstanding.
    exit_kill: Option<G17PExitKill>,
    /// The kill was refused for this queue at least once (logged once).
    exit_kill_refused: bool,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum G17PExitKillPhase {
    /// Published; waiting for event 12 with this cookie.
    Published,
    /// Acknowledged, but a kick may still execute (or the recovery handshake
    /// is open): re-evaluated on later teardown turns.
    Acknowledged,
    /// Packets and VM pins released.
    Released,
    /// Acknowledged without a kill on this page; packets retained.
    Unproven,
}

/// `g17p_exit_kill` state of one queue (see g17_exit_kill.rs).
struct G17PExitKill {
    cookie: u64,
    phase: G17PExitKillPhase,
    /// Render pairs held out of reuse in the registry (for the session).
    held_render_slots: u64,
    /// QIDs the page named when the kill was published.
    qids: u128,
    killed_before: u32,
    published: kernel::time::Instant<kernel::time::Monotonic>,
    late_warned: bool,
}

/// Type-1 firmware events identify completed stamp/QID slots in a 128-bit
/// mask. Keep that identity through dispatch instead of scanning every active
/// queue and its complete 0x800-entry completion ring for one queue's edge.
const fn g17p_async_completion_mask_contains(masks: [u64; 2], queue_id: u8) -> bool {
    if queue_id >= 128 {
        return false;
    }
    let word = queue_id as usize / 64;
    masks[word] & (1u64 << (queue_id as usize % 64)) != 0
}

/// Whether the oldest in-flight kick of `q` has already finished on the GPU:
/// its completion record was latched, or firmware's completed counter and
/// item-ring consumption have passed it. Used only to decide whether the
/// queue needs re-examination on the next event of any QID.
fn g17p_async_compute_front_finished(
    manager: &mut g17_manager::G17PManagerConstruction,
    q: &mut G17PAsyncComputeQueue,
) -> bool {
    let Some((observed, ticket)) = q.inflight.front()
        .map(|front| (front.observed.is_some(), front.retirement)) else { return false; };
    if observed {
        return true;
    }
    let Some(ticket) = ticket else { return false; };
    manager.independent_compute_kick_finished(&mut q.queue, &ticket.firmware).unwrap_or(false)
}

impl G17PLiveRuntime {
    /// Both firmware CPUs must have stopped before this is called. Queue
    /// destruction alone cannot settle the scheduler's retained hardware
    /// fences, and a later timeout cannot find a queue we have removed.
    fn release_async_compute_after_processor_stop(&mut self) {
        for q in &self.async_compute_queues {
            for active in q.inflight.iter() {
                if let Ok(completion) = active.packet.completion() {
                    completion.complete(Err(EIO));
                    completion.release_after_processor_stop();
                }
                active._lease.rehabilitate_after_processor_stop();
            }
            for lease in q.retired_leases.iter() {
                lease.rehabilitate_after_processor_stop();
            }
        }
        self.async_compute_queues.clear();
        // Kills outstanding at the stop are moot: forget their cookies. The
        // registry drops their render-pair holds below.
        crate::g17_exit_kill::forget_all_acks();
        if let Some(registry) = &self.queue_registry {
            registry.release_retained_owners_after_processor_stop();
        }
    }

    pub(crate) fn prepare_queue_execution_context(
        &mut self, vm: &mmu::Vm,
    ) -> Result<Arc<crate::g17_resources::G17PLogicalQueueContext>> {
        if !self.accepting_submissions || self.is_crashed() { return Err(EIO); }
        let manager = self.manager.as_mut().ok_or(ENODEV)?;
        manager.prepare_logical_queue_context(self.drm.as_ref().ok_or(ENODEV)?, vm)
            .inspect_err(|error| crate::g17_queue_slots::note_error(
                crate::g17_queue_slots::Pool::QueueContextQos, *error))
    }

    pub(crate) fn prepare_compute_execution_context(
        &mut self,
        context: &Arc<crate::g17_resources::G17PLogicalQueueContext>,
    ) -> Result<Arc<crate::g17_resources::G17PLogicalQueueContext>> {
        if !self.accepting_submissions || self.is_crashed() { return Err(EIO); }
        self.manager.as_mut().ok_or(ENODEV)?.prepare_compute_execution_context(
            context, module_parameters::g17p_split_execution_id_probe.value() != 0,
        )
    }

    /// Build (or take the boot graph for) a new queue entry and append it.
    /// Out of line and cold: the queue entry is several KiB and is staged on
    /// the stack while it is constructed; keep that off the submit path's
    /// frame (only queue creation pays it).
    #[cold]
    #[inline(never)]
    fn create_async_compute_queue(&mut self) -> Result<usize> {
        self.async_compute_queues.reserve(1, GFP_KERNEL)?;
        // Reuse the unsubmitted boot graph, preserving QID 4 as first queue.
        let queue = if self.compute_binding.is_none() && self.pending_compute.is_none() {
            self.queue.take()
        } else { None };
        let queue = match queue {
            Some(queue) => queue,
            None => self.manager.as_mut().ok_or(ENODEV)?
                .prepare_g17p_compute_queue(self.drm.as_ref().ok_or(ENODEV)?)?,
        };
        self.queue_setup_pending = false;
        let index = self.async_compute_queues.len();
        self.async_compute_queues.push(G17PAsyncComputeQueue {
            owner: None, queue, binding: None, context: None,
            inflight: G17PAsyncComputeInflight::new(),
            previous_binding: None,
            released: false, retirement_ready: false,
            quarantined: false, retired_by_teardown: false,
            terminal_fanout: None,
            retire_pending: false,
            awaiting_retirement_witness: false,
            retired_leases: G17PAsyncComputeRetiredLeases::new(),
            spared_quarantine: false,
            first_kick_watch: false,
            spared_deferred: false,
            exit_kill: None,
            exit_kill_refused: false,
        }, GFP_KERNEL)?;
        Ok(index)
    }

    pub(crate) fn prepare_async_compute_queue(
        &mut self, owner: u64, vm: &mmu::Vm,
        context: Arc<crate::g17_resources::G17PLogicalQueueContext>,
    ) -> Result<(u8, Arc<crate::g17_resources::G17PLogicalQueueContext>)> {
        if !self.accepting_submissions || self.is_crashed() { return Err(EIO); }
        if !context.matches(vm) || !context.is_current() { return Err(EFAULT); }
        if let Some(q) = self.async_compute_queues.iter().find(|q| q.owner == Some(owner)) {
            return Ok((q.queue.queue_id(), q.context.as_ref().ok_or(EIO)?.clone()));
        }
        self.release_firmware_retired_quarantined_compute_nested("client-prepare");
        let manager = self.manager.as_mut().ok_or(ENODEV)?;
        manager.stage_compute_flist_state()?;
        let free = self.async_compute_queues.iter().position(|q|
            q.queue.requires_first_install() && q.previous_binding.is_none() &&
            crate::g17_queue_limits::reusable_idle_compute_graph(
                q.owner.is_none(),
                q.inflight.is_none(),
                q.binding.is_none(),
                q.context.is_none(),
                q.quarantined,
            )).or_else(|| {
                if crate::g17_config::ASYNC_COMPUTE_REBIND == 0 {
                    return None;
                }
                self.async_compute_queues.iter().position(|q|
                    !q.queue.requires_first_install()
                        && q.previous_binding.is_none()
                        && q.binding.is_some()
                        && q.context.is_some()
                        && q.retirement_ready
                        && crate::g17_queue_limits::rebindable_released_compute_graph(
                            q.owner.is_none(),
                            q.inflight.is_none(),
                            q.released,
                            q.quarantined,
                        )
                )
            });
        let index = if let Some(index) = free { index } else {
            let created = self.create_async_compute_queue();
            self.note_queue_slots();
            created?
        };
        let q = &mut self.async_compute_queues[index];
        if q.binding.is_some() != q.context.is_some() {
            return Err(EIO);
        }
        // From here the incoming page can be named by a firmware-visible CL
        // record, so the queue's close must release it through the firmware
        // (`bind_logical_channel_control` marks it again at the exact bind).
        context.channel_control.mark_published();
        // Same-VM reuse keeps the existing aliases, but logical queue identity
        // is independent of VM identity. Install the incoming root/scheduler
        // owner and retain the old root until replacement work completes.
        if let Some(current) = q.context.as_ref().filter(|current|
            current.matches(vm) && current.is_current()
        ) {
            let previous = current.clone();
            let binding = q.binding.as_mut().ok_or(EIO)?;
            if !binding.matches(vm) { return Err(EFAULT); }
            q.queue.bind_logical_channel_control(&context)?;
            binding.retarget_logical_context(context.clone())?;
            q.previous_binding = Some(G17PAsyncComputePreviousBinding {
                _binding: None, _context: previous, publication: None,
            });
            q.context = Some(context.clone());
            q.owner = Some(owner);
            q.released = false;
            q.retirement_ready = false;
            q.retired_by_teardown = false;
            return Ok((q.queue.queue_id(), context));
        }
        let reused_installed = !q.queue.requires_first_install();
        // All fallible mapping work precedes the ownership transition. An
        // allocation/validation failure leaves the old queue fully bound.
        let binding = self.manager.as_mut().ok_or(ENODEV)?.bind_independent_compute_user_vm(
            &mut q.queue, vm, context.clone(),
        )?;
        q.queue.bind_logical_channel_control(&context)?;
        if let (Some(binding), Some(context)) = (q.binding.take(), q.context.take()) {
            q.previous_binding = Some(G17PAsyncComputePreviousBinding {
                _binding: Some(binding),
                _context: context,
                publication: None,
            });
        }
        q.binding = Some(binding);
        q.context = Some(context.clone());
        q.owner = Some(owner);
        q.released = false;
        q.retirement_ready = false;
        q.retired_by_teardown = false;
        if reused_installed {
            if let Some(pdev) = self.pdev.as_ref().filter(|_| crate::util::g17p_verbose()) {
                dev_info!(
                    pdev.as_ref(),
                    "G17P async compute: prepared installed graph rebind qid={} owner={} context={} retaining previous binding until completion\n",
                    q.queue.queue_id(),
                    owner,
                    context.context_id(),
                );
            }
        }
        Ok((q.queue.queue_id(), context))
    }

    /// Validate the frontend's cached scheduler/backend against the runtime
    /// graph it names. A successful processor-stop recovery drops every
    /// firmware-owned graph, but logical DRM queues can survive that boundary.
    /// In that case an absent owner means the frontend must create a fresh
    /// graph before its next compute submission.
    ///
    /// An owner that still exists but names different hardware state is not a
    /// recovery case: tearing it down here could release another live graph.
    /// Treat that as an internal ownership error instead.
    pub(crate) fn async_compute_queue_matches(
        &self, owner: u64, hardware_queue: u8,
        context: &Arc<crate::g17_resources::G17PLogicalQueueContext>,
    ) -> Result<bool> {
        let Some(q) = self.async_compute_queues.iter().find(|q| q.owner == Some(owner)) else {
            return Ok(false);
        };
        let installed_context = q.context.as_ref().ok_or(EIO)?;
        if q.released
            || q.quarantined
            || q.binding.is_none()
            || q.queue.queue_id() != hardware_queue
            || !Arc::ptr_eq(installed_context, context)
            || !context.is_current()
        {
            return Err(EIO);
        }
        Ok(true)
    }

    /// A signaled error fence is still signaled to DRM syncobj waiters. Surface
    /// device failure at the next submit instead of accepting work whose
    /// hardware owner is quarantined and can never make queue progress.
    pub(crate) fn check_async_compute_submission(&self, owner: u64) -> Result {
        if !self.accepting_submissions || self.is_crashed() {
            return Err(EIO);
        }
        // A quarantined queue can never make progress, so its own client must
        // be told. Whether every OTHER client is told too is the question:
        // quarantine retires the queue permanently -- nothing it owns is freed
        // or reused -- so the device-wide refusal protects nothing that the
        // retirement does not already protect. See `g17p_quarantine_scope`.
        let scoped = crate::g17_config::QUARANTINE_SCOPE & 1 != 0;
        let blocked = self.async_compute_queues.iter().any(|q| {
            // A queue retired by client teardown never refuses anyone but its
            // own (already departed) owner. Leaving it in the device-wide test
            // was the second half of the kill: even with the session flag not
            // raised, one exited process holding a quarantined queue would
            // refuse EVERY later compute submit on the device for the rest of
            // the boot.
            q.quarantined
                && (!scoped || q.owner == Some(owner))
                && (!q.retired_by_teardown || q.owner == Some(owner))
        });
        if blocked {
            return Err(EIO);
        }
        Ok(())
    }

    /// The runtime lock serializes this snapshot against publication and
    /// retirement. Keep the published QID stamp, never predict the next kick.
    pub(crate) fn async_compute_native_dependency(
        &mut self, owner: u64, status: &Arc<crate::g17_status::VmStatus>,
        wait_through: Option<u64>,
    ) -> Result<Option<g17_submission::G17PClBarrierDependency>> {
        // BARRIER_NONE permits independent engines to overlap, as on AGX2.
        // Do not turn a live compute queue into an implicit execution barrier.
        if wait_through.is_none() {
            return Ok(None);
        }
        let handshake = self.manager.as_mut().ok_or(ENODEV)?
            .firmware_recovery_handshake_snapshot()?;
        let Some(q) = self.async_compute_queues.iter().find(|q| q.owner == Some(owner)) else {
            return Ok(None);
        };
        // Only kicks inside the requested logical prefix may be parents.
        // One QID orders its own kicks, so the newest covered kick subsumes
        // older ones without waiting for later independent work.
        let Some(active) = q.inflight.iter().filter(|active|
            g17_submission::g17p_render_prefix_contains(
                wait_through, active.packet.command().compute_sequence,
            )).last() else { return Ok(None); };
        let completion = active.packet.completion()?;
        if !Arc::ptr_eq(&completion.status, status) { return Err(EIO); }
        let ticket = active.retirement.as_ref().ok_or(EIO)?;
        // iso4: a queue spared by another client's recovery is waiting for its
        // retirement witness (and replay). The render cannot be ordered after
        // it yet; fail the render without charging its VM a device loss.
        if q.quarantined && q.spared_quarantine
            && crate::g17_recovery_isolation::enabled(crate::g17_recovery_isolation::INNOCENT)
        {
            crate::g17_recovery_isolation::count(
                &crate::g17_recovery_isolation::INHERITED_NOT_POISONED);
            return Err(ECANCELED);
        }
        // Publication settles any open recovery first. A closed recovery since
        // this compute's publication leaves its native stamp meaningful.
        if q.quarantined || handshake.state != 0 {
            return Err(EIO);
        }
        // SAFETY: completion retains this hardware fence throughout the read.
        let fence = completion.fence();
        let state = unsafe { kernel::bindings::dma_fence_get_status(
            kernel::dma_fence::RawDmaFence::raw(&fence),
        ) };
        if state < 0 {
            return Err(crate::g17_drm::inherited_dependency_error(Error::from_errno(state)));
        }
        if state > 0 { return Ok(None); }
        // iso4: this render is ordered on the kick's native token. If another
        // client's recovery cancels the kick, retiring it satisfies the
        // token, so the kick must not be replayed after this render ran.
        completion.note_native_consumer();
        let timestamp = ticket.firmware.kick_timestamp();
        let mut dependency = g17_submission::g17p_render_barrier_from_timestamp(
            timestamp, q.queue.queue_id(),
        ).map_err(|_| EIO)?;
        dependency.native_timestamp = Some(timestamp);
        Ok(Some(dependency))
    }

    pub(crate) fn publish_async_compute(
        &mut self, owner: u64, packet: Arc<crate::g17_job::G17PComputeJobPacket>,
        lease: crate::g17_drm::G17PComputeLeaseGuard,
    ) -> Result {
        crate::g17_timing_buffer::msglog(crate::g17_timing_buffer::MSG_SUBMIT,
            1 << 32, 0, 0);
        self.submit_accounting_begin();
        let result = self.publish_async_compute_inner(owner, packet, lease);
        self.submit_accounting_end();
        crate::g17_timing_buffer::msglog(crate::g17_timing_buffer::MSG_SUBMIT,
            1 << 32, 1, result.is_ok() as u64);
        result
    }

    fn publish_async_compute_inner(
        &mut self, owner: u64, packet: Arc<crate::g17_job::G17PComputeJobPacket>,
        lease: crate::g17_drm::G17PComputeLeaseGuard,
    ) -> Result {
        if !self.accepting_submissions || self.is_crashed() { return Err(EIO); }
        if packet.completion()?.status.get() != 0 { return Err(EIO); }
        if !packet.context().is_current() { return Err(EFAULT); }
        let trace_entry_ns = if crate::g17_timing_buffer::enabled() {
            crate::g17_timing_buffer::now()
        } else { 0 };
        self.settle_open_recovery_before_publication()?;
        let mut dependencies = [g17_submission::G17PClBarrierDependency {
            native_timestamp: None, event_word: 0, shared_stamp: 0, queue_id: 0,
        }; g17_submission::G17P_CL_EXPLICIT_DEPENDENCIES_MAX];
        let mut dependency_sequences = [0; g17_submission::G17P_CL_EXPLICIT_DEPENDENCIES_MAX];
        let mut dependency_count = 0;
        for predecessor in Iterator::chain(
            self.pending_render.iter().flatten(),
            self.pending_render_queued.iter().flatten(),
        ) {
            if let Some(dependency) = predecessor.compute_predecessor_dependency(
                owner, &packet.completion()?.status,
                packet.command().render_wait_through, packet.command().render_wait_last,
            )? {
                g17_submission::g17p_insert_prefix_dependency(
                    &mut dependencies, &mut dependency_sequences, &mut dependency_count,
                    predecessor.render_sequence(), dependency,
                ).map_err(|_| EOVERFLOW)?;
            }
        }
        // Render jobs now retain independent application-GART roots and
        // job-private low views.  Compute QID4+ has no render context lease to
        // reclaim, so publication may proceed while either render slot runs.
        self.sync_g17p_idle_hold();
        if crate::g17_config::POWER_WIRE == 1 { self.wire_g17p_submit_gpu_power()?; }
        let index = self.async_compute_queues.iter().position(|q| q.owner == Some(owner)).ok_or(EINVAL)?;
        let q = &mut self.async_compute_queues[index];
        // No host-retirement edge: an earlier kick of this queue may still be
        // executing. The CL entry's implicit parent orders this kick after it
        // in firmware. Only the host bookkeeping bound (equal to the DRM
        // scheduler's credits) limits how many are outstanding.
        if !q.inflight.has_room() || q.quarantined || q.released { return Err(EBUSY); }
        if q.queue.queue_id() != packet.hardware_queue_id()
            || q.context.as_ref().ok_or(EIO)?.context_id() != packet.context().context_id()
        { return Err(EINVAL); }
        let timestamp_start = packet.completion()?.gpu_address();
        let timestamp_end = timestamp_start.checked_add(8).ok_or(EOVERFLOW)?;
        let state = self.gfx_state.as_ref().cloned().ok_or(ENODEV)?;
        state.submission_phase.store(g17_manager::G17P_SUBMIT_PHASE_IDLE, Ordering::Release);
        let publication_handshake = self.manager.as_mut().ok_or(ENODEV)?
            .firmware_recovery_handshake_snapshot()?;
        if publication_handshake.state != 0 { return Err(EBUSY); }
        let work_state = packet.completion()?.work_state()?;
        // The backend reserves/initializes this context-owned node before
        // acquiring the runtime. Commit only consumes the installed identity.
        let work_state_word = work_state.word();
        if work_state_word == u32::MAX { return Err(EINVAL); }
        // From this point, every error retains the packet, mappings and root.
        // A failed notification does not prove the firmware missed the kick.
        q.retirement_ready = false;
        let published = crate::g17_recovery_isolation::COMPUTE_PUBLICATION
            .fetch_add(1, Ordering::Relaxed) + 1;
        q.inflight.push(G17PAsyncComputeActive {
            packet: packet.clone(), _lease: lease, _work_state: work_state.clone(),
            retirement: None, observed: None, published,
        }).map_err(|_| EBUSY)?;
        let mut tsq_reserved = false;
        let result = (|| {
            work_state.add_submitted_kicks(1)?;
            tsq_reserved = true;
            let manager = self.manager.as_mut().ok_or(ENODEV)?;
            let notifications = manager.submit_translated_compute(
                self.registers.as_ref().ok_or(ENODEV)?, &mut q.queue,
                &state.submission_phase, q.binding.as_ref().ok_or(ENODEV)?, packet.command(),
                g17_manager::G17PComputeSubmissionContext {
                    context_id: packet.context().context_id(),
                    work_state_word,
                    work_state: Some(&work_state),
                    dispatch_identity: 0x0100_01d7_0200_01dc,
                    timestamps: g17_uapi::ComputeTimestampAddresses { start: timestamp_start, end: timestamp_end },
                    descriptor_flag_4c: false,
                    converted_command_timestamp: 1,
                    barriers: &dependencies[..dependency_count], mcache: None,
                    rce_kind: 0, auxiliary: 0x003f_ffff_ffff_ffff,
                },
            )?;
            let firmware = manager
                .capture_independent_compute_retirement_ticket(&mut q.queue, &work_state)?;
            let accepted = manager.firmware_recovery_handshake_snapshot()?;
            if accepted.state != 0 || accepted.epoch != publication_handshake.epoch {
                return Err(EIO);
            }
            let retirement = G17PAsyncComputeRetirementTicket {
                owner,
                submission: packet.submission_id(),
                context: packet.context().context_id(),
                recovery_epoch: accepted.epoch,
                firmware,
            };
            let publication = (retirement.submission, retirement.recovery_epoch);
            q.inflight.back_mut().ok_or(EIO)?.retirement = Some(retirement);
            // The FIRST publication under the replacement binding is the
            // witness that releases the superseded one; later publications
            // stacked behind it retire after it.
            if let Some(previous) = q.previous_binding.as_mut() {
                if previous.publication.is_none() {
                    previous.publication = Some(publication);
                }
            }
            if let Some(notifications) = notifications {
                for message in [notifications.activation, notifications.direct_kick].into_iter().flatten() {
                    fence(Ordering::SeqCst);
                    Pin::new(self.gfx_rtkit.as_mut().ok_or(ENODEV)?)
                        .send_message_logged(g17_rtkit::ENDPOINT_GFX_INTERRUPTS, message)?;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            if state.submission_phase.load(Ordering::Acquire) == g17_manager::G17P_SUBMIT_PHASE_IDLE {
                if tsq_reserved {
                    if let Err(accounting_error) = work_state.cancel_unpublished_kicks(1) {
                        // Accounting failure must not leave an apparently live
                        // queue whose retained packet has no retirement ticket.
                        self.quarantine_async_compute(owner, packet.submission_id(), accounting_error);
                        return Err(accounting_error);
                    }
                }
                // No firmware-facing publication occurred: release this packet
                // and permit a later valid job to retry the prepared queue.
                let q = &mut self.async_compute_queues[index];
                drop(q.inflight.pop_back());
                if let Some(previous) = q.previous_binding.as_mut() {
                    if previous.publication.is_some_and(|(submission, _)|
                        submission == packet.submission_id())
                    {
                        previous.publication = None;
                    }
                }
            } else {
                self.quarantine_async_compute(owner, packet.submission_id(), error);
            }
            return Err(error);
        }
        if crate::g17_config::POWER_WIRE >= 2 {
            if let Err(error) = self.wire_g17p_submit_gpu_power() {
                self.quarantine_async_compute(owner, packet.submission_id(), error);
                return Err(error);
            }
        }
        if trace_entry_ns != 0 {
            let q = &self.async_compute_queues[index];
            if let Some(ticket) = q.inflight.back().and_then(|active| active.retirement) {
                let encode = |d: &g17_submission::G17PClBarrierDependency| {
                    d.native_timestamp.unwrap_or(u64::from(d.event_word) | (d.shared_stamp << 32))
                        | (u64::from(d.queue_id) << 56)
                };
                let mut words = [0; crate::g17_timing_buffer::WORDS];
                words[..17].copy_from_slice(&[
                    crate::g17_timing_buffer::KIND_COMPUTE_PUBLISH, owner, packet.submission_id(),
                    u64::from(q.queue.queue_id()), trace_entry_ns, crate::g17_timing_buffer::now(),
                    crate::g17_timing_buffer::arch_counter(), crate::g17_timing_buffer::arch_frequency(),
                    q.queue.published_compute_dependency().ok()
                        .and_then(|d| d.native_timestamp).unwrap_or(0),
                    q.inflight.len() as u64, dependency_count as u64,
                    dependencies.first().filter(|_| dependency_count > 0).map_or(0, encode),
                    dependencies.get(1).filter(|_| dependency_count > 1).map_or(0, encode),
                    u64::from(ticket.firmware.submitted_target()),
                    u64::from(ticket.firmware.item_producer()),
                    ticket.firmware.descriptor(), ticket.recovery_epoch,
                ]);
                crate::g17_timing_buffer::record(words);
            }
        }
        let pdev = self.pdev.as_ref().ok_or(ENODEV)?;
        if crate::util::g17p_verbose() {
            let outstanding: usize = self.async_compute_queues.iter().map(|q| q.inflight.len()).sum();
            dev_info!(pdev.as_ref(),
                "G17P async compute: PUBLISHED submission={} owner={} qid={} context={} root={:#x} outstanding={}\n",
                packet.submission_id(), owner, packet.hardware_queue_id(),
                packet.context().context_id(), packet.context().root(), outstanding);
        }
        Ok(())
    }

    fn service_async_compute_completions_for_event(&mut self, masks: [u64; 2]) -> Result {
        self.service_async_compute_completions_matching(Some(masks))
    }

    /// Dispatch one drained firmware-event batch. A zero mask has no QID
    /// identity, so it deliberately retains the broad lost-edge/watchdog
    /// observation used before event routing was introduced.
    fn service_async_compute_completion_masks(&mut self, masks: [u64; 2]) -> Result {
        if masks == [0; 2] {
            self.service_async_compute_completions()
        } else {
            self.service_async_compute_completions_for_event(masks)
        }
    }

    /// Full-scan fallback for a final watchdog observation or an event batch
    /// whose record did not carry a usable QID mask. These paths deliberately
    /// preserve the old lost/coalesced-edge recovery behaviour.
    pub(crate) fn service_async_compute_completions(&mut self) -> Result {
        self.service_async_compute_completions_matching(None)
    }

    fn service_async_compute_completions_matching(
        &mut self,
        masks: Option<[u64; 2]>,
    ) -> Result {
        let pdev = self.pdev.as_ref();
        let manager = self.manager.as_mut().ok_or(ENODEV)?;
        // Retired kicks are signalled after the runtime mutex is released
        // (`G17PDeferredCompletion`); borrowed apart from `manager` and the
        // queues so the retirement loop can push and kick in place.
        let deferred = &mut self.deferred_completions;
        let drain_owned = self.deferred_completion_drain_owned;
        let gfx_state = self.gfx_state.as_ref();
        manager.begin_independent_compute_completion_batch();
        let mut requested = false;
        // Quarantined commands awaiting a retirement witness are skipped by
        // the scan below; note them in the same pass so a healthy session
        // pays no extra walk for this.
        let mut witness_pending = false;
        // A queue is examined when the event names its QID (or the batch has
        // no QID identity), when its client tore down (its watchdog is gone),
        // or when its oldest kick already finished on the GPU and only waited
        // for its stamp (`retire_pending`): that last case must not depend on
        // another event for the same QID, which may never come.
        let selected = |q: &G17PAsyncComputeQueue| -> bool {
            q.retired_by_teardown || q.retire_pending || masks.is_none_or(|masks| {
                g17p_async_completion_mask_contains(masks, q.queue.queue_id())
            })
        };
        for q in &self.async_compute_queues {
            witness_pending |= q.awaiting_retirement_witness;
            if q.inflight.is_none() || q.quarantined || !selected(q) { continue; }
            // Request every in-flight kick whose record has not been latched
            // yet, oldest first, by its exact queue timestamp: firmware may
            // report several of them in one batch. A kick without a ticket is
            // not requested; the retirement loop classifies it for this queue.
            for active in q.inflight.iter() {
                let Some(ticket) = active.retirement else { break; };
                if active.observed.is_some() { continue; }
                requested |= manager.stage_independent_compute_completion(
                    &q.queue, &ticket.firmware,
                )?;
            }
        }
        if requested {
            manager.scan_independent_compute_completion_batch()?;
        }
        for q in &mut self.async_compute_queues {
            if q.inflight.is_none() || q.quarantined || !selected(q) { continue; }
            // Latch every record this scan captured, for every in-flight kick,
            // before deciding retirement: a later kick's record is also the
            // upper bound of an earlier recordless kick's interval.
            if requested {
                for active in q.inflight.iter_mut() {
                    let Some(ticket) = active.retirement else { break; };
                    if active.observed.is_some() { continue; }
                    match manager.take_staged_independent_compute_observation(&q.queue, &ticket.firmware) {
                        Ok(observed) => active.observed = observed,
                        // Every kick taken here was requested above under the
                        // same selection; a miss is a host invariant failure,
                        // kept to this queue (the kick stays unobserved).
                        Err(error) => if let Some(pdev) = pdev {
                            dev_err!(pdev.as_ref(),
                                "G17P async compute: UNCLASSIFIABLE qid={} cause=batch-request kick-ts={:#x} ({:?})\n",
                                q.queue.queue_id(), ticket.firmware.kick_timestamp(), error);
                        },
                    }
                }
            }
            // Retire in publication order and stop at the first kick still
            // pending: a later kick cannot have executed before it.
            //
            // A finished kick with no latched record is retired on firmware
            // counters and stamp only after one earlier pass already saw it
            // finished, or on an identity-less/final scan: that grace keeps
            // the real record's timestamps whenever the record merely trails
            // the event, at the cost of one event interval when it is lost.
            let finished_before = q.retire_pending;
            q.retire_pending = false;
            let mut oldest_of_pass = true;
            loop {
                let Some(front) = q.inflight.front() else { break; };
                let completion = front.retirement
                    .ok_or_else(|| {
                        if let Some(pdev) = pdev {
                            dev_err!(pdev.as_ref(),
                                "G17P async compute: retirement-invariant qid={} cause=missing-ticket\n",
                                q.queue.queue_id());
                        }
                        EIO
                    })
                    .and_then(|ticket| {
                        if q.owner != Some(ticket.owner)
                            || front.packet.submission_id() != ticket.submission
                            || front.packet.context().context_id() != ticket.context
                        {
                            if let Some(pdev) = pdev {
                                dev_err!(pdev.as_ref(),
                                    "G17P async compute: retirement-invariant qid={} cause=ticket-identity owner={:?}/{} submission={}/{} context={}/{}\n",
                                    q.queue.queue_id(), q.owner, ticket.owner,
                                    front.packet.submission_id(), ticket.submission,
                                    front.packet.context().context_id(), ticket.context);
                            }
                            return Err(EIO);
                        }
                        // J700 firmware routinely raises fault-free recoveries. An
                        // open one says nothing about this job yet: keep it armed
                        // and let the recovery settle first. Once the handshake is
                        // closed, a changed epoch only means a recovery happened
                        // while the job was live; the exact descriptor, stamp and
                        // ring checks below still decide completed, zero-duration
                        // or pending. Failing here killed healthy sessions.
                        let handshake = manager.firmware_recovery_handshake_snapshot()?;
                        if handshake.state != 0 {
                            return Err(EAGAIN);
                        }
                        if handshake.epoch != ticket.recovery_epoch && crate::util::g17p_verbose() {
                            if let Some(pdev) = pdev {
                                dev_info!(pdev.as_ref(),
                                    "G17P async compute: qid={} retiring across recovery epoch {}/{}\n",
                                    q.queue.queue_id(), handshake.epoch, ticket.recovery_epoch);
                            }
                        }
                        // Earliest recorded end among later kicks: they run
                        // after this one, so it bounds this kick's end.
                        let fallback_end = q.inflight.iter().skip(1)
                            .find_map(|later| later.observed.map(|record| record.timestamps[1]));
                        // Only the oldest kick of a pass may use the grace:
                        // a later one was not seen finished on its own. An
                        // identity-less (final/timeout) scan allows all.
                        let recordless = if handshake.epoch != ticket.recovery_epoch {
                            g17_manager::G17PComputeRecordless::CrossedRecovery
                        } else if masks.is_none() || (finished_before && oldest_of_pass) {
                            g17_manager::G17PComputeRecordless::Allowed
                        } else {
                            g17_manager::G17PComputeRecordless::NotYet
                        };
                        let completion = manager.poll_staged_independent_compute_completion(
                            &mut q.queue, &ticket.firmware, &front._work_state, front.observed,
                            recordless, fallback_end,
                        ).map_err(|error| {
                            if error != EAGAIN {
                                if let Some(pdev) = pdev {
                                    dev_err!(pdev.as_ref(),
                                        "G17P async compute: retirement-invariant qid={} cause=poll errno={}\n",
                                        q.queue.queue_id(), error.to_errno());
                                }
                            }
                            error
                        })?;
                        if completion.is_some()
                            && q.previous_binding.as_ref().is_some_and(|previous|
                                previous.publication
                                    != Some((ticket.submission, ticket.recovery_epoch)))
                        {
                            if let Some(pdev) = pdev {
                                dev_err!(pdev.as_ref(),
                                    "G17P async compute: retirement-invariant qid={} cause=previous-binding-publication expected=({},{}) actual={:?}\n",
                                    q.queue.queue_id(), ticket.submission, ticket.recovery_epoch,
                                    q.previous_binding.as_ref().and_then(|previous| previous.publication));
                            }
                            return Err(EIO);
                        }
                        Ok(completion)
                    });
                oldest_of_pass = false;
                match completion {
                    Ok(Some(retired)) => {
                        let active = q.inflight.take().ok_or(EIO)?;
                        let timestamps = retired.timestamps;
                        if q.inflight.is_none() {
                            q.retirement_ready = true;
                        }
                        // Only the poller's exact descriptor/fresh timestamp/final
                        // stamp success in the publication epoch can release the
                        // superseded aliases. Current binding stays queue-owned.
                        drop(q.previous_binding.take());
                        if crate::g17_timing_buffer::enabled() {
                            let mut words = [0; crate::g17_timing_buffer::WORDS];
                            words[..15].copy_from_slice(&[
                                crate::g17_timing_buffer::KIND_COMPUTE_RETIRE,
                                q.owner.unwrap_or(0), active.packet.submission_id(),
                                u64::from(q.queue.queue_id()), crate::g17_timing_buffer::now(),
                                crate::g17_timing_buffer::arch_counter(),
                                retired.pairs[0], retired.pairs[1], retired.pairs[2], retired.pairs[3],
                                u64::from(retired.counters[0]), u64::from(retired.counters[1]),
                                u64::from(retired.stamp), u64::from(retired.in_flight_after),
                                masks.map_or(0, |_| 1),
                            ]);
                            crate::g17_timing_buffer::record(words);
                        }
                        if q.first_kick_watch {
                            q.first_kick_watch = false;
                            crate::g17_recovery_isolation::count(
                                &crate::g17_recovery_isolation::FIRST_KICKS_OK);
                            if let Some(pdev) = self.pdev.as_ref() {
                                dev_info!(pdev.as_ref(),
                                    "G17P recovery isolation: qid={} first kick after return retired submission={} start={} end={} duration={}\n",
                                    q.queue.queue_id(), active.packet.submission_id(),
                                    timestamps[0], timestamps[1],
                                    timestamps[1].wrapping_sub(timestamps[0]));
                            }
                        }
                        if let Some(pdev) = self.pdev.as_ref().filter(|_| crate::util::g17p_verbose()) {
                            dev_info!(pdev.as_ref(),
                                "G17P async compute: COMPLETED submission={} qid={} context={} start={} end={} in-flight={}\n",
                                active.packet.submission_id(), q.queue.queue_id(),
                                active.packet.context().context_id(), timestamps[0], timestamps[1],
                                q.inflight.len());
                        }
                        let completion = active.packet.completion()?;
                        if q.retired_by_teardown {
                            completion.release_after_retirement();
                        }
                        // Proven retired: the fence is signalled once the
                        // runtime mutex is released. The admission lease in
                        // `active` is still dropped here, under the mutex and
                        // before that fence wakes userspace.
                        crate::g17_drm::G17PDeferredCompletion::Compute {
                            packet: active.packet.clone(),
                            timestamps,
                        }
                        .defer_into(deferred);
                        kick_deferred_completion_drain_from(deferred, gfx_state, drain_owned);
                        drop(active);
                        if q.released && q.inflight.is_none() {
                            // Retain `binding` and `context`. The firmware still has
                            // this QID installed, so its SKSM entry mapping and
                            // context root may not be dropped here -- see
                            // `rebindable_released_compute_graph`.
                            q.owner = None;
                        }
                    }
                    // An RTKit edge may precede the kick's stamp store. Leave
                    // the job armed; if it has already finished on the GPU,
                    // `retire_pending` re-examines it on the next event of any
                    // QID, and the scheduler timeout still performs one final
                    // DRAM observation before quarantine.
                    Ok(None) => {
                        q.retire_pending = g17p_async_compute_front_finished(manager, q);
                        break;
                    }
                    Err(error) if error == EAGAIN => {
                        q.retire_pending = g17p_async_compute_front_finished(manager, q);
                        break;
                    }
                    Err(error) => {
                        // Zero-duration recovery may retire this queue without
                        // entering quarantine_async_compute. Capture admission
                        // before that path consumes the active packet/lease too.
                        if let (Some(pdev), Some(active)) = (self.pdev.as_ref(), q.inflight.front()) {
                            dev_info!(pdev.as_ref(),
                                "G17P async compute: admission-at-retirement-error qid={} submission={} errno={} in-flight={} leases(compute,render-mask,render-quarantine)={:?}\n",
                                q.queue.queue_id(), active.packet.submission_id(), error.to_errno(),
                                q.inflight.len(), active._lease.admission_snapshot());
                        }
                        // This queue is still firmware-owned and cannot be made
                        // reusable from an error fence. Make the proven
                        // processor-stop recovery reachable by the next client.
                        //
                        // Measured 2026-09-11: this killed a device 90s into a boot
                        // whose recovery had just reported sources=0, dm-slot-mask=0,
                        // both render queues at 1057/1057/1057, job list empty, both
                        // fwctl pointers matching and `final clear acknowledged;
                        // state=0 epoch=1`. The hardware was fine. `g17p_quarantine_scope`
                        // bit 0 already means "retire this queue and fail its client,
                        // leave the session up for everyone else"; that is the same
                        // judgement this route needs, so let the one flag cover both.
                        // ENODATA is the zero-duration retire: a submission that
                        // retired without executing, which `g17p_zero_duration`
                        // documents as the expected shape after a fault-blamed
                        // firmware recovery and whose default of 1 means "fail the
                        // submission but do NOT rebuild the firmware session". The
                        // poller used to return EIO for it, so this arm escalated a
                        // documented-expected outcome into a device-wide crash --
                        // every new client ENODEV, and a rebuild known to end in
                        // apple_rtkit_boot ETIME. The queue is still firmware-owned,
                        // so it is retired exactly as before; only the session-wide
                        // escalation is skipped.
                        let submission_scoped = error == ENODATA;
                        if q.first_kick_watch {
                            q.first_kick_watch = false;
                            crate::g17_recovery_isolation::count(
                                &crate::g17_recovery_isolation::FIRST_KICKS_FAILED);
                            if let Some(pdev) = &self.pdev {
                                dev_err!(pdev.as_ref(),
                                    "G17P recovery isolation: qid={} first kick after return FAILED ({:?})\n",
                                    q.queue.queue_id(), error);
                            }
                        }
                        if submission_scoped {
                            if let Some(pdev) = &self.pdev {
                                dev_info!(pdev.as_ref(),
                                    "G17P async compute: queue retired after a zero-duration retire qid={} -- the client fails, the session stays up\n",
                                    q.queue.queue_id());
                            }
                        } else if crate::g17_config::QUARANTINE_SCOPE & 1 == 0 {
                            crate::g17_live_boot::note_session_recovery_pending(
                                &mut self.session_recovery_pending,
                                self.pdev.as_ref(),
                                "async-compute-completion-unclassifiable",
                            );
                        } else if let Some(pdev) = &self.pdev {
                            dev_err!(pdev.as_ref(),
                                "G17PMARK quarantine-scoped unclassifiable-completion qid={} ({:?}) -- queue retired, client fails, session stays up\n",
                                q.queue.queue_id(), error);
                        }
                        q.retired_by_teardown = false;
                        q.quarantined = true;
                        // Every kick of the queue shares its fate: the queue is no
                        // longer observed, so a kick left armed behind the front
                        // would never signal. Packets, mappings and roots stay
                        // retained with the quarantined queue until processor stop.
                        //
                        // A zero-duration retire is the documented outcome after a
                        // firmware recovery on a live, reset firmware; kicks queued
                        // behind it crossed the same recovery. Return their
                        // admission share instead of raising the device admission
                        // poison: nothing is stuck, and the latch would outlive the
                        // recovery that already cleared it.
                        let mut terminal = None;
                        // Recovery isolation: a queue whose every failed kick
                        // was spared fails those fences only (no VM status, no
                        // VM-wide fan-out) and may return after the witness.
                        let mut all_spared = q.inflight.is_some();
                        // A spared zero-duration queue defers its fences to the
                        // retirement witness: firmware never started these kicks,
                        // and completing them only after the witness needs no
                        // VM-job quarantine (which would refuse the innocent
                        // VM's next submission while its other work drains).
                        let defer = submission_scoped
                            && crate::g17_recovery_isolation::enabled(crate::g17_recovery_isolation::INNOCENT)
                            && q.inflight.is_some()
                            && q.inflight.iter().all(|active| active.packet.completion()
                                .is_ok_and(|completion| completion.recovery_spared()));
                        for active in q.inflight.iter_mut() {
                            if submission_scoped {
                                active._lease.release_retired_admission();
                            } else {
                                active._lease.quarantine();
                            }
                            let completion = active.packet.completion()?;
                            let spared = completion.recovery_spared();
                            all_spared &= spared;
                            if !defer {
                                completion.fail_while_owned(error);
                            }
                            if !spared {
                                terminal.get_or_insert_with(|| completion.status.clone());
                            }
                        }
                        q.spared_quarantine = all_spared && submission_scoped;
                        q.spared_deferred = defer;
                        if all_spared {
                            crate::g17_recovery_isolation::count(
                                &crate::g17_recovery_isolation::FANOUT_SKIPPED);
                            if let Some(pdev) = &self.pdev {
                                dev_info!(pdev.as_ref(),
                                    "G17P recovery isolation: qid={} spared after {:?}; failing its {} kick(s) only, VM status untouched\n",
                                    q.queue.queue_id(), error, q.inflight.len());
                            }
                        }
                        if submission_scoped {
                            // The zero-duration record names the front kick's
                            // exact descriptor, but the ENODATA return precedes
                            // the stamp, counter and ring checks. Keep every
                            // stacked packet and its VM pin until those
                            // queue-local words prove the whole queue retired;
                            // before this, nothing but a processor stop ever
                            // released them, so every recovered client kept
                            // its whole VM.
                            witness_pending = true;
                            q.awaiting_retirement_witness = true;
                        }
                        if let Some(status) = terminal {
                            q.terminal_fanout = Some((status, error));
                        }
                        break;
                    }
                }
            }
        }
        // Finish the acquired batch before failing peers. A later queue may
        // have a valid completion in this same snapshot, and proven success
        // must win over another queue's terminal observation. Per-queue slots
        // retain every failed VM identity without allocation or a QID cap.
        for index in 0..self.async_compute_queues.len() {
            let pending = {
                let q = &mut self.async_compute_queues[index];
                let Some(pending) = q.terminal_fanout.take() else { continue; };
                pending
            };
            self.fail_outstanding_jobs_for_vm_after_compute_observation(&pending.0, pending.1);
        }
        if witness_pending {
            self.release_firmware_retired_quarantined_compute_nested("event");
        }
        Ok(())
    }

    /// Release the packet and VM pin of every quarantined compute command that
    /// firmware has provably retired.
    ///
    /// Before this, a queue quarantined by a zero-duration retire (the shape
    /// every in-flight command takes after a stamp-blamed firmware recovery)
    /// or by a scheduler timeout kept `active` -- and with it the command's
    /// `T8140VmJobGuard` -- until both firmware processors stopped. Scoped
    /// quarantine keeps the session up, so that stop never comes: the guard
    /// keeps `job_lifetime.active` non-zero, file close parks the whole user
    /// range in `close_ranges` and every closed BO in `closed_objects`, and
    /// the client's entire VM stays pinned (~700 MB per recovered bench
    /// process on Neo) for the rest of the boot.
    ///
    /// Release point: the firmware recovery handshake is closed and the
    /// queue-local terminal words show exactly this ticket retired (see
    /// `independent_compute_retired_by_firmware`). That is the witness the
    /// ordinary completion path already requires before it drops the same
    /// packet, minus the shared completion-ring record: the zero-duration
    /// path has already consumed that record, and a timed-out command may
    /// have had its record overwritten in the 24-slot ring. Nothing here
    /// touches the installed graph -- binding, context root, SKSM aliases and
    /// the QID remain retained exactly as before, because firmware still has
    /// the QID installed and sweeps it. A command that is still executing
    /// (runaway shader) never satisfies the test and stays pinned.
    ///
    /// Top-level form: only from an outermost Runtime hold (the event and
    /// recovery workers' entries, a client's prepare/submit), because it may
    /// publish iso4 REPLAY kicks, and publication can itself service firmware
    /// events and recoveries inline. Event, recovery and release paths that
    /// can run nested inside a publication use
    /// `release_firmware_retired_quarantined_compute_nested`.
    fn release_firmware_retired_quarantined_compute(&mut self, reason: &'static str) {
        let replays = self.retire_witnessed_quarantined_compute(reason, true);
        for (owner, kept) in replays.into_iter() {
            self.replay_spared_compute(owner, kept);
        }
    }

    /// The witness from a context that may be nested inside a publication
    /// (event service, recovery, owner release). Identical, except that a
    /// queue whose witnessed kicks would be replayed is left untouched --
    /// still quarantined and awaiting its witness, packets, pins and fences
    /// as they are -- for the next top-level witness, which replays it. This
    /// keeps publication out of the event/recovery call chain (kernel stack
    /// depth; see tests/audit_g17_publication_stack.py).
    fn release_firmware_retired_quarantined_compute_nested(&mut self, reason: &'static str) {
        let replays = self.retire_witnessed_quarantined_compute(reason, false);
        // Never collected without `replay`; fail any as the pre-REPLAY path
        // would rather than drop a packet with its fence unsignalled.
        for (_, kept) in replays.into_iter() {
            for replay in kept.into_iter() {
                if let Ok(completion) = replay.packet.completion() {
                    completion.complete(Err(ENODATA));
                }
            }
        }
    }

    /// One witness scan. With `replay` false, replay candidates are skipped
    /// (see `release_firmware_retired_quarantined_compute_nested`).
    #[inline(never)]
    fn retire_witnessed_quarantined_compute(
        &mut self, reason: &'static str, replay: bool,
    ) -> KVec<(u64, KVec<G17PAsyncComputeReplay>)> {
        // iso4 REPLAY: never-started spared kicks kept across the witness, per
        // owner and in publication order, published again after this scan.
        let mut replays: KVec<(u64, KVec<G17PAsyncComputeReplay>)> = KVec::new();
        if !self.async_compute_queues.iter().any(|q| q.awaiting_retirement_witness) {
            return replays;
        }
        let pdev = self.pdev.as_ref();
        let Some(manager) = self.manager.as_mut() else { return replays; };
        match manager.firmware_recovery_handshake_snapshot() {
            Ok(handshake) if handshake.state == 0 => {}
            _ => return replays,
        }
        for q in self.async_compute_queues.iter_mut() {
            if !q.awaiting_retirement_witness {
                continue;
            }
            // Kicks of one QID retire in publication order, so the NEWEST
            // ticket's target and item-ring producer cover every older kick
            // still stacked on the queue; every stacked kick must still be
            // identifiable before any of them is released.
            let identity = q.inflight.iter().all(|active| {
                active.retirement.is_some_and(|ticket| {
                    q.owner == Some(ticket.owner)
                        && active.packet.submission_id() == ticket.submission
                        && active.packet.context().context_id() == ticket.context
                })
            });
            let newest = q.inflight.back().and_then(|active| active.retirement);
            let (true, Some(ticket)) = (identity, newest) else {
                // Not provable any more: keep everything until processor stop.
                q.awaiting_retirement_witness = false;
                fail_spared_deferred(q, EIO);
                if let Some(pdev) = pdev {
                    dev_err!(pdev.as_ref(),
                        "G17P async compute: retirement-witness qid={} lost its ticket identity; retaining {} command(s) until processor stop\n",
                        q.queue.queue_id(), q.inflight.len());
                }
                continue;
            };
            let in_flight = q.inflight.len() as u32;
            match manager.independent_compute_retired_by_firmware(
                &mut q.queue, &ticket.firmware, in_flight,
            ) {
                Ok(false) => {}
                Ok(true) => {
                    // iso4 REPLAY: a Vulkan client cannot be told that one
                    // command buffer was dropped -- any error is device loss
                    // or silently wrong results. When every kick this queue
                    // lost was spared AND its own completion words prove the
                    // firmware retired it without executing (start == end,
                    // see `replay_witness_words`), keep the packets and
                    // publish them again, in order, once the queue is back in
                    // service. The fences stay unsignalled throughout. If any
                    // kick ran, the sequence cannot be repeated safely (a later
                    // kick may already have consumed or overwritten an earlier
                    // one's state), so all fail exactly as before.
                    let decision = crate::g17_recovery_isolation::compute_replay_decision(
                        crate::g17_recovery_isolation::mask(),
                        crate::g17_recovery_isolation::ComputeReplayQueue {
                            spared_deferred: q.spared_deferred,
                            spared_quarantine: q.spared_quarantine,
                            released: q.released,
                            owner_matches: q.owner == Some(ticket.owner),
                            native_consumer: q.inflight.iter().any(|active| active.packet.completion()
                                .is_ok_and(|completion| completion.has_native_consumer())),
                        },
                        q.inflight.iter().map(|active| active.packet.completion().ok()
                            .and_then(|completion| completion.replay_witness_words())),
                    );
                    if !replay && decision == crate::g17_recovery_isolation::ComputeReplay::Replay {
                        // Nested caller: leave this queue for the next
                        // top-level witness, which replays it. Nothing has
                        // changed yet; the witness above only read.
                        continue;
                    }
                    q.awaiting_retirement_witness = false;
                    let replay_candidate =
                        decision != crate::g17_recovery_isolation::ComputeReplay::NotCandidate;
                    let mut unstarted = decision == crate::g17_recovery_isolation::ComputeReplay::Replay;
                    let mut kept: KVec<G17PAsyncComputeReplay> = KVec::new();
                    if unstarted && (kept.reserve(q.inflight.len(), GFP_KERNEL).is_err()
                        || replays.reserve(1, GFP_KERNEL).is_err())
                    {
                        unstarted = false;
                    }
                    if replay_candidate && !unstarted {
                        let consumed = q.inflight.iter().any(|active| active.packet.completion()
                            .is_ok_and(|completion| completion.has_native_consumer()));
                        if consumed {
                            crate::g17_recovery_isolation::count(
                                &crate::g17_recovery_isolation::REPLAY_REFUSED_CONSUMER);
                            if let Some(pdev) = pdev {
                                dev_info!(pdev.as_ref(),
                                    "G17P recovery isolation: qid={} replay refused: a render was published against one of its {} cancelled kick(s)\n",
                                    q.queue.queue_id(), q.inflight.len());
                            }
                        }
                        for _ in q.inflight.iter() {
                            crate::g17_recovery_isolation::count(
                                &crate::g17_recovery_isolation::REPLAY_REFUSED_COMPUTE);
                        }
                    }
                    let mut released = 0usize;
                    while let Some(active) = q.inflight.take() {
                        let packet = active.packet;
                        let lease = active._lease;
                        if unstarted {
                            // Capacity reserved above; the packet keeps its
                            // VM pin and its admission guard for the replay.
                            if let Err(rejected) = kept.push_within_capacity(
                                G17PAsyncComputeReplay { packet, lease })
                            {
                                if let Ok(completion) = rejected.0.packet.completion() {
                                    completion.complete(Err(ENODATA));
                                }
                                drop(rejected);
                            }
                            continue;
                        }
                        if let Ok(completion) = packet.completion() {
                            if q.spared_deferred {
                                // Recovery isolation: firmware retired this
                                // never-started kick (the witness above); fail
                                // its fence now. A spared completion records no
                                // VM status and drops its pin without quarantine.
                                completion.complete(Err(ENODATA));
                            } else {
                                // The fence already carries the error from
                                // `fail_while_owned`; only the VM pin is outstanding.
                                completion.release_after_retirement();
                            }
                        }
                        // A quarantined queue admits nothing new, so the
                        // retired-lease FIFO (same bound as the in-flight
                        // FIFO) always has room for the lease leaving it.
                        if let Err(lease) = q.retired_leases.push(lease) {
                            if let Some(pdev) = pdev {
                                dev_err!(pdev.as_ref(),
                                    "G17P async compute: retirement-witness qid={} retired-lease FIFO full; dropping one admission guard\n",
                                    q.queue.queue_id());
                            }
                            drop(lease);
                        }
                        released += 1;
                        drop(packet);
                    }
                    q.spared_deferred = false;
                    if let Some(pdev) = pdev {
                        dev_info!(pdev.as_ref(),
                            "G17P async compute: quarantined qid={} owner={} newest-submission={} retired by firmware ({}) -- released {} packet(s) and their VM pin, kept {} never-started for replay; the graph stays quarantined\n",
                            q.queue.queue_id(), ticket.owner, ticket.submission, reason, released, kept.len());
                    }
                    let mut returned = false;
                    // Recovery isolation: the cancelled kicks were all spared
                    // and the witness proves firmware retired every one of
                    // them, which is exactly the state an ordinary retirement
                    // of the same kicks would leave. Settle the host-side
                    // accounting those retirements would have settled and
                    // return the queue to its owner.
                    if q.spared_quarantine
                        && crate::g17_recovery_isolation::enabled(crate::g17_recovery_isolation::INNOCENT)
                    {
                        q.spared_quarantine = false;
                        match manager.return_witnessed_compute_queue(&mut q.queue, in_flight) {
                            Ok(()) => {
                                returned = true;
                                q.quarantined = false;
                                q.retire_pending = false;
                                q.retirement_ready = true;
                                // Every publication made with the replacement
                                // binding is among the witnessed kicks.
                                if q.previous_binding.as_ref().is_some_and(|previous|
                                    previous.publication.is_some())
                                {
                                    drop(q.previous_binding.take());
                                }
                                // Their admission shares were already released.
                                while let Some(lease) = q.retired_leases.take() {
                                    drop(lease);
                                }
                                q.first_kick_watch = true;
                                crate::g17_recovery_isolation::count(
                                    &crate::g17_recovery_isolation::QUEUES_RETURNED);
                                if let Some(pdev) = pdev {
                                    dev_info!(pdev.as_ref(),
                                        "G17P recovery isolation: qid={} owner={} returned to service after a spared recovery ({} kick(s) witnessed retired)\n",
                                        q.queue.queue_id(), ticket.owner, in_flight);
                                }
                            }
                            Err(error) => {
                                crate::g17_recovery_isolation::count(
                                    &crate::g17_recovery_isolation::QUEUE_RETURN_FAILED);
                                if let Some(pdev) = pdev {
                                    dev_err!(pdev.as_ref(),
                                        "G17P recovery isolation: qid={} could not be returned to service ({:?}); it stays quarantined\n",
                                        q.queue.queue_id(), error);
                                }
                            }
                        }
                    }
                    if !kept.is_empty() {
                        // Replay only onto a queue that is back in service;
                        // the publication itself happens after this scan.
                        let kept = if returned {
                            // Capacity reserved with `kept`'s.
                            match replays.push_within_capacity((ticket.owner, kept)) {
                                Ok(()) => None,
                                Err(rejected) => Some(rejected.0.1),
                            }
                        } else {
                            Some(kept)
                        };
                        if let Some(kept) = kept {
                            // Exactly the pre-REPLAY outcome for these kicks.
                            for replay in kept.into_iter() {
                                crate::g17_recovery_isolation::count(
                                    &crate::g17_recovery_isolation::REPLAY_REFUSED_COMPUTE);
                                if let Ok(completion) = replay.packet.completion() {
                                    completion.complete(Err(ENODATA));
                                }
                                if let Err(lease) = q.retired_leases.push(replay.lease) {
                                    drop(lease);
                                }
                            }
                        }
                    }
                }
                Err(error) => {
                    q.awaiting_retirement_witness = false;
                    fail_spared_deferred(q, EIO);
                    if let Some(pdev) = pdev {
                        dev_err!(pdev.as_ref(),
                            "G17P async compute: retirement-witness qid={} invariant failed ({:?}); retaining {} command(s) until processor stop\n",
                            q.queue.queue_id(), error, q.inflight.len());
                    }
                }
            }
        }
        replays
    }

    /// `g17p_recovery_isolation` REPLAY: publish again, oldest first, the
    /// spared kicks `owner`'s queue lost to another client's recovery that
    /// firmware provably retired without executing. Each keeps its fence (still
    /// unsignalled), its submission identity and its CLASS_SPARED class, so a
    /// failure anywhere below is still never charged to the VM. The first kick
    /// that cannot be published fails, and every later one with it: publishing
    /// a successor without its predecessor would reorder the queue.
    fn replay_spared_compute(&mut self, owner: u64, kept: KVec<G17PAsyncComputeReplay>) {
        let mut failed: Option<Error> = None;
        for replay in kept.into_iter() {
            let G17PAsyncComputeReplay { packet, mut lease } = replay;
            let Ok(completion) = packet.completion() else {
                crate::g17_recovery_isolation::count(
                    &crate::g17_recovery_isolation::REPLAY_REFUSED_COMPUTE);
                continue;
            };
            let prepared = match failed {
                Some(error) => Err(error),
                None if !completion.begin_replay() => Err(EAGAIN),
                None => completion.reset_for_replay(),
            };
            // Open question recorded per replay: whether firmware counted the
            // cancelled kick in its work-state TSQ completed count. The replay
            // adds one more submitted kick exactly as any publication does.
            let tsq = completion.work_state().and_then(|state| state.tsq_counters()).ok();
            let result = match prepared {
                Ok(()) => {
                    lease.rearm_admission();
                    self.publish_async_compute(owner, packet.clone(), lease)
                }
                Err(error) => { drop(lease); Err(error) }
            };
            match result {
                Ok(()) => {
                    crate::g17_recovery_isolation::count(
                        &crate::g17_recovery_isolation::REPLAYED_COMPUTE);
                    if let Some(pdev) = self.pdev.as_ref() {
                        dev_info!(pdev.as_ref(),
                            "G17P recovery isolation: replayed never-started compute owner={} submission={} (replay {}) tsq(submitted,completed)-before={:?}\n",
                            owner, packet.submission_id(), completion.replay_count(), tsq);
                    }
                }
                Err(error) => {
                    crate::g17_recovery_isolation::count(
                        &crate::g17_recovery_isolation::REPLAY_REFUSED_COMPUTE);
                    // A publication that reached firmware is retained (and
                    // failed, spared) by `quarantine_async_compute`; anything
                    // else is ours to fail exactly as the witness used to.
                    let retained = self.async_compute_queues.iter().any(|q|
                        q.inflight.iter().any(|active| Arc::ptr_eq(&active.packet, &packet)));
                    if !retained {
                        completion.complete(Err(ENODATA));
                    }
                    if failed.is_none() {
                        if let Some(pdev) = self.pdev.as_ref() {
                            dev_warn!(pdev.as_ref(),
                                "G17P recovery isolation: compute replay owner={} submission={} refused ({:?}); it and its successors fail as before\n",
                                owner, packet.submission_id(), error);
                        }
                    }
                    failed = Some(error);
                }
            }
        }
    }

    /// Settle one VM after its compute completion state was already observed.
    /// This split lets the completion dispatcher fan out a poll failure after
    /// releasing the manager borrow, without recursion or a second ring scan.
    fn fail_outstanding_jobs_for_vm_after_compute_observation(
        &mut self,
        status: &Arc<crate::g17_status::VmStatus>,
        error: Error,
    ) -> usize {
        for slot in 0..g17_resources::G17PRenderQueuePair::SLOT_COUNT {
            let matches = self.pending_render[slot as usize]
                .as_ref()
                .is_some_and(|completion| completion.belongs_to_vm(status));
            if !matches {
                continue;
            }
            if let Ok(Some(observed)) = self.poll_deferred_render_completion(slot) {
                let _ = self.settle_deferred_render_completion(slot, observed);
            }
        }

        let mut failed = 0usize;
        for slot in 0..g17_resources::G17PRenderQueuePair::SLOT_COUNT {
            let matches = self.pending_render[slot as usize]
                .as_ref()
                .is_some_and(|completion| completion.belongs_to_vm(status));
            if !matches {
                continue;
            }
            // A failure scoped to ONE client's VM must stay scoped. This used
            // to raise `session_recovery_pending` ("render-completion-failed-
            // with-vm") for every sibling render of the failed VM, which turned
            // the loss of one client into a device-wide `session-crashed` -- and
            // on G17P a crashed session never rebuilds (4/4 RTKit HELLO ETIME),
            // so the GPU was dead for everyone until reboot. Measured on Neo
            // 2026-09-26 08:54 with the render-loss settle already scoped: the
            // client's OTHER pair took this path and killed the session anyway.
            // `fail_while_owned` below already quarantines the slot and pins the
            // mappings firmware may still name; nothing here needs a rebuild.
            // Every queued member has the same pair owner. Settle every fence,
            // but retain all owners until the existing whole-pair drain proof
            // (or processor stop) permits release. Leaving a failed head in the
            // normal FIFO strands successors and bypasses quarantine reclaim.
            self.render_fence_state.settle(slot);
            self.render_event_masks[slot as usize] = [0; 2];
            if let Some(completion) = self.pending_render[slot as usize].take() {
                failed += usize::from(self.quarantine_failed_render_completion(completion, error));
            }
            while let Some(completion) = self.pending_render_queued[slot as usize].take() {
                failed += usize::from(self.quarantine_failed_render_completion(completion, error));
            }
            if let Some(pdev) = &self.pdev {
                dev_err!(
                    pdev.as_ref(),
                    "G17P render: FAILING slot={} after its VM was lost ({:?})\n",
                    slot,
                    error,
                );
            }
        }

        for index in 0..self.async_compute_queues.len() {
            let identity = {
                let q = &self.async_compute_queues[index];
                if q.quarantined {
                    None
                } else {
                    // Every in-flight kick of a queue belongs to its one owner.
                    q.inflight.iter().find_map(|active| {
                        let completion = active.packet.completion().ok()?;
                        if !Arc::ptr_eq(&completion.status, status) {
                            return None;
                        }
                        Some((
                            active.packet.submission_id(),
                            q.queue.queue_id(),
                            active.packet.context().context_id(),
                        ))
                    })
                }
            };
            let Some((submission, queue_id, context)) = identity else { continue };
            let in_flight = {
                // Same rule as the render sweep above: quarantining this
                // client's compute queue (and its admission lease, which
                // QUARANTINE_SCOPE bit 0 keeps client-scoped) is the whole
                // containment. It must not declare the session crashed
                // (first-submit hang, measured on Neo 2026-09-26 08:54: the
                // sibling escalation killed a healthy session).
                let q = &mut self.async_compute_queues[index];
                q.quarantined = true;
                // Normal completion polling skips quarantined queues. Keep
                // the same ticketed retirement path as a scheduler timeout,
                // or a sibling's failure pins this VM until processor stop
                // even after firmware has retired every published kick.
                q.awaiting_retirement_witness =
                    q.inflight.iter().all(|active| active.retirement.is_some());
                for active in q.inflight.iter() {
                    active._lease.quarantine();
                    if let Ok(completion) = active.packet.completion() {
                        completion.fail_while_owned(error);
                    }
                }
                q.inflight.len()
            };
            failed += in_flight;
            if let Some(pdev) = &self.pdev {
                dev_err!(
                    pdev.as_ref(),
                    "G17P async compute: FAILING submission={} (+{} queued) qid={} context={} after its VM was lost ({:?})\n",
                    submission,
                    in_flight.saturating_sub(1),
                    queue_id,
                    context,
                    error,
                );
            }
        }
        failed
    }

    /// Fail every outstanding render or compute job that belongs to one VM.
    ///
    /// A submission that fails terminally loses the client's device: Mesa turns
    /// it into `VK_ERROR_DEVICE_LOST` and tears the context down. Work this VM
    /// had ALREADY handed to the hardware is not covered by the submit ioctl's
    /// output-fence guard -- that guard only promises a fence for the ioctl it
    /// runs in -- and once the VM is dead nothing will ever complete it. Its
    /// client then waits on that syncobj forever, which is how one refused
    /// frame ends with gnome-shell alive but permanently stopped in
    /// `drm_syncobj_array_wait_timeout`.
    ///
    /// Two invariants this must not break:
    ///
    ///  * The packet is NOT released. Firmware may still be executing it, and
    ///    freeing its mappings and root under a live GPU turns a lost client
    ///    into a DMA fault. The queue is quarantined and the packet stays
    ///    retained, exactly as the completion-service and scheduler-timeout
    ///    paths already do.
    ///  * The error is recorded on the VM's sticky status BEFORE the fence
    ///    signals. A DRM syncobj treats an error fence as signalled, so a
    ///    waiter wakes and immediately asks why; if the status were still clean
    ///    it would read the destination buffer as if the job had run.
    ///
    /// Jobs belonging to other VMs are untouched: they are independently
    /// rooted, their client is healthy, and completing them here would be the
    /// same "signalled but never executed" lie for someone else.
    pub(crate) fn fail_outstanding_jobs_for_vm(
        &mut self,
        status: &Arc<crate::g17_status::VmStatus>,
        error: Error,
    ) -> usize {
        // Sticky first: both completion kinds publish a signalled error fence,
        // and the woken client must already be able to observe why its VM died.
        status.record(error.to_errno());

        // A completion may already be sitting in DRAM; take it rather than
        // discarding a job the GPU actually finished. Render and compute have
        // independent completion stores, so observe both before the sweep.
        let _ = self.service_async_compute_completions();
        self.fail_outstanding_jobs_for_vm_after_compute_observation(status, error)
    }

    /// Give up a client's claim on its compute graph.
    ///
    /// Reached from `G17PQueue::drop`, i.e. ordinary `queue_destroy` and file
    /// close -- no processor stop, no session rebuild. This used to drop
    /// `binding` and `context` here, which released the client's SKSM entry
    /// `KernelMapping` and, through `T8140ContextRootLease::drop`, retracted its
    /// TTB root pair out of the UAT context table. The firmware, meanwhile,
    /// still had the QID installed: `unregister_compute_sksm_queue` is not even
    /// called on this path, and would be a suppressed no-op if it were, because
    /// `g17p_sksm_mmio` defaults to 0. Its periodic per-slot sweep then read
    /// byte 0 of a record whose page had just been unmapped, which is a data
    /// abort inside the GFX firmware and the end of the session.
    ///
    /// `g17_live_boot::release_compute_owner` states the rule for the
    /// synchronous path in as many words -- "freeing the page tables here would
    /// leave the firmware's compute context pointing at memory the allocator
    /// has taken back" -- and releases the binding immediately before the next
    /// bind reinstalls those roots, or after both CPUs have stopped. This path
    /// gives up the claim but retains everything firmware can still name.
    /// A prepared replacement is also retained until its exact successful
    /// publication completes; preparing aliases alone does not switch roots.
    pub(crate) fn release_async_compute_queue(&mut self, owner: u64) {
        // A departing client is the usual last event for a recovered queue:
        // its error fence woke it, and its file close has just parked the VM
        // teardown behind the quarantined command's job guard.
        self.release_firmware_retired_quarantined_compute_nested("owner-release");
        if let Some(index) = self.async_compute_queues.iter().position(|q| q.owner == Some(owner)) {
            let q = &self.async_compute_queues[index];
            if q.inflight.is_none() && !q.quarantined && q.previous_binding.is_none()
                && self.manager.as_mut().is_some_and(|manager|
                    manager.cancel_unpublished_compute_queue(&q.queue))
            {
                // No firmware-visible publication used this exact lease. Its
                // private graph and client aliases can be dropped immediately.
                // Published or ambiguous owners still follow retention below.
                self.drop_unpublished_compute_queue(index);
                return;
            }
            let q = &mut self.async_compute_queues[index];
            q.released = true;
            if q.inflight.is_none() && !q.quarantined {
                // A client may disappear before its first publication (or
                // after a definitely-unpublished failure). Cancel only that
                // preparation, restoring the still-installed old binding.
                if q.previous_binding.as_ref().is_some_and(|previous|
                    previous.publication.is_none()
                ) {
                    let previous = q.previous_binding.as_ref().unwrap();
                    let restored = q.queue.bind_logical_channel_control(&previous._context)
                        .and_then(|()| {
                            if previous._binding.is_none() {
                                q.binding.as_mut().ok_or(EIO)?
                                    .retarget_logical_context(previous._context.clone())?;
                            }
                            Ok(())
                        });
                    if restored.is_err() {
                        // Keep both owners and their mappings if restoring an
                        // unpublished handoff cannot be proven. No reuse.
                        q.quarantined = true;
                        q.retired_by_teardown = true;
                        return;
                    }
                    if let Some(previous) = q.previous_binding.take() {
                        if let Some(binding) = previous._binding {
                            q.binding = Some(binding);
                        }
                        q.context = Some(previous._context);
                    }
                    q.retirement_ready = true;
                }
                if q.retirement_ready {
                    q.owner = None;
                }
            }
        }
    }

    /// Remove and drop one never-published queue graph. Out of line: moving
    /// the whole `G17PAsyncComputeQueue` out of the table stages several KiB
    /// on the stack, which must not be part of every owner release's frame.
    #[cold]
    #[inline(never)]
    fn drop_unpublished_compute_queue(&mut self, index: usize) {
        drop(self.async_compute_queues.remove(index));
    }

    /// Cancel only the scheduler waiter. A published command must remain in
    /// the ordinary completion scan until its exact retirement is observed.
    /// No global lease poison or hardware-failure fanout belongs on this path.
    pub(crate) fn cancel_async_compute(&mut self, owner: u64, submission: u64) -> bool {
        let _ = self.service_async_compute_completions();
        let Some(q) = self.async_compute_queues.iter_mut().find(|q| q.owner == Some(owner)) else {
            return false;
        };
        // Any published kick may be pending at teardown, not only the oldest.
        let Some(active) = q.inflight.iter().find(|active|
            active.packet.submission_id() == submission) else { return false; };
        // A real hardware fault already owns its quarantine decision. Queue
        // teardown must neither undo that nor signal the same fence twice.
        if q.quarantined {
            if q.spared_deferred {
                q.spared_deferred = false;
                for queued in q.inflight.iter() {
                    if let Ok(completion) = queued.packet.completion() {
                        completion.cancel_while_owned();
                    }
                }
            }
            return true;
        }
        q.released = true;
        q.retired_by_teardown = true;
        if let Ok(completion) = active.packet.completion() {
            completion.cancel_while_owned();
        }
        if let Some(pdev) = &self.pdev {
            dev_info!(pdev.as_ref(),
                "G17P async compute: client waiter cancelled owner={} qid={} submission={}; retaining resources until exact retirement\n",
                owner, q.queue.queue_id(), submission);
        }
        true
    }

    /// `g17p_recovery_isolation` bit7 VICTIM_DEFER: is this compute timeout
    /// head-of-line blocking behind another VM's older unfinished kick? On
    /// `Spare` every in-flight kick of the queue is classified spared, so the
    /// quarantine that follows fails their fences without VM status.
    pub(crate) fn classify_compute_timeout(&mut self, owner: u64, submission: u64, renewals: u8)
        -> crate::g17_recovery_isolation::VictimTimeout
    {
        use crate::g17_recovery_isolation as iso;
        if !iso::enabled(iso::VICTIM_DEFER) { return iso::VictimTimeout::Own; }
        // The same final DRAM-only retirement pass the quarantine takes.
        let _ = self.service_async_compute_completions();
        let Some(index) = self.async_compute_queues.iter().position(|q| q.owner == Some(owner)) else {
            return iso::VictimTimeout::Own;
        };
        let q = &self.async_compute_queues[index];
        if q.quarantined || !q.inflight.iter().any(|active| active.packet.submission_id() == submission) {
            return iso::VictimTimeout::Own;
        }
        // The queue's own oldest kick: if it already finished, nothing blocks it.
        let Some(front) = q.inflight.front().filter(|front| front.observed.is_none()) else {
            return iso::VictimTimeout::Own;
        };
        let (own_vm, own_published, qid) = (front.packet.context().vm_id(), front.published, q.queue.queue_id());
        // A queue quarantined without retirement tickets keeps its kicks until
        // processor stop whatever firmware did with them: it proves nothing
        // about the pipe now, so it never excuses a timeout.
        let head = |other: &G17PAsyncComputeQueue| other.inflight.front()
            .filter(|_| !other.quarantined || other.awaiting_retirement_witness)
            .map(|head| iso::ComputeHead {
                vm: head.packet.context().vm_id(), published: head.published, finished: head.observed.is_some(),
            });
        let (decision, blocker) = iso::victim_timeout(iso::mask(), own_vm, own_published, renewals,
            self.async_compute_queues.iter().enumerate()
                .filter(|(other, _)| *other != index)
                .filter_map(|(_, other)| head(other)));
        if decision == iso::VictimTimeout::Own { return decision; }
        let blocker = self.async_compute_queues.iter()
            .find(|other| other.inflight.front().is_some_and(|head| Some(head.published) == blocker))
            .map(|other| (other.queue.queue_id(), other.owner, other.quarantined));
        if decision == iso::VictimTimeout::Spare {
            iso::count(&iso::VICTIM_SPARED);
            for active in self.async_compute_queues[index].inflight.iter() {
                if let Ok(completion) = active.packet.completion() {
                    completion.set_recovery_class(iso::CLASS_SPARED);
                }
            }
        } else {
            iso::count(&iso::VICTIM_DEFERRED);
        }
        // One line when the deferral starts and one if it runs out.
        if renewals == 0 || decision == iso::VictimTimeout::Spare {
            let verdict = if decision == iso::VictimTimeout::Spare {
                "still blocked after the renewal budget: failing its kicks spared, VM status untouched"
            } else { "victim, timeout renewed" };
            if let Some(pdev) = &self.pdev {
                dev_warn!(pdev.as_ref(),
                    "G17P recovery isolation: compute timeout owner={} qid={} vm={} submission={} blocked behind (qid,owner,quarantined)={:?}; {}\n",
                    owner, qid, own_vm, submission, blocker, verdict);
            }
        }
        decision
    }

    pub(crate) fn quarantine_async_compute(&mut self, owner: u64, submission: u64, error: Error) -> bool {
        if error == ECANCELED {
            return self.cancel_async_compute(owner, submission);
        }
        // One final DRAM-only retirement attempt distinguishes an event-service
        // race from a job whose completion really is absent. Never throw away
        // a proven successful hardware completion merely because a timer ran.
        let _ = self.service_async_compute_completions();
        let mut matched = false;
        let mut terminal_status = None;
        let mut diagnostic_qids = 0u128;
        if let Some(q) = self.async_compute_queues.iter_mut().find(|q| q.owner == Some(owner)) {
            // A spared zero-duration queue awaiting its witness has not
            // signalled its fences yet; the watchdog fails them now
            // (retaining their pins, still without VM status).
            if q.inflight.iter().any(|active| active.packet.submission_id() == submission) {
                fail_spared_deferred(q, error);
            }
            if let Some(active) = q.inflight.iter().find(|active|
                active.packet.submission_id() == submission)
            {
                {
                    matched = true;
                    if !q.quarantined {
                        if let Some(pdev) = &self.pdev {
                            dev_info!(pdev.as_ref(),
                                "G17P async compute: admission-at-quarantine qid={} submission={} leases(compute,render-mask,render-quarantine)={:?}\n",
                                q.queue.queue_id(), submission,
                                active._lease.admission_snapshot());
                        }
                        // Client cancellation took the separate retention path
                        // above. A real failure still poisons hardware admission
                        // and retains everything firmware may reference.
                        q.retired_by_teardown = false;
                        if crate::g17_config::QUARANTINE_SCOPE & 1 == 0 {
                            crate::g17_live_boot::note_session_recovery_pending(
                                &mut self.session_recovery_pending,
                                self.pdev.as_ref(),
                                "async-compute-submission-quarantined",
                            );
                        } else if let Some(pdev) = &self.pdev {
                            dev_err!(pdev.as_ref(),
                                "G17PMARK quarantine-scoped owner={} qid={} -- this queue is retired and its client fails, but the session stays up for every other client\n",
                                owner, q.queue.queue_id());
                        }
                        q.quarantined = true;
                        // A command with a publication ticket can still be
                        // retired by firmware later (a late completion, or a
                        // recovery that retires it); re-observe it so its VM
                        // pin is not held until a processor stop that scoped
                        // quarantine never performs. Without a ticket the
                        // publication itself is unproven: retain as before.
                        q.awaiting_retirement_witness =
                            q.inflight.iter().all(|queued| queued.retirement.is_some());
                        active._lease.quarantine();
                        if let Some(pdev) = &self.pdev {
                            dev_err!(pdev.as_ref(),
                                "G17P async compute: QUARANTINE submission={} owner={} qid={} context={} vm={} root={:#x} root-current={} global-phase={:?} error={:?}\n",
                                submission, owner, q.queue.queue_id(), active.packet.context().context_id(),
                                active.packet.context().vm_id(), active.packet.context().root(),
                                active.packet.context().is_current(),
                                self.gfx_state.as_ref().map(|s| s.submission_phase.load(Ordering::Acquire)), error);
                        }
                        if let Some(pdev) = &self.pdev {
                            // This lease owns the firmware's real shared TSQ.
                            // Legacy per-graph counters below are not its counts.
                            dev_err!(pdev.as_ref(),
                                "G17P async-timeout shared-state: qid={} word={:#x} node={:x?} counts(submitted,completed)={:x?} list(head,links)={:x?}\n",
                                q.queue.queue_id(), active._work_state.word(),
                                active._work_state.tsq_gpu_va(),
                                active._work_state.tsq_counters(),
                                active._work_state.tsq_list_diagnostic());
                        }
                        if let Some(manager) = self.manager.as_mut() {
                            // Sample the actual retained owner, not the legacy
                            // boot queue. Do this before the completion dump's
                            // console output delays the cursor observation.
                            diagnostic_qids = manager.log_async_compute_timeout(&mut q.queue);
                            let _ = manager.dump_queue_compute_completion_records("async-timeout", &mut q.queue);
                        }
                        if let Ok(completion) = active.packet.completion() {
                            terminal_status = Some(completion.status.clone());
                        }
                        // Recovery isolation: when every kick on the queue was
                        // spared, fail their fences only and let the witness
                        // return the queue; otherwise today's VM loss.
                        let all_spared = q.inflight.iter().all(|queued|
                            queued.packet.completion().is_ok_and(|completion|
                                completion.recovery_spared()));
                        q.spared_quarantine = all_spared && q.awaiting_retirement_witness;
                        if all_spared {
                            terminal_status = None;
                            crate::g17_recovery_isolation::count(
                                &crate::g17_recovery_isolation::FANOUT_SKIPPED);
                        }
                        // The quarantined queue is never observed again, so
                        // every kick queued on it -- older or newer than the one
                        // that timed out -- fails now, while its packet and
                        // mappings stay retained until processor stop.
                        for queued in q.inflight.iter() {
                            if queued.packet.submission_id() != submission {
                                queued._lease.quarantine();
                            }
                            if let Ok(completion) = queued.packet.completion() {
                                completion.fail_while_owned(error);
                            }
                        }
                    }
                }
            }
        }
        if diagnostic_qids != 0 {
            self.log_timeout_scoreboard_after_power_hold(diagnostic_qids);
        }
        if let Some(status) = terminal_status {
            self.fail_outstanding_jobs_for_vm(&status, error);
        }
        matched
    }
}

// ---- g17p_recovery_isolation (see g17_recovery_isolation.rs) ---------------
impl G17PLiveRuntime {
    /// Classify every in-flight render and compute completion for the recovery
    /// being serviced: completions of a VM that owns a guilty queue keep
    /// today's behaviour (CLASS_GUILTY, unless the opt-in grace spares it);
    /// every other VM's completions are CLASS_SPARED. Returns the QID mask of
    /// queues the firmware listed as cancelled whose owners were spared.
    ///
    /// Fail-safe: if the guilty set is empty or any guilty QID has no
    /// in-flight owner the host can name, nothing is marked (today's
    /// behaviour for this recovery).
    fn classify_recovery_victims(&mut self, blamed_stamp: Option<u8>) -> (u128, u128) {
        use crate::g17_recovery_isolation as iso;
        iso::count(&iso::RECOVERIES);
        let mut table = [iso::RecoveryEntry { qid: 0, flags: 0, stamp: 0 }; iso::MAX_ENTRIES];
        let used = iso::entries(&mut table);
        let (firmware_guilty_qids, listed_qids, cancelled_qids) = iso::qid_masks(&table[..used], blamed_stamp);
        // bit5: re-attribute when the firmware's lowest-slot blame names a
        // pass that never started (see g17_recovery_isolation).
        let mut sources = [iso::FaultSource::default(); iso::MAX_SOURCES];
        let (reason, fault_mask, source_count) = iso::sources(&mut sources);
        let mut attribution = iso::Attribution::Firmware;
        if iso::enabled(iso::ATTRIBUTION) && source_count != 0 {
            for source in sources[..source_count].iter_mut() {
                let (Some(qid), Some(manager)) = (source.qid, self.manager.as_mut()) else { continue };
                if let Some(pair) = manager.render_slot_for_qid(qid) {
                    source.started = manager.render_pass_started(pair, source.stamp);
                }
            }
            attribution = iso::attribute(reason, blamed_stamp, fault_mask, &sources[..source_count]);
            if let Some(pdev) = self.pdev.as_ref() {
                for source in sources[..source_count].iter() {
                    dev_info!(pdev.as_ref(),
                        "G17P recovery isolation: fault source slot={} qid={:?} stamp={:#x} dm1-monitor={} pass-started={:?}\n",
                        source.slot, source.qid, source.stamp, source.monitor_state, source.started);
                }
                dev_info!(pdev.as_ref(),
                    "G17P recovery isolation: attribution reason={} mask={:#x} firmware-guilty-qids={:#x} -> {:?}\n",
                    reason, fault_mask, firmware_guilty_qids, attribution);
            }
        }
        let guilty_qids = match attribution {
            iso::Attribution::Firmware => firmware_guilty_qids,
            iso::Attribution::Reattributed(qids) => { iso::count(&iso::REATTRIBUTED); qids }
            iso::Attribution::NoCulprit => { iso::count(&iso::NO_CULPRIT); 0 }
        };
        let no_culprit = matches!(attribution, iso::Attribution::NoCulprit);

        // Resolve every guilty QID to the VM statuses of its in-flight work.
        const MAX_VMS: usize = 8;
        let mut guilty: [Option<Arc<crate::g17_status::VmStatus>>; MAX_VMS] = Default::default();
        let mut guilty_count = 0usize;
        let mut unresolved = guilty_qids == 0 && !no_culprit;
        // v2 collateral: a guilty QID whose OLDEST in-flight work was already
        // spared by an earlier recovery is that earlier recovery's collateral
        // (iso-b1 03:18:24/25: VM 57's kick 0x16110 was queued behind three
        // kicks recovery A cancelled; the host published nothing to qid 13
        // in between, yet it stalled on the CDM and recovery B blamed it).
        // Its owner is not made guilty for it; work of one QID runs in order,
        // so the oldest in-flight item is the one the firmware stalled on.
        let mut collateral_qids = 0u128;
        let mut push = |guilty: &mut [Option<Arc<crate::g17_status::VmStatus>>; MAX_VMS],
                        count: &mut usize, status: Arc<crate::g17_status::VmStatus>| -> bool {
            if guilty[..*count].iter().flatten().any(|known| Arc::ptr_eq(known, &status)) {
                return true;
            }
            if *count >= MAX_VMS { return false; }
            guilty[*count] = Some(status);
            *count += 1;
            true
        };
        for qid in 0..128u8 {
            if guilty_qids & (1u128 << qid) == 0 { continue; }
            let mut found = false;
            if let Some(slot) = self.manager.as_ref().and_then(|manager| manager.render_slot_for_qid(qid)) {
                let index = usize::from(slot);
                let pending = self.pending_render.get(index).and_then(Option::as_ref);
                if pending.is_some_and(|front| front.recovery_spared()) {
                    found = true;
                    collateral_qids |= 1u128 << qid;
                } else {
                    let queued = self.pending_render_queued.get(index);
                    for completion in Iterator::chain(pending.into_iter(), queued.into_iter().flat_map(|fifo| fifo.iter())) {
                        found = true;
                        if !push(&mut guilty, &mut guilty_count, completion.vm_status()) { unresolved = true; }
                    }
                }
            }
            for q in self.async_compute_queues.iter().filter(|q| q.queue.queue_id() == qid) {
                // iso4: a REPLAYED front kick is not a leftover -- it was
                // published again after its recovery and ran on its own since,
                // so a blame on its QID is its own (no replay loop).
                if q.inflight.front().and_then(|front| front.packet.completion().ok())
                    .is_some_and(|front| front.recovery_spared() && front.replay_count() == 0)
                {
                    found = true;
                    collateral_qids |= 1u128 << qid;
                    continue;
                }
                for active in q.inflight.iter() {
                    if let Ok(completion) = active.packet.completion() {
                        found = true;
                        if !push(&mut guilty, &mut guilty_count, completion.status.clone()) { unresolved = true; }
                    }
                }
            }
            unresolved |= !found;
        }
        if collateral_qids != 0 {
            iso::count(&iso::COLLATERAL);
        }
        // Collateral-only blame resolves to "nobody guilty": every in-flight
        // completion is spared again.
        if unresolved || (guilty_count == 0 && collateral_qids == 0 && !no_culprit) {
            iso::count(&iso::UNRESOLVED);
            if let Some(pdev) = self.pdev.as_ref() {
                dev_warn!(pdev.as_ref(),
                    "G17P recovery isolation: blamed stamp {:?} guilty-qids={:#x} entries={} -- guilty owner not resolvable; isolation off for this recovery\n",
                    blamed_stamp, guilty_qids, used);
            }
            return (firmware_guilty_qids, 0);
        }

        // One class per VM for this recovery (the grace is consumed once).
        let grace = iso::enabled(iso::GUILTY_GRACE);
        let now = crate::g17_timing_buffer::now();
        let mut classes = [iso::CLASS_GUILTY; MAX_VMS];
        let mut graced = 0u32;
        for (index, status) in guilty[..guilty_count].iter().enumerate() {
            if let Some(status) = status {
                classes[index] = status.recovery_class(true, grace, now);
                if classes[index] == iso::CLASS_SPARED {
                    graced += 1;
                    iso::count(&iso::GRACED);
                }
            }
        }
        let class_of = |status: &Arc<crate::g17_status::VmStatus>| -> u8 {
            guilty[..guilty_count].iter().zip(classes.iter())
                .find(|(known, _)| known.as_ref().is_some_and(|known| Arc::ptr_eq(known, status)))
                .map_or(iso::CLASS_SPARED, |(_, class)| *class)
        };

        let mut spared_ids = [0u64; MAX_VMS];
        let mut spared_id_count = 0usize;
        let mut note_spared = |id: u64| {
            if !spared_ids[..spared_id_count].contains(&id) && spared_id_count < MAX_VMS {
                spared_ids[spared_id_count] = id;
                spared_id_count += 1;
            }
        };
        let mut spared_renders = 0u32;
        let mut guilty_renders = 0u32;
        let mut spared_slots = 0u64;
        for slot in 0..self.pending_render.len() {
            let pending = self.pending_render[slot].as_ref();
            let queued = self.pending_render_queued.get(slot);
            for completion in Iterator::chain(pending.into_iter(), queued.into_iter().flat_map(|fifo| fifo.iter())) {
                let class = class_of(&completion.vm_status());
                completion.set_recovery_class(class);
                if class == iso::CLASS_SPARED {
                    spared_renders += 1;
                    note_spared(completion.vm_status().vm_id());
                    spared_slots |= 1u64 << (slot & 63);
                    iso::count(&iso::SPARED_RENDERS);
                } else {
                    guilty_renders += 1;
                    iso::count(&iso::GUILTY_MARKED);
                }
            }
        }
        let mut spared_compute = 0u32;
        let mut guilty_compute = 0u32;
        let mut spared_cancelled = 0u128;
        for q in self.async_compute_queues.iter() {
            for active in q.inflight.iter() {
                let Ok(completion) = active.packet.completion() else { continue; };
                let class = class_of(&completion.status);
                completion.set_recovery_class(class);
                if class == iso::CLASS_SPARED {
                    spared_compute += 1;
                    note_spared(completion.status.vm_id());
                    iso::count(&iso::SPARED_COMPUTE);
                } else {
                    guilty_compute += 1;
                    iso::count(&iso::GUILTY_MARKED);
                }
            }
        }
        // Render queues the firmware CANCELLED (bit2) whose owner was spared,
        // settled promptly under SETTLE_CANCELLED (in the 00:06 capture the
        // firmware had already retired every such group in the same pass).
        // Listed-but-not-cancelled work (02:24: Hyprland's parked 3D, flags
        // 0x1) is left to firmware or the watchdog: settling it now could
        // quarantine its pair while firmware still owns it. Guilty QIDs are
        // excluded by construction.
        if let Some(manager) = self.manager.as_ref() {
            for qid in 0..128u8 {
                if cancelled_qids & !(guilty_qids & !collateral_qids) & (1u128 << qid) == 0 { continue; }
                if let Some(slot) = manager.render_slot_for_qid(qid) {
                    if spared_slots & (1u64 << (slot & 63)) != 0 {
                        spared_cancelled |= 1u128 << qid;
                    }
                }
            }
        }
        if let Some(pdev) = self.pdev.as_ref() {
            let mut guilty_ids = [0u64; MAX_VMS];
            for (id, status) in guilty_ids.iter_mut().zip(guilty[..guilty_count].iter()) {
                *id = status.as_ref().map_or(0, |status| status.vm_id());
            }
            dev_info!(pdev.as_ref(),
                "G17P recovery isolation: blamed stamp {:?} guilty-qids={:#x} collateral-qids={:#x} listed-qids={:#x} cancelled-qids={:#x} guilty-vms={:?} graced={} spared-vms={:?} renders spared={} guilty={} compute spared={} guilty={}\n",
                blamed_stamp, guilty_qids, collateral_qids, listed_qids, cancelled_qids, &guilty_ids[..guilty_count], graced,
                &spared_ids[..spared_id_count],
                spared_renders, guilty_renders, spared_compute, guilty_compute);
        }
        (guilty_qids & !collateral_qids, spared_cancelled)
    }

    /// Recovery can retire listed-only work as well as entries marked cancelled.
    /// Inspect every spared head after recovery and on event/teardown turns.
    /// Unproven ownership leaves it pending: no fence, quarantine or unmap.
    pub(crate) fn service_spared_recovery_renders(&mut self) -> Result {
        use crate::g17_recovery_isolation as iso;
        if !iso::enabled(iso::SETTLE_CANCELLED) || self.primary_recovery_pending()
            || self.is_crashed() { return Ok(()); }
        for slot in 0..self.pending_render.len() {
            for _ in 0..g17_resources::G17P_RENDER_MAX_IN_FLIGHT {
                if !self.pending_render[slot].as_ref()
                    .is_some_and(|completion| completion.recovery_spared()) { break; }
                let render_slot = slot as u8;
                // Preserve real successful completions before checking cancellation.
                if let Some(observed) = self.poll_deferred_render_completion(render_slot)? {
                    self.settle_deferred_render_completion(render_slot, observed)?;
                    continue;
                }
                if !self.manager.as_mut().ok_or(ENODEV)?
                    .retire_abandoned_render_if_firmware_retired(render_slot, false)? { break; }
                iso::count(&iso::SETTLED_CANCELLED);
                self.settle_retired_render_completion(render_slot, Err(ECANCELED))?;
                if let Some(pdev) = self.pdev.as_ref() {
                    dev_info!(pdev.as_ref(),
                        "G17P recovery isolation: cancelled render slot {} witnessed retired; slot remains healthy\n", render_slot);
                }
            }
        }
        Ok(())
    }

    /// Top-level witness for the event and recovery workers, called from their
    /// outermost Runtime hold (`G17PRuntimeRef`) after the pass: replays the
    /// queues a nested witness left for it.
    pub(crate) fn service_compute_witness_top(&mut self, reason: &'static str) {
        self.release_firmware_retired_quarantined_compute(reason);
    }

    /// ADMISSION_WAIT for compute submits: observe the retirement witness of
    /// `owner`'s spared quarantined queue. True while it is still pending.
    pub(crate) fn spared_compute_pending(&mut self, owner: u64) -> bool {
        let pending = |runtime: &Self| runtime.async_compute_queues.iter().any(|q|
            q.owner == Some(owner) && q.quarantined && q.spared_quarantine);
        if !pending(self) { return false; }
        self.release_firmware_retired_quarantined_compute("client-submit");
        pending(self)
    }
}

/// Recovery isolation: fail the still-unsignalled fences of a spared
/// zero-duration queue whose retirement can no longer be awaited, retaining
/// every pin exactly as the eager path does (no VM status: they are spared).
fn fail_spared_deferred(q: &mut G17PAsyncComputeQueue, error: Error) {
    if !q.spared_deferred { return; }
    q.spared_deferred = false;
    for active in q.inflight.iter() {
        if let Ok(completion) = active.packet.completion() {
            completion.fail_while_owned(error);
        }
    }
}

// ---- g17p_exit_kill (see g17_exit_kill.rs) ---------------------------------
impl G17PLiveRuntime {
    /// Teardown turn for a closed context whose compute queue still holds
    /// kicks: once the teardown has waited the native process-exit timeout
    /// (250 ms), publish the process-exit kill (device control 0x29) of the
    /// context's scheduler page, as `AGXCommandQueue::processExitTimeoutHandler`
    /// does for a process that exits with work outstanding. Only a queue
    /// whose owner has released it, with every stacked packet belonging to
    /// this context, and only when every QID the page names is this
    /// context's alone (see `g17_exit_kill::kill_scope`). The context's
    /// render pairs named by the kill are held out of reuse for the session.
    /// Never sleeps; a refusal is retried on a later turn.
    // Out of line: keeps these frames off the teardown/event-worker stack
    // except while they actually run (see audit_g17_publication_stack.py).
    #[inline(never)]
    pub(crate) fn publish_exit_kill_for_context(
        &mut self,
        context: &Arc<crate::g17_resources::G17PLogicalQueueContext>,
        teardown_age_ns: i64,
    ) -> Result {
        use crate::g17_exit_kill as kill;
        if !kill::enabled() {
            return Ok(());
        }
        let Some(index) = self.async_compute_queues.iter().position(|q|
            exit_kill_candidate(q, context) && kill::kill_due(teardown_age_ns, q.quarantined)
        ) else { return Ok(()); };
        if self.primary_device_control_backpressured() {
            return Ok(());
        }
        let pdev = self.pdev.as_ref().cloned().ok_or(ENODEV)?;
        let (target_qid, owner, kicks, refusal) = {
            let q = &self.async_compute_queues[index];
            let owner = q.owner.or_else(|| q.inflight.back()
                .and_then(|active| active.retirement.map(|ticket| ticket.owner)));
            let same_context = q.inflight.iter().all(|active|
                Arc::ptr_eq(active.packet.context(), context));
            (q.queue.queue_id(), owner, q.inflight.len(), !same_context || owner.is_none())
        };
        // Compute QIDs: this context's queues are its own; every other
        // queue's QID belongs to someone else (or to nobody).
        let mut own = 0u128;
        let mut foreign = 0u128;
        for q in self.async_compute_queues.iter() {
            let bit = kill::qid_bit(q.queue.queue_id());
            if q.context.as_ref().is_some_and(|owner| Arc::ptr_eq(owner, context)) {
                own |= bit;
            } else if q.context.is_some() || q.owner.is_some() {
                foreign |= bit;
            }
        }
        let manager = self.manager.as_mut().ok_or(ENODEV)?;
        let (render_own, render_foreign, render_slots) = manager.exit_kill_render_scope(context);
        own |= render_own;
        foreign |= render_foreign;
        let view = context.channel_control.exit_kill_view()?;
        let scope = if refusal {
            kill::Scope::Foreign(kill::qid_bit(target_qid))
        } else {
            kill::kill_scope(view, target_qid, own, foreign)
        };
        let kill::Scope::Kill(qids) = scope else {
            let q = &mut self.async_compute_queues[index];
            if !q.exit_kill_refused {
                q.exit_kill_refused = true;
                kill::count(&kill::SCOPE_REFUSED);
                dev_warn!(pdev.as_ref(),
                    "G17P exit kill: context {} qid={} not killed yet ({:?}; page slot={:#x} qids={:#x} own={:#x} foreign={:#x}); retaining {} kick(s), retried each teardown turn\n",
                    context.context_id(), target_qid, scope, view.slot, view.qids, own, foreign, kicks);
            }
            return Ok(());
        };
        // Hold exactly the pairs the kill names, in the registry, before the
        // record becomes visible: no successor is ever handed them.
        let mut hold = 0u64;
        for slot in 0..g17_resources::G17PRenderQueuePair::SLOT_COUNT {
            if render_slots & (1u64 << slot) == 0 { continue; }
            let named = manager.render_slot_qids(slot) & qids;
            if named != 0 { hold |= 1u64 << slot; }
        }
        let owner = owner.ok_or(EINVAL)?;
        if hold != 0 {
            let held = self.queue_registry.as_ref()
                .is_some_and(|registry| registry.hold_render_slots_for_exit_kill(owner, hold));
            if !held {
                return Ok(());
            }
        }
        let cookie = kill::next_cookie();
        let manager = self.manager.as_mut().ok_or(ENODEV)?;
        let publication = match manager.publish_exit_kill(context, cookie) {
            Ok(publication) => publication,
            Err(error) => {
                if let Some(registry) = self.queue_registry.as_ref() {
                    registry.release_exit_kill_hold(hold);
                }
                return if error == EAGAIN { Ok(()) } else { Err(error) };
            }
        };
        fence(Ordering::SeqCst);
        if let Err(error) = Pin::new(self.gfx_rtkit.as_mut().ok_or(ENODEV)?)
            .send_message_logged(g17_rtkit::ENDPOINT_GFX_INTERRUPTS, g17_rtkit::encode_primary_device_control())
        {
            // The record stays queued; the teardown re-announce covers it.
            dev_warn!(pdev.as_ref(),
                "G17P exit kill: announcement for context {} failed ({:?}); the record at slot {} stays queued\n",
                context.context_id(), error, publication.slot);
        }
        kill::count(&kill::PUBLISHED);
        let q = &mut self.async_compute_queues[index];
        q.exit_kill = Some(G17PExitKill {
            cookie,
            phase: G17PExitKillPhase::Published,
            held_render_slots: hold,
            qids,
            killed_before: view.killed,
            published: kernel::time::Instant::<kernel::time::Monotonic>::now(),
            late_warned: false,
        });
        dev_info!(pdev.as_ref(),
            "G17P exit kill: context {} qid={} owner={} published 0x29 page={:#x} cookie={} qids={:#x} held-render-slots={:#x} kicks={} quarantined={} after {} ms (slot {} producer {}->{})\n",
            context.context_id(), target_qid, owner, context.channel_control.gpu_va(), cookie, qids,
            hold, kicks, q.quarantined, teardown_age_ns / 1_000_000, publication.slot,
            publication.producer_before, publication.producer_after);
        Ok(())
    }

    /// Keep the event worker's millisecond teardown poll running, for a
    /// bounded `g17_exit_kill::POLL_NS`, while a closed context's compute is
    /// becoming due for the kill or a published kill awaits its event 12, so
    /// neither waits for an unrelated firmware event.
    // Out of line: keeps these frames off the teardown/event-worker stack
    // except while they actually run (see audit_g17_publication_stack.py).
    #[inline(never)]
    pub(crate) fn exit_kill_polling(&self) -> bool {
        use crate::g17_exit_kill as kill;
        if !kill::enabled() {
            return false;
        }
        if self.async_compute_queues.iter().any(|q| q.exit_kill.as_ref().is_some_and(|exit_kill|
            exit_kill.phase == G17PExitKillPhase::Published
                && exit_kill.published.elapsed().as_nanos() < kill::POLL_NS))
        {
            return true;
        }
        let Some(manager) = self.manager.as_ref() else { return false };
        (0..manager.logical_queue_teardowns_outstanding()).any(|index| {
            manager.logical_queue_teardown_age_ns(index) < kill::POLL_NS
                && manager.logical_queue_teardown_context(index).is_some_and(|context|
                    self.async_compute_queues.iter().any(|q| exit_kill_candidate(q, &context)))
        })
    }

    /// Act on process-exit acknowledgements (event 12). For each queue whose
    /// kill was acknowledged, with the recovery handshake closed and the
    /// firmware kill task's store on the page (`+0x34 == 1`): when every
    /// stacked kick's own completion words show it never launched or ended
    /// (`g17_exit_kill::release_decision`), release the packets and their VM
    /// pins exactly as the retirement witness does, and keep the graph
    /// quarantined. A kick that may still execute keeps everything, as
    /// before, and is looked at again on later turns.
    // Out of line: keeps these frames off the teardown/event-worker stack
    // except while they actually run (see audit_g17_publication_stack.py).
    #[inline(never)]
    pub(crate) fn release_exit_killed_compute(&mut self) {
        use crate::g17_exit_kill as kill;
        if !kill::any_ack_pending() {
            return;
        }
        let pdev = self.pdev.as_ref();
        let recovery_closed = self.manager.as_mut().is_some_and(|manager|
            manager.firmware_recovery_handshake_snapshot().is_ok_and(|handshake| handshake.state == 0));
        for q in self.async_compute_queues.iter_mut() {
            let Some(exit_kill) = q.exit_kill.as_mut() else { continue };
            if !matches!(exit_kill.phase, G17PExitKillPhase::Published | G17PExitKillPhase::Acknowledged) {
                continue;
            }
            if !kill::acked(exit_kill.cookie) {
                if !exit_kill.late_warned && exit_kill.published.elapsed().as_nanos() > 2_000_000_000 {
                    exit_kill.late_warned = true;
                    if let Some(pdev) = pdev {
                        dev_warn!(pdev.as_ref(),
                            "G17P exit kill: qid={} cookie={} not acknowledged after 2 s; render pairs {:#x} stay held and its kicks stay retained\n",
                            q.queue.queue_id(), exit_kill.cookie, exit_kill.held_render_slots);
                    }
                }
                continue;
            }
            let first_ack = exit_kill.phase == G17PExitKillPhase::Published;
            if first_ack {
                exit_kill.phase = G17PExitKillPhase::Acknowledged;
            }
            // The killed render pairs stay held (see
            // `G17PQueueLeaseState::render_kill_hold`) for the session.
            if q.inflight.is_none() {
                // Every kick retired normally before the acknowledgement.
                // The QID was still killed: never hand this graph out again.
                exit_kill.phase = G17PExitKillPhase::Released;
                kill::forget_ack(exit_kill.cookie);
                q.quarantined = true;
                q.retirement_ready = false;
                continue;
            }
            let Some(context) = q.context.as_ref() else { continue };
            let view = match context.channel_control.exit_kill_view() {
                Ok(view) => view,
                Err(_) => continue,
            };
            let decision = kill::release_decision(true, view.killed, recovery_closed,
                q.inflight.iter().map(|active| active.packet.completion().ok()
                    .and_then(|completion| completion.halted_timestamp_snapshot().ok())));
            let cookie = exit_kill.cookie;
            let qids = exit_kill.qids;
            let elapsed_us = exit_kill.published.elapsed().as_nanos() / 1000;
            match decision {
                kill::Release::Pending => {}
                kill::Release::AckWithoutKill => {
                    exit_kill.phase = G17PExitKillPhase::Unproven;
                    kill::forget_ack(cookie);
                    kill::count(&kill::ACK_WITHOUT_KILL);
                    if let Some(pdev) = pdev {
                        dev_warn!(pdev.as_ref(),
                            "G17P exit kill: qid={} cookie={} acknowledged but the page shows no kill (slot={:#x} +0x34={:#x}, before {:#x}); retaining {} kick(s) until processor stop\n",
                            q.queue.queue_id(), cookie, view.slot, view.killed, exit_kill.killed_before,
                            q.inflight.len());
                    }
                }
                kill::Release::Retain { started, unknown } => {
                    if first_ack {
                        kill::count(&kill::RUNNING_RETAINED);
                        if let Some(pdev) = pdev {
                            dev_warn!(pdev.as_ref(),
                                "G17P exit kill: qid={} cookie={} acknowledged after {} us; {} kick(s) started without an end word, {} unreadable -- retaining all {} until they end or processor stop\n",
                                q.queue.queue_id(), cookie, elapsed_us, started, unknown, q.inflight.len());
                        }
                    }
                }
                kill::Release::Release { never_started, ended } => {
                    exit_kill.phase = G17PExitKillPhase::Released;
                    kill::forget_ack(cookie);
                    let released = release_exit_killed_packets(q, pdev);
                    kill::count(&kill::RELEASED_QUEUES);
                    kill::RELEASED_KICKS.fetch_add(released as u64, Ordering::Relaxed);
                    if let Some(pdev) = pdev {
                        dev_info!(pdev.as_ref(),
                            "G17P exit kill: qid={} cookie={} acknowledged after {} us (page +0x34=1, qids {:#x}); released {} packet(s) and their VM pin ({} never launched, {} ended); the graph stays quarantined; totals(published,acked,queues,kicks,running,refused,unproven,unmatched)={:?}\n",
                            q.queue.queue_id(), cookie, elapsed_us, qids, released, never_started, ended,
                            kill::totals());
                    }
                }
            }
        }
        // An acknowledgement no queue is waiting for (a queue removed by a
        // processor stop, or a stray cookie) is dropped.
        for cookie in kill::pending_acks() {
            if cookie != 0 && !self.async_compute_queues.iter().any(|q|
                q.exit_kill.as_ref().is_some_and(|exit_kill| exit_kill.cookie == cookie
                    && matches!(exit_kill.phase,
                        G17PExitKillPhase::Published | G17PExitKillPhase::Acknowledged)))
            {
                kill::forget_ack(cookie);
                kill::count(&kill::ACK_UNMATCHED);
            }
        }
    }
}

/// A closed context's compute queue whose kicks will not retire on their
/// own: its owner released it with kicks outstanding (quarantined, or
/// cancelled at close) and no kill was published for it yet.
fn exit_kill_candidate(
    q: &G17PAsyncComputeQueue,
    context: &Arc<crate::g17_resources::G17PLogicalQueueContext>,
) -> bool {
    q.context.as_ref().is_some_and(|owner| Arc::ptr_eq(owner, context))
        && q.released
        && q.inflight.is_some()
        && q.exit_kill.is_none()
        && (q.quarantined || q.retired_by_teardown)
}

/// Release every stacked packet of an exit-killed queue: settle any fence
/// still unsignalled (ECANCELED, or ENODATA for spared kicks, neither of
/// which records VM status), drop its VM pin, keep its admission guard in
/// `retired_leases` (as the retirement witness does), and leave the graph
/// quarantined: firmware killed its QIDs and the host producer still counts
/// the killed kicks, so it is never handed to another owner.
fn release_exit_killed_packets(q: &mut G17PAsyncComputeQueue, pdev: Option<&ARef<platform::Device>>) -> usize {
    let mut released = 0usize;
    while let Some(active) = q.inflight.take() {
        let G17PAsyncComputeActive { packet, _lease: lease, .. } = active;
        if let Ok(completion) = packet.completion() {
            completion.complete(Err(if q.spared_deferred { ENODATA } else { ECANCELED }));
            completion.release_after_retirement();
        }
        if let Err(lease) = q.retired_leases.push(lease) {
            if let Some(pdev) = pdev {
                dev_err!(pdev.as_ref(),
                    "G17P exit kill: qid={} retired-lease FIFO full; dropping one admission guard\n",
                    q.queue.queue_id());
            }
            drop(lease);
        }
        released += 1;
        drop(packet);
    }
    q.quarantined = true;
    q.awaiting_retirement_witness = false;
    q.spared_deferred = false;
    q.spared_quarantine = false;
    q.retire_pending = false;
    q.retirement_ready = false;
    released
}
