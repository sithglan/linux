#!/usr/bin/env python3
"""Execute the production QID table and manager allocation/rollback contract."""
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
manager = (root / "g17_manager.rs").read_text()


def item(marker, text=manager):
    start = text.index(marker)
    pos = text.index("{", start) + 1
    depth = 1
    while depth:
        depth += (text[pos] == "{") - (text[pos] == "}")
        pos += 1
    return text[start:pos]


first = item("fn prepare_compute_firmware_install(")
repeat = item("fn submit_translated_repeat_channel(")
prepare = item("pub(crate) fn prepare_g17p_compute_queue(")
stop = item("pub(crate) fn release_runtime_objects_after_processor_stop(")
assert "self.channel_ids.matches(queue.channel_lease)" in first
assert repeat.index("self.channel_ids.matches(queue.channel_lease)") < repeat.index(
    "queue.compute.write_g17p_add3_resource_table"
)
assert "self.channel_ids.cancel_unpublished(channel_lease)" in prepare
assert "self.channel_ids = g17p_initial_channel_table()" in stop
assert "self.next_channel_identity =" not in stop
assert manager.index("self.channel_ids.publish(queue.channel_lease)") < manager.index(
    ".publish_compute_channel("
)

completion = (root / 'g17_completion.rs').read_text()
runtime = (root / 'g17_live_boot.rs').read_text()
source = f'''#[path = "{root / 'g17_channel.rs'}"] mod g17_channel;
#[derive(Debug, PartialEq)] enum Error {{ Overflow, NoSpace, Invalid, Io }}
const EOVERFLOW: Error = Error::Overflow;
const ENOSPC: Error = Error::NoSpace;
const EINVAL: Error = Error::Invalid;
const EIO: Error = Error::Io;
const ENODEV: Error = Error::Invalid;
type Result<T> = core::result::Result<T, Error>;
const G17P_COMPUTE_QUEUE_ID_BASE: u8 = 4;
{item('fn g17p_reserve_bootstrap_channels(')}
{item('fn g17p_initial_channel_table()')}
mod g17_resources {{
    #[derive(Debug, PartialEq)] pub struct G17PRenderQueuePair {{pub slot:u8,pub tiling:u16,pub fragment:u16}}
    impl G17PRenderQueuePair {{
        pub fn for_slot(slot:u8)->Option<Self>{{(slot<2).then_some(Self{{slot,tiling:u16::from(slot)*2,fragment:u16::from(slot)*2+1}})}}
        pub fn with_queue_ids(slot:u8,tiling:u16,fragment:u16)->Option<Self>{{Some(Self{{slot,tiling,fragment}})}}
    }}
}}
struct Manager {{ channel_ids: g17_channel::ChannelTable, next_channel_identity: u64,
    render_channel_leases:[Option<[g17_channel::Lease;2]>;64] }}
impl Manager {{ {item('fn allocate_compute_queue_id(')}
    {item('pub(crate) fn render_queue_pair(')}
    {item('fn retained_render_queue_pair(')}
    {item('pub(crate) fn render_slot_for_qid(')}
}}
fn manager() -> Manager {{
    Manager {{ channel_ids: g17p_initial_channel_table(), next_channel_identity: 3,
        render_channel_leases:[None;64] }}
}}
#[derive(Default)]
{item('pub(crate) struct RenderFenceState {', completion)}
{item('impl RenderFenceState {', completion)}
struct Runtime {{manager:Option<Manager>,render_fence_state:RenderFenceState,settled:Vec<(u8,u8)>}}
impl Runtime {{
    {item('fn settle_render_blamed_by_recovery(', runtime)}
    fn settle_failed_deferred_render(&mut self,slot:u8,_submission:Option<u64>,stamp:Option<u8>,_error:Error) {{
        assert!(self.render_fence_state.settle(slot));self.settled.push((slot,stamp.unwrap()));
    }}
}}
#[test] fn all_render_owners_settle_once_and_faults_follow_actual_leases() {{
    let mut state=RenderFenceState::default();
    for slot in 0..64 {{assert!(state.arm(slot));assert!(!state.arm(slot));}}
    assert!(!state.arm(64));
    for slot in (0..64).rev() {{assert!(state.pending(slot));assert!(state.settle(slot));assert!(!state.settle(slot));}}
    let mut m=manager();let cl=m.allocate_compute_queue_id().unwrap();
    let pair=m.render_queue_pair(63).unwrap();
    let mut r=Runtime{{manager:Some(m),render_fence_state:state,settled:Vec::new()}};
    assert!(r.render_fence_state.arm(63));
    r.settle_render_blamed_by_recovery(cl.qid,EIO);
    assert!(r.settled.is_empty());
    r.settle_render_blamed_by_recovery(pair.fragment as u8,EIO);
    r.settle_render_blamed_by_recovery(pair.tiling as u8,EIO);
    assert_eq!(r.settled,vec![(63,pair.fragment as u8)]);
}}
#[test] fn mixed_engines_share_ids_without_slot_arithmetic() {{
    let mut m=manager();
    let cl=m.allocate_compute_queue_id().unwrap();
    let a=m.render_queue_pair(33).unwrap();
    let b=m.render_queue_pair(63).unwrap();
    assert_eq!((a.tiling,a.fragment),(2,3));
    assert_eq!((b.tiling,b.fragment),(5,6));
    assert_eq!(m.render_queue_pair(33).unwrap(),a);
    assert_eq!(m.render_slot_for_qid(cl.qid),None);
    for qid in [a.tiling,a.fragment] {{assert_eq!(m.render_slot_for_qid(qid as u8),Some(33));}}
    for qid in [b.tiling,b.fragment] {{assert_eq!(m.render_slot_for_qid(qid as u8),Some(63));}}
    let first=m.render_channel_leases[33].unwrap()[0];
    assert!(m.channel_ids.cancel_unpublished(first));
    assert_eq!(m.render_slot_for_qid(first.qid),None);
    assert_eq!(m.render_queue_pair(33),Err(EIO));
}}
#[test] fn partial_render_allocation_returns_the_unpublished_qid() {{
    let mut m=manager();
    for _ in 0..110 {{m.allocate_compute_queue_id().unwrap();}}
    for slot in 1..8 {{m.render_queue_pair(slot).unwrap();}}
    m.channel_ids.acquire(g17_channel::ChannelId(1_000_000), g17_channel::Engine::Tiling, None).unwrap();
    assert_eq!(m.channel_ids.free(), 1);
    assert_eq!(m.render_queue_pair(8),Err(ENOSPC));
    assert!(m.render_channel_leases[8].is_none());
    assert_eq!(m.channel_ids.free(), 1);
}}
#[test] fn compute_exhaustion_leaves_render_pairs_servable() {{
    let mut m=manager();
    for _ in 0..110 {{m.allocate_compute_queue_id().unwrap();}}
    let next = m.next_channel_identity;
    assert_eq!(m.allocate_compute_queue_id(),Err(ENOSPC));
    assert_eq!(m.next_channel_identity, next);
    assert_eq!(m.channel_ids.free(), g17_channel::COMPUTE_RENDER_RESERVE);
    for slot in 1..=8 {{m.render_queue_pair(slot).unwrap();}}
    assert_eq!(m.channel_ids.free(), 0);
    assert_eq!(m.render_queue_pair(9),Err(ENOSPC));
}}
#[test] fn rollback_reuses_qid_but_not_graph_identity() {{
    let mut m = manager();
    let old = m.allocate_compute_queue_id().unwrap();
    assert_eq!(old.qid, 4);
    assert!(m.channel_ids.cancel_unpublished(old));
    let new = m.allocate_compute_queue_id().unwrap();
    assert_eq!(new.qid, old.qid);
    assert_ne!(new, old);
    assert!(!m.channel_ids.matches(old));
    assert!(!m.channel_ids.cancel_unpublished(old));
    assert!(m.channel_ids.publish(new));
    assert!(!m.channel_ids.cancel_unpublished(new));
}}
#[test] fn stopped_session_cannot_accept_old_packet_lease() {{
    let mut m = manager();
    let old = m.allocate_compute_queue_id().unwrap();
    assert!(m.channel_ids.publish(old));
    m.channel_ids = g17p_initial_channel_table();
    let new = m.allocate_compute_queue_id().unwrap();
    assert_eq!(old.qid, new.qid);
    assert!(!m.channel_ids.matches(old));
    assert!(m.channel_ids.matches(new));
}}
#[test] fn exhaustion_and_identity_overflow_do_not_change_owner() {{
    let mut m = manager();
    let mut seen = [false; 128];
    for index in 0..126 - g17_channel::COMPUTE_RENDER_RESERVE {{
        let lease = m.allocate_compute_queue_id().unwrap();
        if index == 0 {{ assert_eq!(lease.qid, 4); }}
        assert!(lease.qid >= 2);
        assert!(!seen[lease.qid as usize]);
        seen[lease.qid as usize] = true;
        assert!(m.channel_ids.publish(lease));
    }}
    assert_eq!(seen[2..].iter().filter(|seen| **seen).count(), 126 - g17_channel::COMPUTE_RENDER_RESERVE);
    let next = m.next_channel_identity;
    assert_eq!(m.allocate_compute_queue_id(), Err(ENOSPC));
    assert_eq!(m.next_channel_identity, next);
    m.channel_ids = g17p_initial_channel_table();
    m.next_channel_identity = u64::MAX;
    assert_eq!(m.allocate_compute_queue_id(), Err(EOVERFLOW));
    m.next_channel_identity = next;
    assert_eq!(m.allocate_compute_queue_id().unwrap().qid, 4);
}}
'''
with tempfile.TemporaryDirectory(prefix="g17-channel-table-") as tmp:
    src = Path(tmp) / "test.rs"
    binary = Path(tmp) / "test"
    src.write_text(source)
    subprocess.run(["rustc", "--edition=2021", "--test", str(src), "-o", str(binary)], check=True)
    subprocess.run([str(binary)], check=True)
