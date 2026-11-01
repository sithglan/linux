#!/usr/bin/env python3
"""Verify the admitted J700 A000 instruction windows against the container."""

from __future__ import annotations

import hashlib
import sys
from pathlib import Path


DEFAULT_CONTAINER = Path(
    "/Users/dj/Developer/Asahi/research/gpu/fw-re/"
    "t8140-a18pro-j700/armfw_g17p.im4p"
)
CONTAINER_SHA256 = "b0f84b9972c72e7c0be96af7bc6e504b7693dd82af551e1a4d2bf408a11df83a"
A000_OFFSET = 0x99
A000_SIZE = 0x185BC0
A000_SHA256 = "3e2821bb40d3abffe30488a06ede5f523cfa47544d6a0ad7654a08be6b5dc30f"
MACHO_TEXT_FILE_DELTA = 0x4000

WINDOWS = {
    "boot-main-config-global.hex": (0x1E9BC, 0x30),
    "type4-selector.hex": (0x22D98, 0x1C),
    "type4-publish.hex": (0x22E24, 0x10),
    "type4-common-publish.hex": (0x22EBC, 0x0C),
    "type4-pending-take.hex": (0x258E4, 0x20),
    "scheduler-deferred.hex": (0x25EA4, 0x14),
    "type4-thunk.hex": (0x2635C, 0x28),
    "type4-dispatch-record.hex": (0x73800, 0x18),
    "control-drain-cursors.hex": (0x3BAC, 0x18),
    "peer-handles-init.hex": (0x19184, 0x20),
    "mbi-trace-init.hex": (0x221AC, 0x0C),
    "type9-peer-lease.hex": (0x22DE4, 0x14),
    "mbi-rx-route.hex": (0x3237C, 0x64),
    "mbi-trace-write.hex": (0x324F8, 0x78),
}


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def main() -> int:
    container_path = Path(sys.argv[1]) if len(sys.argv) == 2 else DEFAULT_CONTAINER
    if len(sys.argv) > 2:
        raise SystemExit(f"usage: {sys.argv[0]} [armfw_g17p.im4p]")
    container = container_path.read_bytes()
    assert digest(container) == CONTAINER_SHA256, "J700 container SHA-256 mismatch"
    a000 = container[A000_OFFSET : A000_OFFSET + A000_SIZE]
    assert len(a000) == A000_SIZE, "truncated A000 payload"
    assert digest(a000) == A000_SHA256, "J700 A000 SHA-256 mismatch"

    fixture_dir = Path(__file__).resolve().parent
    for name, (link_offset, size) in WINDOWS.items():
        fixture = bytes.fromhex((fixture_dir / name).read_text().strip())
        assert len(fixture) == size, f"{name}: fixture size mismatch"
        file_offset = link_offset + MACHO_TEXT_FILE_DELTA
        source = a000[file_offset : file_offset + size]
        assert source == fixture, f"{name}: instruction bytes differ from J700 A000"
        print(f"{name}: va=0xfffffc0000{link_offset:06x} sha256={digest(fixture)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
