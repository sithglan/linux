#!/usr/bin/env python3
"""v91 submit-path changes, tested on the actual source text.

* VALIDATE_PROOF: the production translators (`g17_uapi.rs`, compiled whole
  with its own #[path] dependencies, as under `cargo test`) run through the
  production `g17p_translate_with_proof`, `G17PProofSpace`,
  `G17PValidationProof` and `UserAddressSpace` extracted verbatim from
  `g17_drm.rs`, against a mock VM whose every mutation advances the
  mapping-validation epoch (the invariant the kernel VM provides). For random
  mapping histories and payloads, every result must equal the unchanged
  per-range translation; proof hits must take no lock; misses exactly one.
* WORD_STORES: `g17_store.rs` must write exactly the bytes the replaced
  `copy_from_slice` writes, at every alignment, in both knob states.
"""
from pathlib import Path
import re, subprocess, tempfile

ROOT = Path(__file__).resolve().parents[1]


def item(text, needle):
    start = text.index(needle)
    op = text.index('{', start)
    depth = 1
    i = op + 1
    while depth:
        depth += (text[i] == '{') - (text[i] == '}')
        i += 1
    return text[start:i]


def line(text, needle):
    start = text.index(needle)
    return text[start:text.index('\n', start)]


drm = (ROOT / 'g17_drm.rs').read_text()
hp = (ROOT / 'g17_host_path.rs').read_text()
uapi = (ROOT / 'g17_uapi.rs').read_text()

def const_item(text, name):
    m = re.search(r'^(?:pub\(crate\) )?const ' + name + r'\b[^;]*;', text, re.M | re.S)
    return m.group(0)


derive = '#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n'
uapi_items = [const_item(uapi, n) for n in [
    'UAPI_ATTACHMENT_SIZE', 'UAPI_MAX_ATTACHMENTS', 'UAPI_BARRIER_NONE', 'USC_WINDOW_SIZE',
    'G17P_ADD3_BUFFER_BYTES', 'RENDER_VERTEX_SCRATCH', 'RENDER_PROCESS_EMPTY_TILES',
    'RENDER_NO_VERTEX_CLUSTERING', 'RENDER_RSRC_SPEC_HI', 'RENDER_DBIAS_IS_INT', 'RENDER_FLAGS',
    'SAMPLER_SIZE', 'MAX_SAMPLERS', 'PAGE_SIZE']]
uapi_items += [derive + item(uapi, 'pub(crate) enum UapiTranslateError {'),
               item(uapi, 'impl From<RenderBuildError> for UapiTranslateError {'),
               item(uapi, 'impl From<g17_compute::ComputeBuildError> for UapiTranslateError {')]
for name in ['UapiAttachment', 'UapiAttachmentList', 'UapiHelperProgram', 'UapiProgram', 'UapiTimestamp',
             'UapiTimestamps', 'UapiZlsBuffer', 'UapiRenderCommand', 'UapiComputeCommand',
             'QueueUscWindow', 'RenderInternalState', 'TranslatedRenderCommand', 'TranslatedComputeCommand']:
    uapi_items.append(derive + item(uapi, 'pub(crate) struct ' + name + ' {'))
attach_impl = item(uapi, 'impl UapiAttachmentList {')
attach_impl = attach_impl[:attach_impl.index('    fn parse(')] + '}'
uapi_items += [attach_impl, derive + item(uapi, 'pub(crate) enum GpuAccess {'),
               item(uapi, 'pub(crate) trait GpuAddressSpace {'),
               item(uapi, 'fn require_range<'), item(uapi, 'impl QueueUscWindow {'),
               item(uapi, 'fn validate_sampler<'), item(uapi, 'fn validate_attachments<'),
               item(uapi, 'fn validate_zls<'), item(uapi, 'pub(crate) fn translate_render_command<'),
               item(uapi, 'fn g17p_bg_resource_spec('), item(uapi, 'pub(crate) fn translate_compute_command<')]
uapi_mini = '\n'.join(uapi_items)

proof = '\n'.join([
    line(drm, 'const G17P_PROOF_RANGES: usize'),
    item(drm, 'fn g17p_access_code('),
    '#[derive(Copy, Clone)]\n' + item(drm, 'pub(crate) struct G17PRangeRequests {'),
    item(drm, 'impl G17PRangeRequests {'),
    item(drm, 'pub(crate) struct G17PValidationProof {'),
    item(drm, 'impl G17PValidationProof {'),
    item(drm, 'enum G17PProofSpace<'),
    item(drm, "impl g17_uapi::GpuAddressSpace for G17PProofSpace<'_> {"),
    '#[derive(Copy, Clone, Default)]\n' + item(drm, 'pub(crate) struct G17PProofOutcome {'),
    item(drm, 'fn g17p_translate_with_proof<T>('),
    item(drm, "struct UserAddressSpace<'a> {"),
    item(drm, "impl g17_uapi::GpuAddressSpace for UserAddressSpace<'_> {"),
])
host_path = '\n'.join([
    'use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};',
    line(hp, 'pub(crate) const VALIDATE_PROOF: u32'),
    'pub(crate) static MASK: AtomicU32 = AtomicU32::new(VALIDATE_PROOF);',
    *(line(hp, f'pub(crate) static {n}: AtomicU64') for n in ['PROOF_HITS', 'PROOF_BATCHES', 'PROOF_FALLBACKS']),
    item(hp, 'pub(crate) fn enabled(bit: u32)'),
    item(hp, 'pub(crate) fn count(counter: &AtomicU64)'),
])

source = r'''
#![allow(dead_code, unused_imports, unused_variables, unused_mut)]
#[path = "@ROOT@/g17_config.rs"]
mod g17_config;
#[path = "@ROOT@/g17_render.rs"]
mod g17_render;
pub mod module_parameters { pub struct Param(u32); impl Param { pub fn value(&self) -> &u32 { &self.0 } } pub static g17p_frg_hwdsid_clear: Param = Param(0); }
pub mod g17_timing_buffer { pub const DIAG_FRG_222D_CLEAR: u32 = 1; pub fn diag_knob(_: u32) -> bool { false } }
pub mod g17_compute { #[derive(Debug, Copy, Clone, PartialEq, Eq)] pub enum ComputeBuildError { Unused } }
pub mod g17_submission { #[derive(Debug, Copy, Clone, PartialEq, Eq, Default)] pub struct G17PRenderOrder { pub sequence: u64, pub wait_through: Option<u64> } }
pub mod g17_uapi {
    use crate::{g17_compute, g17_render, g17_submission};
    use g17_render::{G17pRenderParameters, RenderBuildError};
    pub(crate) const COMPUTE_G17P_ADD3_PROOF: u32 = 1 << 0;
    @UAPI@
    pub(crate) fn g17_render_params() -> G17pRenderParameters { G17pRenderParameters::source_rsrc8() }
}
#[path = "@ROOT@/g17_store.rs"]
mod g17_store;
pub mod g17_host_path { @HOST_PATH@ }

pub mod mmu {
    use std::cell::{Cell, RefCell};
    /// Mock VM: byte-range mappings with read/write permission. Every mapping
    /// change advances the validation epoch exactly as the kernel VM does
    /// (writers bump it under the exec lock; `None` while one is in progress).
    pub struct Vm {
        pub maps: RefCell<Vec<(u64, u64, bool, bool)>>,
        pub epoch: Cell<Option<u64>>,
        pub locks: Cell<u32>,
        pub in_progress: Cell<bool>,
    }
    impl Vm {
        pub fn new() -> Self { Self { maps: RefCell::new(Vec::new()), epoch: Cell::new(Some(0)), locks: Cell::new(0), in_progress: Cell::new(false) } }
        pub fn map(&self, start: u64, end: u64, r: bool, w: bool) {
            self.maps.borrow_mut().push((start, end, r, w));
            self.epoch.set(self.epoch.get().map(|e| e + 1));
        }
        pub fn unmap_all(&self) {
            self.maps.borrow_mut().clear();
            self.epoch.set(self.epoch.get().map(|e| e + 1));
        }
        fn locked(&self, address: u64, size: u64, r: bool, w: bool) -> bool {
            let Some(end) = address.checked_add(size) else { return false };
            size != 0 && self.maps.borrow().iter().any(|&(s, e, mr, mw)|
                s <= address && end <= e && (!r || mr) && (!w || mw))
        }
        pub fn covers_range(&self, address: u64, size: u64, r: bool, w: bool) -> bool {
            self.locks.set(self.locks.get() + 1);
            self.locked(address, size, r, w)
        }
        pub fn covers_ranges_batch(&self, ranges: &[(u64, u64, u8)]) -> (bool, Option<u64>) {
            self.locks.set(self.locks.get() + 1);
            (ranges.iter().all(|&(a, s, access)| self.locked(a, s, access & 1 != 0, access & 2 != 0)), self.epoch.get())
        }
        pub fn mapping_validation_epoch(&self) -> Option<u64> {
            if self.in_progress.get() { None } else { self.epoch.get() }
        }
    }
}

use std::ops::Range;
@PROOF@

#[cfg(test)]
mod v91 {
    use super::*;
    use std::sync::atomic::Ordering;

    struct Rng(u64);
    impl Rng { fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 } }

    fn render_parts() -> (g17_uapi::UapiRenderCommand, g17_uapi::UapiAttachmentList, g17_uapi::UapiAttachmentList) {
        let program = |usc: u32| g17_uapi::UapiProgram { usc, resource_spec: 0 };
        let zls = g17_uapi::UapiZlsBuffer { base: 0, compression_base: 0, stride: 0, compression_stride: 0 };
        let helper = g17_uapi::UapiHelperProgram { binary: 0, config: 0, data: 0 };
        let ts = g17_uapi::UapiTimestamp { handle: 0, offset: 0 };
        let payload = g17_uapi::UapiRenderCommand {
            flags: 0, depth_dimensions: 0, vdm_base: 0x2_0000, vertex_helper: helper, fragment_helper: helper,
            scissor_base: 0x3_0000, depth_bias_base: 0x3_1000, occlusion_query_base: 0,
            depth: zls, stencil: zls, zls_control: 0, multisample_control: 0, sampler_heap: 0x4_0000,
            ppp_control: 0, width: 1024, height: 1024, layers: 1, sampler_count: 2,
            utile_width: 32, utile_height: 32, samples: 1, sample_size: 4,
            merge_upper_x: 0, merge_upper_y: 0,
            background: program(0x100), end_of_tile: program(0x200),
            partial_background: program(0x300), partial_end_of_tile: program(0x400),
            depth_clear: 0, stencil_clear: 0,
            vertex_timestamps: g17_uapi::UapiTimestamps { start: ts, end: ts },
            fragment_timestamps: g17_uapi::UapiTimestamps { start: ts, end: ts },
        };
        let mut fragment = g17_uapi::UapiAttachmentList::EMPTY;
        fragment.entries[0] = g17_uapi::UapiAttachment { address: 0x100_0000, size: 0x40_0000 };
        fragment.count = 1;
        (payload, g17_uapi::UapiAttachmentList::EMPTY, fragment)
    }
    fn compute_parts() -> (g17_uapi::UapiComputeCommand, g17_uapi::UapiAttachmentList) {
        let helper = g17_uapi::UapiHelperProgram { binary: 0, config: 0, data: 0 };
        let ts = g17_uapi::UapiTimestamp { handle: 0, offset: 0 };
        let mut attachments = g17_uapi::UapiAttachmentList::EMPTY;
        attachments.entries[0] = g17_uapi::UapiAttachment { address: 0x100_0000, size: 0x1000 };
        attachments.count = 1;
        (g17_uapi::UapiComputeCommand { flags: 0, sampler_count: 0, control_stream_base: 0x5_0000,
            control_stream_end: 0x5_0100, sampler_heap: 0, helper,
            timestamps: g17_uapi::UapiTimestamps { start: ts, end: ts }, scs_layout: 0 }, attachments)
    }
    fn window() -> g17_uapi::QueueUscWindow {
        g17_uapi::QueueUscWindow { base: 0x11_0000_0000, user_start: 0x4000, user_end: (1 << 39) - 0x8000 }
    }
    const USER: Range<u64> = 0x4000..((1 << 39) - 0x8000);
    fn internal() -> g17_uapi::RenderInternalState {
        g17_uapi::RenderInternalState { parameters: g17_uapi::g17_render_params() }
    }

    fn translate_render(space: &G17PProofSpace<'_>, p: g17_uapi::UapiRenderCommand,
        v: g17_uapi::UapiAttachmentList, f: g17_uapi::UapiAttachmentList)
        -> Result<g17_uapi::TranslatedRenderCommand, g17_uapi::UapiTranslateError> {
        g17_uapi::translate_render_command(p, v, f, window(), space, internal())
    }

    fn set_mask(on: bool) { g17_host_path::MASK.store(if on { g17_host_path::VALIDATE_PROOF } else { 0 }, Ordering::Relaxed); }

    #[test]
    fn alternating_buffers_reuse_both_proofs_until_mutation() {
        set_mask(true);
        let (mut p, v, f) = render_parts();
        let vm = mmu::Vm::new();
        vm.map(0x1_0000, 0x20_0000, true, true);
        vm.map(0x11_0000_0000, 0x12_0000_0000, true, true);
        vm.map(0x100_0000, 0x200_0000, true, true);
        let mut proof = G17PValidationProof::new();
        for i in 0..100 {
            p.scissor_base = 0x10_0000 + (i % 2) * 0x100;
            let mut o = G17PProofOutcome::default();
            assert!(g17p_translate_with_proof(&vm, USER, &mut proof, &mut o,
                |s| translate_render(s, p, v, f)).is_ok());
            assert_eq!(o.hit, i >= 2);
        }
        assert_eq!(vm.locks.get(), 2);
        vm.unmap_all();
        for i in 0..2 {
            p.scissor_base = 0x10_0000 + i * 0x100;
            let mut o = G17PProofOutcome::default();
            let got = g17p_translate_with_proof(&vm, USER, &mut proof, &mut o,
                |s| translate_render(s, p, v, f));
            assert_eq!(got, Err(g17_uapi::UapiTranslateError::AddressRange));
            assert!(!o.hit && o.fallback);
        }
    }

    #[test]
    fn accumulated_proofs_keep_exact_permissions_and_epoch() {
        let mut a = G17PRangeRequests::new(); a.push((0x4000, 0x1000, 1));
        let mut b = G17PRangeRequests::new(); b.push((0x8000, 0x1000, 2));
        let mut proof = G17PValidationProof::new();
        proof.record(Some(7), &a); proof.record(Some(7), &b);
        assert!(proof.covers(Some(7), a.as_slice()));
        assert!(proof.covers(Some(7), b.as_slice()));
        assert!(!proof.covers(Some(7), &[(0x4000, 0x1000, 3)]));
        assert!(!proof.covers(Some(7), &[(0x4000, 0x2000, 1)]));
        assert!(!proof.covers(None, a.as_slice()));
        proof.record(Some(8), &b);
        assert!(!proof.covers(Some(8), a.as_slice()));
        assert!(proof.covers(Some(8), b.as_slice()));
        proof.record(None, &a);
        assert!(!proof.covers(None, a.as_slice()));
        assert!(!proof.covers(Some(8), b.as_slice()));
    }

    #[test]
    fn accumulated_proof_overflow_replaces_with_complete_latest_set() {
        let mut a = G17PRangeRequests::new();
        for i in 0..G17P_PROOF_RANGES { a.push((0x4000 + i as u64 * 0x1000, 0x1000, 1)); }
        let mut b = G17PRangeRequests::new(); b.push((0x100000, 0x1000, 3));
        let mut proof = G17PValidationProof::new();
        proof.record(Some(7), &a); proof.record(Some(7), &a);
        assert!(proof.covers(Some(7), a.as_slice()));
        proof.record(Some(7), &b);
        assert!(proof.covers(Some(7), b.as_slice()));
        assert!(!proof.covers(Some(7), a.as_slice()));
        assert!(!proof.requests.overflow);
        a.push((0x200000, 0x1000, 1));
        proof.record(Some(7), &a);
        assert!(!proof.covers(Some(7), a.as_slice()));
        assert!(!proof.covers(Some(7), b.as_slice()));
    }

    #[test]
    fn proof_equals_per_range_translation_over_random_histories() {
        set_mask(true);
        let (base, v0, f0) = render_parts();
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let (mut hits, mut batches, mut fallbacks, mut oks, mut errs) = (0, 0, 0, 0, 0);
        for round in 0..3000 {
            let vm = mmu::Vm::new();
            // A random but mostly-covering history: the fixture's regions.
            let regions = [(0x1_0000u64, 0x20_0000u64), (0x11_0000_0000, 0x12_0000_0000), (0x100_0000, 0x200_0000)];
            for &(s, e) in &regions { if rng.next() % 16 != 0 { vm.map(s, e, true, rng.next() % 8 != 0); } }
            let mut proof = G17PValidationProof::new();
            for step in 0..6 {
                let mut p = base;
                let (mut v, mut f) = (v0, f0);
                match rng.next() % 6 {
                    0 => p.scissor_base = 0x10_0000 + (rng.next() % 64) * 8,
                    1 => p.vdm_base = 0x1_0000 + (rng.next() % 0x80000) * 4,
                    2 => { f.entries[0] = g17_uapi::UapiAttachment { address: 0x100_0000 + (rng.next() % 16) * 0x10000, size: 0x10000 }; if f.count == 0 { f.count = 1; } }
                    3 => p.occlusion_query_base = if rng.next() % 2 == 0 { 0 } else { 0x1_8000 },
                    4 => p.depth.base = if rng.next() % 2 == 0 { 0 } else { 0x1_4000 },
                    _ => {}
                }
                if rng.next() % 5 == 0 { vm.map(0x300_0000, 0x400_0000, true, true); }
                if rng.next() % 11 == 0 { vm.unmap_all(); for &(s, e) in &regions { vm.map(s, e, true, true); } }
                vm.in_progress.set(rng.next() % 9 == 0);
                let reference = {
                    let before = vm.locks.get();
                    let r = translate_render(&G17PProofSpace::Checked(UserAddressSpace { vm: &vm, range: USER }), p, v, f);
                    vm.locks.set(before); // reference run is not counted
                    r
                };
                let mut outcome = G17PProofOutcome::default();
                let before = vm.locks.get();
                let got = g17p_translate_with_proof(&vm, USER, &mut proof, &mut outcome,
                    |space| translate_render(space, p, v, f));
                let locks = vm.locks.get() - before;
                assert_eq!(got, reference, "round {} step {}", round, step);
                if outcome.hit { hits += 1; assert_eq!(locks, 0); assert!(!vm.in_progress.get()); }
                if outcome.batched && !outcome.fallback { batches += 1; assert_eq!(locks, 1); }
                if outcome.fallback { fallbacks += 1; }
                if got.is_ok() { oks += 1 } else { errs += 1 }
            }
        }
        println!("hits={} batches={} fallbacks={} ok={} err={}", hits, batches, fallbacks, oks, errs);
        assert!(hits > 1000 && batches > 1000 && fallbacks > 100 && errs > 100 && oks > 5000);
    }

    #[test]
    fn steady_resubmission_takes_no_lock_until_the_vm_changes() {
        set_mask(true);
        let (p, v, f) = render_parts();
        let vm = mmu::Vm::new();
        vm.map(0x1_0000, 0x20_0000, true, true);
        vm.map(0x11_0000_0000, 0x12_0000_0000, true, true);
        vm.map(0x100_0000, 0x200_0000, true, true);
        let mut proof = G17PValidationProof::new();
        let mut outcome = G17PProofOutcome::default();
        let first = g17p_translate_with_proof(&vm, USER, &mut proof, &mut outcome, |s| translate_render(s, p, v, f));
        assert!(first.is_ok() && outcome.batched && !outcome.hit);
        assert_eq!(vm.locks.get(), 1);
        let requests = outcome.requests;
        assert!(requests >= 6, "requests {}", requests);
        for _ in 0..100 {
            let mut o = G17PProofOutcome::default();
            assert_eq!(g17p_translate_with_proof(&vm, USER, &mut proof, &mut o, |s| translate_render(s, p, v, f)), first);
            assert!(o.hit);
        }
        assert_eq!(vm.locks.get(), 1);
        // The v90 path takes one exec lock per validated range.
        let before = vm.locks.get();
        let _ = translate_render(&G17PProofSpace::Checked(UserAddressSpace { vm: &vm, range: USER }), p, v, f);
        assert_eq!(vm.locks.get() - before, requests as u32);
        // Any VM change forces a fresh proof; removal is reported exactly.
        vm.map(0x500_0000, 0x600_0000, true, true);
        let mut o = G17PProofOutcome::default();
        assert!(g17p_translate_with_proof(&vm, USER, &mut proof, &mut o, |s| translate_render(s, p, v, f)).is_ok());
        assert!(o.batched && !o.hit);
        vm.unmap_all();
        let mut o = G17PProofOutcome::default();
        let got = g17p_translate_with_proof(&vm, USER, &mut proof, &mut o, |s| translate_render(s, p, v, f));
        assert_eq!(got, Err(g17_uapi::UapiTranslateError::AddressRange));
        assert!(o.fallback);
        // A mutation in progress never reuses a proof.
        vm.map(0x1_0000, 0x20_0000, true, true); vm.map(0x11_0000_0000, 0x12_0000_0000, true, true);
        vm.map(0x100_0000, 0x200_0000, true, true);
        let mut o = G17PProofOutcome::default();
        g17p_translate_with_proof(&vm, USER, &mut proof, &mut o, |s| translate_render(s, p, v, f)).unwrap();
        vm.in_progress.set(true);
        let mut o = G17PProofOutcome::default();
        g17p_translate_with_proof(&vm, USER, &mut proof, &mut o, |s| translate_render(s, p, v, f)).unwrap();
        assert!(!o.hit && o.batched);
    }

    #[test]
    fn knob_off_is_the_per_range_path_and_compute_is_covered() {
        let (p, v, f) = render_parts();
        let vm = mmu::Vm::new();
        vm.map(0x1_0000, 0x20_0000, true, true);
        vm.map(0x11_0000_0000, 0x12_0000_0000, true, true);
        vm.map(0x100_0000, 0x200_0000, true, true);
        set_mask(false);
        let mut proof = G17PValidationProof::new();
        let mut o = G17PProofOutcome::default();
        let r = g17p_translate_with_proof(&vm, USER, &mut proof, &mut o, |s| translate_render(s, p, v, f));
        assert!(r.is_ok() && !o.hit && !o.batched && !o.fallback && vm.locks.get() > 1);
        set_mask(true);
        let (cp, ca) = compute_parts();
        let reference = g17_uapi::translate_compute_command(cp, ca, window(),
            &G17PProofSpace::Checked(UserAddressSpace { vm: &vm, range: USER }));
        let mut proof = G17PValidationProof::new();
        for i in 0..3 {
            let mut o = G17PProofOutcome::default();
            let got = g17p_translate_with_proof(&vm, USER, &mut proof, &mut o,
                |s| g17_uapi::translate_compute_command(cp, ca, window(), s));
            assert_eq!(got, reference);
            assert_eq!(o.hit, i > 0 && reference.is_ok());
        }
    }

    #[test]
    fn recorder_static_half_matches_user_address_space() {
        let vm = mmu::Vm::new();
        vm.map(0, u64::MAX, true, true);
        let mut rng = Rng(12345);
        let record = G17PProofSpace::Record { range: USER, requests: core::cell::RefCell::new(G17PRangeRequests::new()) };
        let checked = UserAddressSpace { vm: &vm, range: USER };
        use g17_uapi::GpuAddressSpace;
        for _ in 0..200_000 {
            let address = match rng.next() % 4 { 0 => rng.next(), 1 => USER.end - (rng.next() % 64), 2 => USER.start - 8 + rng.next() % 16, _ => rng.next() % (1 << 40) };
            let size = match rng.next() % 3 { 0 => 0, 1 => rng.next() % 256, _ => rng.next() };
            let access = [g17_uapi::GpuAccess::Read, g17_uapi::GpuAccess::Write, g17_uapi::GpuAccess::ReadWrite][(rng.next() % 3) as usize];
            assert_eq!(record.covers(address, size, access), checked.covers(address, size, access), "{:#x} {:#x}", address, size);
        }
    }

    #[test]
    fn request_set_dedups_and_overflows() {
        let mut r = G17PRangeRequests::new();
        for i in 0..G17P_PROOF_RANGES as u64 { r.push((i * 16, 8, 1)); r.push((i * 16, 8, 1)); }
        assert_eq!(r.as_slice().len(), G17P_PROOF_RANGES);
        assert!(!r.overflow);
        r.push((0xffff, 8, 1));
        assert!(r.overflow);
        let mut proof = G17PValidationProof::new();
        proof.record(Some(3), &r);
        assert!(!proof.covers(Some(3), &[(0, 8, 1)]), "an incomplete set is never a proof");
        let mut ok = G17PRangeRequests::new(); ok.push((0, 8, 1));
        proof.record(Some(3), &ok);
        assert!(proof.covers(Some(3), &[(0, 8, 1)]));
        assert!(!proof.covers(Some(4), &[(0, 8, 1)]));
        assert!(!proof.covers(None, &[(0, 8, 1)]));
        assert!(!proof.covers(Some(3), &[(0, 8, 3)]), "write access needs its own proof");
    }

    #[test]
    fn word_stores_write_identical_bytes() {
        let mut rng = Rng(777);
        for on in [false, true] {
            g17_store::TEST_WORD_STORES.store(on, Ordering::Relaxed);
            for base in 0..8 {
                for offset in 0..24 {
                    let value = rng.next();
                    let mut a = vec![0xa5u8; 64]; let mut b = a.clone();
                    g17_store::put_u64(&mut a[base..], offset, value);
                    b[base + offset..base + offset + 8].copy_from_slice(&value.to_le_bytes());
                    assert_eq!(a, b);
                    let mut a = vec![0x5au8; 64]; let mut b = a.clone();
                    g17_store::put_u32(&mut a[base..], offset, value as u32);
                    b[base + offset..base + offset + 4].copy_from_slice(&(value as u32).to_le_bytes());
                    assert_eq!(a, b);
                    let mut a = vec![0x3cu8; 64]; let mut b = a.clone();
                    g17_store::put_u16(&mut a[base..], offset, value as u16);
                    b[base + offset..base + offset + 2].copy_from_slice(&(value as u16).to_le_bytes());
                    assert_eq!(a, b);
                }
            }
        }
    }

    #[test]
    #[should_panic]
    fn word_stores_keep_bounds_checks() {
        g17_store::put_u64(&mut [0u8; 8], 1, 0);
    }
}
'''
source = (source.replace('@ROOT@', str(ROOT)).replace('@HOST_PATH@', host_path)
          .replace('@PROOF@', proof).replace('@UAPI@', uapi_mini))
with tempfile.TemporaryDirectory() as d:
    f = Path(d) / 'v91.rs'
    f.write_text(source)
    subprocess.run(['rustc', '--edition=2021', '-O', '--test', '--crate-name', 'v91', str(f), '-o', str(Path(d) / 'test')], check=True)
    subprocess.run([str(Path(d) / 'test'), '--test-threads=1', '--nocapture', 'v91::'], check=True)
