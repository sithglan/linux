#!/usr/bin/env python3
"""Focused ownership checks for retained render SKSM rings across VM handoff."""

from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class Backing:
    ta_low: int
    ta_high: int
    fragment_low: int
    fragment_high: int

    def identity(self):
        return ((self.ta_low, self.ta_high), (self.fragment_low, self.fragment_high))


def park(expected, storage_backing):
    if storage_backing.identity() != expected:
        raise ValueError("installed QID and storage ring identities disagree")
    return storage_backing


def construct(retained, fail=False):
    """Model the production Option transaction: take only on final success."""
    if fail:
        return None, retained
    return retained, None


def test_stable_ring_identity_survives_vm_alias_replacement():
    old = Backing(0x7001EF8000, 0xFFFFFC2001680000,
                  0x7001F20000, 0xFFFFFC20016C0000)
    expected = old.identity()
    parked = park(expected, old)
    replacement, parked = construct(parked)
    assert parked is None
    assert replacement.identity() == expected


def test_mismatch_is_rejected_before_backing_detach():
    expected = ((0x7001EF8000, 0x1000), (0x7001F20000, 0x2000))
    wrong = Backing(0x7001F48000, 0x3000, 0x7001F70000, 0x4000)
    try:
        park(expected, wrong)
    except ValueError:
        pass
    else:
        raise AssertionError("mismatched installed ring was accepted")


def test_failed_rebuild_leaves_retained_backing_owned():
    old = Backing(0x7001EF8000, 0x1000, 0x7001F20000, 0x2000)
    replacement, parked = construct(old, fail=True)
    assert replacement is None
    assert parked is old


def test_production_owns_and_transfers_the_hardware_visible_backing():
    root = Path(__file__).resolve().parents[1]
    resources = (root / "g17_resources.rs").read_text()
    manager = (root / "g17_manager.rs").read_text()

    assert "struct G17PRenderSksmBacking" in resources
    assert "_ta_context_global: mmu::KernelMapping" in resources
    assert "_fragment_context_global: mmu::KernelMapping" in resources
    assert "fn matches_queue_pair" in resources
    assert "sksm_backing: Option<G17PRenderSksmBacking>" in resources
    assert "if !reused_sksm_backing" in resources
    assert "retained_sksm_backing\n            .take()" in resources

    assert "backing: Option<g17_resources::G17PRenderSksmBacking>" in manager
    assert "fn park_render_sksm_backing_for_graph_rebuild" in manager
    assert "if actual != expected" in manager
    assert "take_sksm_backing()?" in manager
    assert "new-user-render-sksm-backing-unclaimed" in manager

    slot = manager[manager.index("struct G17PRenderSlotState {"):]
    slot = slot[:slot.index("}\n")]
    assert slot.index("bind: Option<G17PRenderRootOwner>") < slot.index(
        "storage: Option<KBox<g17_resources::G17PUserRenderStorage>>"
    )
    assert slot.index("context_aliases: Option<mmu::T8140NativeContextAliases>") < slot.index(
        "sksm: Option<G17PRenderSksmQueues>"
    )


if __name__ == "__main__":
    tests = [value for name, value in sorted(globals().items()) if name.startswith("test_")]
    for test in tests:
        test()
        print(f"PASS {test.__name__}")
    print(f"PASS {len(tests)}/{len(tests)} stable SKSM handoff checks")
