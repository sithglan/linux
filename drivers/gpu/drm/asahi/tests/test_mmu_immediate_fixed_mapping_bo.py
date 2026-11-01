#!/usr/bin/env python3
"""Pin fixed-mapping BO teardown to DRM GPUVM immediate-mode locking."""

from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MMU = (ROOT / "mmu.rs").read_text()


def block(source: str, marker: str, opening: str = "{") -> str:
    start = source.index(marker)
    brace = source.index(opening, start)
    depth = 0
    for index in range(brace, len(source)):
        if source[index] == "{":
            depth += 1
        elif source[index] == "}":
            depth -= 1
            if depth == 0:
                return source[start : index + 1]
    raise AssertionError(f"unterminated block: {marker}")


# Asahi selects the immediate-mode GPUVA-list lock for every VM.
gpuvm_new = MMU[MMU.index("gpuvm::GpuVm::new(") :][:1000]
assert "drm_gpuvm_flags_DRM_GPUVM_IMMEDIATE_MODE," in gpuvm_new

owner = block(MMU, "struct ImmediateGpuVmBoRef")
assert "Option<ARef<gpuvm::GpuVmBo<VmInner>>>" in owner
assert "ARef<gpuvm::GpuVm<VmInner>>" in owner

drop = block(MMU, "impl Drop for ImmediateGpuVmBoRef")
assert "ARef::into_raw(bo)" in drop
assert "drm_gpuvm_bo_put_deferred" in drop
assert drop.index("drm_gpuvm_bo_put_deferred") < drop.index("bo_deferred_cleanup")
assert "dma_resv_lock" not in drop

mapping = block(MMU, "pub(crate) struct KernelMappingInner")
assert "bo: Option<ImmediateGpuVmBoRef>" in mapping
assert "bo: Option<ARef<gpuvm::GpuVmBo" not in mapping

# Both GEM-backed fixed mapping constructors transfer their obtained reference
# to the immediate-mode owner before the returned mapping can escape.
map_in_range = block(MMU, "pub(crate) fn map_in_range(")
assert "self.map_in_range_with_guard_size(" in map_in_range
map_in_range = block(MMU, "pub(crate) fn map_in_range_with_guard_size(")
map_at = block(MMU, "pub(crate) fn map_at(")
for constructor in [map_in_range, map_at]:
    assert "ImmediateGpuVmBoRef::new(" in constructor
    assert constructor.index("ImmediateGpuVmBoRef::new(") < constructor.index("insert_node") \
        if "insert_node" in constructor else constructor.index("ImmediateGpuVmBoRef::new(") < constructor.index("reserve_node")
    assert "node_inner.bo = vm_bo.take();" in constructor

print("ok: fixed mappings retire GPUVM BO references under immediate-mode ownership")
