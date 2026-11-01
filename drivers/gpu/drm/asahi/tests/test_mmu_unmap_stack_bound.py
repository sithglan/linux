#!/usr/bin/env python3
"""Mapping unwind validates ownership without nesting page-table walkers."""

from pathlib import Path

root = Path(__file__).resolve().parents[1]
pgtable = (root / "pgtable.rs").read_text()
resources = (root / "g17_resources.rs").read_text()


def function(source: str, marker: str) -> str:
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


unmap = function(pgtable, "pub(crate) fn unmap_pages(")
dual_root = function(resources, "fn map_shared_render_descriptor_range(")
assert "map_shared_render_range(" in dual_root
dual_root = function(resources, "fn map_shared_render_range(")

# A failed client-side alias attempt intentionally drops the temporary global
# alias before trying the next common VA. That Drop reaches unmap_pages from a
# deep render constructor, so unmap must not call another full walker frame.
assert "drop(global)" in dual_root
assert "self.covers_range" not in unmap
assert unmap.count("self.with_pages") == 2
assert "complete_coverage(expected, visited, fully_mapped)" in unmap
assert unmap.index("complete_coverage(expected, visited, fully_mapped)") < unmap.index(
    "pte.store(0, Ordering::Relaxed)"
)


def unmap_model(pages):
    # First pass is all-or-nothing. The second pass runs only after every leaf
    # was observed, matching the kernel's ownership invariant.
    if not pages or not all(pages):
        return False, pages[:]
    return True, [False] * len(pages)


for count in (1, 2, 5, 64, 1024):
    ok, result = unmap_model([True] * count)
    assert ok and result == [False] * count
    for hole in (0, count // 2, count - 1):
        source = [True] * count
        source[hole] = False
        ok, result = unmap_model(source)
        assert not ok and result == source

print("PASS iterative two-pass UAT unmap keeps partial ranges intact without nested walker")
