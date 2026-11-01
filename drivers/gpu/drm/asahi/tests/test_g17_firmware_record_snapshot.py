#!/usr/bin/env python3
"""Execute the production firmware-record snapshot with aligned and bad ranges."""
from pathlib import Path
import os
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
text = (root / "g17_resources.rs").read_text()
def item(marker):
    start = text.index(marker)
    pos = text.index("{", start) + 1
    depth = 1
    while depth:
        depth += (text[pos] == "{") - (text[pos] == "}")
        pos += 1
    return text[start:pos] + "\n"
# The snapshot reads through the same checked natural-width live-word helpers
# as the host/firmware ring cursors.
helper = "".join(item(marker) for marker in [
    "fn g17p_live_word_bounds(", "fn g17p_live_read_u32(", "fn g17p_snapshot_fw_record("])
source = r'''
type Result<T=()> = core::result::Result<T, i32>;
const EINVAL: i32 = 22;
''' + helper + r'''
#[repr(align(8))] struct Aligned([u8; 160]);
#[test] fn all_words_preserve_the_wire_bytes() {
    let mut bytes = Aligned([0; 160]);
    for (i, byte) in bytes.0.iter_mut().enumerate() {
        *byte = (i as u8).wrapping_mul(37);
    }
    for offset in [0usize, 4, 8, 16, 40, 64] {
        for size in [4usize, 8, 24, 40, 72] {
            let mut output = vec![0; size];
            assert_eq!(g17p_snapshot_fw_record(&bytes.0, offset, &mut output), Ok(()));
            assert_eq!(output, &bytes.0[offset..offset + size]);
        }
    }
}
#[test] fn invalid_ranges_leave_the_output_untouched() {
    let bytes = Aligned([0x33; 160]);
    for (offset, size) in [(1,4), (2,72), (3,8), (0,6), (0,0), (156,8), (usize::MAX,8)] {
        let mut output = vec![0x55; size];
        assert_eq!(g17p_snapshot_fw_record(&bytes.0, offset, &mut output), Err(EINVAL));
        assert!(output.iter().all(|byte| *byte == 0x55));
    }
    let mut output = [0x55; 4];
    assert_eq!(g17p_snapshot_fw_record(&bytes.0[1..], 0, &mut output), Err(EINVAL));
    assert_eq!(output, [0x55; 4]);
}
'''
with tempfile.TemporaryDirectory(prefix="g17-fw-record-") as temp:
    path = Path(temp)
    (path / "snapshot.rs").write_text(source)
    subprocess.run([os.environ.get("RUSTC", "rustc"), "--edition=2021", "--test",
                    str(path / "snapshot.rs"), "-o", str(path / "snapshot")], check=True)
    subprocess.run([str(path / "snapshot")], check=True)

# Both producer-published rings go through the snapshot on every build: a
# diagnostic gate would let the default path fall back to a byte copy.
for drain in ["    pub(crate) fn drain_firmware_event(", "    pub(crate) fn drain_ktrace_ring("]:
    body = item(drain)
    assert "g17p_snapshot_fw_record(bytes, record_offset, &mut raw)" in body, drain
    assert "copy_from_slice(bytes.get(record_offset" not in body, drain
    assert "DIAG_WORD_FW_RECORDS" not in body, drain
print("PASS firmware-record snapshot: word-aligned wire bytes, untouched output on bad ranges, used by both drains")
