#!/usr/bin/env python3
"""Pinned host detach witnesses and wait-predicate checks; no firmware execution."""
import argparse
import hashlib
import re
import struct
import subprocess
from pathlib import Path

AGX_SHA = 'ccbe120a3a197038216426b507b5409873397b947308239b4ecc75d76759e87f'
IOGPU_SHA = 'cd124ce0fb35e9af5fab2950e4d975e739b52a7e7754cdfcc1853e077c180e19'
BASE = 0xfffffe0000000000
KC_BASE = 0xfffffe0007004000

AGX = {
    0x8b03408: 'ldr x1, [x19, #0xe0]',
    0x8b03418: 'ldr x8, [x19, #0xc8]',
    0x8b0341c: 'ldrb w2, [x8]',
    0x8b03420: 'ldrb w3, [x8, #0x1]',
    0x8b03424: 'ldrb w4, [x8, #0x4]',
    0x8b03428: 'ldrb w5, [x8, #0x26]',
    0x8b0344c: 'blraa x8, x16',
    0x8b03450: 'ldr x8, [x19, #0x10]',  # No result check before continuation.
    0x8b37c6c: 'mov x0, x1',  # Address converter is identity in this override.
    0x8b37ca8: 'stp q0, q0, [sp, #0x20]',
    0x8b37cac: 'stp q0, q0, [sp]',
    0x8b37cb0: 'mov w8, #0x13',
    0x8b37cb4: 'str w8, [sp]',
    0x8b37ce4: 'stur x0, [sp, #0xc]',
    0x8b37ce8: 'strb w23, [sp, #0x9]',
    0x8b37cec: 'strb w22, [sp, #0xa]',
    0x8b37cf0: 'strb w21, [sp, #0xb]',
    0x8b37cf4: 'strb w20, [sp, #0x8]',
    0x8b37d88: 'cbnz w0, 0xfffffe0008b37da8',
    0x8b37d90: 'tbz w8, #0x1, 0xfffffe0008b37da8',
    0x8b37da4: 'bl 0xfffffe0008b0dd6c',
    0x8b10cc8: 'cmp w0, w26, uxtb',  # Full-ring check, next producer == read.
    0x8b10d90: 'ldp q0, q1, [x21]',
    0x8b10d94: 'ldp q2, q3, [x21, #0x20]',
    0x8b10db4: 'str w8, [x22]',  # Return written slot, not producer-after.
    0x8b10e18: 'dmb ish',
    0x8b10e44: 'and w1, w8, #0xff',
    0x8b203d8: 'ldr w0, [x8, #0x20]',
    0x8b20428: 'ldr w0, [x8]',
    0x8b0dde0: 'sub w26, w3, w2',
    0x8b0de58: 'sub w8, w23, w19',
    0x8b0de5c: 'cmp w8, w26',
    0x8b0de60: 'ccmp w23, w0, #0x4, ls',
    0x8b0de64: 'b.ne 0xfffffe0008b0de90',
    0x8b0de68: 'mov w23, #0x0',
    0x8b0de98: 'mov w23, #0x2bc',
    0x8b0de9c: 'movk w23, #0xe000, lsl #16',
    0x8b0deac: 'b.hs 0xfffffe0008b0de6c',
    0x8b0debc: 'cbnz w8, 0xfffffe0008b0de6c',
    0x8b0dee8: 'tbnz w0, #0x0, 0xfffffe0008b0de6c',
    0x8b0def4: 'tbnz w9, #0x0, 0xfffffe0008b0de6c',
    0x8b0df74: 'ccmp w25, w0, #0x4, ls',
}
IOGPU = {
    0xa866cbc: 'str w8, [x11]',
    0xa86896c: 'cmp x9, x23',
    0xa868970: 'b.eq 0xfffffe000a868bdc',
    0xa8689cc: 'ldr w13, [x13, #0x10]',
    0xa8689d0: 'cmn w13, #0x1',
    0xa8689ec: 'sub w14, w14, w15',
    0xa8689f4: 'b.gt 0xfffffe000a868a08',
    0xa868a90: 'str x23, [x26]',
    0xa868d28: 'cmp x8, x20',
    0xa868d54: 'str xzr, [x22]',
}


def inspect(path, sha, witnesses):
    data = path.read_bytes()
    assert hashlib.sha256(data).hexdigest() == sha, 'unsupported binary'
    output = subprocess.check_output(['llvm-objdump', '-d', '--no-show-raw-insn',
                                      str(path)], text=True)
    instructions = {}
    for line in output.splitlines():
        match = re.match(r'^([0-9a-f]{16}):\s+(.+)$', line)
        if match:
            instructions[int(match[1], 16)] = ' '.join(match[2].split(' ;', 1)[0].split())
    for offset, expected in witnesses.items():
        actual = instructions.get(BASE + offset, '')
        assert actual == expected or actual.startswith(expected + ' <'), (hex(offset), actual, expected)
    return data


def read_va(data, address, fmt):
    cursor = 32
    size = struct.calcsize(fmt)
    for _ in range(struct.unpack_from('<I', data, 16)[0]):
        command, length = struct.unpack_from('<II', data, cursor)
        if command == 0x19:  # LC_SEGMENT_64; require file-backed bytes.
            va, _, offset, file_size = struct.unpack_from('<QQQQ', data, cursor + 24)
            if va <= address and address + size <= va + file_size:
                return struct.unpack_from(fmt, data, offset + address - va)[0]
        cursor += length
    raise AssertionError(f'unbacked address {address:x}')


def native_wait_done(initial_read, written_slot, read, write):
    # Emulate SUB W / CMP / CCMP #4,LS / B.NE, including 32-bit wrap.
    span = (written_slot - initial_read) & 0xffffffff
    delta = (read - initial_read) & 0xffffffff
    carry, zero = delta >= span, delta == span
    ls = not carry or zero
    final_zero = read == write if ls else bool(4 & 4)
    return final_zero


def predicate_checks():
    checks = 0
    # One reserved slot, at most 255 outstanding records. Test start, target,
    # wrap and one-past-target against independent ring-step advancement.
    for start in range(256):
        for position in range(255):
            slot = (start + position) & 255
            for advanced in {0, position, position + 1, 255}:
                read = (start + advanced) & 255
                write = (read + 1) & 255  # Nonempty; isolate the interval arm.
                assert native_wait_done(start, slot, read, write) == (advanced > position)
                assert native_wait_done(start, slot, read, read)  # Empty-ring arm.
                checks += 2
    assert not native_wait_done(3, 3, 3, 4)
    assert native_wait_done(3, 3, 4, 5)
    return checks + 2


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('agx', type=Path)
    parser.add_argument('iogpu', type=Path)
    args = parser.parse_args()
    data = inspect(args.agx, AGX_SHA, AGX)
    inspect(args.iogpu, IOGPU_SHA, IOGPU)
    assert read_va(data, 0xfffffe000714fa08 + 19 * 4, '<I') == 0
    # Encoded pointer targets are checked as image evidence, not a replacement
    # for the original kernel collection's unavailable chained-fixup metadata.
    for table, slot, target in [
        (0xfffffe0007edfec8, 0x30, 0xfffffe0008b20420),
        (0xfffffe0007edfec8, 0x38, 0xfffffe0008b203d0),
        (0xfffffe0007ee0e78, 0x288, 0xfffffe0008b37c74),
        (0xfffffe0007ee0e78, 0x2d0, 0xfffffe0008b37c68),
        (0xfffffe0007ee0e78, 0x460, 0xfffffe0008b30388),
    ]:
        raw = read_va(data, table + 16 + slot, '<Q')
        assert KC_BASE + (raw & 0xffffffff) == target
    print(f'PASS {len(AGX) + len(IOGPU)} instruction witnesses, route table and 5 encoded vtable targets')
    print(f'PASS {predicate_checks()} host wait-predicate boundary/wrap cases')
    print('Firmware handler effects and acknowledgement ordering remain unverified; detach stays disabled')


if __name__ == '__main__':
    main()
