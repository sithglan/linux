#!/usr/bin/env python3
"""A render-storage stage that cannot succeed must fail before it destroys.

`new_user_render_storage` tested the TTBAT context lookup inside `finish`, long
after it had unconditionally cleared the VM's render pool and appended the
job-context aliases to it. Its failure path cannot undo either -- its idea of
undoing the pool is to clear it a second time, and it removes no aliases -- so
every attempt left the VM worse off, and a client retrying went round that loop
once per try. Measured on a live wedge: four identical
`STAGE FAIL stage=new-user-render-storage errno=-2` from one client.

The lookup reads only, so it belongs before the first mutation. And when it does
fail, the VM has no context holding its root, which makes every later submission
against it undispatchable -- accepted by firmware, never executed, never
completed, and only discovered when the client times out. Recording it on the
VM turns a desktop-wide wedge into one application failing to start.
"""

from pathlib import Path
import subprocess
import sys
import tempfile


root = Path(__file__).resolve().parents[1]
resources = (root / "g17_resources.rs").read_text()
mmu = (root / "mmu.rs").read_text()
control = "--negative-control" in sys.argv
if control:
    # Put the lookup back after the first mutation.
    resources = resources.replace(
        "if let Err(error) = uat.require_t8140_vm_context(vm) {",
        "if false {")


def item(text: str, marker: str) -> str:
    start = text.index(marker)
    pos = text.index("{", start) + 1
    depth = 1
    while depth:
        depth += (text[pos] == "{") - (text[pos] == "}")
        pos += 1
    return text[start:pos]


ctor = item(resources[resources.index("impl G17PUninstalledRenderGraph {"):], "    pub(crate) fn build(")

# --- the lookup runs before ANY mutation of the VM ------------------------
guard = ctor.index("require_t8140_vm_context(vm)")
for mutation in (
    "vm.clear_render_pool_mappings(",
    "G17PUserRenderStorage::new(",
    "install_t8140_job_context_aliases(vm)",

):
    assert guard < ctor.index(mutation), mutation

# --- and it refuses without having touched anything -----------------------
refusal = ctor[guard : ctor.index("vm.clear_render_pool_mappings(")]
assert "return Err(error);" in refusal
assert "vm.status().record(error.to_errno());" in refusal

# --- the late path records the same permanent condition -------------------
commit = item(resources, "    pub(crate) fn finish_user_render_storage(")
tail = commit[commit.index("if let Err(error) = finish {"): ]
assert "if error == ENOENT" in tail
assert "vm.status().record(error.to_errno());" in tail

# --- the lookup itself must stay pure ------------------------------------
mask = item(mmu, "    pub(crate) fn t8140_vm_context_mask(")
for mutation in ("tlbi_asid", "store(", "mem::sync()"):
    assert mutation not in mask, mutation
require = item(mmu, "    pub(crate) fn require_t8140_vm_context(")
assert "t8140_vm_context_mask(vm)? == 0" in require
assert "ENOENT" in require
# the flush keeps its own check and still flushes
flush = item(mmu, "    pub(crate) fn flush_t8140_vm_contexts(")
assert "t8140_vm_context_mask(vm)?" in flush
assert "contexts == 0" in flush and "ENOENT" in flush
assert "tlbi_asid" in flush and "mem::sync()" in flush

rust = r'''
// Model the constructor's effect on a VM, with the lookup early and late.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct VmState { pool_cleared: bool, stray_aliases: u32, status: i32 }

fn attempt(vm: &mut VmState, has_context: bool, lookup_first: bool) -> Result<(), i32> {
    if lookup_first && !has_context {
        vm.status = -2;                 // ENOENT recorded, nothing touched
        return Err(-2);
    }
    vm.pool_cleared = true;             // clear_render_pool_mappings, unconditional
    vm.stray_aliases += 1;              // job-context aliases appended
    if !has_context {
        // finish fails at flush_t8140_vm_contexts; the rollback clears the
        // pool again and removes no aliases.
        vm.pool_cleared = true;
        vm.status = -2;
        return Err(-2);
    }
    vm.pool_cleared = false;
    vm.stray_aliases -= 1;
    Ok(())
}

#[test]
fn the_early_lookup_leaves_the_vm_untouched() {
    let mut vm = VmState::default();
    assert_eq!(attempt(&mut vm, false, true), Err(-2));
    assert_eq!(vm, VmState { pool_cleared: false, stray_aliases: 0, status: -2 });
}

#[test]
fn four_retries_accumulate_nothing() {
    // The live wedge: one client, four identical STAGE FAILs.
    let mut vm = VmState::default();
    for _ in 0..4 {
        assert_eq!(attempt(&mut vm, false, true), Err(-2));
    }
    assert_eq!(vm.stray_aliases, 0);
    assert!(!vm.pool_cleared);
}

#[test]
fn the_late_lookup_is_what_accumulated_damage() {
    let mut vm = VmState::default();
    for _ in 0..4 {
        assert_eq!(attempt(&mut vm, false, false), Err(-2));
    }
    assert_eq!(vm.stray_aliases, 4);    // one per try, never removed
    assert!(vm.pool_cleared);           // cleared, never restored
}

#[test]
fn a_healthy_vm_is_unaffected_by_the_early_lookup() {
    let mut vm = VmState::default();
    assert_eq!(attempt(&mut vm, true, true), Ok(()));
    assert_eq!(vm, VmState::default());
}

// VmStatus::record compare-exchanges from 0 and ignores non-negatives.
fn record(status: &mut i32, error: i32) {
    if error < 0 && *status == 0 {
        *status = error;
    }
}

#[test]
fn the_recorded_status_is_sticky_and_only_negative() {
    let mut status = 0i32;
    record(&mut status, 0);
    assert_eq!(status, 0);
    record(&mut status, -2);
    assert_eq!(status, -2);
    record(&mut status, -16);
    assert_eq!(status, -2);   // first error wins
}
'''

with tempfile.TemporaryDirectory() as directory:
    directory = Path(directory)
    source = directory / "stage.rs"
    source.write_text(rust)
    executable = directory / "stage"
    subprocess.run(
        ["rustc", "--edition=2021", "--test", str(source), "-o", str(executable)],
        check=True,
    )
    subprocess.run([str(executable), "--test-threads=1"], check=True, timeout=60)

print(
    "PASS render storage fails before mutating: TTBAT lookup precedes every VM "
    "mutation, refusal is pure, and the VM is marked so later submissions fail fast"
)
