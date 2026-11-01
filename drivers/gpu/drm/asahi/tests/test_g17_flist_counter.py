#!/usr/bin/env python3
"""Exercise the production FList counter plan across overlap, reuse and wrap."""
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
source = (root / 'g17_resources.rs').read_text()
start = source.index('fn g17p_flist_submitted_next(')
end = source.index('\n}\n', start) + 2
unit = "type Result<T=()> = std::result::Result<T,i32>;const EINVAL:i32=22;#[repr(align(8))] struct Buffer([u8;8]);\n" + source[source.index("fn g17p_live_word_bounds("):source.index("fn put_u32(")] + source[start:end] + r'''
fn submit(raw: &mut [u8], kicks: u32) {
    let next = g17p_flist_submitted_next(raw, 0, kicks).unwrap();
    raw[..8].copy_from_slice(&next.to_le_bytes());
}
#[test] fn overlapping_pairs_reach_equality_only_after_last_engine() {
    let mut storage = Buffer([0;8]); let raw = &mut storage.0;
    for _ in 0..16 { submit(raw, 2); }
    let submitted = u64::from_le_bytes(*raw);
    assert_eq!(submitted, 32);
    for completed in 0..32 { assert_ne!(completed, submitted); }
    assert_eq!(32, submitted);
    // Released-pool reinitialization copies this baseline; another pair
    // must finish both engines before equality becomes true again.
    let baseline = g17p_flist_submitted_next(raw, 0, 0).unwrap();
    submit(raw, 2);
    assert_ne!(baseline + 1, u64::from_le_bytes(*raw));
    assert_eq!(baseline + 2, u64::from_le_bytes(*raw));
}
#[test] fn split_publication_and_aborted_plan_do_not_overcount() {
    let mut storage = Buffer([0;8]); let raw = &mut storage.0;
    let _unpublished = g17p_flist_submitted_next(raw, 0, 2).unwrap();
    assert_eq!(u64::from_le_bytes(*raw), 0);
    submit(raw, 1);
    assert_eq!(u64::from_le_bytes(*raw), 1);
    submit(raw, 1);
    assert_eq!(u64::from_le_bytes(*raw), 2);
}
#[test] fn native_u64_wrap_and_full_width() {
    let mut storage = Buffer((u64::MAX - 1).to_le_bytes()); let raw = &mut storage.0;
    submit(raw, 2);
    assert_eq!(u64::from_le_bytes(*raw), 0);
    *raw = (1u64 << 40).to_le_bytes();
    submit(raw, 2);
    assert_eq!(u64::from_le_bytes(*raw), (1u64 << 40) + 2);
}
#[test] fn rejects_invalid_ranges_before_publication() {
    assert!(g17p_flist_submitted_next(&[0; 7], 0, 2).is_none());
    assert!(g17p_flist_submitted_next(&[0; 8], usize::MAX, 1).is_none());
    assert!(g17p_flist_submitted_next(&[0; 8], 0, 3).is_none());
}
'''
with tempfile.TemporaryDirectory() as tmp:
    src = Path(tmp) / 'counter.rs'
    src.write_text(unit)
    binary = Path(tmp) / 'counter'
    subprocess.run(['rustc', '--edition=2021', '--test', str(src), '-o', str(binary)], check=True)
    subprocess.run([str(binary)], check=True)
