#!/usr/bin/env python3
"""A retained HWPB must restamp its PBDesc ID before every submission."""

from pathlib import Path


root = Path(__file__).resolve().parents[1]
resources = (root / "g17_resources.rs").read_text()


def block(source, marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


prepare = block(resources, "fn prepare_g17p_retained_scene(")
restage = block(resources, "    fn prepare_retained_scene(")

assert "hardware_buffer_id: u32" in prepare
assert "put_u32(raw, layout.hwpb_state + 0x0c, hardware_buffer_id)" in prepare
assert "self.manager_generation," in restage
assert "hardware_buffer_id," in restage

# Reproduce the crash witness at the ownership boundary. Firmware had replaced
# PBState +0x0c with 0x380000; treating that as an ID made the completion path
# index a firmware-private 16-byte table far outside its valid allocation.
firmware_static_base = 0xFFFF_FC00_0017_4DD0
output_offset = 0x478
corrupt_id = 0x38_0000
fault = firmware_static_base + corrupt_id * 16 + output_offset
assert fault == 0xFFFF_FC00_0397_5248

pbstate = bytearray(0xC0)
pbstate[0x0C:0x10] = corrupt_id.to_bytes(4, "little")
hardware_buffer_id = 0
pbstate[0x0C:0x10] = hardware_buffer_id.to_bytes(4, "little")
assert int.from_bytes(pbstate[0x0C:0x10], "little") == hardware_buffer_id
assert firmware_static_base + hardware_buffer_id * 16 + output_offset == 0xFFFF_FC00_0017_5248

print("PASS retained HWPB restamps the native PBDesc ID before every submission")
