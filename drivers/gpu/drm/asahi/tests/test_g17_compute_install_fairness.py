#!/usr/bin/env python3
"""Regression model for first-install writer starvation in G17P admission."""

from pathlib import Path


ROOT = Path(__file__).resolve().parents[5]
SOURCE = (ROOT / "drivers/gpu/drm/asahi/g17_drm.rs").read_text()


def block(marker: str) -> str:
    start = SOURCE.index(marker)
    brace = SOURCE.index("{", start)
    depth = 0
    for offset, char in enumerate(SOURCE[brace:], brace):
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return SOURCE[start : offset + 1]
    raise AssertionError(f"unterminated block: {marker}")


acquire_compute = block("fn acquire_compute(")
acquire_render = block("fn acquire_lease(")

# A first installer announces writer intent before it starts draining existing
# leases. Both render and repeat-compute admission observe the same flag.
intent = acquire_compute.index("held.compute_install_pending = true;")
drain = acquire_compute.index("while held.render_slots != 0")
reader_drain = acquire_compute.index("self.compute_readers.load(Ordering::Acquire) != 0", drain)
assert intent < drain < reader_drain
assert "|| held.render_transition" in acquire_compute[drain:]
assert "while held.compute_install_pending || held.compute_install || held.render_transition" in acquire_compute
assert "held.compute_install_pending = false;\n            held.compute_install = true;" in acquire_compute
assert "if !held.compute_install_pending && !held.compute_install" in acquire_render
assert acquire_compute.count("held.render_slots & held.render_quarantined != 0") >= 2

# Fault and signal exits after writer intent must withdraw it and wake peers.
tail = acquire_compute[intent:]
assert tail.count("held.compute_install_pending = false;") >= 4
assert tail.count("self.lease_free.notify_all();") >= 3


class Gate:
    """Minimal admission model; firmware execution is intentionally absent."""

    def __init__(self, renders: int = 0, computes: int = 0) -> None:
        self.render_readers = renders
        self.compute_readers = computes
        self.writer_intent = False
        self.install_active = False
        self.quarantined_render = False

    def request_install(self) -> bool:
        if self.writer_intent or self.install_active:
            return False
        if self.quarantined_render and self.render_readers:
            return False
        self.writer_intent = True
        return True

    def try_render(self) -> bool:
        if self.writer_intent or self.install_active:
            return False
        self.render_readers += 1
        return True

    def try_steady_compute(self) -> bool:
        if self.writer_intent or self.install_active:
            return False
        self.compute_readers += 1
        return True

    def retire_render(self) -> None:
        assert self.render_readers
        self.render_readers -= 1

    def retire_compute(self) -> None:
        assert self.compute_readers
        self.compute_readers -= 1

    def progress_install(self) -> str:
        assert self.writer_intent
        if self.quarantined_render and self.render_readers:
            self.writer_intent = False
            return "failed"
        if self.render_readers or self.compute_readers:
            return "waiting"
        self.writer_intent = False
        self.install_active = True
        self.compute_readers = 1
        return "active"

    def finish_install(self) -> None:
        assert self.install_active and self.compute_readers == 1
        self.compute_readers = 0
        self.install_active = False


# This is the r151 shape under 20,000 varied schedules: one to four active
# renders, zero to three steady-compute readers, spurious progress checks and
# adversarial attempts to re-enter either reader class. Writer intent makes the
# empty interval durable; every pre-existing reader can still retire.
for seed in range(20_000):
    gate = Gate(1 + seed % 4, (seed // 4) % 4)
    assert gate.request_install()
    state = seed | 1
    for _ in range(64):
        state = (state * 1664525 + 1013904223) & 0xFFFFFFFF
        choice = state % 5
        if choice == 0 and gate.render_readers:
            gate.retire_render()
        elif choice == 1 and gate.compute_readers:
            gate.retire_compute()
        elif choice == 2:
            assert not gate.try_render()
        elif choice == 3:
            assert not gate.try_steady_compute()
        result = gate.progress_install()
        if result == "active":
            break
        assert result == "waiting"
    else:
        while gate.render_readers:
            gate.retire_render()
        while gate.compute_readers:
            gate.retire_compute()
        assert gate.progress_install() == "active"
    assert gate.install_active and not gate.writer_intent
    assert not gate.try_render() and not gate.try_steady_compute()
    gate.finish_install()
    assert gate.try_render()
    gate.retire_render()


# Interrupted/faulted writer withdrawal must restore ordinary render admission.
gate = Gate(renders=1)
assert gate.request_install()
gate.writer_intent = False
assert gate.try_render()
assert gate.render_readers == 2


# A quarantined render retains its lease until processor stop and therefore can
# never drain. The unpublished installer must withdraw writer intent instead of
# blocking every healthy queue behind an impossible condition.
gate = Gate(renders=1)
gate.quarantined_render = True
assert not gate.request_install()
assert not gate.writer_intent


# A second first-installer cannot overwrite the first reservation. It can claim
# the next interval only after the first active install retires.
gate = Gate(renders=1)
assert gate.request_install()
assert not gate.request_install()
gate.retire_render()
assert gate.progress_install() == "active"
gate.finish_install()
assert gate.request_install()
assert gate.progress_install() == "active"
gate.finish_install()

print("g17 compute-install fairness tests: 6 passed; 20000 adversarial schedules")
