#!/usr/bin/env python3
"""A layered render publishes its layer metadata apart from the heap metadata.

AGXRenderTargetMemory::updateRenderTarget allocates one render-target-memory
record as the page-aligned tilemap, then 0x100 bytes of layer metadata when the
target has more than one layer, then 0x200 bytes of heap metadata. The TA
register generator publishes the layer metadata through 0x1c078/0x1c9d8 and the
heap metadata through 0x1c030/0x1c9c0; the 3D generator publishes the layer
metadata through 0x16060 and the heap metadata through 0x16098. With one layer
the two addresses coincide, which is why a single-layer capture cannot tell
them apart.
"""
from pathlib import Path
import re

root = Path(__file__).resolve().parents[1]
render = (root / "g17_render.rs").read_text()
resources = (root / "g17_resources.rs").read_text()


def block(source, marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


layout = block(resources, "fn g17p_render_state_layout(")
assert "let layermeta_size = if layers > 1 { 0x100usize } else { 0 };" in layout
assert "let heapmeta_size = 0x200usize;" in layout
assert "let layermeta = tilemap.checked_add(tilemap_span)?;" in layout
assert "let heapmeta = layermeta.checked_add(layermeta_size)?;" in layout

memory = resources[resources.index("impl G17PRenderTargetMemory {"):]
prepare = block(memory, "    fn prepare_rt_slot(")
assert "layermeta: self.rt_region_va(submission_ordinal, layout, layout.layermeta)?" in prepare
assert "heapmeta: self.heapmeta_va(submission_ordinal, layout)?" in prepare

storage = resources[resources.index("impl G17PUserRenderStorage {"):]
constructor = block(storage, "    fn new(\n        dev: &AsahiDevice,")
restage = block(resources, "    pub(crate) fn restage_command(")
for path in (constructor, restage):
    assert "translated.parameters.layermeta = render_target_addresses.layermeta;" in path
    assert "translated.parameters.heapmeta = render_target_addresses.heapmeta;" in path

ta = block(render, "fn build_ta_registers_for_work_key(")
assert "let layermeta = ta_resource_offset(p, p.layermeta)?;" in ta
assert "RegisterWrite::new(0x1c079, layermeta)," in ta
assert "RegisterWrite::new(0x1c9d8, layermeta)," in ta
assert "RegisterWrite::new(0x1c031, heapmeta | 0x8000_0000_0000_0000)," in ta
assert "RegisterWrite::new(0x1c9c0, heapmeta | 0x8000_0000_0000_0000)," in ta
assert "RegisterWrite::new(0x10169, layer_mode)," in ta
assert "RegisterWrite::new(0x1c9e8, layer_mode & 0x47ff)," in ta
assert re.search(r"let mut layer_mode = 0x8000 \| \(p\.layers as u64 - 1\);", ta)
assert re.search(
    r"if p\.layers > 1 \{\s*layer_mode \|= 0x4000 \| if p\.process_empty_tiles "
    r"\{ 0x2000 \} else \{ 0x1000 \};\s*\}",
    ta,
)

fragment = block(render, "fn build_fragment_registers_for_work_key(")
assert "RegisterWrite::new(0x16060, p.layermeta)," in fragment
assert "RegisterWrite::new(0x16098, p.heapmeta)," in fragment


def native_layout(width, height, layers, utile=32):
    tiles_x = (width + 31) // 32
    tiles_y = (height + 31) // 32
    per_x = (((tiles_x + 3) >> 2) + 3) & ~3
    per_y = (((tiles_y + 3) >> 2) + 3) & ~3
    utiles = (4 * per_x * (32 // utile)) * (4 * per_y * (32 // utile))
    tilemap_size = 5 * utiles * layers
    layermeta = (tilemap_size + 0xfff) & ~0xfff
    heapmeta = layermeta + (0x100 if layers > 1 else 0)
    return layermeta, heapmeta


def native_layer_mode(layers, process_empty_tiles):
    mode = 0x8000 | (layers - 1)
    if layers > 1:
        mode |= 0x4000 | (0x2000 if process_empty_tiles else 0x1000)
    return mode, mode & 0x47ff


# probe-c: 256x256, two layers, 32x32 utiles.
assert native_layout(256, 256, 2) == (0x1000, 0x1100)
assert native_layout(256, 256, 1) == (0x1000, 0x1000)
assert native_layer_mode(1, True) == (0x8000, 0)
assert native_layer_mode(2, True) == (0xe001, 0x4001)
assert native_layer_mode(2, False) == (0xd001, 0x4001)
assert native_layer_mode(12, False) == (0xd00b, 0x400b)

print("layered render metadata: OK")
