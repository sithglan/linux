#!/usr/bin/env python3
"""Execute exact production payload parsers and logical-prefix helpers.

Extracting these pure items avoids the driver module's unrelated kernel-only
manager dependencies. No translation or ordering function is reimplemented.
"""
from pathlib import Path
import os, re, subprocess, tempfile
root = Path(__file__).resolve().parents[1]
sub = (root / 'g17_submission.rs').read_text()
uapi = (root / 'g17_uapi.rs').read_text()
def item(s, marker):
    start = s.index(marker)
    end = s.index('{', start) + 1
    depth = 1
    while depth:
        depth += (s[end] == '{') - (s[end] == '}')
        end += 1
    return s[start:end]
def const(s, name):
    return re.search(r'^(?:pub\(crate\) )?const '+name+r'\b[^;]*;',s,re.M).group()
derive = '#[derive(Debug, Copy, Clone, PartialEq, Eq)]\n'
program = '#![allow(dead_code)]\n'
for n in ['KICK_TS_BITS','KICK_TS_MASK','KICK_QID_BITS','KICK_QID_MASK']:
    program += const(sub,n)+'\n'
for n in ['SubmissionError','G17PClBarrierDependency','G17PRenderOrder']:
    kind = 'enum' if n=='SubmissionError' else 'struct'
    program += derive+item(sub,'pub(crate) '+kind+' '+n+' {')+'\n'
for n in ['g17p_render_barrier_from_timestamp','g17p_render_order','g17p_render_order_with_prior','g17p_render_prefix_contains','g17p_insert_prefix_dependency']:
    program += item(sub,'pub(crate) fn '+n+'(')+'\n'
program += item(sub,'mod exact_prior_tests {')+'\n'
program += 'type RenderBuildError = u32; mod g17_compute { pub type ComputeBuildError = u32; }\n'
program += derive+item(uapi,'pub(crate) enum UapiTranslateError {')+'\n'
for n in ['UAPI_RENDER_SIZE','UAPI_COMPUTE_SIZE']:
    program += const(uapi,n)+'\n'
for n in ['UapiHelperProgram','UapiProgram','UapiTimestamp','UapiTimestamps','UapiZlsBuffer','UapiRenderCommand','UapiComputeCommand']:
    program += derive+item(uapi,'pub(crate) struct '+n+' {')+'\n'
for n in ['get_u16','get_u32','get_u64','helper','program','timestamps','zls']:
    program += item(uapi,'fn '+n+'(')+'\n'
program += item(uapi,'fn padded_payload<const')+'\n'
for n in ['UapiRenderCommand','UapiComputeCommand']:
    program += item(uapi,'impl '+n+' {')+'\n'
program += item(uapi,'mod exact_prior_payload_tests {')+'\n'
# Verify all four engine combinations use the helper with frozen entry bases,
# and that the capability is backend-specific and exported through GET_PARAMS.
drm = (root/'g17_drm.rs').read_text()
translate = item(drm,'    fn translate_submission(')
assert translate.count('g17p_render_order_with_prior(')==4
for engine, prior in [('render','vdm'),('compute','cdm')]:
    assert translate.count(f'{engine}_base, {engine}_preceding, header.{engine}_barrier, payload.prior_{prior},')==2
    assert f'let {engine}_base = self.{engine}_sequence;' in translate
assert translate.count('SubmissionError::PriorBarrierFuture')==4
assert 'fn supports_exact_prior_barriers(&self) -> bool { true }' in drm
assert 'fn supports_exact_prior_barriers(&self) -> bool { false }' in (root/'drm_gpu.rs').read_text()
assert 'if gpu.supports_exact_prior_barriers() {' in (root/'file.rs').read_text()
header=(root.parents[3]/'include/uapi/drm/asahi_drm.h').read_text()
assert 'DRM_ASAHI_FEATURE_EXACT_PRIOR_BARRIERS = (1UL) << 5' in header
with tempfile.TemporaryDirectory() as tmp:
    out=Path(os.environ.get('G17_TEST_OUTPUT',tmp));out.mkdir(parents=True,exist_ok=True)
    src=out/'exact-prior.rs';binary=out/'exact-prior-tests';src.write_text(program)
    subprocess.run(['rustc','--edition=2021','--test',str(src),'-o',str(binary)],check=True)
    subprocess.run([str(binary),'--test-threads=1'],check=True,timeout=60)
print('PASS: production parsers/prefix helpers and four-way integration wiring')
