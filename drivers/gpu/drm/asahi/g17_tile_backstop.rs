// SPDX-License-Identifier: GPL-2.0-only OR MIT
//! Kernel tile-hazard backstop (`g17p_tile_backstop`).
//!
//! The hardware loads and stores whole tiles. Two render pass instances
//! that write pixel-disjoint parts of one attachment need no barrier in
//! Vulkan, but when their render areas share a tile and they run at the same
//! time on two render pairs, the later one can reload the tile before the
//! earlier one stored it and write stale pixels back (tilewaw, 2026-10-02:
//! 480 pixel-disjoint 13x11 passes on one image). Mesa orders such passes
//! within a queue (agx3 1a57a3b1168 era fix: cmdbuf, cmdbufs and submits
//! 0/50), but it cannot see a second VkQueue (`queues`, 50/50 corrupt) or a
//! second process sharing the image through dma-buf (`xproc`, 46/50).
//!
//! The kernel sees, per render, the fragment attachment spans it validates
//! (`validate_attachments`, whole image planes as Mesa reports them). It
//! cannot see render areas, so it treats any two in-flight renders whose
//! spans overlap as possibly sharing a tile:
//!   A. every render holding a physical pair lease is recorded with its
//!      owner (logical queue), render sequence and footprint;
//!   B. same owner, overlapping, and not ordered by the render's own prefix
//!      (`wait_through`): publish it on the conflicting render's pair, whose
//!      queue order then serializes the two (`SAME_OWNER`);
//!   C. different owner, overlapping: wait for that render to complete --
//!      its lease is released before its fence signals -- through the
//!      registry's lease condvar, bounded by `WAIT_CAP_MS` (`CROSS_OWNER`).
//!      A waiting render is recorded as pending with an arrival ticket, and
//!      a newer render of another owner that overlaps it waits behind it, so
//!      one busy queue cannot starve the other past the cap;
//!   D. different process: spans inside bindings of shareable (non
//!      VM-private) GEM objects are also recorded as (object, offset range),
//!      which two VMs mapping one dma-buf share, so C applies across VMs.
//! Correctly synchronized clients never have two overlapping renders in
//! flight from different queues (a semaphore wait retires the first before
//! the second runs), and barrier-ordered renders of one queue are ordered
//! by their prefix, so neither is affected. Nothing waits or moves unless
//! footprints overlap; the cost otherwise is one comparison pass over the
//! in-flight records under the lease mutex.
use core::sync::atomic::{AtomicU64, Ordering};

/// Footprint capacity: the 16 UAPI fragment attachments plus 8 spans of
/// shareable objects. A footprint that overflows is marked `truncated` and
/// keeps every span it could record.
pub(crate) const MAX_SPANS: usize = 24;

/// Same-owner pair affinity (B).
pub(crate) const SAME_OWNER: u32 = 1 << 0;
/// Cross-owner (and, through object spans, cross-process) waits (C, D).
pub(crate) const CROSS_OWNER: u32 = 1 << 1;
pub(crate) const DEFAULT_MASK: u32 = SAME_OWNER | CROSS_OWNER;
/// A conflict wait never exceeds this, so a render that never completes
/// cannot stall another queue: well inside the 2 s scheduler timeout.
pub(crate) const WAIT_CAP_MS: u32 = 1000;

#[cfg(not(test))]
#[inline(always)]
pub(crate) fn mask() -> u32 {
    crate::module_parameters::g17p_tile_backstop.value()
}

pub(crate) static STACKED: AtomicU64 = AtomicU64::new(0);
pub(crate) static WAITS: AtomicU64 = AtomicU64::new(0);
pub(crate) static WAIT_TIMEOUTS: AtomicU64 = AtomicU64::new(0);
pub(crate) static OBJECT_FOOTPRINTS: AtomicU64 = AtomicU64::new(0);
pub(crate) static TRUNCATED: AtomicU64 = AtomicU64::new(0);
pub(crate) static UNRECORDED: AtomicU64 = AtomicU64::new(0);
static TICKETS: AtomicU64 = AtomicU64::new(1);

/// Arrival order of waiting renders (never zero).
pub(crate) fn next_ticket() -> u64 { TICKETS.fetch_add(1, Ordering::Relaxed) }

#[inline(always)]
pub(crate) fn count(counter: &AtomicU64) -> u64 { counter.fetch_add(1, Ordering::Relaxed) + 1 }

/// `[stacked, waits, wait-timeouts, object-footprints, truncated, unrecorded]`.
pub(crate) fn totals() -> [u64; 6] {
    [&STACKED, &WAITS, &WAIT_TIMEOUTS, &OBJECT_FOOTPRINTS, &TRUNCATED, &UNRECORDED]
        .map(|counter| counter.load(Ordering::Relaxed))
}

/// One written byte range, `[start, end)`, in a key's address space: a VM's
/// GPU VAs (even keys) or one GEM object's offsets (odd keys).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) key: u64,
    pub(crate) start: u64,
    pub(crate) end: u64,
}

impl Span {
    const EMPTY: Self = Self { key: 0, start: 0, end: 0 };
}

/// The attachment ranges one render may store to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Footprint {
    vm: u64,
    spans: [Span; MAX_SPANS],
    count: u8,
    objects: u8,
    truncated: bool,
}

impl Footprint {
    pub(crate) const fn vm_key(vm: u64) -> u64 { vm << 1 }
    /// Object identities are addresses, so bit 0 is free to tag them.
    pub(crate) const fn object_key(object: usize) -> u64 { object as u64 | 1 }

    pub(crate) const fn new(vm: u64) -> Self {
        Self { vm, spans: [Span::EMPTY; MAX_SPANS], count: 0, objects: 0, truncated: false }
    }

    pub(crate) fn spans(&self) -> &[Span] { &self.spans[..self.count as usize] }
    pub(crate) fn is_empty(&self) -> bool { self.count == 0 }
    pub(crate) fn object_spans(&self) -> u8 { self.objects }
    pub(crate) fn truncated(&self) -> bool { self.truncated }

    fn push(&mut self, key: u64, start: u64, end: u64) -> bool {
        if start >= end { return true; }
        let Some(slot) = self.spans.get_mut(self.count as usize) else {
            self.truncated = true;
            return false;
        };
        *slot = Span { key, start, end };
        self.count += 1;
        true
    }

    /// A GPU VA range of this footprint's VM.
    pub(crate) fn push_vm_span(&mut self, start: u64, end: u64) -> bool {
        self.push(Self::vm_key(self.vm), start, end)
    }

    /// An offset range of a shareable GEM object.
    pub(crate) fn push_object_span(&mut self, object: usize, start: u64, end: u64) -> bool {
        let pushed = self.push(Self::object_key(object), start, end);
        if pushed && start < end { self.objects += 1; }
        pushed
    }

    /// Whether the two renders may store to the same bytes. Different VMs
    /// can only meet in a shared object, so they compare nothing unless both
    /// carry object spans.
    pub(crate) fn overlaps(&self, other: &Footprint) -> bool {
        if self.vm != other.vm && (self.objects == 0 || other.objects == 0) {
            return false;
        }
        self.spans().iter().any(|a| other.spans().iter().any(|b|
            a.key == b.key && a.start < b.end && b.start < a.end))
    }
}

/// `g17_submission::g17p_render_prefix_contains`: the render's own logical
/// prefix covers `sequence`, so firmware already orders it after that one.
pub(crate) const fn ordered_after(wait_through: Option<u64>, sequence: u64) -> bool {
    match wait_through {
        Some(limit) => sequence != 0 && sequence <= limit,
        None => false,
    }
}

/// What a render about to take a pair lease must do.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    /// No unordered overlap: the ordinary pair selection.
    Free,
    /// Publish on this pair, behind the same owner's overlapping render.
    Stack(u8),
    /// Wait for an overlapping render to complete (another owner, or the
    /// same owner's renders on two different pairs).
    Wait,
}

/// One render holding a pair lease, or (`pending`) waiting for one.
pub(crate) struct Inflight<'a> {
    pub(crate) owner: u64,
    pub(crate) sequence: u64,
    pub(crate) slot: u8,
    pub(crate) footprint: &'a Footprint,
    /// Its pair is quarantined: it may never complete, and its client has
    /// already been failed; it constrains nothing.
    pub(crate) quarantined: bool,
    /// Waiting for a conflict, with its arrival `ticket`; holds no pair.
    pub(crate) pending: bool,
    pub(crate) ticket: u64,
}

/// The plan for a render of `owner` with prefix `wait_through`; `ticket` is
/// its own arrival ticket once it has waited.
pub(crate) fn plan<'a>(
    mask: u32,
    owner: u64,
    wait_through: Option<u64>,
    ticket: Option<u64>,
    footprint: &Footprint,
    inflight: impl IntoIterator<Item = Inflight<'a>>,
) -> Plan {
    if mask & DEFAULT_MASK == 0 || footprint.is_empty() {
        return Plan::Free;
    }
    let mut stack: Option<u8> = None;
    for other in inflight {
        if other.quarantined || !footprint.overlaps(other.footprint) {
            continue;
        }
        if other.pending {
            // An older waiting render of another queue goes first. A queue's
            // own renders are already published one at a time, in order.
            if other.owner != owner && mask & CROSS_OWNER != 0
                && ticket.is_none_or(|mine| other.ticket < mine)
            {
                return Plan::Wait;
            }
            continue;
        }
        if other.owner == owner {
            if mask & SAME_OWNER == 0 || ordered_after(wait_through, other.sequence) {
                continue;
            }
            match stack {
                None => stack = Some(other.slot),
                Some(slot) if slot == other.slot => {}
                // Unordered overlaps on two pairs: one pair cannot order both.
                Some(_) => return Plan::Wait,
            }
        } else if mask & CROSS_OWNER != 0 {
            return Plan::Wait;
        }
    }
    stack.map_or(Plan::Free, Plan::Stack)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMAGE: (u64, u64) = (0x1_0000_0000, 0x1_0004_0000);

    fn image(vm: u64) -> Footprint {
        let mut footprint = Footprint::new(vm);
        assert!(footprint.push_vm_span(IMAGE.0, IMAGE.1));
        footprint
    }

    fn inflight(owner: u64, sequence: u64, slot: u8, footprint: &Footprint) -> Inflight<'_> {
        Inflight { owner, sequence, slot, footprint, quarantined: false, pending: false, ticket: 0 }
    }

    #[test]
    fn spans_compare_per_key() {
        let a = image(7);
        let mut other_image = Footprint::new(7);
        other_image.push_vm_span(IMAGE.1, IMAGE.1 + 0x4000);
        assert!(a.overlaps(&image(7)));
        assert!(!a.overlaps(&other_image), "adjacent images do not overlap");
        assert!(!a.overlaps(&image(8)), "same VA in another VM is other memory");
        // One dma-buf, two VMs at different VAs: the object span decides.
        let mut parent = Footprint::new(7);
        parent.push_vm_span(0x2_0000_0000, 0x2_0004_0000);
        parent.push_object_span(0xffff_8000_1234_5600, 0, 0x4_0000);
        let mut child = Footprint::new(9);
        child.push_vm_span(0x5_0000_0000, 0x5_0004_0000);
        child.push_object_span(0xffff_8000_1234_5600, 0x3_c000, 0x4_0000);
        assert!(parent.overlaps(&child) && child.overlaps(&parent));
        let mut stranger = Footprint::new(9);
        stranger.push_object_span(0xffff_8000_1234_5700, 0, 0x4_0000);
        assert!(!parent.overlaps(&stranger));
        // A VM span never equals an object span key.
        assert_ne!(Footprint::vm_key(0x7f), Footprint::object_key(0xfe));
    }

    #[test]
    fn capacity_is_marked_not_silent() {
        let mut footprint = Footprint::new(1);
        for index in 0..MAX_SPANS as u64 {
            assert!(footprint.push_vm_span(index * 0x10000, index * 0x10000 + 0x4000));
        }
        assert!(!footprint.push_object_span(0x1000, 0, 0x4000));
        assert!(footprint.truncated() && footprint.spans().len() == MAX_SPANS);
        assert!(footprint.push_vm_span(5, 5), "an empty range records nothing");
    }

    #[test]
    fn nothing_moves_without_an_unordered_overlap() {
        let mine = image(7);
        let elsewhere = {
            let mut footprint = Footprint::new(7);
            footprint.push_vm_span(0x3_0000_0000, 0x3_0001_0000);
            footprint
        };
        // MC/d2 shape: renders of one queue on one image, barrier-ordered.
        assert_eq!(plan(DEFAULT_MASK, 1, Some(41), None, &mine,
            [inflight(1, 40, 3, &mine), inflight(1, 41, 4, &mine)]), Plan::Free);
        // Another queue writing another image (compositor-style).
        assert_eq!(plan(DEFAULT_MASK, 1, None, None, &mine, [inflight(2, 9, 5, &elsewhere)]), Plan::Free);
        // Another process at the same VA but another VM, no shared object.
        assert_eq!(plan(DEFAULT_MASK, 1, None, None, &mine, [inflight(2, 9, 5, &image(8))]), Plan::Free);
        assert_eq!(plan(0, 1, None, None, &mine, [inflight(2, 9, 5, &mine)]), Plan::Free);
        assert_eq!(plan(DEFAULT_MASK, 1, None, None, &Footprint::new(7), [inflight(2, 9, 5, &mine)]), Plan::Free);
    }

    #[test]
    fn same_owner_unordered_overlap_stacks_on_its_pair() {
        let mine = image(7);
        assert_eq!(plan(DEFAULT_MASK, 1, None, None, &mine, [inflight(1, 40, 3, &mine)]), Plan::Stack(3));
        assert_eq!(plan(DEFAULT_MASK, 1, Some(39), None, &mine, [inflight(1, 40, 3, &mine)]), Plan::Stack(3));
        assert_eq!(plan(DEFAULT_MASK, 1, None, None, &mine,
            [inflight(1, 40, 3, &mine), inflight(1, 41, 3, &mine)]), Plan::Stack(3));
        // Two pairs cannot both be the predecessor: wait for one to retire.
        assert_eq!(plan(DEFAULT_MASK, 1, None, None, &mine,
            [inflight(1, 40, 3, &mine), inflight(1, 41, 4, &mine)]), Plan::Wait);
        assert_eq!(plan(CROSS_OWNER, 1, None, None, &mine, [inflight(1, 40, 3, &mine)]), Plan::Free);
    }

    #[test]
    fn another_queue_or_process_waits_for_the_overlapping_render() {
        let mine = image(7);
        // tilewaw `queues`: two VkQueues of one VM.
        assert_eq!(plan(DEFAULT_MASK, 1, None, None, &mine, [inflight(2, 9, 5, &mine)]), Plan::Wait);
        // A same-owner affinity does not override another owner's conflict.
        assert_eq!(plan(DEFAULT_MASK, 1, None, None, &mine,
            [inflight(1, 40, 3, &mine), inflight(2, 9, 5, &mine)]), Plan::Wait);
        // tilewaw `xproc`: the child's VM, the parent's object.
        let mut parent = Footprint::new(7);
        parent.push_vm_span(IMAGE.0, IMAGE.1);
        parent.push_object_span(0xffff_8000_0000_1000, 0, 0x4_0000);
        let mut child = Footprint::new(9);
        child.push_vm_span(0x9_0000_0000, 0x9_0004_0000);
        child.push_object_span(0xffff_8000_0000_1000, 0, 0x4_0000);
        assert_eq!(plan(DEFAULT_MASK, 1, None, None, &parent, [inflight(2, 9, 5, &child)]), Plan::Wait);
        assert_eq!(plan(SAME_OWNER, 1, None, None, &parent, [inflight(2, 9, 5, &child)]), Plan::Free);
        // A quarantined pair's render never completes; it constrains nothing.
        let stuck = Inflight { quarantined: true, ..inflight(2, 9, 5, &mine) };
        assert_eq!(plan(DEFAULT_MASK, 1, None, None, &mine, [stuck]), Plan::Free);
    }

    #[test]
    fn an_older_waiting_render_goes_first() {
        let mine = image(7);
        let waiting = |owner, ticket| Inflight { pending: true, ticket, ..inflight(owner, 9, u8::MAX, &mine) };
        // A newer arrival of another queue waits behind the waiting render...
        assert_eq!(plan(DEFAULT_MASK, 1, None, None, &mine, [waiting(2, 5)]), Plan::Wait);
        assert_eq!(plan(DEFAULT_MASK, 1, None, Some(6), &mine, [waiting(2, 5)]), Plan::Wait);
        // ...the older one does not wait for a newer one, nor for itself.
        assert_eq!(plan(DEFAULT_MASK, 1, None, Some(4), &mine, [waiting(2, 5)]), Plan::Free);
        assert_eq!(plan(DEFAULT_MASK, 2, None, Some(5), &mine, [waiting(2, 5)]), Plan::Free);
        // Disjoint or cross-owner-disabled: nothing.
        assert_eq!(plan(DEFAULT_MASK, 1, None, None, &image(8), [waiting(2, 5)]), Plan::Free);
        assert_eq!(plan(SAME_OWNER, 1, None, None, &mine, [waiting(2, 5)]), Plan::Free);
    }

    #[test]
    fn prefix_matches_the_submission_rule() {
        assert!(ordered_after(Some(5), 5) && ordered_after(Some(5), 1));
        assert!(!ordered_after(Some(5), 6) && !ordered_after(None, 1) && !ordered_after(Some(5), 0));
    }
}
