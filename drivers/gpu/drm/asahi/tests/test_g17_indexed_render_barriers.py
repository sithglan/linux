#!/usr/bin/env python3
"""Execute actual lease allocation, prefix resolution and native token encoding."""
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
source = (root / 'g17_submission.rs').read_text()


def item(name):
    m = re.search(r'^(?:pub\(crate\) )?(?:(?:const )?fn|enum|struct) ' + name + r'\b', source, re.M)
    assert m, name
    end = source.index('{', m.end()) + 1
    depth = 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[m.start():end]


def constant(name):
    m = re.search(r'^(?:pub\(crate\) )?const ' + name + r': .*?;', source, re.M | re.S)
    assert m, name
    return m.group()


# Reuse the existing host scaffolding around the real registry/lease methods;
# stop before its compilation step, then add the actual token code below.
affinity = Path(__file__).with_name('test_g17_render_slot_affinity.py')
scope = {'__file__': str(affinity)}
exec(compile(affinity.read_text().split('code = preamble + tests')[0], str(affinity), 'exec'), scope)
codec = '\n'.join([
    'mod codec {',
    *[constant(n) for n in ['KICK_TS_BITS', 'KICK_TS_MASK', 'KICK_QID_BITS',
        'KICK_QID_MASK', 'KICK_QID_SHIFT', 'KICK_STAMP_SHIFT',
        'KICK_STAMP_INDEX_MASK', 'KICK_RESERVED_MASK', 'KICK_SELECTOR_BIT']],
    *['#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n' + item(n) for n in
      ['SubmissionError', 'KickTimestamp', 'SksmKickWord', 'G17PClBarrierDependency', 'G17PRenderOrder']],
    '''#[derive(Copy, Clone)] struct Geometry(u16);
       impl Geometry { fn stamp_count(self) -> u16 { self.0 } }
       #[derive(Copy, Clone)] struct G17PClKickProducerState { base_timestamp:u64, geometry:Geometry }''',
    *[item(n) for n in ['encode_kick_word', 'decode_kick_word', 'encode_kick_timestamp',
        'g17p_render_barrier_from_timestamp', 'g17p_render_order',
        'g17p_live_render_dependency', 'g17p_render_prefix_contains', 'encode_g17p_cl_dependency',
        'encode_g17p_cl_barrier_dependency']],
    r'''
fn native(order:G17PRenderOrder, sequence:u64, qid:u8, stamp:u64) -> Option<G17PClBarrierDependency> {
    g17p_live_render_dependency(order, sequence, 0, encode_kick_word(stamp,qid).unwrap()).unwrap()
}
fn encoded(dependency:G17PClBarrierDependency) -> u64 {
    // Deliberately unrelated consumer geometry/seed: native tokens must not
    // be rebased using the newly selected pair's history.
    encode_g17p_cl_barrier_dependency(G17PClKickProducerState {
        base_timestamp:0xabc0, geometry:Geometry(4),
    },dependency).unwrap()
}
#[test] fn alternate_pair_names_exact_predecessor_not_selected_pair_history() {
    let mut leases=super::G17PQueueLeaseState::default();
    leases.render_owner[4]=7; leases.render_owner[5]=7;
    assert_eq!(leases.acquire_render_slot(7),Some(4));
    // R1 remains live; real allocator permits R2 on the other pair.
    assert_eq!(leases.acquire_render_slot(7),Some(5));
    let r2=g17p_render_order(0,1,1).unwrap();
    let edge=native(r2,1,11,0x01000005).unwrap();
    assert_eq!((edge.event_word,edge.shared_stamp,edge.queue_id),(0x500,1,11));
    assert_eq!(encoded(edge),encode_kick_word(0x01000005,11).unwrap());
    assert_ne!(encoded(edge),encode_kick_word(0x23,13).unwrap());
}
#[test] fn none_keeps_independent_admission_and_never_reads_missing_token() {
    let independent=g17p_render_order(0,1,u16::MAX).unwrap();
    assert_eq!(g17p_live_render_dependency(independent,1,0,u64::MAX),Ok(None));
}
#[test] fn zero_snapshots_only_prior_ioctls() {
    let order=g17p_render_order(6,2,0).unwrap();
    assert_eq!((order.sequence,order.wait_through),(9,Some(6)));
    assert!(native(order,6,11,0x40).is_some());
    assert!(native(order,7,13,0x41).is_none());
}
#[test] fn positive_non_immediate_index_selects_prefix() {
    let order=g17p_render_order(100,4,2).unwrap();
    assert_eq!((order.sequence,order.wait_through),(105,Some(102)));
    assert!(native(order,101,11,1).is_some());
    assert!(native(order,102,13,2).is_some());
    assert!(native(order,103,13,3).is_none());
}
#[test] fn two_independent_prior_renders_both_belong_to_frontier() {
    let order=g17p_render_order(0,2,2).unwrap();
    let a=native(order,1,11,0x42).unwrap(); let b=native(order,2,13,0x23).unwrap();
    assert_eq!([encoded(a),encoded(b)],[encode_kick_word(0x42,11).unwrap(),encode_kick_word(0x23,13).unwrap()]);
    assert_eq!(g17p_live_render_dependency(order,1,1,u64::MAX),Ok(None));
    assert!(native(order,2,13,0x23).is_some());
}
#[test] fn long_independent_prefix_is_bounded_by_real_pair_retirement() {
    let mut leases=super::G17PQueueLeaseState::default();
    leases.render_owner[4]=7; leases.render_owner[5]=7;
    let mut live=Vec::new();
    let capacity=2*super::g17_resources::G17P_RENDER_MAX_IN_FLIGHT;
    for sequence in 1..=64 {
        if live.len()==capacity { let (slot,_)=live.remove(0); leases.release_render_slot(slot); }
        let slot=leases.acquire_render_slot(7).unwrap(); live.push((slot,sequence));
        assert!(live.len()<=capacity);
    }
    let order=g17p_render_order(0,64,64).unwrap();
    assert_eq!(live.len(),capacity);
    assert_eq!(live.iter().filter(|(slot,n)|native(order,*n,*slot+7,*n).is_some()).count(),capacity);
    // Full depth cannot create a third physical pair or overwrite a live slot.
    assert_eq!(leases.acquire_render_slot(7),None);
}
#[test] fn compute_prefix_covers_independent_renders_across_ioctl_zero_snapshot() {
    let outstanding=[(1,11),(2,13),(3,11)];
    let compute_zero=g17p_render_order(2,0,0).unwrap().wait_through;
    let selected=outstanding.iter().filter(|(n,_)|g17p_render_prefix_contains(compute_zero,*n)).copied().collect::<Vec<_>>();
    assert_eq!(selected,vec![(1,11),(2,13)]);
    assert!(!g17p_render_prefix_contains(None,1));
    let compute_positive=g17p_render_order(2,1,1).unwrap().wait_through;
    assert!(g17p_render_prefix_contains(compute_positive,3));
}
#[test] fn native_stamp_crosses_event_epoch_and_keeps_high_bits() {
    let order=g17p_render_order(0,1,1).unwrap();
    for stamp in [0xffffff,0x1000000,0x1000001,KICK_TS_MASK] {
        assert_eq!(encoded(native(order,1,11,stamp).unwrap()),encode_kick_word(stamp,11).unwrap());
    }
}
#[test] fn errors_missing_tokens_future_indices_and_overflow_fail_closed() {
    let order=g17p_render_order(0,1,1).unwrap();
    assert_eq!(g17p_live_render_dependency(order,1,-5,0),Err(SubmissionError::RenderBarrierPredecessorFailed));
    assert_eq!(g17p_live_render_dependency(order,1,0,u64::MAX),Err(SubmissionError::RenderBarrierMissingToken));
    assert_eq!(g17p_render_order(0,1,2),Err(SubmissionError::RenderBarrierFuture));
    assert_eq!(g17p_render_order(u64::MAX,0,0),Err(SubmissionError::RenderBarrierSequenceOverflow));
    assert!(encode_kick_word(KICK_TS_MASK+1,11).is_err());
    assert!(encode_kick_word(1,128).is_err());
}
#[test] fn high_qid_predecessor_token_resolves_with_all_seven_qid_bits() {
    // Dead-pair DeviceLost: QID bit 6 is kick-word bit 46, the firmware's
    // mask-word selector. A predecessor on QID 64..127 must resolve exactly.
    let order=g17p_render_order(0,1,1).unwrap();
    for qid in [63u8,64,0x5d,0x7f] {
        let edge=native(order,1,qid,0x1234).unwrap();
        assert_eq!(edge.queue_id,qid);
        assert_eq!(encoded(edge),encode_kick_word(0x1234,qid).unwrap());
    }
    // Bits [63:47] stay unmodeled and refused.
    assert_eq!(g17p_live_render_dependency(order,1,0,1u64<<47),Err(SubmissionError::KickWordUnmodeledBits));
}
}''',
])

# Check that the tested native-token path is the production publication path.
live = (root / 'g17_live_boot.rs').read_text()
manager = (root / 'g17_manager.rs').read_text()
assert 'completion.predecessor_dependency(predecessor)?' in live
assert 'self.pending_render.iter().flatten(),' in live
assert 'self.pending_render_queued.iter().flatten(),' in live
assert 'get_mut(dependency_count).ok_or(EOVERFLOW)?' in live
assert '&render_dependencies[..dependency_count]' in live
assert 'command.render_barrier != g17_uapi::UAPI_BARRIER_NONE' not in manager
assert 'MAX_IN_FLIGHT_PER_QUEUE: u32 = 2' in (root / 'g17_queue_limits.rs').read_text()
with tempfile.TemporaryDirectory() as directory:
    source_file = Path(directory) / 'indexed.rs'
    binary = Path(directory) / 'indexed'
    source_file.write_text(scope['preamble'] + codec)
    subprocess.run(['rustc', '--edition=2021', '--test', str(source_file), '-o', str(binary)], check=True)
    subprocess.run([str(binary), '--test-threads=1'], check=True, timeout=60)
