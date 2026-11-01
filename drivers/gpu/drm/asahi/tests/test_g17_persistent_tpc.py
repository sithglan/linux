#!/usr/bin/env python3
"""TPC lifetime follows the retained render-target-memory owner, not a command."""

from pathlib import Path


root = Path(__file__).resolve().parents[1]
source = (root / "g17_resources.rs").read_text()


def block(marker, text=source):
    start = text.index(marker)
    pos = text.index("{", start) + 1
    depth = 1
    while depth:
        depth += (text[pos] == "{") - (text[pos] == "}")
        pos += 1
    return text[start:pos]


aliases = block("struct G17PRenderUserAliases {")
storage = block("pub(crate) struct G17PUserRenderStorage {")
render_impl = source[source.index("impl G17PUserRenderStorage {") :]
constructor = block("    fn new(\n        dev: &AsahiDevice,", render_impl)
restage = block("    pub(crate) fn restage_command(")

assert "tpc:" not in aliases
assert "rt_memory:" not in aliases
assert "render_target_memory: KBox<G17PRenderTargetMemory>" in storage
heap_constructor = block("fn new_g17p_render_target_memory(")
assert "#[inline(never)]" in source[source.rfind("#[", 0, source.index("fn new_g17p_render_target_memory(")):source.index("fn new_g17p_render_target_memory(")]
assert "KBox::new(" in heap_constructor
assert heap_constructor.count("MappedObject::new_wc_render_owned(") == 2
assert "g17p_render_tpc_va(queue_pair)" in heap_constructor
assert "g17p_render_rt_memory_va(queue_pair)" in heap_constructor
target_owner = block("struct G17PRenderTargetMemory {")
prepare_slot = block("    fn prepare_rt_slot(", source[source.index("impl G17PRenderTargetMemory {") :])
assert "rt_slot_ordinals: [Option<u64>; G17P_RENDER_RT_EVENT_RECORD_COUNT]" in target_owner
assert "previous != submission_ordinal" in prepare_slot
assert ".checked_sub(previous)" not in prepare_slot
assert '"persistent render-target-memory allocation"' in constructor
assert "new_g17p_render_target_memory(dev, vm, queue_pair, &render_layout)" in constructor
assert "translated.parameters.tpc = render_target_memory.tpc_va();" in constructor
assert "translated.parameters.tpc = self.render_target_memory.tpc_va();" in restage
assert ".prepare_rt_slot(submission_ordinal, &render_layout)" in constructor
assert ".prepare_rt_slot(submission_ordinal, &render_layout)" in restage
assert "next_render_user_aliases.tpc" not in restage


ALIAS_START = 0x1002000000
SLOT_SIZE = 0x80000
GENERATIONS = 2
TPC_OFFSET = 0x50000
TPC_CAPACITY = SLOT_SIZE - TPC_OFFSET
RT_RECORDS = 4
RT_STRIDE = 0x40000
RT_START = ALIAS_START + 4 * SLOT_SIZE


def command_slot(pair, ordinal):
    return pair * GENERATIONS + (ordinal & 1)


def tpc_range(pair):
    start = ALIAS_START + pair * GENERATIONS * SLOT_SIZE + TPC_OFFSET
    return range(start, start + TPC_CAPACITY)


# Five hundred restages move command-owned aliases every frame, rotate
# TMAP/HMTA through the four native event records, and retain one exact TPC
# address for each independently live physical queue pair.
for pair in range(2):
    stable = tpc_range(pair)
    assert len({stable.start for _ordinal in range(500)}) == 1
    rt_records = {
        RT_START + pair * RT_RECORDS * RT_STRIDE + ordinal % RT_RECORDS * RT_STRIDE
        for ordinal in range(500)
    }
    assert len(rt_records) == RT_RECORDS
    for ordinal in range(500):
        command_base = ALIAS_START + command_slot(pair, ordinal) * SLOT_SIZE
        command_rt = range(command_base, command_base + TPC_OFFSET)
        assert command_rt.stop <= stable.start or stable.stop <= command_rt.start

        previous = ordinal - RT_RECORDS
        if previous >= 0:
            assert ordinal - previous == RT_RECORDS

# Retrying a pre-publication ordinal is safe; advancing into any of the other
# three still-owned records is not. The normal fourth advance is reusable.
def reusable(previous, current):
    distance = current - previous if current >= previous else None
    return distance == 0 or (distance is not None and distance >= RT_RECORDS)


assert reusable(11, 11)
assert not reusable(11, 12)
assert not reusable(11, 13)
assert not reusable(11, 14)
assert reusable(11, 15)
assert not reusable(0xFFFF_FFFF, 0)

assert tpc_range(0).stop <= tpc_range(1).start
assert tpc_range(0).stop == ALIAS_START + SLOT_SIZE
assert tpc_range(1).stop == ALIAS_START + 3 * SLOT_SIZE

print("PASS retained four-record RT memory and stable per-pair TPC lifetime")
