"""Shared host stand-ins for harnesses that compile the in-flight compute FIFO.

The production runtime keeps every published, unretired compute kick of a
queue in `g17_completion::PairFifo`, sizes host bookkeeping with
`g17_queue_limits`, and records opt-in timing through `g17_timing_buffer`.
Harnesses that extract runtime methods need those names; the FIFO itself is
extracted from production source, never re-implemented here.
"""
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def _item(source, marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


def pair_fifo_source():
    completion = (ROOT / "g17_completion.rs").read_text()
    start = completion.index("pub(crate) struct PairFifo<T, const N: usize> {")
    end = completion.index("/// Render-only normalization:", start)
    return completion[start:end]


def queue_limits_mock():
    limits = (ROOT / "g17_queue_limits.rs").read_text()
    depth = [line for line in limits.splitlines()
             if line.startswith("pub(crate) const MAX_COMPUTE_IN_FLIGHT_PER_QUEUE")][0]
    return ("mod g17_queue_limits {\n pub const MAX_QUEUES: u32 = 58;\n "
            + depth.replace("pub(crate) const", "pub const") + "\n}\n")


TIMING_BUFFER_MOCK = r'''
mod g17_timing_buffer {
 pub const WORDS: usize = 24;
 pub const KIND_COMPUTE_PUBLISH: u64 = 3;
 pub const KIND_COMPUTE_RETIRE: u64 = 4;
 pub fn enabled() -> bool { false }
 pub fn now() -> u64 { 0 }
 pub fn arch_counter() -> u64 { 0 }
 pub fn arch_frequency() -> u64 { 0 }
 pub fn record(_: [u64; WORDS]) {}
}
'''

KVEC_MOCK = r'''
const GFP_KERNEL: u32 = 0;
/// Host stand-in for the kernel KVec: fixed capacity, never grows.
struct KVec<T>(Vec<T>);
/// The kernel's `kvec::PushError`: the rejected value is field `.0`.
struct PushError<T>(T);
impl<T> KVec<T> {
 fn new() -> Self { Self(Vec::new()) }
 fn with_capacity(n: usize, _flags: u32) -> std::result::Result<Self, i32> { Ok(Self(Vec::with_capacity(n))) }
 fn reserve(&mut self, n: usize, _flags: u32) -> std::result::Result<(), i32> { self.0.reserve(n); Ok(()) }
 fn len(&self) -> usize { self.0.len() }
 fn capacity(&self) -> usize { self.0.capacity() }
 fn clear(&mut self) { self.0.clear() }
 fn push_within_capacity(&mut self, v: T) -> std::result::Result<(), PushError<T>> {
  if self.0.len() < self.0.capacity() { self.0.push(v); Ok(()) } else { Err(PushError(v)) }
 }
}
impl<T> IntoIterator for KVec<T> { type Item = T; type IntoIter = std::vec::IntoIter<T>; fn into_iter(self) -> Self::IntoIter { self.0.into_iter() } }
impl<T> std::ops::Deref for KVec<T> { type Target = [T]; fn deref(&self) -> &[T] { &self.0 } }
impl<T> std::ops::DerefMut for KVec<T> { fn deref_mut(&mut self) -> &mut [T] { &mut self.0 } }
type KVVec<T> = KVec<T>;
'''


def completion_mod(extra=""):
    """`mod g17_completion` with the production PairFifo plus `extra`."""
    return "mod g17_completion {\n" + pair_fifo_source() + "\n" + extra + "\n}\n"
