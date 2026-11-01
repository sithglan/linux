#!/usr/bin/env python3
"""Exercise the production VM alias transaction across competing builders."""
from pathlib import Path
import subprocess
import sys
import tempfile

source = (Path(__file__).resolve().parents[1] / "mmu.rs").read_text()
start = source.index("    fn retain_job_context_aliases(")
end = source.index("{", start) + 1
depth = 1
while depth:
    depth += (source[end] == "{") - (source[end] == "}")
    end += 1
method = source[start:end]
if "--negative-control" in sys.argv:
    method = method.replace("let _install =", "let install =").replace(
        "        self.append_driver_mappings", "        drop(install);\n        self.append_driver_mappings")

rust = r'''
#![allow(dead_code)]
use std::sync::{Arc, Mutex, MutexGuard, atomic::{AtomicBool, Ordering}};
use std::time::Duration;
type Result<T=()> = std::result::Result<T, i32>;
type KVec<T> = Vec<T>;
const EINVAL: i32 = 22;
struct Lock(Mutex<()>);
impl Lock { fn lock(&self) -> MutexGuard<'_, ()> { self.0.lock().unwrap() } }
struct KernelMapping { visible: Arc<AtomicBool> }
impl Drop for KernelMapping { fn drop(&mut self) { self.visible.store(false, Ordering::SeqCst); } }
struct Vm {
    t8140_context_alias_install: Option<Arc<Lock>>,
    retained: Mutex<Vec<KernelMapping>>,
    fail_retention: AtomicBool,
}
impl Vm {
    fn new() -> Self { Self {
        t8140_context_alias_install: Some(Arc::new(Lock(Mutex::new(())))),
        retained: Mutex::new(Vec::new()), fail_retention: AtomicBool::new(false),
    } }
    fn append_driver_mappings(&self, mappings: KVec<KernelMapping>) -> Result {
        if self.fail_retention.swap(false, Ordering::SeqCst) { return Err(12); }
        self.retained.lock().unwrap().extend(mappings); Ok(())
    }
''' + method + r'''
}
#[test]
fn failed_builder_cannot_lend_tentative_alias_to_another_pair() {
    let vm = Arc::new(Vm::new());
    let visible = Arc::new(AtomicBool::new(false));
    let (built_tx, built_rx) = std::sync::mpsc::channel();
    let (finish_tx, finish_rx) = std::sync::mpsc::channel();
    let first_vm = vm.clone(); let first_visible = visible.clone();
    let first = std::thread::spawn(move || first_vm.retain_job_context_aliases(|| {
        first_visible.store(true, Ordering::SeqCst);
        let mapping = KernelMapping { visible: first_visible };
        built_tx.send(()).unwrap(); finish_rx.recv().unwrap();
        drop(mapping); Err(5)
    }));
    built_rx.recv().unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let second_vm = vm.clone(); let second_visible = visible.clone();
    let second = std::thread::spawn(move || second_vm.retain_job_context_aliases(|| {
        entered_tx.send(()).unwrap();
        if second_visible.load(Ordering::SeqCst) { return Ok(vec![]); }
        second_visible.store(true, Ordering::SeqCst);
        Ok(vec![KernelMapping { visible: second_visible }])
    }));
    let entered_early = entered_rx.recv_timeout(Duration::from_millis(100)).is_ok();
    finish_tx.send(()).unwrap();
    assert_eq!(first.join().unwrap(), Err(5));
    second.join().unwrap().unwrap();
    assert!(!entered_early, "another pair accepted a tentative alias");
    assert!(visible.load(Ordering::SeqCst));
    assert_eq!(vm.retained.lock().unwrap().len(), 1);
}
#[test]
fn retention_failure_unmaps_before_next_builder_and_retry_is_owned() {
    let vm = Vm::new(); let visible = Arc::new(AtomicBool::new(false));
    vm.fail_retention.store(true, Ordering::SeqCst);
    let build = || { visible.store(true, Ordering::SeqCst);
        Ok(vec![KernelMapping { visible: visible.clone() }]) };
    assert_eq!(vm.retain_job_context_aliases(build), Err(12));
    assert!(!visible.load(Ordering::SeqCst));
    vm.retain_job_context_aliases(build).unwrap();
    assert!(visible.load(Ordering::SeqCst));
    drop(vm); assert!(!visible.load(Ordering::SeqCst));
}
#[test]
fn unrelated_vm_installs_without_waiting_for_this_vm() {
    let busy = Vm::new(); let independent = Arc::new(Vm::new());
    let held = busy.t8140_context_alias_install.as_ref().unwrap().lock();
    let (tx, rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        independent.retain_job_context_aliases(|| Ok(vec![])).unwrap(); tx.send(()).unwrap();
    });
    let progressed = rx.recv_timeout(Duration::from_secs(1)).is_ok();
    drop(held); thread.join().unwrap(); assert!(progressed);
}
'''
with tempfile.TemporaryDirectory() as directory:
    path = Path(directory) / "test.rs"
    path.write_text(rust)
    binary = Path(directory) / "test"
    subprocess.run(["rustc", "--edition=2021", "--test", str(path), "-o", str(binary)], check=True)
    subprocess.run([str(binary)], check=True)

# The real caller keeps the page/backing validation inside this transaction,
# and no longer exports tentative mappings to a later retention operation.
install = source[source.index("    pub(crate) fn install_t8140_job_context_aliases("):]
install = install[:install.index("    /// Invalidate every live T8140 context")]
assert ") -> Result {" in install
assert install.index("source.retain_job_context_aliases(||") < install.index("existing_pages != 0")
assert install.index("source.retain_job_context_aliases(||") < install.index("source.map_in_range(")
print("PASS VM alias validation/map/retention is one transaction")
