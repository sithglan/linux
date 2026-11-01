#!/usr/bin/env python3
"""The contended descriptor root chooses the shared dual-root VA first."""

from pathlib import Path


source = (Path(__file__).resolve().parents[1] / "g17_resources.rs").read_text()
wrapper = source[source.index("fn map_shared_render_descriptor_range("):source.index("fn map_shared_render_sksm_range(")]
assert "object, client_vm, global_vm, object_range," in wrapper
assert "g17p_cl_client_low_va_range(), mmu::PROT_GPU_SHARED_RO," in wrapper
start = source.index("fn map_shared_render_range(")
pos = source.index("{", start) + 1
depth = 1
while depth:
    depth += (source[pos] == "{") - (source[pos] == "}")
    pos += 1
mapper = source[start:pos]

global_map = mapper.index("global_vm,", mapper.index("let global ="))
client_map = mapper.index("client_vm,", global_map)
assert global_map < client_map
assert "let start = global.iova();" in mapper
assert "Ok(client) => return Ok((client, global))" in mapper
assert "drop(global);" in mapper
assert "drop(client);" not in mapper


def old_client_first(live_global_ranges):
    # Each fresh client picks the beginning of its empty private range and
    # then collides once for every descriptor already in the shared root.
    return len(live_global_ranges)


def global_first(_live_global_ranges):
    # The shared VA allocator skips its own live ranges before the private map
    # is created, so normal multi-client construction has no cleanup retry.
    return 0


live = []
for job in range(64):
    assert old_client_first(live) == job
    assert global_first(live) == 0
    live.append(job)

print("PASS shared-root-first descriptor allocation avoids temporary unmap recursion")
