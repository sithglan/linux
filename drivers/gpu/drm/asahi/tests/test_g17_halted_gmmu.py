#!/usr/bin/env python3
"""Exercise the actual narrow register-reader against ordered, fallible mocks."""
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
source = (root / "regs.rs").read_text()


def extract_from(text, marker):
    start = text.index(marker)
    pos = text.index("{", start) + 1
    depth = 1
    while depth:
        depth += (text[pos] == "{") - (text[pos] == "}")
        pos += 1
    return text[start:pos]


def extract(marker):
    return extract_from(source, marker)


method = extract("pub(crate) fn g17p_halted_gmmu_snapshot(")
fields = extract("pub(crate) struct G17PHaltedGmmuSnapshot")
assert "try_write" not in method
assert ".relaxed()" not in method
assert "FAULT_REQUESTOR" not in method
assert "0xd918" not in method and "0x1_0a" not in method
live = (root / "g17_live_boot.rs").read_text()
assert "report.state, report.host_requested, report.reason" in live
manager = (root / "g17_manager.rs").read_text()
assert "render_bind_for_descriptor_context(fault_vm_slot)" in manager
assert "log_compute_fault_address_candidates" in manager
shadow_peer = extract_from(manager, "const fn g17p_fault_sparse_ro_lower(")
selector = manager[manager.index("fn render_bind_for_descriptor_context("):]
selector = selector[:selector.index("\n    pub(crate) fn log_fault_address_candidates(")]
assert "storage.descriptor_context_id() == u16::from(descriptor_context_id)" in selector
# Live and quarantined render storage carries the descriptor context exactly.
# Context-retirement owners no longer have storage, so their retained root slot
# is the only identity still available and is expected in those two branches.
assert selector.count(".root_slot()") == 2
assert "self.render_slots.active().bind.as_ref().vm()" not in manager
assert "snapshot.info_before >> 17" in live
assert "snapshot.fault_info >> 17" in live
assert 'then_some((context, "active-compute"))' in live
assert 'then_some((context, "retained-compute"))' in live
assert manager.count("Honeykrisp sparse-RO lower peer") == 1
assert "g17p_fault_sparse_ro_lower(decoded)" in manager

unit = r'''
use std::cell::RefCell;
type Result<T> = core::result::Result<T, i32>;
const ENODEV: i32 = 19;
const T8140_HOST_IRQ_SUMMARY: usize = 0xe01000;
const FAULT_INFO_G14X: usize = 0xd8c0;
const FAULT_ADDR_G14X: usize = 0xd8c8;
const G17P_HONEYKRISP_SPARSE_RO_BIT: u64 = 1 << 40;
struct Mock { reads: RefCell<Vec<usize>>, gpc: u32, valid: bool, fail: usize }
impl Mock {
    fn read(&self, offset: usize) -> Result<u64> {
        self.reads.borrow_mut().push(offset);
        if self.reads.borrow().len() == self.fail { return Err(5); }
        Ok(match offset {
            T8140_HOST_IRQ_SUMMARY => self.gpc as u64,
            FAULT_INFO_G14X => if self.valid { 0x40001 } else { 0 },
            FAULT_ADDR_G14X => 0x1234,
            _ => panic!("unexpected register"),
        })
    }
    fn try_read32(&self, offset: usize) -> Result<u32> { self.read(offset).map(|x|x as u32) }
    fn try_read64(&self, offset: usize) -> Result<u64> { self.read(offset) }
}
struct Window { mmio: Mock }
impl Window { fn try_access(&self) -> Option<&Mock> { Some(&self.mmio) } }
struct Resources { sgx: Window }
fn resources(gpc: u32, valid: bool, fail: usize) -> Resources {
    Resources { sgx: Window { mmio: Mock { reads: RefCell::new(Vec::new()), gpc, valid, fail } } }
}
#[test]
fn reads_only_the_valid_retained_requestor_with_info_bracketing() {
    let r = resources(0xa, true, 0);
    let s = r.g17p_halted_gmmu_snapshot().unwrap().unwrap();
    assert_eq!(s.address_word, Some(0x1234));
    assert_eq!(*r.sgx.mmio.reads.borrow(), [0xe01000, 0xd8c0, 0xd8c8, 0xd8c0]);
}
#[test]
fn no_low_access_when_gpc_is_off() {
    let r = resources(0, true, 0);
    assert!(r.g17p_halted_gmmu_snapshot().unwrap().is_none());
    assert_eq!(*r.sgx.mmio.reads.borrow(), [0xe01000]);
}
#[test]
fn no_address_read_when_info_is_invalid() {
    let r = resources(0xa, false, 0);
    assert!(r.g17p_halted_gmmu_snapshot().unwrap().unwrap().address_word.is_none());
    assert_eq!(*r.sgx.mmio.reads.borrow(), [0xe01000, 0xd8c0, 0xd8c0]);
}
#[test]
fn each_read_error_stops_without_later_access() {
    for failure in 1..=4 {
        let r = resources(0xa, true, failure);
        assert!(r.g17p_halted_gmmu_snapshot().is_err());
        assert_eq!(r.sgx.mmio.reads.borrow().len(), failure);
    }
}
#[test]
fn sparse_ro_fault_maps_to_the_exact_lower_allocation_peer() {
    assert_eq!(
        g17p_fault_sparse_ro_lower(0x177bfef1f80),
        Some(0x77bfef1f80),
    );
    assert_eq!(g17p_fault_sparse_ro_lower(0x77bfef1f80), None);
    assert_eq!(
        g17p_fault_sparse_ro_lower(G17P_HONEYKRISP_SPARSE_RO_BIT),
        Some(0),
    );
}
''' + shadow_peer
with tempfile.TemporaryDirectory(prefix="g17-halted-gmmu-") as tmp:
    test = Path(tmp) / "test.rs"
    test.write_text(unit + fields + "\nimpl Resources {\n" + method + "\n}\n")
    exe = Path(tmp) / "test"
    subprocess.run(["rustc", "-Awarnings", "--edition=2021", "--test", str(test), "-o", str(exe)], check=True)
    subprocess.run([str(exe)], check=True)
print("halted-GMMU source scope checks passed")
