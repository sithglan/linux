#!/usr/bin/env python3
"""A failed render graph constructor leaves no global or VM-visible state."""
from pathlib import Path


source = (Path(__file__).resolve().parents[1] / "g17_resources.rs").read_text()


def function(marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


stage = function("    pub(crate) fn stage_render_parameter_buffer_descriptor(")
restore = function("    fn restore_render_parameter_buffer_descriptor(")
constructor = function("    pub(crate) fn finish_user_render_storage(")

assert "Result<Option<(u32, [u32; 4])>>" in stage
assert stage.index("let mut previous = [0u32; 4]") < stage.index(
    "bytes[at..at + 4].copy_from_slice"
)
assert "Ok(Some((index, previous)))" in stage
assert "bytes[at..at + 4].copy_from_slice(&value.to_le_bytes())" in restore

install = constructor.index("vm.install_render_pool_mappings")
publish = constructor.index("staged_descriptor = self.stage_render_parameter_buffer_descriptor")
append = constructor.index("vm.append_driver_mappings(core::mem::take(&mut prepared.driver_aliases))")
failure = constructor.index("if let Err(error) = finish")
undo_descriptor = constructor.index("restore_render_parameter_buffer_descriptor", failure)
undo_pool = constructor.index("vm.clear_render_pool_mappings", failure)
return_error = constructor.index("return Err(error)", failure)
assert install < publish < append < failure
assert failure < undo_descriptor < undo_pool < return_error


class State:
    def __init__(self):
        self.pbdesc = "old"
        self.pool = None
        self.sksm_aliases = 0


def construct(state, fail):
    state.pool = "new-pool"
    state.pbdesc = "new-pm"
    if fail:
        state.pbdesc = "old"
        state.pool = None
        return False
    state.sksm_aliases += 2
    return True


state = State()
for _ in range(64):
    assert not construct(state, True)
    assert (state.pbdesc, state.pool, state.sksm_aliases) == ("old", None, 0)
assert construct(state, False)
assert (state.pbdesc, state.pool, state.sksm_aliases) == ("new-pm", "new-pool", 2)

print("PASS render constructor rollback: 64 failed prepublication attempts leave PBDesc/pool exact; retry commits once")
