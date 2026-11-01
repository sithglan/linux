#!/usr/bin/env python3
"""Exercise the real AddKicks encoder and dependency decoder across stamp epochs."""
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
source = (root/'g17_submission.rs').read_text()


def block(name):
    match = re.search(r'^(?:pub\(crate\) )?(?:(?:const )?fn|enum|struct) '
                      + name + r'\b', source, re.M)
    assert match, name
    end = source.index('{', match.end()) + 1
    depth = 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[match.start():end]


def constant(name):
    match = re.search(r'^pub\(crate\) const ' + name + r': .*?;', source, re.M | re.S)
    assert match, name
    return match.group()


program = '\n'.join([
    '#![allow(dead_code)]',
    *[constant(name) for name in [
        'KICK_TS_BITS', 'KICK_TS_MASK', 'KICK_QID_BITS', 'KICK_QID_MASK',
        'KICK_QID_SHIFT', 'KICK_STAMP_SHIFT', 'KICK_STAMP_INDEX_MASK',
        'G17P_COLD_OPENING_EVENT_SIZE', 'G17P_NATIVE_RENDER_DOORBELL_PRIORITY']],
    *['#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n' + block(name) for name in [
        'SubmissionError', 'KickTimestamp', 'G17PClBarrierDependency',
        'G17PColdOpeningStage']],
    # The decoder reads only the seed and stamp count from the producer. No
    # firmware behavior is mocked; these are its two explicit test inputs.
    '''
#[derive(Copy, Clone)]
struct Geometry(u16);
impl Geometry { fn stamp_count(self) -> u16 { self.0 } }
#[derive(Copy, Clone)]
struct G17PClKickProducerState { base_timestamp: u64, geometry: Geometry }
''',
    *[block(name) for name in [
        'put_u32_le', 'put_u64_le', 'apply_g17p_cold_opening_event',
        'apply_g17p_retained_event_with_stamp', 'g17p_render_barrier_from_timestamp',
        'encode_kick_timestamp', 'encode_g17p_cl_dependency',
        'encode_g17p_cl_barrier_dependency']],
    r'''
#[test]
fn native_u64_addkicks_field_preserves_epoch_priority_and_data_master() {
    for (stage, qid, dm) in [(G17PColdOpeningStage::Tiling, 17, 0),
                              (G17PColdOpeningStage::Fragment, 18, 1)] {
        let mut raw = [0xa5; G17P_COLD_OPENING_EVENT_SIZE];
        apply_g17p_retained_event_with_stamp(stage, 0x123456789a, qid, &mut raw).unwrap();
        assert_eq!(&raw[0..8], &[14, 0, 0, 0, qid as u8, 0, 1, 0]);
        assert_eq!(raw[8], G17P_NATIVE_RENDER_DOORBELL_PRIORITY);
        assert_eq!(&raw[9..17], &[0x9a, 0x78, 0x56, 0x34, 0x12, 0, 0, 0]);
        assert_eq!(raw[17], dm);
        assert!(raw[18..].iter().all(|byte| *byte == 0));
    }
}

#[test]
fn dependencies_retain_all_forty_bits_through_both_boundary_wraps() {
    let queue = G17PClKickProducerState { base_timestamp: 1, geometry: Geometry(256) };
    for timestamp in [0, 1, 255, 256, 0xfffffe, 0xffffff, 0x1000000,
                      0x1000001, 0xffffffff, 0x100000000, 0xffffffffff] {
        for qid in [0, 1, 2, 3, 63, 64, 127] {
            let barrier = g17p_render_barrier_from_timestamp(timestamp, qid).unwrap();
            let wire = encode_g17p_cl_barrier_dependency(queue, barrier).unwrap();
            assert_eq!(wire, timestamp | ((qid as u64) << 40));
            let mut raw = [0xa5; G17P_COLD_OPENING_EVENT_SIZE];
            apply_g17p_retained_event_with_stamp(G17PColdOpeningStage::Tiling,
                timestamp, qid as u16, &mut raw).unwrap();
            assert_eq!(u64::from_le_bytes(raw[9..17].try_into().unwrap()), timestamp);
        }
    }
}

#[test]
fn missing_epoch_is_observably_the_wrong_dependency() {
    let queue = G17PClKickProducerState { base_timestamp: 1, geometry: Geometry(256) };
    let timestamp = 0x1000000;
    let correct = g17p_render_barrier_from_timestamp(timestamp, 0).unwrap();
    let wrong = G17PClBarrierDependency { shared_stamp: 0, ..correct };
    assert_eq!(encode_g17p_cl_barrier_dependency(queue, wrong).unwrap(), 0);
    assert_eq!(encode_g17p_cl_barrier_dependency(queue, correct).unwrap(), timestamp);
}

#[test]
fn invalid_timestamp_does_not_mutate_addkicks() {
    for timestamp in [1u64 << 40, u64::MAX] {
        let mut raw = [0xa5; G17P_COLD_OPENING_EVENT_SIZE];
        let before = raw;
        assert_eq!(apply_g17p_retained_event_with_stamp(G17PColdOpeningStage::Tiling,
            timestamp, 0, &mut raw), Err(SubmissionError::KickTimestampOutOfRange));
        assert_eq!(raw, before);
        assert!(g17p_render_barrier_from_timestamp(timestamp, 0).is_err());
    }
    assert!(g17p_render_barrier_from_timestamp(1, 128).is_err());
}
'''])

with tempfile.TemporaryDirectory(prefix='g17-render-stamp-epoch-') as directory:
    binary = Path(directory)/'tests'
    subprocess.run(['rustc', '--edition=2021', '--test', '-', '-o', str(binary)],
                   input=program, text=True, check=True)
    subprocess.run([str(binary)], check=True)
