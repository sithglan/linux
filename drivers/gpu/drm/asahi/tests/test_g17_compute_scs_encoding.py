#!/usr/bin/env python3
"""Exercise the joint SCS descriptor and SKSM entry encoding on the host.

The complete production compute/submission codecs are compiled without their
in-module tests. UAPI types and preparation methods, and the manager's scratch
geometry, are extracted unchanged from the current source. Only unrelated error
variants are omitted to avoid linking the kernel runtime.

Publication is a host-side sentinel, not MMIO: an invalid layout must return an
error without changing descriptor storage or producing an entry to publish.
This checks the preparation contract, not live manager ordering or GPU behavior.
Requires Python 3 and rustc; all generated files live in a temporary directory.
"""

import json
from pathlib import Path
import re
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]


def block(source, marker):
    """Extract one current Rust item, retaining its body verbatim."""
    if source.count(marker) != 1:
        raise ValueError(f"expected one Rust item: {marker}")
    start = source.index(marker)
    cursor = source.index("{", start) + 1
    depth = 1
    while depth:
        depth += (source[cursor] == "{") - (source[cursor] == "}")
        cursor += 1
    return source[start:cursor]


def rust_source():
    uapi = (ROOT / "g17_uapi.rs").read_text()
    manager = (ROOT / "g17_manager.rs").read_text()
    source = "#![allow(dead_code)]\n"
    for module in ("g17_config", "g17_compute", "g17_submission"):
        source += f"#[path = {json.dumps(str(ROOT / (module + '.rs')))}] mod {module};\n"

    # Submission's pure codec only needs this geometry from the live manager.
    source += "mod g17_manager {\n"
    source += "#[derive(Debug)] pub(crate) enum ConstructionError { SksmScratchOffsetOverflow }\n"
    for name in ("G17SksmScratchGeometry", "G17SksmScratchOffsets"):
        source += "#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n"
        source += block(manager, f"pub(crate) struct {name} {{") + "\n"
    source += block(manager, "impl G17SksmScratchGeometry {") + "\n}\n"

    source += "mod uapi {\nuse crate::{g17_compute, g17_submission::*};\n"
    source += "#[derive(Debug, PartialEq, Eq)] pub(crate) enum UapiTranslateError { ComputeBuild(g17_compute::ComputeBuildError) }\n"
    source += block(uapi, "impl From<g17_compute::ComputeBuildError> for UapiTranslateError {") + "\n"
    constants = re.findall(r"^pub\(crate\) const UAPI_MAX_ATTACHMENTS:.*;$", uapi, re.M)
    if len(constants) != 1:
        raise ValueError("expected one UAPI_MAX_ATTACHMENTS declaration")
    source += constants[0] + "\n"
    for name in (
        "UapiAttachment", "UapiAttachmentList", "UapiTimestamp", "UapiTimestamps",
        "TranslatedComputeCommand", "ComputeInternalState", "ComputeTimestampAddresses",
    ):
        source += "#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n"
        source += block(uapi, f"pub(crate) struct {name} {{") + "\n"
    source += block(uapi, "pub(crate) struct ComputeEntryContext<'a> {") + "\n"
    for marker in (
        "pub(crate) fn compute_resume_checkpoints(",
        "pub(crate) fn build_compute_descriptor(",
        "impl TranslatedComputeCommand {",
    ):
        source += block(uapi, marker) + "\n"
    return source + "}\n" + HARNESS


HARNESS = r'''
use g17_compute::*;
use g17_submission::*;
use uapi::{ComputeEntryContext, TranslatedComputeCommand, UapiTranslateError};

const DESCRIPTOR_LOW: u64 = 0x70_0034_0000;
const DESCRIPTOR_HIGH: u64 = 0xffff_fc20_c035_8000;

fn command(layout: u64) -> TranslatedComputeCommand {
    TranslatedComputeCommand {
        compute_sequence: 0,
        render_wait_through: None,
        render_wait_last: None,
        usc_exec_base: 0x1_0000_0000,
        control_stream_base: 0x100_0410_0000,
        control_stream_end: 0x100_0410_0030,
        control_stream_terminator: 0x100_0410_002c,
        sampler_heap: 0, sampler_count: 0,
        attachments: uapi::UapiAttachmentList {
            entries: [uapi::UapiAttachment { address: 0, size: 0 }; uapi::UAPI_MAX_ATTACHMENTS],
            count: 0,
        },
        g17p_add3_buffers: None,
        timestamps: uapi::UapiTimestamps {
            start: uapi::UapiTimestamp { handle: 0, offset: 0 },
            end: uapi::UapiTimestamp { handle: 0, offset: 0 },
        },
        scs_layout: layout,
        g18_texture_state: false,
        g18_resource_descriptor: None,
    }
}

fn internal() -> uapi::ComputeInternalState {
    uapi::ComputeInternalState {
        preempt_base: 0x100_0404_0000,
        dispatch_identity: 1, context_id: 3, work_ordinal: 7,
        robustness: 0x100_0430_0000, operand_state_base: 0x70_0022_8000,
        usc_flist_id: 2,
    }
}

fn objects() -> ComputeDescriptorObjects {
    ComputeDescriptorObjects {
        scheduler_record: 0xffff_fc20_c000_0300,
        descriptor_low_alias: DESCRIPTOR_LOW,
        dispatch_a: 0x70_0035_0000, dispatch_b: 0x70_0035_4000,
        status_a: 0x70_0036_0000, status_b: 0x70_0036_4000,
        shared_control: 0x70_0037_0000, usage_record: 0x70_0038_0000,
    }
}

fn metadata() -> ComputeDescriptorMetadata {
    ComputeDescriptorMetadata {
        submit_sequence: 1, context_id: 3, grid_index: 0, work_ordinal: 7,
        queue_submission: 1, queue_ordinal: 0, submission_index: 1,
        support_control: 0, support_flags: 0,
    }
}

fn descriptor(command: &TranslatedComputeCommand, raw: &mut [u8])
    -> Result<[u16; 4], UapiTranslateError>
{
    uapi::build_compute_descriptor(command, internal(), objects(), metadata(),
        uapi::ComputeTimestampAddresses { start: 0, end: 0 }, raw)
}

fn prepare(command: &TranslatedComputeCommand, raw: &mut [u8],
           queue: G17PClKickProducerState, ordinal: u64)
    -> Result<PreparedG17PClKickEntry, UapiTranslateError>
{
    let checkpoints = uapi::compute_resume_checkpoints(command, internal())?;
    assert_eq!(descriptor(command, raw)?, checkpoints);
    let state = prepare_g17p_compute_cl_command_state(
        DESCRIPTOR_LOW, ordinal, u8::try_from(checkpoints[0]).unwrap()).unwrap();
    let operands = command.entry_operands(ComputeEntryContext {
        descriptor_flag_4c: false, converted_command_timestamp: 1,
        barriers: &[], mcache: None,
        payload: [DESCRIPTOR_HIGH, objects().scheduler_record],
        event_mask: state.event_mask, rce_kind: 0,
        rce_bindings: state.rce_bindings, auxiliary: 0x003f_ffff_ffff_ffff,
    });
    Ok(prepare_g17p_cl_kick_entry(queue, operands).unwrap())
}

fn word(raw: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(raw[offset..offset + 8].try_into().unwrap())
}

fn reused_descriptor_transitions() {
    let queue = T8140_G17P_HAL200_SKSM_QUEUE_GEOMETRY
        .initial_producer_state(4, 2, G17PClQosConfig {
            queue_byte_32: 1, class_40: 8, word_48: 0xffff,
        }).unwrap();
    let mut raw = [0xa5; COMPUTE_DESCRIPTOR_SIZE];
    let mut publications = 0;
    // Reuse storage after assumed retirement. This deliberately models no
    // scheduling: the same valid producer fixture isolates descriptor reuse.
    for layout in [1u64 << 48, u64::MAX, 0, 1u64 << 32] {
        let previous = raw;
        if layout == u64::MAX {
            assert!(prepare(&command(layout), &mut raw, queue, 0).is_err());
            assert_eq!(raw, previous, "rejected layout changed a prior descriptor");
            assert!(descriptor(&command(layout), &mut raw).is_err());
            assert_eq!(raw, previous, "descriptor rejection changed prior state");
            assert_eq!(publications, 1);
            continue;
        }
        let entry = prepare(&command(layout), &mut raw, queue, 0).unwrap();
        publications += 1;
        let enabled = layout != 0;
        assert_eq!((word(&entry.entry, 0) >> 61) & 1, u64::from(enabled));
        assert_eq!((word(&entry.entry, 0x150) >> 43) & 0x7f,
                   if enabled { 39 } else { 38 });
        let first = u32::from_le_bytes(raw[0x40..0x44].try_into().unwrap());
        assert_eq!(first == 0x17d9, enabled, "stale prefix after layout change");
        assert_eq!(&raw[0x22..0x26], &[0; 4], "stale firmware resume marker");
        // Model only the captured B1 marker write, not execution of firmware.
        // The next successful construction must clear it; rejection must not.
        raw[0x22..0x26].copy_from_slice(&1u32.to_le_bytes());
    }
    assert_eq!(publications, 3);
}

fn main() {
    // Expected behavior is explicit, not computed with the predicate under test.
    let cases = [
        ("zero", 0, Some(false)),
        ("lower32-only", 0xffff_ffff, Some(false)),
        ("user-only", 1u64 << 32, Some(true)),
        ("driver-only", 1u64 << 48, Some(true)),
        ("user-with-dimensions", (1u64 << 32) | 0x0200_0010, Some(true)),
        ("driver-with-dimensions", (0x48u64 << 48) | 0x6000_0070, Some(true)),
        ("mixed-payloads", (0x48u64 << 48) | (0x20u64 << 32) | 0x6000_0070, Some(true)),
        ("overflow", u64::MAX, None),
    ];
    let mut accepted = 0;
    let mut rejected = 0;
    for (name, layout, enabled) in cases {
        for qid in [2, 4] {
            for ordinal in [0, 7] {
                let command = command(layout);
                let mut queue = T8140_G17P_HAL200_SKSM_QUEUE_GEOMETRY
                    .initial_producer_state(qid, 2, G17PClQosConfig {
                        queue_byte_32: 1, class_40: 8, word_48: 0xffff,
                    }).unwrap();
                if ordinal != 0 {
                    queue.current_timestamp = 255;
                    queue.previous_valid = true;
                    queue.previous_timestamp = 254;
                }
                let before = queue;
                let mut raw = [0xa5; COMPUTE_DESCRIPTOR_SIZE];
                let mut published = None;
                match prepare(&command, &mut raw, queue, ordinal) {
                    Ok(entry) => published = Some(entry),
                    Err(error) => {
                        assert!(enabled.is_none(), "unexpected rejection: {name}");
                        assert_eq!(error, UapiTranslateError::ComputeBuild(
                            ComputeBuildError::ScsRequestOutOfRange));
                    }
                }
                assert_eq!(queue, before, "pure preparation mutated producer: {name}");
                let Some(enabled) = enabled else {
                    assert!(published.is_none(), "invalid entry reached publication: {name}");
                    assert!(raw.iter().all(|&b| b == 0xa5));
                    // Also exercise the descriptor entry point independently of
                    // the early checkpoint gate; it must reject before writing.
                    assert_eq!(descriptor(&command, &mut raw), Err(
                        UapiTranslateError::ComputeBuild(ComputeBuildError::ScsRequestOutOfRange)));
                    assert!(raw.iter().all(|&b| b == 0xa5));
                    rejected += 1;
                    continue;
                };
                let entry = published.expect("valid entry must reach publication");
                assert_eq!(command.scs_enabled(), enabled, "{name}");
                assert_eq!(scs_pages_for_layout(layout).unwrap() != 0, enabled, "{name}");
                let expected = if enabled { [39, 40, 40, 41] } else { [38, 39, 39, 40] };
                for (i, checkpoint) in expected.into_iter().enumerate() {
                    assert_eq!(u16::from_le_bytes(raw[0x18 + 2*i..0x1a + 2*i]
                        .try_into().unwrap()), checkpoint, "checkpoint {i}: {name}");
                }
                let count = if enabled { 41 } else { 40 };
                // Inspect the serialized register list, not just the temporary
                // builder. No other register may masquerade as the prefix.
                let names: Vec<u32> = (0..count).map(|i| {
                    let start = 0x40 + i * 12;
                    u32::from_le_bytes(raw[start..start + 4].try_into().unwrap())
                }).collect();
                assert_eq!(names.iter().filter(|&&n| n == 0x17d9).count(),
                           usize::from(enabled), "prefix count: {name}");
                if enabled {
                    assert_eq!(names[0], 0x17d9, "prefix order: {name}");
                    assert_eq!(word(&raw, 0x44), 1, "prefix value: {name}");
                }
                assert_eq!((word(&entry.entry, 0x150) >> 43) & 0x7f,
                           u64::from(expected[0]), "initial RCE tag: {name}");
                assert_eq!((word(&entry.entry, 0) >> 61) & 1,
                           u64::from(enabled), "kick header bit61: {name}");
                accepted += 1;
            }
        }
    }
    assert_eq!((accepted, rejected), (28, 4));
    reused_descriptor_transitions();
    println!("PASS: 8 layouts x 2 QIDs x 2 submission states; {accepted} complete encodings, {rejected} overflow rejections before publication");
    println!("PASS: reused descriptor on -> rejected overflow -> off -> on; no stale prefix, header bit, RCE tag, or resume marker");
}
'''


def main():
    with tempfile.TemporaryDirectory(prefix="g17-scs-encoding-") as directory:
        source = Path(directory) / "check.rs"
        executable = Path(directory) / "check"
        source.write_text(rust_source())
        subprocess.run(
            ["rustc", "--edition=2021", str(source), "-o", str(executable)], check=True
        )
        subprocess.run([str(executable)], check=True)


if __name__ == "__main__":
    main()
