#!/usr/bin/env python3
"""Exercise the real rebind range check with a page-aligned walker contract."""
from pathlib import Path
import subprocess
import tempfile

text = (Path(__file__).resolve().parents[1] / "g17_resources.rs").read_text()
start = text.index("let available = |name: &str, address: u64, size: u64|")
end = text.index("{", start) + 1
depth = 1
while depth:
    depth += (text[end] == "{") - (text[end] == "}")
    end += 1
closure = text[start:end] + ";"
source = r'''
#![allow(dead_code, unused_variables)]
use std::{cell::RefCell, ops::Range};
macro_rules! pr_info { ($($arg:tt)*) => {}; }
mod mmu { pub const UAT_PGMSK: usize = 0x3fff; }
mod pgtable { #[derive(Default, Copy, Clone)] pub struct MappedRange { pub start: u64, pub end: u64 } }
struct Vm { occupied: Option<Range<u64>>, walked: RefCell<Vec<Range<u64>>>, fail: bool }
impl Vm {
    fn mapped_ranges(&self, range: Range<u64>, out: &mut [pgtable::MappedRange]) -> Result<(usize, bool), ()> {
        assert_eq!(range.start & 0x3fff, 0);
        assert_eq!(range.end & 0x3fff, 0);
        self.walked.borrow_mut().push(range.clone());
        if self.fail { return Err(()); }
        if self.occupied.as_ref().is_some_and(|r| r.start < range.end && range.start < r.end) {
            out[0] = pgtable::MappedRange { start: range.start, end: range.end };
            Ok((1, false))
        } else { Ok((0, false)) }
    }
}
fn available(vm: &Vm, address: u64, size: u64) -> bool {
''' + closure + r'''
    available("test", address, size)
}
fn vm(occupied: Option<Range<u64>>) -> Vm { Vm { occupied, walked: RefCell::new(vec![]), fail: false } }
#[test] fn subpage_objects_walk_their_backing_page() {
    let vm = vm(None);
    assert!(available(&vm, 0x8000, 0x80));
    assert_eq!(*vm.walked.borrow(), [0x8000..0xc000]);
}
#[test] fn aligned_end_does_not_include_next_object() {
    let vm = vm(Some(0xc000..0x10000));
    assert!(available(&vm, 0x8000, 0x4000));
    assert_eq!(*vm.walked.borrow(), [0x8000..0xc000]);
}
#[test] fn partial_page_collision_rejects_the_graph() {
    let vm = vm(Some(0xc000..0x10000));
    assert!(!available(&vm, 0xbfff, 2));
    assert_eq!(*vm.walked.borrow(), [0x8000..0x10000]);
}
#[test] fn overflow_is_rejected_before_a_walk() {
    let vm = vm(None);
    assert!(!available(&vm, u64::MAX - 1, 4));
    assert!(!available(&vm, u64::MAX - 0x100, 1));
    assert!(vm.walked.borrow().is_empty());
}
#[test] fn walker_failure_does_not_authorize_reuse() {
    let mut vm = vm(None); vm.fail = true;
    assert!(!available(&vm, 0x8000, 0x4000));
}
'''
with tempfile.TemporaryDirectory(prefix="g17-alias-availability-") as temp:
    src = Path(temp) / "test.rs"
    binary = Path(temp) / "test"
    src.write_text(source)
    subprocess.run(["rustc", "--edition=2021", "--test", str(src), "-o", str(binary)], check=True)
    subprocess.run([str(binary)], check=True)
