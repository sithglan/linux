// SPDX-License-Identifier: GPL-2.0-only OR MIT

//! Register-state identities shared by the engines of one execution context.
//!
//! The hardware register key contains both the context and the low six state
//! bits. Keep independent contexts independent; do not impose a device-wide
//! 63-command limit. This is Linux's explicit ownership policy, not a claim
//! that Apple's task-scoped allocator has identical exclusion semantics.

use core::sync::atomic::{AtomicU32, Ordering};

const COUNT: usize = 64;
const FREE: u32 = u32::MAX; // Low bits 63 are reserved, so never a live word.

/// Native timestamp-queue state. The first word is a pointer to the separate
/// submitted count; +8 is the SAME full allocation word used by execution.
/// Firmware sorts nodes by this full word and uses its low six bits for cache
/// maintenance. Neither the word nor the node may change before final stamps.
pub(crate) const TSQ_NODE_STRIDE: usize = 0x100;
pub(crate) const TSQ_NODE_BYTES: usize = 0xc8;
pub(crate) const TSQ_STATE_COUNT: usize = 63;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TsqSlot(u32);

impl TsqSlot {
    pub(crate) const fn new(value: u32) -> Option<Self> {
        if value < 4 { Some(Self(value)) } else { None }
    }
    pub(crate) const fn value(self) -> u32 { self.0 }

    /// TA's u32 slot field is unaligned, so never access it through a u32 ref.
    pub(crate) fn write(self, descriptor: &mut [u8], offset: usize) -> bool {
        let Some(end) = offset.checked_add(4) else { return false; };
        let Some(bytes) = descriptor.get_mut(offset..end) else { return false; };
        bytes.copy_from_slice(&self.0.to_le_bytes());
        true
    }

    pub(crate) fn bind_compute(self, descriptor: &mut [u8], node: u64) -> bool {
        if node == 0 || node & 0xff != 0 || descriptor.len() < 0xf5c { return false; }
        descriptor[0x10..0x18].copy_from_slice(&node.to_le_bytes());
        self.write(descriptor, 0xf58)
    }

    pub(crate) fn bind_render(self, ta: &mut [u8], fragment: &mut [u8], node: u64) -> bool {
        if node == 0 || node & 0xff != 0 || ta.len() < 0x8c2 || fragment.len() < 0x215c {
            return false;
        }
        ta[0x10..0x18].copy_from_slice(&node.to_le_bytes());
        fragment[0x20..0x28].copy_from_slice(&node.to_le_bytes());
        self.write(ta, 0x8be) && self.write(fragment, 0x2158)
    }
}

/// Stable owner-local render workqueue ordinals. A physical graph's QID is
/// only its lookup key: it is never the slot value or reduced modulo two.
/// Entries live as long as the logical context, including quarantined graphs.
pub(crate) struct RenderTsqSlots {
    pairs: [Option<u8>; 2],
    // Physical ordinals persist across clients; channel payloads do not.
    payload_origins: [Option<u64>; 2],
}

impl RenderTsqSlots {
    pub(crate) const fn new() -> Self { Self { pairs: [None; 2], payload_origins: [None; 2] } }

    /// Next publication's byte for this logical owner's channel. Retrying
    /// unpublished work is idempotent. Neither PM growth nor another channel
    /// changes its origin; a fresh logical owner starts a new set of origins.
    pub(crate) fn channel_payload(&mut self, pair: u8, ordinal: u64) -> Option<u8> {
        let slot = self.get_or_assign(pair)?.value() as usize;
        let origin = *self.payload_origins[slot].get_or_insert(ordinal);
        Some(ordinal.checked_sub(origin)?.wrapping_add(1) as u8)
    }

    pub(crate) fn get_or_assign(&mut self, pair: u8) -> Option<TsqSlot> {
        if let Some(index) = self.pairs.iter().position(|p| *p == Some(pair)) {
            return TsqSlot::new(index as u32);
        }
        let index = self.pairs.iter().position(Option::is_none)?;
        self.pairs[index] = Some(pair);
        TsqSlot::new(index as u32)
    }
}

/// Initialize ONLY a newly acquired state, before any descriptor exposes it.
/// Callers must hold the state lease, not infer reuse from list membership:
/// firmware unlinks before issuing maintenance and writing visibility stamps.
pub(crate) fn initialize_tsq_node(
    node: &mut [u8], submitted_address: u64, word: u32, threshold: u32,
) -> bool {
    if word & 63 == 63 || submitted_address == 0 || submitted_address & 3 != 0 {
        return false;
    }
    let Some(node) = node.get_mut(..TSQ_NODE_BYTES) else { return false; };
    node.fill(0);
    node[..8].copy_from_slice(&submitted_address.to_le_bytes());
    node[8..12].copy_from_slice(&word.to_le_bytes());
    node[0x10..0x14].copy_from_slice(&threshold.to_le_bytes());
    true
}

pub(crate) struct WorkStatePool {
    next: AtomicU32,
    live: [AtomicU32; COUNT],
}

impl WorkStatePool {
    pub(crate) const fn new() -> Self {
        Self {
            next: AtomicU32::new(0),
            live: [const { AtomicU32::new(FREE) }; COUNT],
        }
    }

    pub(crate) fn allocate(&self) -> Option<u32> {
        for _ in 0..COUNT {
            let word = self.next.fetch_add(1, Ordering::Relaxed);
            let index = (word & 63) as usize;
            if index == 63 { continue; }
            if self.live[index].compare_exchange(
                FREE, word, Ordering::AcqRel, Ordering::Acquire,
            ).is_ok() {
                return Some(word);
            }
        }
        // Contention can make this a transient miss. Never wait while holding
        // another queue's publication lock; admission retries outside it.
        None
    }

    pub(crate) fn has_free(&self) -> bool {
        self.live[..63].iter().any(|state| state.load(Ordering::Acquire) == FREE)
    }

    pub(crate) fn release(&self, word: u32) -> bool {
        let index = (word & 63) as usize;
        index != 63 && self.live[index].compare_exchange(
            word, FREE, Ordering::AcqRel, Ordering::Acquire,
        ).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsq_initialization_preserves_native_identity_and_separate_count_pointer() {
        let mut node = [0xa5; TSQ_NODE_STRIDE];
        let pointer = 0xffff_fc20_0010_4004;
        let word = 0x1234_5605;
        assert!(initialize_tsq_node(&mut node, pointer, word, 80));
        assert_eq!(u64::from_le_bytes(node[..8].try_into().unwrap()), pointer);
        assert_eq!(u32::from_le_bytes(node[8..12].try_into().unwrap()), word);
        assert_eq!(u32::from_le_bytes(node[12..16].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(node[16..20].try_into().unwrap()), 80);
        assert!(node[20..TSQ_NODE_BYTES].iter().all(|b| *b == 0));
        assert!(node[TSQ_NODE_BYTES..].iter().all(|b| *b == 0xa5));
    }

    #[test]
    fn invalid_tsq_identity_does_not_partially_rewrite_a_live_node() {
        let before = [0x5a; TSQ_NODE_STRIDE];
        for (address, word) in [(0, 0), (0x1001, 0), (0x1000, 63), (0x1000, u32::MAX)] {
            let mut node = before;
            assert!(!initialize_tsq_node(&mut node, address, word, 80));
            assert_eq!(node, before);
        }
        let mut short = [0x5a; TSQ_NODE_BYTES - 1];
        assert!(!initialize_tsq_node(&mut short, 0x1000, 0, 80));
        assert!(short.iter().all(|b| *b == 0x5a));
    }

    #[test]
    fn two_pairs_with_same_physical_parity_keep_distinct_stable_slots() {
        let mut slots = RenderTsqSlots::new();
        let a = slots.get_or_assign(3).unwrap();
        let b = slots.get_or_assign(7).unwrap();
        assert_eq!((a.value(), b.value()), (0, 1));
        assert_eq!(slots.get_or_assign(7), Some(b));
        assert_eq!(slots.get_or_assign(3), Some(a));
        assert_eq!(slots.get_or_assign(11), None);
        let mut other = RenderTsqSlots::new();
        assert_eq!(other.get_or_assign(11).unwrap().value(), 0);
    }

    #[test]
    fn new_client_payload_restarts_while_physical_ordinal_continues() {
        for first in [0, 6, 11, 16, 21, 26, 17689] {
            let mut channels = RenderTsqSlots::new();
            assert_eq!(channels.channel_payload(3, first), Some(1));
            assert_eq!(channels.channel_payload(3, first), Some(1));
            assert_eq!(channels.channel_payload(3, first + 1), Some(2));
            assert_eq!(channels.channel_payload(3, first + 255), Some(0));
            assert_eq!(channels.channel_payload(3, first + 256), Some(1));
            assert_eq!(channels.channel_payload(7, first + 400), Some(1));
            assert_eq!(channels.channel_payload(3, first + 257), Some(2));
            if first != 0 { assert_eq!(channels.channel_payload(3, first - 1), None); }
            assert_eq!(channels.channel_payload(11, first), None);
        }
    }

    #[test]
    fn tsq_slot_is_a_full_u32_even_at_the_unaligned_ta_offset() {
        let mut bytes = [0xa5; 0x8c4];
        assert!(TsqSlot::new(3).unwrap().write(&mut bytes, 0x8be));
        assert_eq!(&bytes[0x8be..0x8c2], &[3, 0, 0, 0]);
        assert_eq!(bytes[0x8bd], 0xa5);
        assert_eq!(bytes[0x8c2], 0xa5);
        assert!(TsqSlot::new(4).is_none());
        let before = bytes;
        assert!(!TsqSlot::new(0).unwrap().write(&mut bytes, usize::MAX));
        assert_eq!(bytes, before);
    }

    #[test]
    fn mixed_group_shares_node_without_aliasing_engine_channel_slots() {
        let node = 0xffff_fc20_0100_0500u64;
        let mut pairs = RenderTsqSlots::new();
        let mut ta = [[0xa5; 0x900]; 2];
        let mut fragment = [[0xa5; 0x2200]; 2];
        for (i, physical) in [3, 7].into_iter().enumerate() {
            let slot = pairs.get_or_assign(physical).unwrap();
            assert!(slot.bind_render(&mut ta[i], &mut fragment[i], node));
            assert_eq!(&ta[i][0x10..0x18], &node.to_le_bytes());
            assert_eq!(&fragment[i][0x20..0x28], &node.to_le_bytes());
            assert_eq!(&ta[i][0x8be..0x8c2], &(i as u32).to_le_bytes());
            assert_eq!(&fragment[i][0x2158..0x215c], &(i as u32).to_le_bytes());
            // Event index and force-drain flag belong to different namespaces.
            assert_eq!(&ta[i][0x8ba..0x8be], &[0xa5; 4]);
            assert_eq!(&fragment[i][0x2154..0x2158], &[0xa5; 4]);
        }
        let mut cl = [0xa5; 0x1000];
        assert!(TsqSlot::new(0).unwrap().bind_compute(&mut cl, node));
        assert_eq!(&cl[0x10..0x18], &node.to_le_bytes());
        assert_eq!(&cl[0xf58..0xf5c], &[0; 4]);
        assert_eq!(&cl[0xf54..0xf58], &[0xa5; 4]);
    }

    #[test]
    fn rejected_render_binding_changes_neither_descriptor() {
        let mut ta = [0xa5; 0x900];
        let mut fragment = [0xa5; 0x215b];
        assert!(!TsqSlot::new(0).unwrap().bind_render(&mut ta, &mut fragment, 0x1000));
        assert!(ta.iter().chain(fragment.iter()).all(|b| *b == 0xa5));
    }

    #[test]
    fn independent_contexts_have_full_capacity() {
        let a = WorkStatePool::new();
        let b = WorkStatePool::new();
        let words: Vec<_> = (0..63).map(|_| a.allocate().unwrap()).collect();
        assert!(!a.has_free());
        assert!(a.allocate().is_none());
        for _ in 0..63 { assert!(b.allocate().is_some()); }
        for word in words { assert!(a.release(word)); }
        assert!(a.has_free());
    }

    #[test]
    fn stale_release_cannot_free_a_reused_identity() {
        let pool = WorkStatePool::new();
        let old = pool.allocate().unwrap();
        assert!(pool.release(old));
        pool.next.store(64, Ordering::Relaxed);
        let new = pool.allocate().unwrap();
        assert_eq!(old & 63, new & 63);
        assert!(!pool.release(old));
        assert_eq!(pool.live[0].load(Ordering::Relaxed), new);
        assert!(pool.release(new));
    }

    #[test]
    fn wrap_skips_reserved_state() {
        let pool = WorkStatePool::new();
        pool.next.store(u32::MAX - 1, Ordering::Relaxed);
        assert_eq!(pool.allocate(), Some(u32::MAX - 1));
        assert_eq!(pool.allocate(), Some(0));
        assert!(!pool.release(u32::MAX));
    }

    #[test]
    fn mixed_engine_depth_never_waits_for_an_unrelated_context() {
        let contexts: Vec<_> = (0..8).map(|_| WorkStatePool::new()).collect();
        for pool in &contexts {
            // Two 16-deep render pairs plus one 16-deep compute queue.
            let words: Vec<_> = (0..48).map(|_| pool.allocate().unwrap()).collect();
            for (i, word) in words.iter().enumerate() {
                assert!(words[..i].iter().all(|earlier| earlier & 63 != word & 63));
            }
            for word in words.into_iter().rev() { assert!(pool.release(word)); }
        }
    }
}
