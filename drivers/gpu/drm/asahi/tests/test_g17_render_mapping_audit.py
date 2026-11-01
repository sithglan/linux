#!/usr/bin/env python3
"""Differential tests of production audit reads, with page tables/pages mocked."""

from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
source = (root / "mmu.rs").read_text()


def item(prefix):
    start = source.index(prefix)
    opening = source.index("{", start)
    depth, end = 1, opening + 1
    while depth:
        depth += (source[end] == "{") - (source[end] == "}")
        end += 1
    return source[start:end]


program = r'''
use std::{cell::{Cell, RefCell, RefMut}, collections::BTreeMap, ops::{Deref, DerefMut, Range}};
use core::cmp;
type PhysicalAddr = u64;
type Result<T = ()> = std::result::Result<T, i32>;
const EINVAL: i32 = 22;
const EFAULT: i32 = 14;
const EOVERFLOW: i32 = 75;
const EIO: i32 = 5;
const ENXIO: i32 = 6;
const UAT_PGMSK: usize = 0x3fff;
const T8140_CLIENT_NULL_VIEW_SIZE: usize = 0x4000;
const T8140_CLIENT_NULL_VIEW_START: u64 = 0;
struct View { source_address: u64 }
const T8140_CLIENT_NULL_VIEW: View = View { source_address: 0x10000 };
mod kernel { pub mod page { pub const PAGE_SIZE: usize = 0x4000; } }
thread_local! {
    static TRACE: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static HELD: Cell<usize> = const { Cell::new(0) };
    static PAGES: RefCell<BTreeMap<u64, Box<[u8; 0x4000]>>> = RefCell::new(BTreeMap::new());
    static COPY_FAILURE: Cell<Option<u64>> = const { Cell::new(None) };
}
fn trace(value: String) { TRACE.with(|t| t.borrow_mut().push(value)); }
fn take_trace() -> Vec<String> { TRACE.with(|t| std::mem::take(&mut *t.borrow_mut())) }
struct Page { base: u64 }
impl Page {
    unsafe fn borrow_phys(base: &u64) -> Option<Self> {
        trace(format!("borrow:{base:x}"));
        PAGES.with(|p| p.borrow().contains_key(base)).then_some(Self { base: *base })
    }
    fn with_pointer_into_page<T>(&self, offset: usize, count: usize,
                                f: impl FnOnce(*const u8) -> Result<T>) -> Result<T> {
        trace(format!("copy:{:x}:{offset}:{count}", self.base));
        if COPY_FAILURE.with(|failure| failure.get()) == Some(self.base) { return Err(EIO); }
        PAGES.with(|p| {
            let pages = p.borrow();
            let page = pages.get(&self.base).ok_or(EFAULT)?;
            page.get(offset..offset + count).ok_or(EINVAL)?;
            f(unsafe { page.as_ptr().add(offset) })
        })
    }
}
#[derive(Default)]
struct PageTable { entries: BTreeMap<u64, (u64, bool, bool)> }
impl PageTable {
    fn translate_iova(&mut self, address: u64) -> Result<u64> {
        assert_eq!(HELD.with(Cell::get), 1);
        trace(format!("translate:{address:x}"));
        let offset = address & UAT_PGMSK as u64;
        let (physical, _, _) = self.entries.get(&(address - offset)).ok_or(ENXIO)?;
        Ok(physical + offset)
    }
    fn covers_range(&mut self, range: Range<u64>, read: bool, write: bool) -> Result<bool> {
        assert_eq!(HELD.with(Cell::get), 1);
        trace(format!("covers:{:x}:{:x}:{read}:{write}", range.start, range.end));
        Ok(range.step_by(kernel::page::PAGE_SIZE).all(|va| {
            self.entries.get(&va).is_some_and(|&(_, r, w)| (!read || r) && (!write || w))
        }))
    }
}
struct VmInner { va_range: Range<u64>, page_table: PageTable }
struct Inner { value: RefCell<VmInner>, locks: Cell<usize>, fail_lock: Cell<bool> }
struct Guard<'a> { value: RefMut<'a, VmInner> }
impl Deref for Guard<'_> { type Target = VmInner; fn deref(&self) -> &VmInner { &self.value } }
impl DerefMut for Guard<'_> { fn deref_mut(&mut self) -> &mut VmInner { &mut self.value } }
impl Drop for Guard<'_> { fn drop(&mut self) { HELD.with(|h| h.set(h.get() - 1)); } }
impl Inner {
    fn exec_lock(&self, _: Option<()>, _: bool) -> Result<Guard<'_>> {
        self.locks.set(self.locks.get() + 1);
        if self.fail_lock.get() { return Err(EIO); }
        assert_eq!(HELD.with(Cell::get), 0, "nested GPUVM lock");
        HELD.with(|h| h.set(1));
        Ok(Guard { value: self.value.borrow_mut() })
    }
}
struct Vm { inner: Inner }
impl Vm {
    fn new() -> Self {
        PAGES.with(|p| p.borrow_mut().clear());
        COPY_FAILURE.with(|failure| failure.set(None));
        take_trace();
        Self { inner: Inner { value: RefCell::new(VmInner {
            va_range: 0..u64::MAX, page_table: PageTable::default(),
        }), locks: Cell::new(0), fail_lock: Cell::new(false) } }
    }
    fn map(&self, va: u64, pa: u64, readable: bool, page: bool) {
        self.inner.value.borrow_mut().page_table.entries.insert(va, (pa, readable, false));
        if page { PAGES.with(|p| {
            let mut bytes = Box::new([0; 0x4000]);
            for (i, byte) in bytes.iter_mut().enumerate() { *byte = (i as u64 ^ (pa >> 14)) as u8; }
            p.borrow_mut().insert(pa, bytes);
        }); }
    }
    fn original_audit_read(&self, iova: u64, out: &mut [u8]) -> Result<u64> {
        let physical = self.translate_iova(iova)?;
        self.read_bytes(iova, out)?;
        Ok(physical)
    }
'''
for name in ["fn covers_range_locked(", "pub(crate) fn covers_range(",
             "pub(crate) fn translate_iova(", "pub(crate) fn t8140_client_null_view_probe(",
             "pub(crate) fn read_bytes(", "pub(crate) fn read_bytes_with_physical("]:
    program += item(name) + "\n"
program += r'''
}
fn compare(vm: &Vm, address: u64, size: usize) -> Result<u64> {
    let mut old = vec![0xa5; size];
    let mut new = old.clone();
    take_trace();
    let expected = vm.original_audit_read(address, &mut old);
    let old_trace = take_trace();
    vm.inner.locks.set(0);
    let result = vm.read_bytes_with_physical(address, &mut new);
    let new_trace = take_trace();
    assert_eq!(result, expected, "first error/physical address");
    assert_eq!(new, old, "copied prefix/output on error");
    assert_eq!(new_trace, old_trace, "validation and read order");
    assert_eq!(vm.inner.locks.get(), 1);
    assert_eq!(HELD.with(Cell::get), 0, "guard must end before caller GEM access");
    result
}
#[test]
fn rce_offsets_match_original_reads_with_one_lock_each() {
    let vm = Vm::new();
    vm.map(0x20000, 0x40000, true, true);
    for offset in [0xa0, 0x7c0, 0xee0, 0x1600] {
        assert_eq!(compare(&vm, 0x20000 + offset, 16), Ok(0x40000 + offset));
    }
}
#[test]
fn read_spans_noncontiguous_physical_pages_without_relocking() {
    let vm = Vm::new();
    vm.map(0x20000, 0x40000, true, true);
    vm.map(0x24000, 0x80000, true, true);
    assert_eq!(compare(&vm, 0x23ff8, 32), Ok(0x43ff8));
}
#[test]
fn initial_translation_error_precedes_empty_or_coverage_error() {
    let vm = Vm::new();
    assert_eq!(compare(&vm, 0x20000, 0), Err(ENXIO));
    assert_eq!(compare(&vm, 0x20000, 16), Err(ENXIO));
    vm.map(0x20000, 0x40000, false, true);
    assert_eq!(compare(&vm, 0x20000, 0), Err(EINVAL));
    assert_eq!(compare(&vm, 0x20000, 16), Err(EFAULT));
}
#[test]
fn incomplete_coverage_or_vm_bounds_reject_before_copy() {
    let vm = Vm::new();
    vm.map(0x20000, 0x40000, true, true);
    assert_eq!(compare(&vm, 0x23ff8, 32), Err(EFAULT));
    vm.inner.value.borrow_mut().va_range = 0..0x20000;
    assert_eq!(compare(&vm, 0x20000, 16), Err(EFAULT));
}
#[test]
fn overflow_read_and_missing_struct_page_keep_error_and_output() {
    let vm = Vm::new();
    let last_page = u64::MAX & !(UAT_PGMSK as u64);
    vm.map(last_page, 0x40000, true, true);
    assert_eq!(compare(&vm, u64::MAX - 7, 16), Err(EFAULT));
    vm.map(0x20000, 0x80000, true, false);
    assert_eq!(compare(&vm, 0x20000, 16), Err(EFAULT));
}
#[test]
fn later_page_failure_preserves_already_copied_prefix() {
    let vm = Vm::new();
    vm.map(0x20000, 0x40000, true, true);
    vm.map(0x24000, 0x80000, true, true);
    COPY_FAILURE.with(|f| f.set(Some(0x80000)));
    assert_eq!(compare(&vm, 0x23ff8, 32), Err(EIO));
}
#[test]
fn exec_failure_does_not_translate_or_copy() {
    let vm = Vm::new();
    vm.inner.fail_lock.set(true);
    assert_eq!(compare(&vm, 0x20000, 16), Err(EIO));
}
#[test]
fn null_view_pair_uses_one_lock_and_preserves_translation_order() {
    let vm = Vm::new();
    vm.map(0, 0x40000, true, true);
    vm.map(0x10000, 0x40000, true, true);
    for offset in [0x1e00, 0x1f80] {
        vm.inner.locks.set(0);
        take_trace();
        assert_eq!(vm.t8140_client_null_view_probe(offset), Ok((0x40000 + offset, 0x40000 + offset)));
        assert_eq!(vm.inner.locks.get(), 1);
        assert_eq!(take_trace(), [format!("translate:{:x}", 0x10000 + offset), format!("translate:{offset:x}")]);
        assert_eq!(HELD.with(Cell::get), 0);
    }
    vm.map(0, 0x80000, true, true);
    assert_eq!(vm.t8140_client_null_view_probe(0x1e00), Ok((0x41e00, 0x81e00)));
}
#[test]
fn null_view_range_and_first_translation_fail_before_later_work() {
    let vm = Vm::new();
    assert_eq!(vm.t8140_client_null_view_probe(0x4000), Err(EINVAL));
    assert_eq!(vm.inner.locks.get(), 0);
    assert_eq!(vm.t8140_client_null_view_probe(0x1e00), Err(ENXIO));
    assert_eq!(take_trace(), ["translate:11e00"]);
    vm.map(0x10000, 0x40000, true, true);
    assert_eq!(vm.t8140_client_null_view_probe(0x1e00), Err(ENXIO));
    assert_eq!(take_trace(), ["translate:11e00", "translate:1e00"]);
    assert_eq!(HELD.with(Cell::get), 0);
}
'''

# The audit must finish its VM transaction before each GEM VMap and comparison;
# a caller-wide guard would invert GPUVM/GEM reservation acquisition order.
resource = (root / "g17_resources.rs").read_text()
audit = resource.split("pub(crate) fn audit_fragment_rce_mapping(", 1)[1].split(
    "/// Read descriptor +0x20f0", 1)[0]
assert audit.index("read_bytes_with_physical(") < audit.index("self.descriptors.with_bytes_mut(")
assert "if gpu_bytes != cpu_bytes" in audit and "return Err(EIO)" in audit

with tempfile.TemporaryDirectory(prefix="g17p-mapping-audit-") as directory:
    path = Path(directory)
    (path / "audit.rs").write_text(program)
    subprocess.run(["rustc", "--edition=2021", "--test", str(path / "audit.rs"),
                    "-o", str(path / "audit-tests")], check=True)
    subprocess.run([str(path / "audit-tests")], check=True)
