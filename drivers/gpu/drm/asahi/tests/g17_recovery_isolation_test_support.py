"""Rust stubs for extracted code that consults recovery isolation (iso1/iso2).

The class constants, mask bits and counter names are read from
g17_recovery_isolation.rs, so a renamed or renumbered item breaks the
tests instead of silently diverging. `enabled()` reads a test-owned mask
(MASK, default 0 = isolation off, i.e. the pre-isolation behaviour).
"""
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]


def _block(source, marker):
    start = source.index(marker)
    end = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


def recovery_isolation_mod():
    src = (ROOT / "g17_recovery_isolation.rs").read_text()
    consts = re.findall(r"^pub\(crate\) const ((?:CLASS|DEFAULT)_\w+|[A-Z_]+): (u8|u32|u64) = ([^;]+);",
                        src, re.M)
    statics = re.findall(r"^pub\(crate\) static (\w+): AtomicU64 = AtomicU64::new\(0\);", src, re.M)
    out = ["mod g17_recovery_isolation {",
           " #![allow(dead_code)]",
           " use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};"]
    for name, ty, value in consts:
        out.append(f" pub const {name}: {ty} = {value};")
    for name in statics:
        out.append(f" pub static {name}: AtomicU64 = AtomicU64::new(0);")
    # The pure, production decoders, classifier and iso4 replay/inheritance
    # decisions, verbatim.
    for marker in ("pub(crate) struct RecoveryEntry {",
                   "pub(crate) const fn decode_entry(",
                   "pub(crate) fn qid_masks(",
                   "pub(crate) const fn classify(",
                   "pub(crate) struct ComputeReplayQueue {",
                   "pub(crate) enum ComputeReplay {",
                   "pub(crate) const fn never_executed(",
                   "pub(crate) fn compute_replay_decision<",
                   "pub(crate) const fn inherited_errno(",
                   "pub(crate) const fn replay_budget_left(",
                   "pub(crate) struct ComputeHead {",
                   "pub(crate) enum VictimTimeout {",
                   "pub(crate) fn victim_timeout<"):
        typed = "struct" in marker or "enum" in marker
        out.append(" #[derive(Copy, Clone, Debug, PartialEq, Eq)]" if typed else "")
        out.append(_block(src, marker).replace("pub(crate)", "pub"))
    out += [" pub static MASK: AtomicU32 = AtomicU32::new(0);",
            " pub fn enabled(bit: u32) -> bool { MASK.load(Ordering::Relaxed) & bit != 0 }",
            " pub fn mask() -> u32 { MASK.load(Ordering::Relaxed) }",
            " pub fn count(counter: &AtomicU64) { counter.fetch_add(1, Ordering::Relaxed); }",
            "}"]
    return "\n".join(out) + "\n"


# The v93 host-path bits consulted by extracted completion code. All off:
# extracted completions then behave exactly as before v93.
HOST_PATH_V93 = r'''
 pub const FW_DEPS: u32 = 1 << 13;
 pub const AGG_FIRST: u32 = 1 << 14;
 pub const EARLY_DEPS: u32 = 1 << 15;
 pub static AGG_FIRST_TAKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
 pub fn enabled(_bit: u32) -> bool { false }
 pub fn count(counter: &std::sync::atomic::AtomicU64) { counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed); }
'''
