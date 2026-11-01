#!/usr/bin/env python3
"""Routine G17P success-path logs stay opt-in on desktop and CTS boots."""
from pathlib import Path


root = Path(__file__).resolve().parents[1]
sources = {
    name: (root / name).read_text()
    for name in (
        "asahi.rs",
        "g17_async_compute.rs",
        "g17_live_boot.rs",
        "g17_manager.rs",
        "g17_resources.rs",
        "mmu.rs",
    )
}

assert "g17p_verbose: u32 {\n            default: 0," in sources["asahi.rs"]
assert "g17p_completion_trace: u32 {\n            default: 0," in sources["asahi.rs"]

messages = {
    "g17_resources.rs": (
        "G17P completion client aliases:",
        "G17P resources: client VM mapped private render FList",
        "G17P VM: {} operand buffers mapped GPU-shared",
    ),
    "mmu.rs": (
        "MMU: T8140 job context reused all",
        "MMU: T8140 job context reused {} GPUVM low views",
    ),
    "g17_live_boot.rs": ("G17P render: client queue {} released descriptor context",),
    "g17_async_compute.rs": ("G17P async compute: prepared installed graph rebind",),
    "g17_manager.rs": ("G17P render VM handoff: pair {} graph of closed owner",),
}

for name, needles in messages.items():
    source = sources[name]
    for needle in needles:
        position = source.index(needle)
        assert "g17p_verbose()" in source[max(0, position - 500):position], (name, needle)

print("PASS G17P routine success-path logs are off by default and opt-in via g17p_verbose=1")
