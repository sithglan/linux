#!/usr/bin/env python3
"""Guard compiled ARM64 G17 frame budgets; not a whole-kernel stack proof.

Run against the built, partially linked asahi.o. Measures immediate prologue
allocations before the first call/return. The direct-call path estimate excludes
external/indirect calls, tail calls and recursive re-entry. Hardware and caller
stack qualification remain necessary.
"""
import argparse
import re
import subprocess
from pathlib import Path


def audit(path):
    output = subprocess.check_output(
        ['llvm-objdump', '-d', '--no-show-raw-insn', '--demangle', str(path)],
        text=True,
    )
    functions = {}
    for block in re.split(r'\n(?=[0-9a-f]+ <)', output):
        header = re.match(r'([0-9a-f]+) <(.+)>:', block)
        if not header:
            continue
        frame = 0
        for line in block.splitlines()[1:]:
            if re.search(r'\b(?:bl|blr|ret)\s', line):
                break
            sub = re.search(r'\bsub\s+sp, sp, #0x([0-9a-f]+)(, lsl #12)?', line)
            push = re.search(r'\[(?:sp), #-0x([0-9a-f]+)\]!', line)
            if sub:
                frame += int(sub[1], 16) * (4096 if sub[2] else 1)
            if push:
                frame += int(push[1], 16)
        calls = {int(x, 16) for x in re.findall(r'\bbl\s+0x([0-9a-f]+)', block)}
        functions[int(header[1], 16)] = (header[2], frame, calls)

    def find(fragment):
        matches = [(address, entry) for address, entry in functions.items()
                   if entry[0].endswith(fragment)]
        assert len(matches) == 1, (fragment, len(matches))
        return matches[0]

    limits = {
        'AsahiDriver as kernel::platform::Driver>::probe': 5120,
        'G17PLiveRuntime>::start': 2048,
        'g17_live_boot::boot_initial_session': 4096,
        'G17PManagerConstruction>::new_with_uat': 1024,
        'G17ResourceOwner>::new_with_uat': 8192,
        'G17PManagerConstruction>::select_render_slot': 256,
    }
    violations = []
    for name, limit in limits.items():
        _, (_, frame, _) = find(name)
        print(f'{name}: {frame} bytes (budget {limit})')
        if frame > limit:
            violations.append((name, frame, limit))

    # Finite simple-path estimate; cycles are cut, not certified safe.
    cache = {}
    visiting = set()
    def longest(address):
        if address not in functions or address in visiting:
            return 0, []
        if address in cache:
            return cache[address]
        visiting.add(address)
        name, frame, calls = functions[address]
        weight, chain = max((longest(c) for c in calls), default=(0, []),
                            key=lambda entry: entry[0])
        visiting.remove(address)
        result = frame + weight, [(name, frame)] + chain
        cache[address] = result
        return result

    address, _ = find('AsahiDriver as kernel::platform::Driver>::probe')
    total, chain = longest(address)
    print(f'Module-internal direct-call estimate: {total} bytes')
    for name, frame in chain:
        print(f'  {frame:5d} {name}')
    if total > 18432:
        violations.append(('module-internal direct-call estimate', total, 18432))
    assert not violations, violations
    print('PASS frame budgets; external/indirect/recursive paths are unmeasured')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('object', nargs='?', type=Path,
                        default=Path(__file__).resolve().parents[1] / 'asahi.o')
    audit(parser.parse_args().object)
