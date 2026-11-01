#!/usr/bin/env python3
"""Compile the real ingress log predicate and check callback/drain separation."""
import pathlib
import shutil
import subprocess
import tempfile

source = (pathlib.Path(__file__).resolve().parents[1] / "g17_live_boot.rs").read_text()
start = source.index("const fn g17p_notification_log_due(")
end = source.index("\n}", start) + 2
predicate = source[start:end]
# One shared helper counts a notification, whichever RTKit callback took it.
helper = source[source.index("    fn note_firmware_event_notification("):]
helper = helper[:helper.index("\n    }\n") + 6]
assert ".firmware_event_notifications\n            .fetch_add(1, Ordering::Relaxed)" in helper
assert ".firmware_event_notifications_total\n            .fetch_add(1, Ordering::Relaxed)" in helper
assert "if g17p_notification_log_due(total)" in helper
callback = source[source.index("fn recv_message("):source.index("} else if endpoint == g17_rtkit::ENDPOINT_GFX_INTERRUPTS")]
early = source[source.index("fn recv_message_early("):source.index("fn crashed(")]
for name, body in (("recv_message", callback), ("recv_message_early", early)):
    assert "note_firmware_event_notification()" in body, name
    assert "queue_g17p_firmware_event_worker" in body, name
    # Never counted twice: neither callback touches the counters directly.
    assert ".fetch_add(" not in body, name
# The hard-IRQ path consumes the notification only for an armed worker; it
# counts, queues the worker and then tells the RTKit core the message is
# handled. Anything else still goes to the workqueue callback.
assert early.index("data.event_worker_armed.load(Ordering::Acquire)") < early.index(
    "decode_g17p_firmware_event_notification(endpoint, message)")
assert (early.index("data.note_firmware_event_notification();")
        < early.index("queue_g17p_firmware_event_worker")
        < early.index("true\n                } else {\n                    false"))
assert ".firmware_event_notifications\n        .swap(0, Ordering::AcqRel)" in source
assert ".firmware_event_notifications_total\n        .swap" not in source

tests = r'''
#[test]
fn budget_does_not_reset_when_each_notification_is_drained() {
    let emitted: Vec<u32> = (1..=1_000_000)
        .filter(|total| g17p_notification_log_due(*total)).collect();
    assert_eq!(&emitted[..7], &[1, 2, 3, 4, 8, 16, 32]);
    assert_eq!(emitted.len(), 21);
    assert_eq!(emitted.last(), Some(&524288));
    // The pending counter can return to zero after every IRQ without changing
    // this lifetime budget. Notification delivery is not conditional on it.
    for total in 1..=1000 {
        let pending = std::sync::atomic::AtomicU32::new(0);
        pending.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(pending.swap(0, std::sync::atomic::Ordering::AcqRel), 1);
        assert_eq!(g17p_notification_log_due(total),
                   total <= 4 || total.is_power_of_two());
    }
}
#[test]
fn zero_and_counter_wrap_are_bounded() {
    assert!(!g17p_notification_log_due(0));
    assert!(!g17p_notification_log_due(u32::MAX));
    assert!(g17p_notification_log_due(1 << 31));
    assert!(!g17p_notification_log_due(u32::MAX.wrapping_add(1)));
}
'''
with tempfile.TemporaryDirectory(prefix="g17-notification-budget-") as temp:
    binary = str(pathlib.Path(temp) / "test")
    subprocess.run([shutil.which("rustc") or "rustc", "--edition=2021", "--test",
                    "-", "-o", binary], input=predicate + tests, text=True, check=True)
    subprocess.run([binary], check=True)
print("actual notification budget, shared exact-once counting and IRQ-path wake wiring PASS")
