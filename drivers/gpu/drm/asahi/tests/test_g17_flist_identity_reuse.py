#!/usr/bin/env python3
"""Guard: a new render FList pool on a recycled (ID, VAs) tuple is a new owner.

A closed context's pool is dropped once its teardown completes; its FList ID
lease (lowest free) and its control/backing VAs are then free. The next
context's pool can therefore present exactly the previous owner's
(ID, control, run-table, grow-end) tuple. Matching owners by that tuple alone
treated the new pool as the retained owner, so its image was not staged and
the global UMA descriptor row kept the cursors firmware had advanced for the
old pool, while the new pool's page list holds only its constructor entries.
Native stages every newly prepared pool (populateInitialFirmwareState +
halLoadUMADesc). The owner identity therefore also carries a boot-unique pool
object serial.
"""

from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]
RESOURCES = (ROOT / "g17_resources.rs").read_text()
MANAGER = (ROOT / "g17_manager.rs").read_text()


class Allocator:
    """Lowest-free ID and lowest-fit VA, as the pool constructor sees them."""

    def __init__(self):
        self.ids = set()
        self.vas = set()
        self.serial = 0

    def new_pool(self):
        flist_id = next(i for i in range(129, 255) if i not in self.ids)
        va = next(v for v in range(0x1000, 0x100000, 0x1000) if v not in self.vas)
        self.ids.add(flist_id)
        self.vas.add(va)
        self.serial += 1
        return (flist_id, va, self.serial)

    def drop(self, pool):
        self.ids.discard(pool[0])
        self.vas.discard(pool[1])


def model():
    alloc = Allocator()
    owners = {}
    tuple_only_reuse = 0
    restaged = 0
    for _process in range(64):
        pool = alloc.new_pool()
        previous = owners.get(pool[0])
        if previous is not None and previous[:2] == pool[:2]:
            tuple_only_reuse += 1
        if previous is None or previous != pool:
            restaged += 1
        owners[pool[0]] = pool
        # Teardown completes before the next client opens (v92b settle).
        alloc.drop(pool)
    assert tuple_only_reuse == 63, tuple_only_reuse
    assert restaged == 64, restaged


def source_guards():
    assert re.search(r"serial: G17P_RENDER_FLIST_SERIAL\.fetch_add\(1, Ordering::Relaxed\) \+ 1", RESOURCES)
    assert "pub(crate) fn render_flist_serial(&self) -> u64" in RESOURCES
    assert "fn same_pool_object(&self, serial: u64) -> bool" in MANAGER
    takeover_sites = MANAGER.count(") && owner.same_pool_object(storage.render_flist_serial())")
    assert takeover_sites == 2, takeover_sites
    assert MANAGER.count("same_pool_object(storage.render_flist_serial())") == 5
    assert "prepared.pool_serial = storage.render_flist_serial();" in MANAGER
    # The serial is host-only: never part of a firmware record.
    assert "pool_serial" not in (ROOT / "g17_submission.rs").read_text()


model()
source_guards()
print("ok")
