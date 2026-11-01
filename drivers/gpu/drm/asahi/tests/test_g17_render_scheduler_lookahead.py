#!/usr/bin/env python3
"""Render scheduling keeps one ready host waiter without widening hardware."""

from pathlib import Path
import re

root = Path(__file__).resolve().parents[1]
limits = (root / "g17_queue_limits.rs").read_text()
drm = (root / "g17_drm.rs").read_text()

assert re.search(r"MAX_IN_FLIGHT_PER_QUEUE:\s*u32\s*=\s*2\s*;", limits)
assert re.search(
    r"RENDER_SCHEDULER_CREDITS:\s*u32\s*=\s*MAX_IN_FLIGHT_PER_QUEUE\s*\+\s*1\s*;",
    limits,
)
assert "owned >= crate::g17_queue_limits::MAX_IN_FLIGHT_PER_QUEUE as usize" in drm
assert "crate::g17_queue_limits::RENDER_SCHEDULER_CREDITS" in drm
assert "G17PRenderScheduler::new(" in drm

print("PASS render scheduler look-ahead preserves the native two-pair limit")
