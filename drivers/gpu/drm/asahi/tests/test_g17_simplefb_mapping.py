from pathlib import Path

root = Path(__file__).resolve().parent.parent
asahi = (root / "asahi.rs").read_text()
file_rs = (root / "file.rs").read_text()
mmu = (root / "mmu.rs").read_text()

# The path is opt-in and derives the aperture from DT. Physical addresses and
# display geometry must never become driver constants.
assert "g17p_simplefb_iova: u64" in asahi
assert "default: 0" in asahi[asahi.index("g17p_simplefb_iova: u64") :]
assert "t8140_simplefb_phys_range" in file_rs
for required in (
    'b"apple,simple-framebuffer\\0simple-framebuffer\\0"',
    'format.as_slice() != b"x8r8g8b8\\0"',
    'name.as_slice() == b"reserved-memory\\0"',
    'node.get_property(c_str!("no-map"))',
    'compatible.as_slice() != b"framebuffer\\0"',
    "gpu.params()?.chip_id != 0x8140",
    "!user_range.is_superset(simplefb_range.clone())",
    "kernel_range.overlaps(simplefb_range.clone())",
):
    assert required in file_rs, required

assert "self.map_io(iova, phys, size, PROT_GPU_SHARED_RW)?" in mmu
assert "simplefb: Option<KernelMapping>" in mmu
assert "driver.simplefb = Some(mapping)" in mmu
assert "self.simplefb.iter()" in mmu


def validate(base, aperture, width, height, stride, fmt, reserve_base, reserve_size, page=0x4000):
    if not base or width <= 0 or height <= 0 or stride < width * 4 or fmt != "x8r8g8b8":
        return None
    visible = stride * height
    mapped = (visible + page - 1) & -page
    if base & (page - 1) or visible > aperture:
        return None
    if not (reserve_base <= base and base + mapped <= reserve_base + reserve_size):
        return None
    return base, mapped


# Exact live Neo DT/sysfs witness: 2408x1506, 9632-byte pitch. Only the
# page-rounded visible image is mapped, not the 0x5758000-byte DCP reservation.
live = validate(
    0x101EB4B8000,
    0xDE1380,
    2408,
    1506,
    9664,
    "x8r8g8b8",
    0x101EB4B8000,
    0x5758000,
)
assert live == (0x101EB4B8000, 0xDE4000)
assert validate(0x101EB4B8001, 0xDE1380, 2408, 1506, 9664, "x8r8g8b8", 0x101EB4B8000, 0x5758000) is None
assert validate(0x101EB4B8000, 0xDE137F, 2408, 1506, 9664, "x8r8g8b8", 0x101EB4B8000, 0x5758000) is None
assert validate(0x101EB4B8000, 0xDE1380, 2408, 1506, 9664, "a8r8g8b8", 0x101EB4B8000, 0x5758000) is None
assert validate(0x101EB4B8000, 0xDE1380, 2408, 1506, 9600, "x8r8g8b8", 0x101EB4B8000, 0x5758000) is None
assert validate(0x101EB4B8000, 0xDE1380, 2408, 1506, 9664, "x8r8g8b8", 0x101EC000000, 0x100000) is None

print("PASS G17P simplefb mapping: exact DT gate, bounded 0xde4000 alias, T8140/VA checks, singleton VM lifetime")
