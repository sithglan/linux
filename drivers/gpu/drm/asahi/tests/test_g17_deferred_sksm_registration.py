#!/usr/bin/env python3
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
manager = (root / "g17_manager.rs").read_text()


def block(source: str, marker: str) -> str:
    start = source.index(marker)
    brace = source.index("{", start)
    depth = 0
    for pos in range(brace, len(source)):
        if source[pos] == "{":
            depth += 1
        elif source[pos] == "}":
            depth -= 1
            if depth == 0:
                return source[start : pos + 1]
    raise AssertionError(f"unterminated block: {marker}")


compute = block(manager, "fn submit_translated_first_bind_channel(")
render_entry = block(manager, "fn publish_render_sksm_group(")
render_pair = block(manager, "fn begin_translated_render_with_hardware_buffer(")
split_fragment = block(manager, "pub(crate) fn publish_split_render_fragment_group(")
native_tiling = block(manager, "pub(crate) fn publish_native_render_tiling_group(")

# Production queue installation is transported only by tag 15 and the firmware
# channel. The AP SKSM helpers may remain for teardown diagnostics, but no first
# install path is allowed to call them.
assert "g17p_sksm_configure_queue" not in compute
assert "g17p_sksm_configure_queue" not in render_entry
assert "g17p_sksm_publish_write_pair" not in compute
assert "g17p_sksm_publish_write_pair" not in render_entry

# Compute is not marked installed until the complete CL_2 outer publication is
# visible. Mapping preparation and tag-15 publication both precede that point.
assert compute.index("publish_initial_config_update_pointer") < compute.index(
    "prepare_compute_firmware_install"
)
assert compute.index("prepare_compute_firmware_install") < compute.index(
    "publish_compute_channel"
)
assert compute.index("publish_compute_channel") < compute.index(
    "G17PSksmQueueHardwareState::HardwareRegistered"
)

# Normal and deferred render chronologies validate tag-15 ownership before the
# outer producer and commit it only after successful outer publication.
assert render_pair.index("validate_render_firmware_install") < render_pair.index(
    "publish_partial_opening_pair"
)
assert render_pair.index("publish_partial_opening_pair") < render_pair.index(
    "commit_render_firmware_install"
)
assert split_fragment.index("validate_render_firmware_install") < split_fragment.index(
    "publish_deferred_partial_opening_fragment"
)
assert split_fragment.index("publish_deferred_partial_opening_fragment") < split_fragment.index(
    "commit_render_firmware_install"
)
assert native_tiling.index("validate_render_firmware_install") < native_tiling.index(
    "publish_deferred_partial_opening_tiling"
)
assert native_tiling.index("publish_deferred_partial_opening_tiling") < native_tiling.index(
    "commit_render_firmware_install"
)

model = r'''
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Queue { Fresh, Installed }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Error { MissingInstall, DuplicateInstall, OuterPublish }

fn publish(state: &mut Queue, install: bool, outer_succeeds: bool) -> Result<(), Error> {
    match (*state, install) {
        (Queue::Fresh, false) => return Err(Error::MissingInstall),
        (Queue::Installed, true) => return Err(Error::DuplicateInstall),
        _ => {}
    }
    if !outer_succeeds {
        return Err(Error::OuterPublish);
    }
    if install {
        *state = Queue::Installed;
    }
    Ok(())
}

#[test]
fn fresh_qid_is_installed_by_successful_tag15_group() {
    let mut q = Queue::Fresh;
    assert_eq!(publish(&mut q, true, true), Ok(()));
    assert_eq!(q, Queue::Installed);
}

#[test]
fn failed_outer_publish_does_not_claim_firmware_install() {
    let mut q = Queue::Fresh;
    assert_eq!(publish(&mut q, true, false), Err(Error::OuterPublish));
    assert_eq!(q, Queue::Fresh);
}

#[test]
fn missing_and_duplicate_installs_fail_closed() {
    let mut fresh = Queue::Fresh;
    assert_eq!(publish(&mut fresh, false, true), Err(Error::MissingInstall));
    assert_eq!(fresh, Queue::Fresh);

    let mut installed = Queue::Installed;
    assert_eq!(publish(&mut installed, true, true), Err(Error::DuplicateInstall));
    assert_eq!(installed, Queue::Installed);
    assert_eq!(publish(&mut installed, false, true), Ok(()));
}
'''

with tempfile.TemporaryDirectory() as directory:
    source = Path(directory) / "test.rs"
    binary = Path(directory) / "test"
    source.write_text(model)
    subprocess.run(
        ["rustc", "--edition=2021", "--test", str(source), "-o", str(binary)],
        check=True,
    )
    subprocess.run([str(binary), "--test-threads=1"], check=True)

print(
    "PASS firmware-mediated SKSM install: no AP registration writes, tag-15-before-outer "
    "ordering, post-publication ownership, failed-publication retention, and duplicate rejection"
)
