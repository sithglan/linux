#!/usr/bin/env python3
"""v97d g17p_recovery_path bits 4-6 (3D stall watch): compile the production
mask parser, stall predicate and clearance accounting and test them."""

from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
recovery = (root / "g17_recovery_path.rs").read_text()


def item(source, prefix):
    start = source.index(prefix)
    opening = source.index("{", start)
    depth = 1
    end = opening + 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


core = recovery[recovery.index("pub(crate) const RECLAIM_DRAINED"):recovery.index("pub(crate) fn watch_grow_reply(")]
core = core.replace("crate::g17_timing_buffer::arch_counter()", "0")
parse = item(recovery, "fn parse_mask(")

source = r'''
#![allow(dead_code, unused_imports)]
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
@CORE@
@PARSE@

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_slot() -> StallSlot {
        StallSlot { fragment_completed: AtomicU64::new(u64::MAX),
            since: AtomicU64::new(0), stage: AtomicU32::new(0),
            acted_at: AtomicU64::new(0) }
    }

    #[test]
    fn another_client_cannot_hide_a_starved_queue() {
        let stalled = fresh_slot();
        let busy = fresh_slot();
        let start = 1_000_000_000;
        for tick in 0..=6 {
            let now = start + tick * 4_000_000;
            // Interleave a different queue's advancing completions before
            // every sample. It must not restart this queue's elapsed time.
            assert!(!sample_stall_slot(&busy, now, tick, true));
            assert_eq!(sample_stall_slot(&stalled, now, 0xae43, true), tick >= 5);
        }
    }

    #[test]
    fn interrupted_observation_starts_a_fresh_interval() {
        let slot = fresh_slot();
        let start = 1_000_000_000;
        assert!(!sample_stall_slot(&slot, start, 0xae43, true));
        assert!(sample_stall_slot(&slot, start + 20_000_000, 0xae43, true));
        slot.stage.store(2, Ordering::Relaxed);
        let credited = STALL_CLEARED_AFTER_DOORBELL.load(Ordering::Relaxed);
        // The graph was borrowed or no dependency snapshot was readable.
        discard_stall_observation(&slot);
        assert_eq!(STALL_CLEARED_AFTER_DOORBELL.load(Ordering::Relaxed), credited);
        assert!(!sample_stall_slot(&slot, start + 100_000_000, 0xae43, true));
        assert!(!sample_stall_slot(&slot, start + 116_000_000, 0xae43, true));
        assert!(sample_stall_slot(&slot, start + 120_000_000, 0xae43, true));
    }

    #[test]
    fn losing_the_candidate_predicate_is_not_a_rescue() {
        let slot = fresh_slot();
        slot.fragment_completed.store(0xae43, Ordering::Relaxed);
        slot.since.store(1_000_000, Ordering::Relaxed);
        slot.stage.store(2, Ordering::Relaxed);
        let credited = STALL_CLEARED_AFTER_DOORBELL.load(Ordering::Relaxed);
        assert!(!sample_stall_slot(&slot, 50_000_000, 0xae43, false));
        assert_eq!(STALL_CLEARED_AFTER_DOORBELL.load(Ordering::Relaxed), credited);
        assert_eq!(slot.since.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn failed_send_is_not_a_sent_doorbell() {
        let slot = fresh_slot();
        slot.fragment_completed.store(0xae43, Ordering::Relaxed);
        slot.since.store(1_000_000, Ordering::Relaxed);
        slot.stage.store(4, Ordering::Relaxed);
        let credited = STALL_CLEARED_AFTER_DOORBELL.load(Ordering::Relaxed);
        assert!(!sample_stall_slot(&slot, 50_000_000, 0xae44, true));
        assert_eq!(STALL_CLEARED_AFTER_DOORBELL.load(Ordering::Relaxed), credited);
    }

    #[test]
    fn completion_progress_resets_this_queues_interval() {
        let slot = fresh_slot();
        let start = 1_000_000_000;
        assert!(!sample_stall_slot(&slot, start, 0xae43, true));
        assert!(sample_stall_slot(&slot, start + 20_000_000, 0xae43, true));
        assert!(!sample_stall_slot(&slot, start + 24_000_000, 0xae44, true));
        assert!(!sample_stall_slot(&slot, start + 40_000_000, 0xae44, true));
        assert!(sample_stall_slot(&slot, start + 44_000_000, 0xae44, true));
    }

    #[test]
    fn mask_accepts_stall_bits_and_rejects_reserved_bit3() {
        assert_eq!(parse_mask("0x75"), Some(0x75));
        assert_eq!(parse_mask("0x15"), Some(0x15));
        assert_eq!(parse_mask("0x8"), None);
        assert_eq!(parse_mask("0x80"), None);
        assert_eq!(STALL_WATCH | STALL_REDOORBELL | STALL_FRAGMENT, 0x70);
    }

    #[test]
    fn candidate_requires_outstanding_3d_and_completed_ta_parent() {
        // T2 hang: TA completed 0xae44, 3D completed 0xae43, latest 0xae4a.
        assert!(stall_candidate(0xae44, 0xae43, 0xae4a, Some(0xae44)));
        // TA not yet at the 3D's next stamp: waiting on its parent.
        assert!(!stall_candidate(0xae43, 0xae43, 0xae4a, Some(0xae44)));
        // Nothing outstanding on the 3D queue.
        assert!(!stall_candidate(0xae4a, 0xae4a, 0xae4a, Some(0xae4b)));
        // TA ahead by several stamps still counts; 40-bit wrap is handled.
        assert!(stall_candidate(0xae48, 0xae43, 0xae4a, Some(0xae44)));
        assert!(stall_candidate(0x2, 0xff_ffff_fffe, 0x5, Some(0xff_ffff_ffff)));
        assert!(!stall_candidate(0xff_ffff_fffe, 0xff_ffff_fffe, 0x5, Some(0xff_ffff_ffff)));
        // Unknown next stamp is never a candidate.
        assert!(!stall_candidate(0xae44, 0xae43, 0xae4a, None));
    }

    #[test]
    fn clearance_is_attributed_to_the_doorbell_only_after_one_was_sent() {
        STALL_CLEARED_UNAIDED.store(0, Ordering::Relaxed);
        STALL_CLEARED_AFTER_DOORBELL.store(0, Ordering::Relaxed);
        STALL_MAX_NS.store(0, Ordering::Relaxed);
        let slot = &STALL_SLOT[0];
        slot.since.store(1_000, Ordering::Relaxed);
        slot.stage.store(1, Ordering::Relaxed);
        note_stall_cleared(slot, 31_000_000);
        assert_eq!(STALL_CLEARED_UNAIDED.load(Ordering::Relaxed), 1);
        assert_eq!(STALL_CLEARED_AFTER_DOORBELL.load(Ordering::Relaxed), 0);
        slot.since.store(2_000, Ordering::Relaxed);
        slot.stage.store(2, Ordering::Relaxed);
        note_stall_cleared(slot, 50_000_000);
        assert_eq!(STALL_CLEARED_AFTER_DOORBELL.load(Ordering::Relaxed), 1);
        assert_eq!(STALL_MAX_NS.load(Ordering::Relaxed), 50_000_000 - 2_000);
        assert_eq!(slot.stage.load(Ordering::Relaxed), 0);
        assert_eq!(slot.since.load(Ordering::Relaxed), 0);
        // A slot that never detected a stall clears silently.
        slot.since.store(5, Ordering::Relaxed);
        note_stall_cleared(slot, 60_000_000);
        assert_eq!(STALL_CLEARED_UNAIDED.load(Ordering::Relaxed), 1);
        // Recovery counts open stalls.
        STALL_SLOT[3].stage.store(2, Ordering::Relaxed);
        STALL_SLOT[3].fragment_completed.store(0xae43, Ordering::Relaxed);
        let open = note_recovery_for_stalls();
        assert_eq!(open.count, 1);
        assert_eq!(open.pairs[0], (3, 2, 0xae43));
        assert_eq!(STALL_AT_RECOVERY.load(Ordering::Relaxed), 1);
    }
}
'''.replace("@CORE@", core).replace("@PARSE@", parse)

with tempfile.TemporaryDirectory() as tmp:
    path = Path(tmp)
    (path / "t.rs").write_text(source)
    subprocess.run(["rustc", "--edition=2021", "--test", str(path / "t.rs"), "-o", str(path / "t")], check=True)
    subprocess.run([str(path / "t"), "--test-threads=1"], check=True)
print("PASS stall watch: 8 tests including multi-client starvation and interrupted observations")
