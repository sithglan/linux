// SPDX-License-Identifier: GPL-2.0-only OR MIT
//! Completion ownership for the bounded M5 TA/fragment pair.
//!
//! 25G76 GFX 0x17544 compares scheduler.completed with *scheduler.submitted;
//! 0x1763c unlinks the completed scheduler. The submitted total is host-owned,
//! not a firmware-decremented pending count. 0x17a68/0x17ae0 then write the
//! first stamp pointer and 0x17b98 publishes the corresponding event mask.

// The 32-bit completion value reserves its low byte. Stop before it wraps;
// storage-ring wrap is independent of this monotonically increasing stamp.
pub(crate) const MAX_GRAPHICS_GENERATION: u32 = 0x00ff_ffff;
pub(crate) const GRAPHICS_POINTER_COUNT: u32 = 0x500;

pub(crate) const fn graphics_pointer_head(generation: u32) -> u32 {
    (1 + generation * 3) % GRAPHICS_POINTER_COUNT
}

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub(crate) struct M5GraphicsCompletion {
    pub(crate) submitted: u32,
    pub(crate) completed: u32,
    pub(crate) linked: u32,
    pub(crate) list: [u64; 2],
    pub(crate) stamps: [u32; 2],
    pub(crate) signalled: u128,
}

/// The same native scheduler retirement contract for one compute record.
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub(crate) struct M5ComputeCompletion {
    pub(crate) submitted: u32,
    pub(crate) completed: u32,
    pub(crate) linked: u32,
    pub(crate) list: [u64; 2],
    pub(crate) stamp: u32,
    pub(crate) signalled: u128,
}

impl M5ComputeCompletion {
    pub(crate) fn complete_at(self, list_address: u64, queue_id: u8, stamp: u32) -> bool {
        list_address != 0 && queue_id < 128 && stamp != 0 && stamp & 0xff == 0
            && self.submitted == 1 && self.completed == 1 && self.linked == 0
            && self.list == [0, list_address] && self.stamp == stamp
            && self.signalled & (1u128 << queue_id) != 0
    }
}

impl M5GraphicsCompletion {
    /// This diagnostic owns qids 10/11 and event stamp 0x100 on a fresh boot.
    /// Queue retirement and framebuffer verification remain separate gates.
    pub(crate) fn pair_complete(self, list_address: u64) -> bool {
        self.pair_complete_at(list_address, 0x100)
    }

    pub(crate) fn pair_complete_at(self, list_address: u64, stamp: u32) -> bool {
        list_address != 0
            && stamp != 0 && stamp & 0xff == 0
            && self.submitted == 2
            && self.completed == self.submitted
            && self.linked == 0
            && self.list == [0, list_address]
            && self.stamps == [stamp; 2]
            && self.signalled & 0xc00 == 0xc00
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: u64 = 0xffff_fc20_c002_0000;

    fn finished() -> M5GraphicsCompletion {
        M5GraphicsCompletion { submitted: 2, completed: 2, linked: 0,
            list: [0, LIST], stamps: [0x100; 2], signalled: 0xc00 }
    }

    #[test]
    fn submitted_total_does_not_decrement_on_completion() {
        assert!(finished().pair_complete(LIST));
        assert!(!M5GraphicsCompletion { submitted: 0, ..finished() }.pair_complete(LIST));
    }

    #[test]
    fn rendered_but_stranded_pair_is_not_complete() {
        // Retained Linux draw before sharing the two channels' job list.
        let stranded = M5GraphicsCompletion { completed: 2, linked: 1,
            list: [0xffff_fc20_c100_8000, LIST], stamps: [0; 2],
            signalled: 0, ..finished() };
        assert!(!stranded.pair_complete(LIST));
    }

    #[test]
    fn each_stage_needs_its_own_current_stamp_and_signal() {
        for stage in 0..2 {
            let mut state = finished();
            state.stamps[stage] = 0;
            assert!(!state.pair_complete(LIST));
            state.stamps[stage] = 0x200;
            assert!(!state.pair_complete(LIST));
            state = finished();
            state.signalled &= !(1 << (stage + 10));
            assert!(!state.pair_complete(LIST));
        }
    }

    #[test]
    fn empty_or_partial_observations_cannot_release_the_pair() {
        assert!(!M5GraphicsCompletion::default().pair_complete(LIST));
        assert!(!M5GraphicsCompletion { completed: 1, ..finished() }.pair_complete(LIST));
        assert!(!M5GraphicsCompletion { list: [0, LIST + 8], ..finished() }.pair_complete(LIST));
        assert!(!M5GraphicsCompletion { linked: 1, ..finished() }.pair_complete(LIST));
        assert!(!finished().pair_complete(0));
    }

    #[test]
    fn previous_frame_signals_cannot_complete_a_new_frame() {
        assert!(!finished().pair_complete_at(LIST, 0x200));
        assert!(M5GraphicsCompletion { stamps: [0x200; 2], ..finished() }
            .pair_complete_at(LIST, 0x200));
        assert!(!M5GraphicsCompletion { stamps: [0; 2], ..finished() }
            .pair_complete_at(LIST, 0));
        assert!(!M5GraphicsCompletion { stamps: [0x101; 2], ..finished() }
            .pair_complete_at(LIST, 0x101));
    }

    #[test]
    fn ring_wrap_does_not_wrap_completion_identity() {
        // Three records cross the 1280-entry pointer ring on frame427.
        assert_eq!(graphics_pointer_head(426), 1279);
        assert_eq!(graphics_pointer_head(427), 2);
        assert_eq!(graphics_pointer_head(1280), 1);
        for frame in [127, 128, 255, 256, 427, 1280, MAX_GRAPHICS_GENERATION] {
            let current = M5GraphicsCompletion { stamps: [frame * 0x100; 2], ..finished() };
            assert!(current.pair_complete_at(LIST, frame * 0x100));
            assert!(!current.pair_complete_at(LIST, (frame - 1) * 0x100));
        }
    }

    #[test]
    fn compute_needs_its_own_retired_scheduler_stamp_and_notification() {
        let done = M5ComputeCompletion { submitted: 1, completed: 1, linked: 0,
            list: [0, LIST], stamp: 0x100, signalled: 1 << 12 };
        assert!(done.complete_at(LIST, 12, 0x100));
        for pending in [
            M5ComputeCompletion { submitted: 0, ..done },
            M5ComputeCompletion { completed: 0, ..done },
            M5ComputeCompletion { linked: 1, ..done },
            M5ComputeCompletion { list: [LIST + 8, LIST], ..done },
            M5ComputeCompletion { stamp: 0, ..done },
            M5ComputeCompletion { signalled: 0xc00, ..done },
        ] { assert!(!pending.complete_at(LIST, 12, 0x100)); }
        assert!(!done.complete_at(LIST, 12, 0x200));
        assert!(!done.complete_at(LIST, 128, 0x100));
        assert!(!done.complete_at(0, 12, 0x100));
    }
}
