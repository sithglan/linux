#!/usr/bin/env python3
"""Check G17P range-TLBI encoding against the native PI_300 primitive."""
from pathlib import Path
import random
import re


mem = (Path(__file__).resolve().parents[1] / "mem.rs").read_text()
mmu = (Path(__file__).resolve().parents[1] / "mmu.rs").read_text()
resources = (Path(__file__).resolve().parents[1] / "g17_resources.rs").read_text()
t8140 = mem.split("pub(crate) fn tlbi_range_t8140", 1)[1]
legacy = mem.split("pub(crate) fn tlbi_range", 1)[1].split(
    "pub(crate) fn tlbi_range_t8140", 1
)[0]
mask_match = re.search(r"UAT_PGBIT\) as u64\) & (0x[0-9a-f_]+)", t8140)
assert mask_match, "range-TLBI address mask not found"
assert int(mask_match.group(1).replace("_", ""), 16) == (1 << 38) - 1
assert "CONFIG_ARM64_4K_PAGES" not in t8140
assert "len >= 0x0200_0000" in t8140
assert "len <= PAGE_SIZE" in t8140
assert "CONFIG_ARM64_4K_PAGES" in legacy
assert "0x1fffffffff" in legacy
assert "if len == 0" in legacy
assert "mem::tlbi_range_t8140" in mmu
assert "mem::tlbi_range(" in mmu
assert "mem::tlbi_range_t8140" in resources


def driver(asid: int, va: int, length: int, cpu_page_shift: int):
    if length == 0:
        return None
    if va == 0 or length >= 0x2000000:
        return ("asid", asid << 48)
    if length <= 1 << cpu_page_shift:
        return ("page", (asid << 48) | ((va >> 12) & ((1 << 44) - 1)))
    scale, shift = (1, 20) if length >= 0x100000 else (0, 15)
    count = ((length - 1) >> shift) + 1
    return (
        "range",
        (asid << 48)
        | (2 << 46)
        | (scale << 44)
        | ((count - 1) << 39)
        | ((va >> 14) & ((1 << 38) - 1)),
    )


def native_pi300(asid: int, va: int, length: int, cpu_page_shift: int = 12):
    if va == 0 or length == 0 or length >= 0x2000000:
        return ("asid", asid << 48)
    if length <= 1 << cpu_page_shift:
        return ("page", (asid << 48) | ((va >> 12) & ((1 << 44) - 1)))
    scale, shift = (1, 20) if length >> 20 else (0, 15)
    count = (length + (1 << shift) - 1) >> shift
    return (
        "range",
        (asid << 48)
        | (2 << 46)
        | (scale << 44)
        | ((count - 1) << 39)
        | ((va >> 14) & ((1 << 38) - 1)),
    )


addresses = [
    0,
    0x0000010069310000,       # application/retained compute mapping
    0x000003FFFFFF8000,       # top of the ordinary client range
    0xFFFFFC2001854000,       # B1 firmware-high object
    0xFFFFFC20C0790000,       # B1 report/status high object
    0xFFFFFFFFFFFFC000,
]
rng = random.Random(0xB1)
addresses.extend((rng.getrandbits(64) >> 14) << 14 for _ in range(10_000))
lengths = [0x1000, 0x4000, 0x8000, 0x80000, 0x100000, 0x800000, 0x1FFF000, 0x2000000, 0x4000000]
vectors = 0
for cpu_page_shift in (12, 14):
    for asid in (0, 5, 63, 255):
        for va in addresses:
            for length in lengths:
                assert driver(asid, va, length, cpu_page_shift) == native_pi300(
                    asid, va, length, cpu_page_shift
                ), (cpu_page_shift, asid, hex(va), hex(length))
                vectors += 1

high = 0xFFFFFC2001854000
old_mask = (1 << 37) - 1
assert ((high >> 14) & old_mask) == (((high ^ (1 << 51)) >> 14) & old_mask)
assert driver(5, high, 0x8000, 14) != driver(5, high ^ (1 << 51), 0x8000, 14)

print(f"PASS G17P range TLBI matches native PI_300 mode/VA/NUM/SCALE across {vectors:,} operands")
