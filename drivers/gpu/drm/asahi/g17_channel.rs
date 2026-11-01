// SPDX-License-Identifier: GPL-2.0-only OR MIT
//! Device-wide EventMachine stamp/QID ownership.
//!
//! A QID identifies a lease, not an engine or a logical queue. The inspected
//! native EventMachine validates the channel owner before reusing a cached ID.
//! Published Linux channels currently retain their leases until processor stop:
//! a completed command does not by itself prove scheduler-resource detachment.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub(crate) const QID_COUNT: usize = 128;

/// Device-wide QID gauges for `g17p_queue_slots`, indexed by [`Engine`].
/// Kept by every table mutation and undone by `Drop`, so replacing the table
/// at processor stop leaves only the new table's leases counted.
pub(crate) static LEASED: [AtomicU32; 3] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];
pub(crate) static PUBLISHED: [AtomicU32; 3] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];
/// Most QIDs leased at once since load.
pub(crate) static HIGH_WATER: AtomicU32 = AtomicU32::new(0);
/// `acquire` calls that found no free QID (the ENOSPC of queue creation and
/// of a new render pair).
pub(crate) static EXHAUSTED: AtomicU64 = AtomicU64::new(0);
/// New compute graphs refused to keep [`COMPUTE_RENDER_RESERVE`] QIDs free.
pub(crate) static RESERVE_REFUSED: AtomicU64 = AtomicU64::new(0);

/// QIDs a new compute graph may never take: kept for render pairs.
///
/// A published QID is only returned at processor stop, and a quarantined or
/// exit-killed compute graph is never reused, so compute faults consume the
/// table one QID at a time. Without a reserve the last free QID goes to a
/// compute graph, and from then on every client that needs a new render
/// pair -- every new desktop client -- fails its first render with ENOSPC.
/// With it, exhaustion fails the next compute queue creation instead. 16 is
/// eight render pairs.
pub(crate) const COMPUTE_RENDER_RESERVE: usize = 16;

/// Total QIDs currently leased, all engines.
pub(crate) fn leased_total() -> u32 {
    LEASED.iter().map(|count| count.load(Ordering::Relaxed)).sum()
}

/// Stable host channel identity, distinct from its reusable numerical QID.
/// Identity zero is invalid; identities must not be recycled within a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChannelId(pub(crate) u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Engine {
    Tiling,
    Fragment,
    Compute,
}

impl Engine {
    const fn gauge(self) -> usize {
        match self {
            Engine::Tiling => 0,
            Engine::Fragment => 1,
            Engine::Compute => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Lease {
    pub(crate) qid: u8,
    channel: ChannelId,
    engine: Engine,
}

#[derive(Clone, Copy)]
struct Entry {
    channel: ChannelId,
    engine: Engine,
    published: bool,
}

/// All mutation is serialized by the runtime mutex. No per-submit allocation,
/// sleeping, engine partition or production scheduling gate is required.
pub(crate) struct ChannelTable {
    entries: [Option<Entry>; QID_COUNT],
}

impl ChannelTable {
    pub(crate) const fn new() -> Self {
        Self { entries: [None; QID_COUNT] }
    }

    /// Only an owner match validates a cached QID. Also reject engine changes:
    /// TA and 3D are separate channels even when they share one logical owner.
    pub(crate) fn matches(&self, lease: Lease) -> bool {
        self.entries.get(usize::from(lease.qid)).is_some_and(|entry| {
            entry.is_some_and(|entry| {
                entry.channel == lease.channel && entry.engine == lease.engine
            })
        })
    }

    /// Acquire the existing channel's ID or an empty slot. A preferred index is
    /// placement policy for the unsubmitted bootstrap only, not an engine cap.
    pub(crate) fn acquire(
        &mut self, channel: ChannelId, engine: Engine, preferred: Option<u8>,
    ) -> Option<Lease> {
        if channel.0 == 0 { return None; }
        if let Some((qid, entry)) = self.entries.iter().enumerate().find_map(|(qid, entry)| {
            entry.filter(|entry| entry.channel == channel).map(|entry| (qid, entry))
        }) {
            return (entry.engine == engine).then_some(Lease { qid: qid as u8, channel, engine });
        }
        let qid = preferred.filter(|qid| {
            self.entries.get(usize::from(*qid)).is_some_and(Option::is_none)
        }).map(usize::from).or_else(|| self.entries.iter().position(Option::is_none));
        let Some(qid) = qid else {
            EXHAUSTED.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        self.entries[qid] = Some(Entry { channel, engine, published: false });
        LEASED[engine.gauge()].fetch_add(1, Ordering::Relaxed);
        HIGH_WATER.fetch_max(leased_total(), Ordering::Relaxed);
        Some(Lease { qid: qid as u8, channel, engine })
    }

    /// Free QIDs left in the table.
    pub(crate) fn free(&self) -> usize {
        self.entries.iter().filter(|entry| entry.is_none()).count()
    }

    /// [`acquire`](Self::acquire), except that a new lease may not take one
    /// of the last `reserve` free QIDs. An existing channel keeps its QID.
    pub(crate) fn acquire_reserving(
        &mut self, channel: ChannelId, engine: Engine, preferred: Option<u8>, reserve: usize,
    ) -> Option<Lease> {
        let existing = self.entries.iter().any(|entry| entry.is_some_and(|entry| entry.channel == channel));
        if !existing && self.free() <= reserve {
            RESERVE_REFUSED.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        self.acquire(channel, engine, preferred)
    }

    /// Publication is irreversible until a verified detach or processor stop.
    /// In particular, timeout, cancellation and a signalled error fence do not
    /// return an installed QID to the free pool.
    pub(crate) fn publish(&mut self, lease: Lease) -> bool {
        if !self.matches(lease) { return false; }
        let entry = self.entries[usize::from(lease.qid)].as_mut().unwrap();
        if !entry.published {
            PUBLISHED[entry.engine.gauge()].fetch_add(1, Ordering::Relaxed);
        }
        entry.published = true;
        true
    }

    /// Constructor rollback is safe only before any firmware-visible publish.
    /// The exact owner check prevents a stale rollback from freeing a new lease.
    pub(crate) fn cancel_unpublished(&mut self, lease: Lease) -> bool {
        if !self.matches(lease) { return false; }
        let entry = &mut self.entries[usize::from(lease.qid)];
        if entry.is_some_and(|entry| entry.published) { return false; }
        *entry = None;
        LEASED[lease.engine.gauge()].fetch_sub(1, Ordering::Relaxed);
        true
    }
}

impl Drop for ChannelTable {
    fn drop(&mut self) {
        for entry in self.entries.iter().flatten() {
            LEASED[entry.engine.gauge()].fetch_sub(1, Ordering::Relaxed);
            if entry.published {
                PUBLISHED[entry.engine.gauge()].fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_engines_share_one_pool_and_cached_ids_need_owner_match() {
        let mut table = ChannelTable::new();
        let ta = table.acquire(ChannelId(1), Engine::Tiling, Some(15)).unwrap();
        let cl = table.acquire(ChannelId(2), Engine::Compute, Some(15)).unwrap();
        let d3 = table.acquire(ChannelId(3), Engine::Fragment, Some(6)).unwrap();
        assert_eq!((ta.qid, cl.qid, d3.qid), (15, 0, 6));
        assert_eq!(table.acquire(ChannelId(1), Engine::Tiling, Some(0)), Some(ta));
        assert_eq!(table.acquire(ChannelId(1), Engine::Compute, None), None);
        assert!(!table.matches(Lease { channel: ChannelId(4), ..ta }));
    }

    #[test]
    fn unsubmitted_failure_returns_credit_but_old_token_cannot_release_replacement() {
        let mut table = ChannelTable::new();
        let old = table.acquire(ChannelId(1), Engine::Compute, None).unwrap();
        assert!(table.cancel_unpublished(old));
        let new = table.acquire(ChannelId(2), Engine::Fragment, None).unwrap();
        assert_eq!(old.qid, new.qid);
        assert!(!table.cancel_unpublished(old));
        assert!(!table.publish(old));
        assert!(table.matches(new));
    }

    #[test]
    fn installed_channel_is_retained_and_namespace_never_wraps() {
        let mut table = ChannelTable::new();
        for index in 0..QID_COUNT {
            let engine = [Engine::Tiling, Engine::Fragment, Engine::Compute][index % 3];
            let lease = table.acquire(ChannelId(index as u64 + 1), engine, None).unwrap();
            assert_eq!(usize::from(lease.qid), index);
            assert!(table.publish(lease));
            assert!(!table.cancel_unpublished(lease));
        }
        assert_eq!(table.acquire(ChannelId(129), Engine::Tiling, None), None);
        assert_eq!(table.acquire(ChannelId(128), Engine::Fragment, None).unwrap().qid, 127);
    }

    #[test]
    fn reserve_refuses_only_new_leases() {
        let mut table = ChannelTable::new();
        for index in 0..QID_COUNT - 4 {
            assert!(table.acquire(ChannelId(index as u64 + 1), Engine::Tiling, None).is_some());
        }
        assert_eq!(table.free(), 4);
        assert_eq!(table.acquire_reserving(ChannelId(1000), Engine::Compute, None, 4), None);
        let kept = table.acquire_reserving(ChannelId(1), Engine::Tiling, None, 4).unwrap();
        assert_eq!(kept.qid, 0);
        assert!(table.acquire_reserving(ChannelId(1000), Engine::Compute, None, 3).is_some());
        assert_eq!(table.free(), 3);
    }

    #[test]
    fn invalid_owner_or_cached_qid_cannot_touch_table() {
        let mut table = ChannelTable::new();
        assert_eq!(table.acquire(ChannelId(0), Engine::Tiling, None), None);
        let lease = table.acquire(ChannelId(1), Engine::Tiling, Some(255)).unwrap();
        assert_eq!(lease.qid, 0);
        assert!(!table.matches(Lease { qid: 255, ..lease }));
        assert!(!table.publish(Lease { qid: 128, ..lease }));
        assert!(!table.cancel_unpublished(Lease { qid: 255, ..lease }));
    }
}
