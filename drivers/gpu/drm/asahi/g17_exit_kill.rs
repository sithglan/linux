// SPDX-License-Identifier: GPL-2.0-only OR MIT
//! Process-exit kill of a closed context's unretired compute (`g17p_exit_kill`).
//!
//! The leak (boot 6915cf0d, Steam G2a): a compute kick that firmware never
//! launches (qid 18 `kick-ts=0x7..=0x8`: kick 7 has a completion record,
//! kick 8 none, its parent already satisfied) times out and quarantines its
//! queue. Every stacked packet keeps its `T8140VmJobGuard` until the
//! queue-local retirement witness (`independent_compute_retired_by_firmware`),
//! which needs the completion stamp to cover the newest kick -- a kick that
//! never runs never produces it. So `job_lifetime.active` stays non-zero,
//! the closed client's `close_ranges`/`closed_objects` stay parked, and its
//! whole VM stays allocated for the boot. Its logical-queue teardown stays
//! `Draining` too (`compute_idle_for_context` is false). A client killed
//! while such a kick is outstanding takes the cancel path
//! (`retired_by_teardown`) into the same wait.
//!
//! Native (static RE of kext25.dis / fw25-b100.dis):
//!   * `AGXCommandQueue::processExitTimeoutHandler` @fffffe000886a494, an
//!     IOTimerEventSource callback (`halGetProcessExitDefaultTimeoutMS` is
//!     250 ms; where the timer is armed was not traced), sets
//!     reclaimable+0x2f9 = 1, allocates an ID for the queue from the ID
//!     manager at accel+0x1afe8, stores it at queue+0x8e0, and submits
//!     primary device control 0x29 with submit flags 1 (no completion wait):
//!     `+0x04` u64 = queue+0x8b8 (written by `allocateSchedulerState`; the
//!     scheduler-state page, Linux `channel_control`), `+0x0c` u64 = the ID.
//!   * Firmware 0x29 (fw 0x49cc, from the decoded device-control jump
//!     table): if page byte `[1] == 0xff` it posts event 12 at once
//!     (fw 0x5110). Otherwise it queues (cookie, page) in a 128-entry ring
//!     and signals a task (fw 0x26ab8) that stores u32 1 at page+0x34, passes
//!     the two u64 QID masks at page+0x12/+0x1a with reason 0x11 to fw 0xa2a8
//!     and fw 0x1359c (the latter clears those QID bits from per-reason
//!     tables; "kills" is our reading, the routines are not fully decoded),
//!     calls fw 0x127c4, then posts event 12 `{+0x04 u64 cookie}`.
//!   * Event 12 is `kAGFIFirmwareEventTypeProcessExitComplete`: the type-12
//!     arm of `drainFirmwareEventRing` (@fffffe000887e904) re-checks the type
//!     against 12 and asserts with exactly that validator string (`RET =
//!     AGFIFirmwareEventProcessExitComplete`). It looks the queue up by the
//!     cookie, requires queue+0x8e0 == cookie, and calls the queue's virtual
//!     method at +0x28 (not decoded; presumably the release of the exit).
//!
//! Linux: once a closed context's compute queue still holds kicks
//! (quarantined, or cancelled by its departed client) and the teardown has
//! waited the native 250 ms, publish the same 0x29 record for the context's
//! scheduler page. On event 12 with that cookie, with the recovery handshake
//! closed and page+0x34 == 1 (the kill task, not the 0xff shortcut, ran on
//! this page), each stacked kick's own completion words decide: both zero
//! (never launched) or a written end word (finished) release the packet and
//! its VM pin; a started kick without an end word keeps everything, exactly
//! as before, and is looked at again later. The compute graph stays
//! quarantined (its QIDs were killed and the host producer still counts the
//! killed kicks); once the packets are gone the existing teardown publishes
//! the 0x14 scheduler-state release and settles the context's graphs.
//!
//! Scope: every QID the page's masks name must be this context's alone --
//! its compute queue and the render pairs whose graph is bound to it --
//! otherwise nothing is published and the next teardown turn retries. Those
//! render pairs are held in the lease registry from just before publication,
//! so no successor can be handed a pair the kill may still reach, and stay
//! held until processor stop: the kill task writes per-QID state whose
//! effect on Linux's rebind (+0x1a=0, no host KSMFE write) is unknown, so a
//! killed pair is never handed to an innocent client. Cost: up to two of the
//! 63 render slots per kill, for the session. The killed compute graph stays
//! quarantined as before.
//!
//! Not established (hardware questions, see the repro): that firmware never
//! launches a killed, never-started kick afterwards (if it did, the kick
//! would run in a VM whose user mappings are gone and whose root stays
//! reserved by the quarantined graph: a GPU fault, not a stray write); and
//! whether firmware performs a finished-but-unflushed kick's deferred cache
//! maintenance when it kills the context (qid 18's kick 7 finished with the
//! stamp still at 0x600).
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

// Record and event: `g17_initdata::encode_context_kill_record` (opcode
// 0x29) and `g17_completion::decode_g17p_process_exit_event` (type 12).
/// `AGXAccelerator::halGetProcessExitDefaultTimeoutMS`.
pub(crate) const PROCESS_EXIT_TIMEOUT_NS: i64 = 250_000_000;
/// The compute scheduler timeout (`g17_job::SCHEDULER_TIMEOUT_MS`).
pub(crate) const COMPUTE_TIMEOUT_NS: i64 = 2_000_000_000;
/// How long the event worker keeps polling for a due kill or its ack.
pub(crate) const POLL_NS: i64 = COMPUTE_TIMEOUT_NS + 500_000_000;

/// Whether a closed context's compute is due for the kill, `age_ns` after
/// its teardown was armed. A quarantined queue already outlived the compute
/// timeout: the native 250 ms. A queue whose departing client merely
/// cancelled its waiters may hold a kick that is still legitimately running
/// and would retire on its own; it is killed only once it has outlived the
/// compute timeout, i.e. exactly when a live client's job would have been
/// quarantined. Killing it earlier could turn a kick that would have retired
/// into one that never does.
pub(crate) const fn kill_due(age_ns: i64, quarantined: bool) -> bool {
    age_ns >= if quarantined { PROCESS_EXIT_TIMEOUT_NS } else { COMPUTE_TIMEOUT_NS }
}

/// Scheduler-state page offsets used by the firmware kill task.
pub(crate) const STATE_SLOT_BYTE: usize = 0x01;
pub(crate) const STATE_QID_MASK_LOW: usize = 0x12;
pub(crate) const STATE_QID_MASK_HIGH: usize = 0x1a;
pub(crate) const STATE_KILLED_WORD: usize = 0x34;
/// Bytes the kill task reads and cleans (`bl 0x28ad0` with 0x38).
pub(crate) const STATE_KILL_BYTES: usize = 0x38;

/// bit0: publish the exit kill and release on its proof. 0 keeps every
/// quarantined packet until processor stop, as before.
pub(crate) const ENABLE: u32 = 1 << 0;

#[cfg(not(test))]
#[inline(always)]
pub(crate) fn enabled() -> bool {
    crate::module_parameters::g17p_exit_kill.value() & ENABLE != 0
}

/// Counters, reported with the release lines.
pub(crate) static PUBLISHED: AtomicU64 = AtomicU64::new(0);
pub(crate) static ACKED: AtomicU64 = AtomicU64::new(0);
pub(crate) static ACK_UNMATCHED: AtomicU64 = AtomicU64::new(0);
pub(crate) static SCOPE_REFUSED: AtomicU64 = AtomicU64::new(0);
pub(crate) static ACK_WITHOUT_KILL: AtomicU64 = AtomicU64::new(0);
pub(crate) static RELEASED_QUEUES: AtomicU64 = AtomicU64::new(0);
pub(crate) static RELEASED_KICKS: AtomicU64 = AtomicU64::new(0);
pub(crate) static RUNNING_RETAINED: AtomicU64 = AtomicU64::new(0);

#[inline(always)]
pub(crate) fn count(counter: &AtomicU64) { counter.fetch_add(1, Ordering::Relaxed); }

/// `[published, acked, released-queues, released-kicks, running-retained,
/// scope-refused, ack-without-kill, ack-unmatched]`, for the log lines.
pub(crate) fn totals() -> [u64; 8] {
    [&PUBLISHED, &ACKED, &RELEASED_QUEUES, &RELEASED_KICKS, &RUNNING_RETAINED,
     &SCOPE_REFUSED, &ACK_WITHOUT_KILL, &ACK_UNMATCHED].map(|counter| counter.load(Ordering::Relaxed))
}

static NEXT_COOKIE: AtomicU32 = AtomicU32::new(1);

/// Boot-unique, never zero. Native's cookie is a small ID-manager integer
/// zero-extended to u64; firmware only stores and echoes it.
pub(crate) fn next_cookie() -> u64 {
    loop {
        let cookie = NEXT_COOKIE.fetch_add(1, Ordering::Relaxed);
        if cookie != 0 { return u64::from(cookie); }
    }
}

/// Event-12 cookies seen by the event drain and not yet consumed by the
/// compute release scan. Lock-free: the drain runs under the runtime mutex
/// like the scan, but this keeps the event path free of the queue table.
const ACK_SLOTS: usize = 16;
#[allow(clippy::declare_interior_mutable_const)]
const EMPTY_ACK: AtomicU64 = AtomicU64::new(0);
static ACKS: [AtomicU64; ACK_SLOTS] = [EMPTY_ACK; ACK_SLOTS];

/// Record an event-12 acknowledgement. False when every slot is busy (only
/// possible with more than 16 kills outstanding at once); the queue then
/// keeps its packets until processor stop, as before.
pub(crate) fn note_ack(cookie: u64) -> bool {
    if cookie == 0 { return false; }
    count(&ACKED);
    for slot in ACKS.iter() {
        if slot.load(Ordering::Acquire) == cookie { return true; }
    }
    for slot in ACKS.iter() {
        if slot.compare_exchange(0, cookie, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            return true;
        }
    }
    false
}

/// Whether `cookie` has been acknowledged (kept until `forget_ack`).
pub(crate) fn acked(cookie: u64) -> bool {
    cookie != 0 && ACKS.iter().any(|slot| slot.load(Ordering::Acquire) == cookie)
}

pub(crate) fn any_ack_pending() -> bool {
    ACKS.iter().any(|slot| slot.load(Ordering::Acquire) != 0)
}

pub(crate) fn forget_ack(cookie: u64) {
    for slot in ACKS.iter() {
        let _ = slot.compare_exchange(cookie, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}

/// Snapshot of the acknowledged cookies (zero = empty slot).
pub(crate) fn pending_acks() -> [u64; ACK_SLOTS] {
    let mut out = [0u64; ACK_SLOTS];
    for (out, slot) in out.iter_mut().zip(ACKS.iter()) {
        *out = slot.load(Ordering::Acquire);
    }
    out
}

pub(crate) fn forget_all_acks() {
    for slot in ACKS.iter() {
        slot.store(0, Ordering::Release);
    }
}

/// The scheduler-state bytes the kill task consults, read from the page.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct StateView {
    pub(crate) slot: u8,
    pub(crate) qids: u128,
    pub(crate) killed: u32,
}

pub(crate) fn state_view(prefix: &[u8; STATE_KILL_BYTES]) -> StateView {
    let u64_at = |at: usize| {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&prefix[at..at + 8]);
        u64::from_le_bytes(raw)
    };
    let mut killed = [0u8; 4];
    killed.copy_from_slice(&prefix[STATE_KILLED_WORD..STATE_KILLED_WORD + 4]);
    StateView {
        slot: prefix[STATE_SLOT_BYTE],
        qids: u128::from(u64_at(STATE_QID_MASK_LOW)) | (u128::from(u64_at(STATE_QID_MASK_HIGH)) << 64),
        killed: u32::from_le_bytes(killed),
    }
}

pub(crate) const fn qid_bit(qid: u8) -> u128 {
    if qid >= 128 { 0 } else { 1u128 << qid }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    /// Kill exactly these QIDs.
    Kill(u128),
    /// Firmware has not adopted the page (slot 0xff): it would acknowledge
    /// without killing anything, which proves nothing.
    NotAdopted,
    /// The page's masks do not name the quarantined queue's QID.
    TargetAbsent,
    /// The masks name a QID now owned by another context, a bootstrap QID,
    /// or a QID whose owner is unknown (a render preparation in progress).
    Foreign(u128),
}

/// The bootstrap render pair; never part of a client context.
pub(crate) const BOOTSTRAP_QIDS: u128 = 0b11;

/// `own`: QIDs Linux currently attributes to this context (its compute
/// queues and the render pairs bound to it). `foreign`: QIDs bound to any
/// other context or held by a preparer. Every QID the page names must be
/// this context's alone; a QID nobody owns is refused too, because a new
/// client could take it before firmware runs the kill.
pub(crate) const fn kill_scope(view: StateView, target_qid: u8, own: u128, foreign: u128) -> Scope {
    if view.slot == 0xff {
        return Scope::NotAdopted;
    }
    let target = qid_bit(target_qid);
    if target == 0 || view.qids & target == 0 {
        return Scope::TargetAbsent;
    }
    let allowed = (own | target) & !foreign & !BOOTSTRAP_QIDS;
    let refused = view.qids & !allowed;
    if refused != 0 {
        return Scope::Foreign(refused);
    }
    Scope::Kill(view.qids)
}

/// One stacked kick, from its own completion words after the kill.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Kick {
    /// Both words still at the zeroed allocation value: never launched.
    NeverStarted,
    /// End written (including start == end zero-duration retirement).
    Ended,
    /// Started without an end word: may still be executing.
    Started,
    /// Words unreadable.
    Unknown,
}

pub(crate) const fn classify(words: Option<[u64; 2]>) -> Kick {
    match words {
        None => Kick::Unknown,
        Some([0, 0]) => Kick::NeverStarted,
        Some([_, end]) if end != 0 => Kick::Ended,
        Some(_) => Kick::Started,
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Release {
    /// Not acknowledged yet (or the feature is off): unchanged path.
    Pending,
    /// Event 12 arrived but the page shows no kill (0xff shortcut, or a
    /// different page): proves nothing; keep everything.
    AckWithoutKill,
    /// Release every stacked packet and VM pin.
    Release { never_started: u32, ended: u32 },
    /// At least one kick may still execute; keep everything.
    Retain { started: u32, unknown: u32 },
}

/// The exit-kill release decision for one quarantined queue.
pub(crate) fn release_decision<I>(acked: bool, killed_word: u32, recovery_closed: bool, kicks: I) -> Release
where I: IntoIterator<Item = Option<[u64; 2]>> {
    if !acked || !recovery_closed {
        return Release::Pending;
    }
    if killed_word != 1 {
        return Release::AckWithoutKill;
    }
    let (mut never_started, mut ended, mut started, mut unknown) = (0u32, 0u32, 0u32, 0u32);
    for words in kicks {
        match classify(words) {
            Kick::NeverStarted => never_started += 1,
            Kick::Ended => ended += 1,
            Kick::Started => started += 1,
            Kick::Unknown => unknown += 1,
        }
    }
    if started != 0 || unknown != 0 || never_started + ended == 0 {
        return Release::Retain { started, unknown };
    }
    Release::Release { never_started, ended }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `async-timeout channel-control` prefix of context 12 (qid 18) and
    /// context 10 (qid 23), boot 6915cf0d (steam/hw-results/g2-6915cf0d).
    const STEAM_CONTEXT_12: [u8; STATE_KILL_BYTES] = [
        1, 4, 4, 2, 4, 0, 0x64, 0, 0, 0, 0x70, 0x17, 0, 0, 0x94, 0xc, 0, 0, 0, 0, 0x1c, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0x95, 0x65, 0x16,
        0, 9, 0, 0, 0, 0,
    ];
    const STEAM_CONTEXT_10: [u8; STATE_KILL_BYTES] = [
        1, 5, 4, 2, 4, 0, 0x42, 0, 0, 0, 0xb8, 0xb, 0, 0, 0xbd, 6, 0, 0, 0, 0, 0x80, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0x94, 0x65, 0x16,
        0, 7, 0, 0, 0, 0,
    ];

    fn bits(qids: &[u8]) -> u128 { qids.iter().fold(0, |mask, qid| mask | qid_bit(*qid)) }

    #[test]
    fn steam_pages_name_the_compute_qid_and_the_kicked_render_pair() {
        let view = state_view(&STEAM_CONTEXT_12);
        assert_eq!(view, StateView { slot: 4, qids: bits(&[18, 19, 20]), killed: 0 });
        let view = state_view(&STEAM_CONTEXT_10);
        assert_eq!(view, StateView { slot: 5, qids: bits(&[23]), killed: 0 });
        // The high mask (+0x1a) covers QIDs 64..127.
        let mut raw = [0u8; STATE_KILL_BYTES];
        raw[STATE_QID_MASK_HIGH..STATE_QID_MASK_HIGH + 8].copy_from_slice(&(1u64 << 3).to_le_bytes());
        raw[STATE_KILLED_WORD] = 1;
        assert_eq!(state_view(&raw).qids, qid_bit(67));
        assert_eq!(state_view(&raw).killed, 1);
    }

    #[test]
    fn scope_kills_only_a_page_whose_qids_are_all_this_contexts() {
        let view = state_view(&STEAM_CONTEXT_12);
        // Compute 18 plus its own bound pair 19/20: kill exactly the mask.
        assert_eq!(kill_scope(view, 18, bits(&[18, 19, 20, 21, 22]), bits(&[0, 1, 4, 23])),
            Scope::Kill(bits(&[18, 19, 20])));
        // The pair was handed to another context meanwhile: never kill it.
        assert_eq!(kill_scope(view, 18, bits(&[18]), bits(&[19, 20])),
            Scope::Foreign(bits(&[19, 20])));
        // The pair has no bound graph (nobody owns it): not provable either.
        assert_eq!(kill_scope(view, 18, bits(&[18]), 0), Scope::Foreign(bits(&[19, 20])));
        // A QID both sides claim (a preparation in flight) is foreign.
        assert_eq!(kill_scope(view, 18, bits(&[18, 19, 20]), bits(&[20])),
            Scope::Foreign(bits(&[20])));
        // Compute-only page.
        assert_eq!(kill_scope(state_view(&STEAM_CONTEXT_10), 23, bits(&[23]), bits(&[18])),
            Scope::Kill(bits(&[23])));
        // The queue's QID must be on the page.
        assert_eq!(kill_scope(view, 23, bits(&[18, 19, 20, 23]), 0), Scope::TargetAbsent);
        assert_eq!(kill_scope(view, 200, bits(&[18, 19, 20]), 0), Scope::TargetAbsent);
        // An unadopted page would be acknowledged without any kill.
        let mut raw = STEAM_CONTEXT_12;
        raw[STATE_SLOT_BYTE] = 0xff;
        assert_eq!(kill_scope(state_view(&raw), 18, bits(&[18, 19, 20]), 0), Scope::NotAdopted);
        // The bootstrap pair is never a client's.
        raw = STEAM_CONTEXT_12;
        raw[STATE_QID_MASK_LOW] = 0b11;
        assert_eq!(kill_scope(state_view(&raw), 18, bits(&[0, 1, 18, 19, 20]), 0),
            Scope::Foreign(bits(&[0, 1])));
    }

    #[test]
    fn a_cancelled_queue_is_killed_only_after_the_compute_timeout() {
        assert!(!kill_due(249_999_999, true));
        assert!(kill_due(250_000_000, true));
        assert!(!kill_due(250_000_000, false));
        assert!(!kill_due(1_999_999_999, false));
        assert!(kill_due(2_000_000_000, false));
        assert!(POLL_NS > COMPUTE_TIMEOUT_NS);
    }

    #[test]
    fn kicks_classify_by_their_own_completion_words() {
        assert_eq!(classify(Some([0, 0])), Kick::NeverStarted);
        assert_eq!(classify(Some([0xa768c676d, 0xa768c67a0])), Kick::Ended);
        assert_eq!(classify(Some([0xa768c676d, 0xa768c676d])), Kick::Ended);
        assert_eq!(classify(Some([0, 0xa768c67a0])), Kick::Ended);
        assert_eq!(classify(Some([0xa768c676d, 0])), Kick::Started);
        assert_eq!(classify(None), Kick::Unknown);
    }

    #[test]
    fn release_needs_the_ack_the_kill_store_and_no_running_kick() {
        // Steam qid 18: kick 7 finished, kick 8 never launched.
        let steam = [Some([0xa768c676d, 0xa768c67a0]), Some([0, 0])];
        assert_eq!(release_decision(false, 1, true, steam), Release::Pending);
        assert_eq!(release_decision(true, 1, false, steam), Release::Pending);
        assert_eq!(release_decision(true, 0, true, steam), Release::AckWithoutKill);
        assert_eq!(release_decision(true, 1, true, steam),
            Release::Release { never_started: 1, ended: 1 });
        // Both kicks never launched (the second stamp-blamed shape).
        assert_eq!(release_decision(true, 1, true, [Some([0, 0]), Some([0, 0])]),
            Release::Release { never_started: 2, ended: 0 });
        // A runaway shader keeps the whole VM, exactly as before the kill.
        assert_eq!(release_decision(true, 1, true, [Some([5, 9]), Some([7, 0])]),
            Release::Retain { started: 1, unknown: 0 });
        assert_eq!(release_decision(true, 1, true, [Some([0, 0]), None]),
            Release::Retain { started: 0, unknown: 1 });
        assert_eq!(release_decision(true, 1, true, core::iter::empty()),
            Release::Retain { started: 0, unknown: 0 });
    }

    #[test]
    fn acknowledgements_match_cookies_exactly_once() {
        forget_all_acks();
        assert!(!any_ack_pending());
        let (a, b) = (next_cookie(), next_cookie());
        assert!(a != 0 && b != 0 && a != b);
        assert!(!note_ack(0), "zero is never a cookie");
        assert!(note_ack(a));
        assert!(note_ack(a), "a duplicate event finds its slot");
        assert!(acked(a) && !acked(b) && !acked(0));
        assert_eq!(pending_acks().iter().filter(|cookie| **cookie == a).count(), 1);
        forget_ack(a);
        assert!(!acked(a) && !any_ack_pending());
        for cookie in 1..=ACK_SLOTS as u64 { assert!(note_ack(1000 + cookie)); }
        assert!(!note_ack(5000), "a full table refuses; that queue stays retained");
        forget_all_acks();
        assert!(!any_ack_pending());
        assert!(totals()[1] >= 2);
    }
}
