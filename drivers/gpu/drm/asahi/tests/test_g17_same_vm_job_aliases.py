#!/usr/bin/env python3
"""One VM may build multiple render storages without remapping low view 0."""

from pathlib import Path


root = Path(__file__).resolve().parents[1]
mmu = (root / "mmu.rs").read_text()


def block(source, marker):
    start = source.index(marker)
    pos = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[pos] == "{") - (source[pos] == "}")
        pos += 1
    return source[start:pos]


install = block(mmu, "    pub(crate) fn install_t8140_job_context_aliases(")
append = block(mmu, "    pub(crate) fn append_driver_mappings(")

assert install.index("existing_pages != 0") < install.index("source.map_in_range(")
assert "existing_pages != expected_pages" in install
assert "source.translate_iova(high)? != source.translate_iova(low)?" in install
assert install.count("!= source_generation") == 2
assert "return Ok(mappings);" in install
assert "if mappings.is_empty()" in append


PAGE = 0x4000
SIZE = 0x10000
canonical = [0x1000_0000 + i * PAGE for i in range(SIZE // PAGE)]


def install_null_view(low_pages):
    """Model the production three-way ownership classification."""
    present = [page is not None for page in low_pages]
    if any(present):
        if not all(present):
            return "EFAULT", low_pages
        if low_pages != canonical:
            return "EFAULT", low_pages
        return "REUSED", low_pages
    return "MAPPED", canonical.copy()


state, first = install_null_view([None] * 4)
assert state == "MAPPED"
state, second = install_null_view(first)
assert state == "REUSED" and second == first

# Occupancy is idempotent only for the exact canonical physical pages.
wrong = first.copy()
wrong[2] += PAGE
assert install_null_view(wrong)[0] == "EFAULT"
partial = first.copy()
partial[1] = None
assert install_null_view(partial)[0] == "EFAULT"

print("PASS same-VM render storage reuses exact null view; partial/wrong backing fails")
