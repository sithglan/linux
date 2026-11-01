#!/usr/bin/env python3
"""Reproduce scheduler-owner evidence from the identified native G17 kext.

This audits a supplied binary, not Linux behavior or the firmware consumer.
It deliberately fails on a different image instead of applying stale offsets.
"""

import argparse
import hashlib
import re
import subprocess
from pathlib import Path


SHA256 = "ccbe120a3a197038216426b507b5409873397b947308239b4ecc75d76759e87f"

# Exact dataflow witnesses, with unslid addresses in the identified image.
WITNESSES = {
    "one accelerator pool constructs 128 KSM queues": {
        0xFFFFFE0008ADB6D0: "mov x19, #0x0",
        0xFFFFFE0008ADB6F4: "add x8, x0, #0x12, lsl #12",
        0xFFFFFE0008ADB6F8: "add x23, x8, #0xf38",
        0xFFFFFE0008ADB750: "mov x2, x19",
        0xFFFFFE0008ADB760: "str x21, [x23, x19, lsl #3]",
        0xFFFFFE0008ADB764: "add x19, x19, #0x1",
        0xFFFFFE0008ADB768: "cmp x19, #0x80",
        0xFFFFFE0008ADB76C: "b.ne 0xfffffe0008adb6fc <__ZN14AGXAccelerator19createKSMKickQueuesEv+0x4c>",
    },
    "resource release encodes type19 identity and conditionally waits": {
        0xFFFFFE0008B37CB0: "mov w8, #0x13",
        0xFFFFFE0008B37CB4: "str w8, [sp]",
        0xFFFFFE0008B37CE4: "stur x0, [sp, #0xc]",
        0xFFFFFE0008B37CE8: "strb w23, [sp, #0x9]",
        0xFFFFFE0008B37CEC: "strb w22, [sp, #0xa]",
        0xFFFFFE0008B37CF0: "strb w21, [sp, #0xb]",
        0xFFFFFE0008B37CF4: "strb w20, [sp, #0x8]",
        0xFFFFFE0008B37D88: "cbnz w0, 0xfffffe0008b37da8 <__ZN14AGXArmFirmware21submitReleaseResourceEyhhhh+0x134>",
        0xFFFFFE0008B37D90: "tbz w8, #0x1, 0xfffffe0008b37da8 <__ZN14AGXArmFirmware21submitReleaseResourceEyhhhh+0x134>",
        0xFFFFFE0008B37DA4: "bl 0xfffffe0008b0dd6c <__ZN11AGXFirmware30waitForDeviceControlCompletionEP33AGFIAcceleratorDeviceControlEntryjj>",
    },
    "G17X context LUT setter is a no-op, not a missing hardware store": {
        0xFFFFFE0008B5D678: "bti c",
        0xFFFFFE0008B5D67C: "ret",
        0xFFFFFE0008AC6C90: "bti c",
        0xFFFFFE0008AC6C94: "ret",
        0xFFFFFE0008B5DFF0: "mov w0, #0x0",
    },
    "native root removal invalidates before TLBI and DSB": {
        0xFFFFFE0008B995C0: "ldr q0, [x8]",
        0xFFFFFE0008B995C4: "mov x9, #-0x2",
        0xFFFFFE0008B995CC: "and.16b v0, v0, v1",
        0xFFFFFE0008B995D0: "str q0, [x8]",
        0xFFFFFE0008B995D4: "dmb ish",
        0xFFFFFE0008B995E4: "bl 0xfffffe0008b980b4 <__ZN6AGXUAT9issueTLBIEjyy>",
        0xFFFFFE0008B99604: "b 0xfffffe0008b9853c <__ZN6AGXUAT8issueDSBEv>",
    },
    "CL ConfigUpdate process identity is a full channel getter result": {
        0xFFFFFE0008AE6A40: "ldr x8, [x0, #0x58]",
        0xFFFFFE0008AE6A44: "ldr w0, [x8, #0x44]",
        0xFFFFFE0008AEDC74: "ldr x0, [x20, #0x180]",
        0xFFFFFE0008AEDC9C: "add x8, x16, #0x1f8",
        0xFFFFFE0008AEDCA0: "ldr x9, [x16, #0x1f8]",
        0xFFFFFE0008AEDCB4: "stur w0, [x22, #0x5a]",
    },
    "QoS config membership is cached separately from active work": {
        0xFFFFFE0008AEF6D8: "ldr w9, [x20, #0x38]",
        0xFFFFFE0008AEF6DC: "ldr w10, [x20, #0x10]",
        0xFFFFFE0008AEF6EC: "add x9, x20, #0x28",
        0xFFFFFE0008AEF704: "ldr w8, [x20, #0x68]",
        0xFFFFFE0008AEF718: "strb w8, [x19, #0x2ec]",
        0xFFFFFE0008AEDC10: "ldrb w8, [x20, #0x2ec]",
        0xFFFFFE0008AEDC18: "stur w8, [x22, #0x52]",
        0xFFFFFE0008AEF8F4: "add x11, x8, #0x28",
        0xFFFFFE0008AEF910: "str w9, [x8, #0x38]",
        0xFFFFFE0008AEF914: "str wzr, [x8, #0x68]",
        0xFFFFFE0008B7A740: "stp xzr, xzr, [x19, #0x28]",
        0xFFFFFE0008B7A27C: "add x9, x19, #0x18",
        0xFFFFFE0008B7A5F8: "add x12, x19, #0x18",
        0xFFFFFE0008B7A604: "str x11, [x12, w9, uxtw #3]",
    },
    "QoS completion retires shared QID row only after host counters match": {
        0xFFFFFE0008B7A4F8: "add x8, x0, #0x648",
        0xFFFFFE0008B7A524: "add x10, x0, #0x448",
        0xFFFFFE0008B7A540: "cmp w8, w9",
        0xFFFFFE0008B7A54C: "add x8, x0, #0xc48",
        0xFFFFFE0008B7A578: "add x9, x0, #0xa48",
        0xFFFFFE0008B7A608: "ldr x9, [x0, #0x20]",
        0xFFFFFE0008B7A624: "mov w11, #0xff",
        0xFFFFFE0008B7A628: "strb w11, [x12]",
        0xFFFFFE0008B7A644: "strb wzr, [x8]",
    },
    "QoS DM interval compares shared submitted against host completed": {
        0xFFFFFE0008B7A2A4: "ldr x9, [x22, #0x20]",
        0xFFFFFE0008B7A2F8: "add x8, x9, #0xe20",
        0xFFFFFE0008B7A31C: "add x11, x22, #0xc48",
        0xFFFFFE0008B7A338: "cmp x10, x9",
        0xFFFFFE0008B7A400: "add x10, x9, #0xe08",
        0xFFFFFE0008B7A418: "str x0, [x11]",
        0xFFFFFE0008B7A440: "ldr x8, [x9, #0xe00]",
        0xFFFFFE0008B7A448: "str x8, [x9, #0xe00]",
    },
    "completion boolean records willSubmit for each engine": {
        0xFFFFFE0008AEE61C: "add x8, x16, #0x168",
        0xFFFFFE0008AEE630: "strb w22, [x20, #0x2eb]",
        0xFFFFFE0008AEF158: "ldrb w10, [x19, #0x2eb]",
        0xFFFFFE0008AEF15C: "and w3, w10, #0x1",
        0xFFFFFE0008AC4E30: "add x8, x16, #0x168",
        0xFFFFFE0008AC4E44: "strb w21, [x20, #0x30d]",
        0xFFFFFE0008AC5F58: "ldrb w10, [x19, #0x30d]",
        0xFFFFFE0008AC5F5C: "and w3, w10, #0x1",
        0xFFFFFE0008B929C8: "add x8, x16, #0x168",
        0xFFFFFE0008B929DC: "strb w26, [x20, #0xe71]",
        0xFFFFFE0008B9425C: "ldrb w10, [x19, #0xe71]",
        0xFFFFFE0008B94260: "and w3, w10, #0x1",
    },
    "one command queue QoS identity across engines": {
        0xFFFFFE0008AEDC28: "ldr x8, [x0, #0x868]",
        0xFFFFFE0008AEDC2C: "ldr w8, [x8, #0x10]",
        0xFFFFFE0008AEDC30: "stur w8, [x22, #0x56]",
        0xFFFFFE0008AC3C64: "ldr x8, [x0, #0x868]",
        0xFFFFFE0008AC3C68: "ldr w8, [x8, #0x10]",
        0xFFFFFE0008AC3C6C: "stur w8, [x27, #0x56]",
        0xFFFFFE0008B91F38: "ldr x8, [x0, #0x868]",
        0xFFFFFE0008B91F3C: "ldr w8, [x8, #0x10]",
        0xFFFFFE0008B91F40: "stur w8, [x22, #0x56]",
    },
    "QoS allocator differs from TA PB and USC pool allocators": {
        0xFFFFFE0008B79D18: "mov w2, #0x80",
        0xFFFFFE0008B7A69C: "ldr x8, [x19, #0x58]",
        0xFFFFFE0008B7A6A0: "ldr x8, [x8, #0x518]",
        0xFFFFFE0008B7A6A4: "add x0, x8, #0xc98",
        0xFFFFFE0008B94900: "ldr x8, [x19, #0x10]",
        0xFFFFFE0008B94904: "add x8, x8, #0x14, lsl #12",
        0xFFFFFE0008B94908: "add x0, x8, #0xf50",
        0xFFFFFE0008B9DB78: "add x8, x8, #0x14, lsl #12",
        0xFFFFFE0008B9DB7C: "add x0, x8, #0xf98",
    },
    "CL tag15 owner": {
        0xFFFFFE0008AEDC1C: "ldr x8, [x0, #0x618]",
        0xFFFFFE0008AEDC20: "ldr x8, [x8, #0xe0]",
        0xFFFFFE0008AEDC24: "stur x8, [x22, #0x4a]",
    },
    "3D tag15 owner": {
        0xFFFFFE0008AC3C58: "ldr x8, [x0, #0x618]",
        0xFFFFFE0008AC3C5C: "ldr x8, [x8, #0xe0]",
        0xFFFFFE0008AC3C60: "stur x8, [x27, #0x4a]",
    },
    "TA tag15 owner": {
        0xFFFFFE0008B91F2C: "ldr x8, [x0, #0x618]",
        0xFFFFFE0008B91F30: "ldr x8, [x8, #0xe0]",
        0xFFFFFE0008B91F34: "stur x8, [x22, #0x4a]",
    },
    "common channel owner": {
        0xFFFFFE0008AE8684: "ldr x9, [x21, #0x618]",
        0xFFFFFE0008AE8688: "ldr x9, [x9, #0xe0]",
        0xFFFFFE0008AE868C: "str x9, [x19, #0xf8]",
        0xFFFFFE0008AE6ADC: "ldr x10, [x0, #0xf8]",
        0xFFFFFE0008AE6AE0: "str x10, [x9, #0x70]",
    },
    "logical queue retains reclaimable state": {
        0xFFFFFE0008AF25E0: "ldr x8, [x0, #0x618]",
        0xFFFFFE0008AF2638: "mov x2, x21",
        0xFFFFFE0008AF266C: "str x19, [x21, #0x618]",
        0xFFFFFE0008B02D94: "stp x1, x2, [x0, #0x10]",
        0xFFFFFE0008B031C8: "str x8, [x19, #0xe0]",
    },
    "CL context update is a comparison": {
        0xFFFFFE0008AEF5E8: "ldr x10, [x19, #0x158]",
        0xFFFFFE0008AEF600: "ldr x12, [x23, #0x38]",
        0xFFFFFE0008AEF604: "cmp x12, x10",
        0xFFFFFE0008AEF608: "cset w10, ne",
        0xFFFFFE0008AEF618: "strb w10, [x19, #0x190]",
        0xFFFFFE0008AEDB6C: "ldrb w8, [x20, #0x190]",
        0xFFFFFE0008AEDB70: "and w8, w8, #0x1",
        0xFFFFFE0008AEDB74: "stur w8, [x22, #0x62]",
        0xFFFFFE0008AEF924: "ldr x8, [x19, #0x158]",
        0xFFFFFE0008AEF928: "str x8, [x21, #0x38]",
    },
    "free passes scheduler identity to firmware": {
        0xFFFFFE0008B03408: "ldr x1, [x19, #0xe0]",
        0xFFFFFE0008B03418: "ldr x8, [x19, #0xc8]",
        0xFFFFFE0008B0341C: "ldrb w2, [x8]",
        0xFFFFFE0008B03420: "ldrb w3, [x8, #0x1]",
        0xFFFFFE0008B03424: "ldrb w4, [x8, #0x4]",
        0xFFFFFE0008B03428: "ldrb w5, [x8, #0x26]",
        0xFFFFFE0008B0344C: "blraa x8, x16",
    },
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--objdump", default="llvm-objdump")
    args = parser.parse_args()
    digest = hashlib.sha256(args.binary.read_bytes()).hexdigest()
    if digest != SHA256:
        parser.error(f"unsupported binary SHA256 {digest}; expected {SHA256}")
    result = subprocess.run(
        [args.objdump, "-d", "--no-show-raw-insn", str(args.binary)],
        check=True, capture_output=True, text=True,
    )
    instructions = {}
    for line in result.stdout.splitlines():
        match = re.match(r"^([0-9a-f]{16}):\s+(.+)$", line)
        if match:
            instruction = match[2].split(" ;", 1)[0]
            instructions[int(match[1], 16)] = " ".join(instruction.split())
    failures = []
    for name, expected in WITNESSES.items():
        for address, instruction in expected.items():
            actual = instructions.get(address)
            if actual != instruction:
                failures.append(f"{name}: {address:#x}: {actual!r} != {instruction!r}")
    if failures:
        raise SystemExit("\n".join(failures))
    print(f"Verified {len(WITNESSES)} native producer/ownership witnesses; SHA256 {digest}")
    print("Firmware table indexing, B2 lifetime and Linux runtime correctness are separate checks.")


if __name__ == "__main__":
    main()
