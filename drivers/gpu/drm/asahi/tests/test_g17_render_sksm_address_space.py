#!/usr/bin/env python3
"""Execute the production mappers against page-owned fake VMs across handoff."""
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
source = (root / "g17_resources.rs").read_text()


def function(name):
    start = source.index(f"fn {name}(")
    end = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


constants = []
for name in ["G17P_CL_DYNAMIC_LOW_VA_START", "G17P_CL_LOW_VA_END",
             "G17P_BOOTSTRAP_CONTEXT_PEER_SIZE"]:
    constants.append(re.search(rf"const {name}:[^;]+;", source)[0])
limits = (root / "g17_queue_limits.rs").read_text()
constants.append(re.search(r"const CLIENT_LOW_VA_START:[^;]+;", limits)[0])
constants.append("const G17P_CL_CLIENT_LOW_VA_START: u64 = CLIENT_LOW_VA_START;")

# Both physical queue peers must use the mapper exercised below.
constructor = source[source.index("let reused_sksm_backing ="):source.index(
    "let ta_context_low_va =")]
assert constructor.count("map_shared_render_sksm_range(") == 2

rust = r'''
#![allow(dead_code)]
use std::{cell::RefCell, collections::BTreeMap, ops::Range, rc::Rc};
type Result<T = ()> = std::result::Result<T, i32>;
const ENOSPC: i32 = 28;
const EFAULT: i32 = 14;
const EOVERFLOW: i32 = 75;
macro_rules! pr_err { ($($arg:tt)*) => { let _ = format_args!($($arg)*); }; }
mod mmu {
    use super::*;
    pub const UAT_PGSZ: usize = 0x4000;
    pub type Prot = u8;
    pub const PROT_GPU_FW_SHARED_RW: Prot = 3;
    #[derive(Default, Clone)]
    pub struct Vm(Rc<RefCell<BTreeMap<u64, (u64, Prot)>>>);
    impl Vm {
        pub fn id(&self) -> u64 { Rc::as_ptr(&self.0) as usize as u64 }
        pub fn covers_range(&self, va: u64, size: u64, read: bool, write: bool) -> bool {
            (va..va + size).step_by(UAT_PGSZ).all(|p| self.0.borrow().get(&p)
                .is_some_and(|(_, prot)| (!read || prot & 1 != 0) && (!write || prot & 2 != 0)))
        }
        pub fn translate_iova(&self, va: u64) -> Result<u64> {
            self.0.borrow().get(&va).map(|p| p.0).ok_or(EFAULT)
        }
        fn map(&self, physical: u64, size: usize, range: Range<u64>, prot: Prot) -> Result<KernelMapping> {
            let mut start = range.start;
            while start.checked_add(size as u64).is_some_and(|end| end <= range.end) {
                if (start..start + size as u64).step_by(UAT_PGSZ)
                    .all(|p| !self.0.borrow().contains_key(&p)) {
                    for off in (0..size).step_by(UAT_PGSZ) {
                        self.0.borrow_mut().insert(start + off as u64, (physical + off as u64, prot));
                    }
                    return Ok(KernelMapping { vm: self.clone(), start, size });
                }
                start += UAT_PGSZ as u64;
            }
            Err(ENOSPC)
        }
    }
    pub struct KernelMapping { vm: Vm, start: u64, size: usize }
    impl KernelMapping { pub fn iova(&self) -> u64 { self.start } }
    impl Drop for KernelMapping {
        fn drop(&mut self) {
            for off in (0..self.size).step_by(UAT_PGSZ) {
                self.vm.0.borrow_mut().remove(&(self.start + off as u64));
            }
        }
    }
    pub struct Uat(pub Vm);
    impl Uat { pub fn kernel_vm(&self) -> &Vm { &self.0 } }
    pub fn map(vm: &Vm, physical: u64, size: usize, range: Range<u64>, prot: Prot) -> Result<KernelMapping> {
        vm.map(physical, size, range, prot)
    }
}
mod gem {
    use super::*;
    pub struct ObjectRef { pub physical: u64 }
    impl ObjectRef {
        pub fn map_range_into_range(&mut self, vm: &mmu::Vm, object: Range<usize>, range: Range<u64>,
            _: u64, prot: mmu::Prot, _: bool) -> Result<mmu::KernelMapping> {
            mmu::map(vm, self.physical + object.start as u64, object.len(), range, prot)
        }
    }
}
struct MappedObject { object: gem::ObjectRef, address: u64, size: usize }
impl MappedObject {
    fn iova(&self) -> u64 { self.address }
    fn map_alias_at(&mut self, vm: &mmu::Vm, address: u64, prot: mmu::Prot) -> Result<mmu::KernelMapping> {
        self.object.map_range_into_range(vm, 0..self.size, address..address+self.size as u64,
            mmu::UAT_PGSZ as u64, prot, false)
    }
    fn map_alias_in_range(&mut self, vm: &mmu::Vm, range: Range<u64>, align: u64,
        prot: mmu::Prot) -> Result<mmu::KernelMapping> {
        self.object.map_range_into_range(vm, 0..self.size, range, align, prot, false)
    }
}
struct G17PRenderClientAlias(u64);
impl G17PRenderClientAlias {
    fn from_range(address: u64, _: usize) -> Self { Self(address) }
    fn new(mapping: &mmu::KernelMapping) -> Self { Self(mapping.iova()) }
}
struct G17PComputeUserMappings {
    descriptors: mmu::KernelMapping, preempt: mmu::KernelMapping, robustness: mmu::KernelMapping,
    operand_state: mmu::KernelMapping, queue_context: mmu::KernelMapping,
}
impl G17PComputeUserMappings {
    fn all_reachable_from(&self, vm: &mmu::Vm) -> bool {
        [&self.descriptors, &self.preempt, &self.robustness, &self.operand_state, &self.queue_context]
            .iter().all(|m| vm.covers_range(m.iova(), mmu::UAT_PGSZ as u64, true, true))
    }
}
struct Compute { descriptors: MappedObject, preempt: MappedObject, robustness: MappedObject,
    operand_state: MappedObject, queue_context_low: MappedObject }
'''
rust += "\n".join(constants) + "\n"
for name in ["g17p_cl_dynamic_low_va_range", "g17p_cl_client_low_va_range",
             "map_shared_render_range", "map_shared_render_sksm_range",
             "prepare_render_sksm_vm_alias"]:
    rust += function(name) + "\n"
rust += "impl Compute {" + function("map_user_execution_state") + "}\n"
rust += r'''
fn compute() -> Compute {
    let object = |physical, address, size| MappedObject { object: gem::ObjectRef { physical }, address, size };
    Compute {
        descriptors: object(0x100000, G17P_CL_DYNAMIC_LOW_VA_START + 0x1000000, 0x4000),
        preempt: object(0x200000, 0, 0x4000), robustness: object(0x300000, 0, 0x4000),
        operand_state: object(0x400000, 0, 0x14000), queue_context_low: object(0x500000, 0, 0x100000),
    }
}
fn peer() -> (mmu::Uat, MappedObject, mmu::KernelMapping) {
    let uat = mmu::Uat(mmu::Vm::default());
    let mut peer = MappedObject { object: gem::ObjectRef { physical: 0x10000000 },
        address: 0x100000000, size: G17P_BOOTSTRAP_CONTEXT_PEER_SIZE };
    let high = peer.map_alias_at(uat.kernel_vm(), peer.address, 3).unwrap();
    (uat, peer, high)
}
#[test]
fn old_client_band_reproduces_foreign_mapping_after_compute_first() {
    let (uat, mut peer, _high) = peer();
    let old_vm = mmu::Vm::default();
    let global = mmu::Vm::default();
    let (_old, fixed) = map_shared_render_range(&mut peer.object, &old_vm, &global,
        0..peer.size, g17p_cl_client_low_va_range(), 3).unwrap();
    assert_eq!(fixed.iova(), CLIENT_LOW_VA_START);
    let next_vm = mmu::Vm::default();
    let compute = compute().map_user_execution_state(&next_vm).unwrap();
    assert_eq!(compute.preempt.iova(), fixed.iova());
    assert!(matches!(prepare_render_sksm_vm_alias(&uat, &next_vm, &mut peer, fixed.iova(), 3, true), Err(EFAULT)));
}
#[test]
fn stable_peer_survives_both_client_orders_and_repeated_handoffs() {
    let (uat, mut peer, _high) = peer();
    let global = mmu::Vm::default();
    let first = mmu::Vm::default();
    let (_first_alias, fixed) = map_shared_render_sksm_range(&mut peer.object, &first,
        &global, 0..peer.size, 3).unwrap();
    assert!(fixed.iova() + peer.size as u64 <= CLIENT_LOW_VA_START);
    for compute_first in [true, false] {
        for _ in 0..64 {
            let vm = mmu::Vm::default();
            let mut mappings = None;
            if compute_first { mappings = Some(compute().map_user_execution_state(&vm).unwrap()); }
            let (alias, owner) = prepare_render_sksm_vm_alias(&uat, &vm, &mut peer, fixed.iova(), 3, true).unwrap();
            if !compute_first { mappings = Some(compute().map_user_execution_state(&vm).unwrap()); }
            assert!(mappings.is_some() && owner.is_some());
            assert_eq!(alias.0, fixed.iova());
            assert!(prepare_render_sksm_vm_alias(&uat, &vm, &mut peer, fixed.iova(), 3, true).unwrap().1.is_none());
            for offset in (0..peer.size).step_by(mmu::UAT_PGSZ) {
                assert_eq!(vm.translate_iova(fixed.iova() + offset as u64),
                    uat.kernel_vm().translate_iova(peer.address + offset as u64));
            }
        }
    }
}
#[test]
fn partial_and_foreign_aliases_still_fail_closed() {
    let (uat, mut peer, _high) = peer();
    let vm = mmu::Vm::default();
    let va = G17P_CL_DYNAMIC_LOW_VA_START;
    let partial = mmu::map(&vm, peer.object.physical, mmu::UAT_PGSZ, va..va+0x4000, 3).unwrap();
    assert!(matches!(prepare_render_sksm_vm_alias(&uat, &vm, &mut peer, va, 3, true), Err(EFAULT)));
    drop(partial);
    let _foreign = mmu::map(&vm, 0x90000000, peer.size, va..va+peer.size as u64, 3).unwrap();
    assert!(matches!(prepare_render_sksm_vm_alias(&uat, &vm, &mut peer, va, 3, true), Err(EFAULT)));
}
'''
with tempfile.TemporaryDirectory(prefix="g17-sksm-address-space-") as directory:
    src = Path(directory) / "test.rs"
    binary = Path(directory) / "test"
    src.write_text(rust)
    subprocess.run(["rustc", "--edition=2021", "--test", str(src), "-o", str(binary)], check=True)
    subprocess.run([str(binary)], check=True)
