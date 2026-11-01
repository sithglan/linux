// SPDX-License-Identifier: MIT
//! Register-only fixture from the verified native 25G76 256x256 M5 triangle.
//! No shader binaries, host descriptors or captured physical memory are embedded.
use super::*;

fn objects() -> RenderDescriptorObjects {
    RenderDescriptorObjects {
        record_a: 0xffff_fc20_c040_4000,
        shared: 0xffff_fc20_c040_5000,
        record_b: 0xffff_fc20_c040_6000,
        zero: 0xffff_fc20_c040_8000,
    }
}

fn metadata(p: &G17pRenderParameters) -> RenderDescriptorMetadata {
    RenderDescriptorMetadata {
        context_id: 3,
        submission_ordinal: 0,
        command_buffer_state_word: 0,
        ta_hardware_buffer_id: p.ta_hardware_buffer_id,
        submit_sequence: 1,
    }
}

fn read_u64(raw: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(raw[offset..offset + 8].try_into().unwrap())
}

fn read_u32(raw: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(raw[offset..offset + 4].try_into().unwrap())
}

#[test]
fn parameter_metrics_follow_the_owned_scene_after_relocation() {
    assert_eq!(parameter_buffer_metrics_record(0x10_0008_0040), Ok(0x80041));
    // First application VM: the old native address falls in an unmapped
    // PB guard. Hardware reported gPM_SP0 / Unmapped at 0x1000080040.
    assert_eq!(parameter_buffer_metrics_record(0x10_001c_0040), Ok(0x1c0041));
    let (mut p, controls) = reference();
    p.record_index = parameter_buffer_metrics_record(0x10_001c_0040).unwrap();
    let registers = build_ta_registers(&p, &controls).unwrap();
    assert_eq!(registers.iter().find(|r| r.number == 0x1c910).unwrap().value,
        0x1c0041);
    assert_eq!(parameter_buffer_metrics_record(0x10_ffff_ffc0), Ok(0xffff_ffc1));
    for address in [0x0f_ffff_ffc0, 0x11_0000_0000, 0x10_001c_0041, u64::MAX] {
        assert!(parameter_buffer_metrics_record(address).is_err());
    }
}

#[test]
fn initial_parameter_buffer_slot_matches_native_ta_publication() {
    // Native 25G76 first TA: 48 parameter pages, no cursor progress and no
    // enable bit in dword 0. Verified stable in the live hardware table.
    assert_eq!(initial_parameter_buffer_descriptor(0x10_0023_8000, 48),
        Ok([0x23800, 0x30, 0, 0]));
    assert!(initial_parameter_buffer_descriptor(0x10_0023_8001, 48).is_err());
    assert!(initial_parameter_buffer_descriptor(0x10_0023_8000, 1 << 22).is_err());
    assert!(initial_parameter_buffer_descriptor(0xffff_fc20_c100_0000, 48).is_err());
}

fn check_program(raw: &[u8], offset: usize, expected: &[(u32, u64)]) {
    for (index, (number, value)) in expected.iter().enumerate() {
        assert_eq!(read_u32(raw, offset + index * 12), *number);
        assert_eq!(read_u64(raw, offset + index * 12 + 4), *value);
    }
}

#[test]
fn usc_native_accounting_is_a_size_and_a_separate_statistics_slot() {
    // Independent 25G76 TA/fragment observations. The fragment scalar has no
    // mapping in either context 0 or context 3; allocatePagesEnd produces
    // them by byte-size arithmetic. They must never become user aliases.
    let ta_state = M5RenderUscState {
        control: 0xffff_fc20_c08a_8000,
        required_bytes: 0x7d4000,
        allocated_bytes: 0x2600000,
        statistics: 0xffff_fc20_0170_8000,
    };
    let fragment_state = M5RenderUscState {
        required_bytes: 0x934000,
        statistics: 0xffff_fc20_0172_8000,
        ..ta_state
    };
    let mut ta = [0xa5; g17_render::TA_DESCRIPTOR_SIZE];
    let mut fragment = [0xa5; g17_render::FRAGMENT_DESCRIPTOR_SIZE];
    apply_usc_submission_state(&mut ta, &mut fragment, ta_state, fragment_state).unwrap();
    for (raw, offset, expected) in [
        (&ta[..], 0x934, ta_state.control),
        (&ta[..], 0x93d, 0x7d4000),
        (&ta[..], 0x945, ta_state.statistics),
        (&fragment[..], 0x21ce, fragment_state.control),
        (&fragment[..], 0x21d7, 0x934000),
        (&fragment[..], 0x21df, fragment_state.statistics),
    ] {
        assert_eq!(read_u64(raw, offset), expected);
    }
    // Preserve flags and the following fields, including the unaligned gap.
    assert_eq!(ta[0x93c], 0xa5);
    assert_eq!(fragment[0x21d6], 0xa5);
    assert_eq!(ta[0x94d], 0xa5);
    assert_eq!(fragment[0x21e7], 0xa5);

    let before_ta = ta;
    let before_fragment = fragment;
    for bad in [
        M5RenderUscState { required_bytes: 0, ..fragment_state },
        M5RenderUscState { required_bytes: 0x2600001, ..fragment_state },
        M5RenderUscState { required_bytes: 0x10_0025_8000, ..fragment_state },
        M5RenderUscState { statistics: 0x934000, ..fragment_state },
        M5RenderUscState { statistics: u64::MAX & !63, ..fragment_state },
        M5RenderUscState { control: 0x70_0000_0000, ..fragment_state },
    ] {
        assert_eq!(apply_usc_submission_state(&mut ta, &mut fragment, ta_state, bad),
            Err(M5RenderBuildError::InvalidUscState));
        assert_eq!(ta, before_ta);
        assert_eq!(fragment, before_fragment);
    }
    assert_eq!(apply_usc_submission_state(&mut ta, &mut fragment[..32], ta_state, fragment_state),
        Err(M5RenderBuildError::Common(RenderBuildError::BufferTooSmall)));
    assert_eq!(ta, before_ta);
    assert_eq!(fragment, before_fragment);
}

#[test]
fn descriptor_programs_counts_aliases_and_bounds_match_native_layout() {
    let (p, controls) = reference();
    let objects = objects();
    let metadata = metadata(&p);
    let ta_alias = 0x70_0040_0000;
    let fragment_alias = 0x70_0040_4000;
    let mut ta = [0xa5; g17_render::TA_DESCRIPTOR_SIZE + 64];
    let mut fragment = [0xa5; g17_render::FRAGMENT_DESCRIPTOR_SIZE + 64];
    build_ta_descriptor_base(&p, &controls, objects, metadata, ta_alias, &mut ta).unwrap();
    build_fragment_descriptor_base(
        &p,
        &controls,
        objects,
        metadata,
        fragment_alias,
        &mut fragment,
    )
    .unwrap();
    check_program(&ta, 0x60, &TA);
    check_program(&fragment, 0xa0, &FRAGMENT);
    check_program(&fragment, 0x7c0, &PARTIAL_STORE);
    check_program(&fragment, 0xee0, &PARTIAL_RESUME);
    check_program(&fragment, 0x1600, &PARTIAL_LOAD);
    assert_eq!(&ta[0x38..0x3e], &[79, 0, 82, 0, 82, 0]);
    assert_eq!(&fragment[0x80..0x86], &[87, 0, 89, 0, 89, 0]);
    assert_eq!(&fragment[0x88..0x8a], &[90, 0]);
    assert_eq!(read_u32(&ta, 0x768), 0x03d80052);
    for (header, count) in [(0x7a8, 90), (0xec8, 18), (0x15e8, 25), (0x1d08, 12)] {
        assert_eq!(read_u32(&fragment, header), ((count * 12) << 16) | count);
    }
    assert_eq!(read_u64(&ta, 0x760), ta_alias + 0x60);
    for (header, offset) in [
        (0x7a0, 0xa0),
        (0xec0, 0x7c0),
        (0x15e0, 0xee0),
        (0x1d00, 0x1600),
    ] {
        assert_eq!(read_u64(&fragment, header), fragment_alias + offset);
    }
    assert_eq!(read_u64(&ta, 0x10), objects.record_a);
    assert_eq!(read_u64(&fragment, 0x20), objects.record_a);
    assert_eq!(read_u32(&ta, 0x18), p.ta_hardware_buffer_id);
    assert!(ta[g17_render::TA_DESCRIPTOR_SIZE..]
        .iter()
        .all(|b| *b == 0xa5));
    assert!(fragment[g17_render::FRAGMENT_DESCRIPTOR_SIZE..]
        .iter()
        .all(|b| *b == 0xa5));
}

#[test]
fn descriptor_metadata_controls_register_identity() {
    let (p, controls) = reference();
    let mut metadata = metadata(&p);
    metadata.context_id = 5;
    metadata.command_buffer_state_word = 0x85;
    let mut ta = [0; g17_render::TA_DESCRIPTOR_SIZE];
    build_ta_descriptor_base(&p, &controls, objects(), metadata, 0x70_0040_0000, &mut ta).unwrap();
    assert_eq!(read_u32(&ta, 0xc), 5);
    for (index, (number, _)) in TA.iter().enumerate() {
        if matches!(number, 0x10209 | 0x1c9f0 | 0x14320) {
            assert_eq!(read_u64(&ta, 0x60 + index * 12 + 4), 0x505);
        }
    }
}

#[test]
fn descriptor_rejects_wrong_owner_context_overflow_and_short_storage() {
    let (p, controls) = reference();
    let mut metadata = metadata(&p);
    let mut raw = [0xa5; g17_render::TA_DESCRIPTOR_SIZE];
    metadata.ta_hardware_buffer_id += 1;
    assert_eq!(
        build_ta_descriptor_base(&p, &controls, objects(), metadata, 0x70_0040_0000, &mut raw),
        Err(M5RenderBuildError::OwnershipMismatch)
    );
    metadata.ta_hardware_buffer_id = p.ta_hardware_buffer_id;
    metadata.context_id = 256;
    assert_eq!(
        build_ta_descriptor_base(&p, &controls, objects(), metadata, 0x70_0040_0000, &mut raw),
        Err(M5RenderBuildError::InvalidContext)
    );
    metadata.context_id = 3;
    assert_eq!(
        build_ta_descriptor_base(&p, &controls, objects(), metadata, u64::MAX - 16, &mut raw),
        Err(M5RenderBuildError::Common(
            RenderBuildError::ArithmeticOverflow
        ))
    );
    metadata.submission_ordinal = u64::MAX;
    assert_eq!(
        build_ta_descriptor_base(&p, &controls, objects(), metadata, 0x70_0040_0000, &mut raw),
        Err(M5RenderBuildError::Common(
            RenderBuildError::ArithmeticOverflow
        ))
    );
    metadata.submission_ordinal = 0;
    assert_eq!(
        build_ta_descriptor_base(
            &p,
            &controls,
            objects(),
            metadata,
            0x70_0040_0000,
            &mut raw[..128]
        ),
        Err(M5RenderBuildError::Common(RenderBuildError::BufferTooSmall))
    );
    assert!(raw.iter().all(|b| *b == 0xa5));
}

#[test]
fn resource_ids_are_caller_owned_and_sign_extended() {
    let (mut p, controls) = reference();
    p.ta_hardware_buffer_id = 9;
    p.usc_flist_hardware_buffer_id = 0xffff_fffe;
    let ta = build_ta_registers(&p, &controls).unwrap();
    let fragment = build_fragment_registers(&p, &controls).unwrap();
    assert_eq!(value(&ta, 0x1c830), Ok(9));
    assert_eq!(value(&fragment, 0x1c838), Ok(9));
    assert_eq!(value(&ta, 0x14308), Ok(u64::MAX - 1));
    assert_eq!(value(&fragment, 0x14048), Ok(u64::MAX - 1));
}

#[test]
fn stage_controls_and_zero_ptc_fallback_are_independent() {
    let (p, mut controls) = reference();
    controls.ta.work_key = 0x400;
    controls.fragment.work_key = 0x405;
    controls.ta.ptc_max = 0;
    controls.fragment.ptc_max = 0x81;
    let ta = build_ta_registers(&p, &controls).unwrap();
    let fragment = build_fragment_registers(&p, &controls).unwrap();
    assert_eq!(value(&ta, 0x10209), Ok(0x400));
    assert_eq!(value(&fragment, 0x10211), Ok(0x405));
    assert_eq!(value(&ta, 0xa5a1), Ok(0x6000400020));
    assert_eq!(value(&fragment, 0xa5a9), Ok(0x8100400020));
}

#[test]
fn tile_fields_reject_overflow_and_overlapping_controls() {
    let (mut p, mut controls) = reference();
    p.width = 32769;
    assert_eq!(
        build_fragment_registers(&p, &controls),
        Err(M5RenderBuildError::TileCountOutOfRange)
    );
    p.width = 256;
    controls.fragment_tile_flags |= 1 << 42;
    assert_eq!(
        build_ta_registers(&p, &controls),
        Err(M5RenderBuildError::OverlappingTileFlags)
    );
    controls.fragment_tile_flags &= !(1 << 42);
    controls.ta_dispatch_control |= 1;
    assert_eq!(
        build_ta_registers(&p, &controls),
        Err(M5RenderBuildError::DispatchBitAlreadySet)
    );
}

#[test]
fn rectangular_tile_fields_do_not_overlap() {
    let (mut p, controls) = reference();
    p.width = 1024;
    p.height = 640;
    let ta = build_ta_registers(&p, &controls).unwrap();
    let fragment = build_fragment_registers(&p, &controls).unwrap();
    assert_eq!(value(&ta, 0x1c069), Ok(0x804018));
    assert_eq!(value(&ta, 0x1c071), Ok(0x804018));
    assert_eq!(value(&ta, 0x1ca70), value(&ta, 0x1c069));
    let state = value(&fragment, 0x16068).unwrap();
    assert_eq!((state >> 42) & 0x3ff, 31);
    assert_eq!((state >> 52) & 0x3ff, 19);
    assert_eq!((state >> 38) & 0xf, 0xa);
}

fn reference() -> (G17pRenderParameters, M5RenderControls) {
    let mut p = G17pRenderParameters::source_rsrc8();
    p.aux_fb = 0x10000288000;
    p.aux_fb_flags = 0x10c001;
    p.aux_fb_page_count = 0x100000;
    p.context_base = 0x1000000000;
    p.cycle = 0x220040;
    p.deflake_1 = 0x1000068340;
    p.deflake_2 = 0x1000068040;
    p.deflake_3 = 0x1000068000;
    p.depth_aux_buffer = 0x0;
    p.depth_aux_stride = 0x0;
    p.depth_buffer = 0x0;
    p.depth_clear_value_bits = 0x3f800000;
    p.depth_dimensions = 0x0;
    p.depth_flags = 0x0;
    p.depth_stride = 0x0;
    p.encoder = 0x1000018000;
    p.fragment_lifecycle = 0x4089f4c0508e74c;
    p.fragment_status = 0x1000258000;
    p.heapmeta = 0x1000261000;
    p.height = 0x100;
    p.layers = 0x1;
    p.lifecycle = 0x4089f4c0508e74d;
    p.load_pipeline = 0x10000170200;
    p.load_pipeline_bind = 0xffff800000000040;
    p.merge_upper_x_bits = 0x3bddb3d9;
    p.merge_upper_y_bits = 0x3bddb3d9;
    p.multisample_control = 0x88;
    p.occlusion_query_base = 0x0;
    p.ppp_control = 0x202;
    p.record_index = 0x80041;
    p.stencil_aux_buffer = 0x0;
    p.stencil_aux_stride = 0x0;
    p.stencil_buffer = 0x0;
    p.stencil_clear_value = 0x0;
    p.stencil_stride = 0x0;
    p.store_pipeline = 0x10000170400;
    p.store_pipeline_bind = 0x0;
    p.ta_hardware_buffer_id = 0x4;
    p.ta_status = 0x1000078000;
    p.tib_blocks = 0x0;
    p.tile_config = 0x10280;
    p.tilemap = 0x1000260000;
    p.tpc = 0x1000288000;
    p.usc_flist_hardware_buffer_id = 0x2;
    p.utile_config = 0xa000;
    p.utile_height = 0x20;
    p.utile_width = 0x20;
    p.width = 0x100;
    p.absolute_pointers = false;
    p.partial_store_pipeline_bind = 0;
    p.partial_store_pipeline = 0x10000170400;
    p.partial_load_pipeline_bind = 0xffff800000000040;
    p.partial_load_pipeline = 0x10000170000;
    let controls = M5RenderControls {
        ta: M5RenderStageControls {
            resource_flags: 0x4054b,
            resource_control: 0x200000001,
            ptc_max: 0xc6,
            work_key: 0x300,
        },
        fragment: M5RenderStageControls {
            resource_flags: 0x104054b,
            resource_control: 0x200000001,
            ptc_max: 0xce,
            work_key: 0x300,
        },
        ta_tile_control: 0x10c08860,
        ta_dispatch_control: 0x700,
        fragment_raster_control: 0x200000ff0200,
        fragment_tile_flags: 0x200003717f,
        store_pipeline_2_bind: 0x0,
        store_pipeline_2: 0x10000170400,
        partial_store_pipeline_2_bind: 0,
        partial_store_pipeline_2: 0x10000170400,
        partial_tile_flags: 0x200000,
    };
    (p, controls)
}

const TA: [(u32, u64); 82] = [
    (0x01748, 0x0000000000000001),
    (0x10141, 0x0000000000000200),
    (0x1c039, 0x0000000000260000),
    (0x1c9c8, 0x0000000000260000),
    (0x1c0a1, 0x0000000000288000),
    (0x1c031, 0x8000000000261000),
    (0x1c9c0, 0x8000000000261000),
    (0x1c051, 0x003a0012006b0003),
    (0x1c061, 0x0000000000000001),
    (0x10149, 0x000000000000a000),
    (0x10139, 0x0000000000000088),
    (0x10111, 0x0000000000068340),
    (0x1c9b0, 0x0000000000068340),
    (0x10119, 0x0000000000068040),
    (0x1c9b8, 0x0000000000068040),
    (0x1c958, 0x0000000000000001),
    (0x1c950, 0x0004000000068000),
    (0x1c930, 0x0000000000000000),
    (0x1c880, 0x0000000000018000),
    (0x1c079, 0x0000000000261000),
    (0x1c9d8, 0x0000000000261000),
    (0x10151, 0x0000000000000000),
    (0x1c199, 0x0000000000000000),
    (0x1c1a1, 0x0000000000000000),
    (0x1c1a9, 0x0000000000000000),
    (0x1c1b1, 0x0000000000000000),
    (0x1c1b9, 0x0000000000000000),
    (0x0d499, 0x000000000004054b),
    (0x0d419, 0x0000000200000001),
    (0x0a5a1, 0x000000c600400020),
    (0x1c8f8, 0x0000000010c08860),
    (0x1c0b1, 0x0000000000000014),
    (0x1c850, 0x0000000000000014),
    (0x10131, 0x0000000000000088),
    (0x10121, 0x0000000000000202),
    (0x10129, 0x0000000000ff00ff),
    (0x101b9, 0x0000000000007007),
    (0x1c069, 0x000000000040200c),
    (0x1c071, 0x000000000040200c),
    (0x1c081, 0x0000000000000010),
    (0x1c0a9, 0x0000000000000020),
    (0x1ca50, 0x0000000000007007),
    (0x1ca58, 0x000000000000a000),
    (0x1ca60, 0x0000000000000200),
    (0x1ca70, 0x000000000040200c),
    (0x1ca78, 0x000000000040200c),
    (0x1ca68, 0x0000000000000010),
    (0x10171, 0x0000000000000100),
    (0x10169, 0x0000000000008000),
    (0x1c8e0, 0xffffffffffffffff),
    (0x1c8e8, 0xffffffffffffffff),
    (0x1c898, 0x0000000000000700),
    (0x101e1, 0x000000000000001c),
    (0x1c9e8, 0x0000000000000000),
    (0x1a099, 0x0000000000000000),
    (0x1a0a1, 0x0000000000000000),
    (0x1a069, 0x0000000000000000),
    (0x1a071, 0x0000000000000000),
    (0x1a0c9, 0x0000000000000000),
    (0x1a0d1, 0x0000000000000000),
    (0x101c9, 0x0000000000000000),
    (0x0d471, 0x0000000000000000),
    (0x1a0f1, 0x0000000000000008),
    (0x1a149, 0x0000000000000000),
    (0x10799, 0x0000000000ff0000),
    (0x1c830, 0x0000000000000004),
    (0x1ca30, 0x0000000000220040),
    (0x16c39, 0x0000000000220040),
    (0x1c910, 0x0000000000080041),
    (0x1ca10, 0x04089f4c0508e74d),
    (0x014a1, 0x04089f4c0508e74d),
    (0x0a349, 0x04089f4c0508e74d),
    (0x0a739, 0x0000000000000000),
    (0x10209, 0x0000000000000300),
    (0x1c9f0, 0x0000000000000300),
    (0x14320, 0x0000000000000300),
    (0x14308, 0x0000000000000002),
    (0x14318, 0x0000001000078001),
    (0x01740, 0x0000000000000001),
    (0x0a739, 0x0000000000000000),
    (0x1c880, 0x0000000000068340),
    (0x1c898, 0x0000000000000701),
];

const FRAGMENT: [(u32, u64); 90] = [
    (0x01739, 0x0000000000000001),
    (0x10009, 0x000000000000a000),
    (0x15379, 0x0000000000000000),
    (0x15381, 0x0000010000170400),
    (0x153b1, 0x0000000000000000),
    (0x153b9, 0x0000010000170400),
    (0x15369, 0xffff800000000040),
    (0x15371, 0x0000010000170200),
    (0x15131, 0x000000003bddb3d9),
    (0x15139, 0x000000003bddb3d9),
    (0x100a1, 0x0000000000000000),
    (0x15069, 0x0000000000000000),
    (0x15071, 0x0000000000000000),
    (0x16058, 0x0000000000000000),
    (0x10019, 0x0000000000000088),
    (0x16469, 0x0000000000040004),
    (0x16030, 0x0000000000040004),
    (0x16471, 0x0000000000007007),
    (0x10791, 0x0000200000ff0200),
    (0x16098, 0x0000001000261000),
    (0x15021, 0x000000000010c001),
    (0x15211, 0x0000010000000100),
    (0x15049, 0x0000000000100000),
    (0x10051, 0x0000000000000000),
    (0x0d491, 0x000000000104054b),
    (0x0d429, 0x0000000200000001),
    (0x0a5a9, 0x000000ce00400020),
    (0x15321, 0x0000000000000000),
    (0x15301, 0x000000003f800000),
    (0x15309, 0x0000000000000300),
    (0x15311, 0x0000000000000000),
    (0x15319, 0x0000000000000000),
    (0x15349, 0x0000000004040404),
    (0x15351, 0x0000000000000000),
    (0x15329, 0x0000000000000000),
    (0x15331, 0x0000000000000000),
    (0x15339, 0x0000000000000000),
    (0x15341, 0x0000000000000000),
    (0x15231, 0x0000000000000000),
    (0x15221, 0x0000000000000000),
    (0x15239, 0x0000000000000000),
    (0x15229, 0x0000000000000000),
    (0x15401, 0x0000000000000000),
    (0x15421, 0x0000000000000000),
    (0x15409, 0x0000000000000000),
    (0x15429, 0x0000000000000000),
    (0x153c1, 0x0000000000000000),
    (0x15411, 0x0000000000000000),
    (0x153c9, 0x0000000000000000),
    (0x15431, 0x0000000000000000),
    (0x153d1, 0x0000000000000000),
    (0x15419, 0x0000000000000000),
    (0x153d9, 0x0000000000000000),
    (0x15439, 0x0000000000000000),
    (0x16429, 0x0000001000260000),
    (0x16060, 0x0000001000261000),
    (0x16431, 0x0000000050000000),
    (0x10039, 0x0000000000010280),
    (0x16020, 0x0000000000000000),
    (0x16451, 0x0000000000000000),
    (0x15359, 0x0000000000000000),
    (0x16138, 0x0000000000008860),
    (0x16461, 0x0000010000288000),
    (0x16090, 0x0000010000288000),
    (0x101e9, 0x000000000000001c),
    (0x16068, 0x00701ea00003717f),
    (0x1a0a9, 0x0000000000000000),
    (0x1a0b1, 0x0000000000000000),
    (0x1a079, 0x0000000000000000),
    (0x1a081, 0x0000000000000000),
    (0x1a0d9, 0x0000000000000000),
    (0x1a0e1, 0x0000000000000000),
    (0x101c1, 0x0000000000000000),
    (0x0d469, 0x0000000000000000),
    (0x1a0f9, 0x0000000000000008),
    (0x1a151, 0x0000000000000000),
    (0x160e0, 0x04089f4c0508e74c),
    (0x01499, 0x04089f4c0508e74c),
    (0x0a341, 0x04089f4c0508e74c),
    (0x1c838, 0x0000000000000004),
    (0x1ca28, 0x0000000000220040),
    (0x0a741, 0x0000000000000000),
    (0x10211, 0x0000000000000300),
    (0x10420, 0x0000000000000300),
    (0x14048, 0x0000000000000002),
    (0x14080, 0x0000001000258001),
    (0x01731, 0x0000000000000001),
    (0x0a741, 0x0000000000000000),
    (0x16020, 0x0000000000000001),
    (0x16068, 0x0000000000040000),
];

#[test]
fn native_m5_register_lists_match_every_write() {
    let (p, controls) = reference();
    let ta = build_ta_registers(&p, &controls).unwrap();
    let fragment = build_fragment_registers(&p, &controls).unwrap();
    let mut mismatches = 0;
    for (stage, actual, expected) in [
        ("TA", ta.as_slice(), TA.as_slice()),
        ("fragment", fragment.as_slice(), FRAGMENT.as_slice()),
    ] {
        for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
            if (actual.number, actual.value) != *expected {
                eprintln!(
                    "{stage}[{index}] actual={:x}/{:x} native={:x}/{:x}",
                    actual.number, actual.value, expected.0, expected.1
                );
                mismatches += 1;
            }
        }
    }
    assert_eq!(mismatches, 0);
}

#[test]
fn native_m5_d32_clear_store_registers_match_color_depth_capture() {
    // The 25G76 RGB-near/white-far reference passed LESS/write occlusion:
    // 60928 independently checked output pixels, zero mismatches. Its depth
    // allocation adds sixteen 16K data pages and one compression page.
    let (mut p, controls) = reference();
    p.depth_dimensions = 0x00ff00ff;
    p.depth_flags = 0x80044;
    p.depth_buffer = 0x100000c8000;
    p.depth_aux_buffer = 0x10000108000;
    let actual = build_fragment_registers(&p, &controls).unwrap();
    let expected = [
        (0x15321, 0x00ff00ff), (0x15301, 0x3f800000),
        (0x15309, 0x300), (0x15311, 0), (0x15319, 0x80044),
        (0x15329, 0x100000c8000), (0x15331, 0x100000c8000),
        (0x15339, 0), (0x15341, 0), (0x15401, 0), (0x15421, 0),
        (0x15409, 0), (0x15429, 0),
        (0x153c1, 0x10000108000), (0x153c9, 0x10000108000),
        (0x15411, 0), (0x15431, 0),
    ];
    for (register, native_value) in expected {
        assert_eq!(value(&actual, register), Ok(native_value),
            "native M5 depth register {register:#x}");
    }
    // Depth fields must not change the other state of the existing reference.
    for (register, baseline) in actual.iter().zip(FRAGMENT) {
        if !expected.iter().any(|(number, _)| *number == register.number) {
            assert_eq!((register.number, register.value), baseline);
        }
    }
}

const PARTIAL_STORE: [(u32, u64); 18] = [
    (0x15379, 0x0000000000000000),
    (0x15381, 0x0000010000170400),
    (0x153b1, 0x0000000000000000),
    (0x153b9, 0x0000010000170400),
    (0x10039, 0x0000000000210280),
    (0x15359, 0x0000000000000020),
    (0x15331, 0x0000000000000000),
    (0x153c9, 0x0000000000000000),
    (0x15341, 0x0000000000000000),
    (0x153d9, 0x0000000000000000),
    (0x15421, 0x0000000000000000),
    (0x15431, 0x0000000000000000),
    (0x15429, 0x0000000000000000),
    (0x15439, 0x0000000000000000),
    (0x15221, 0x0000000000000000),
    (0x15229, 0x0000000000000000),
    (0x15319, 0x0000000000000000),
    (0x15349, 0x0000000004040404),
];

const PARTIAL_RESUME: [(u32, u64); 25] = [
    (0x15379, 0x0000000000000000),
    (0x15381, 0x0000010000170400),
    (0x153b1, 0x0000000000000000),
    (0x153b9, 0x0000010000170400),
    (0x15369, 0xffff800000000040),
    (0x15371, 0x0000010000170000),
    (0x10039, 0x0000000000200280),
    (0x15359, 0x0000000000000020),
    (0x15331, 0x0000000000000000),
    (0x153c9, 0x0000000000000000),
    (0x15341, 0x0000000000000000),
    (0x153d9, 0x0000000000000000),
    (0x15421, 0x0000000000000000),
    (0x15431, 0x0000000000000000),
    (0x15429, 0x0000000000000000),
    (0x15439, 0x0000000000000000),
    (0x15221, 0x0000000000000000),
    (0x15229, 0x0000000000000000),
    (0x15309, 0x0000000000000300),
    (0x15329, 0x0000000000000000),
    (0x153c1, 0x0000000000000000),
    (0x15339, 0x0000000000000000),
    (0x153d1, 0x0000000000000000),
    (0x15319, 0x0000000000000000),
    (0x15349, 0x0000000004040404),
];

const PARTIAL_LOAD: [(u32, u64); 12] = [
    (0x15369, 0xffff800000000040),
    (0x15371, 0x0000010000170000),
    (0x10039, 0x0000000000200280),
    (0x15309, 0x0000000000000300),
    (0x153b1, 0x0000000000000000),
    (0x153b9, 0x0000010000170400),
    (0x15329, 0x0000000000000000),
    (0x153c1, 0x0000000000000000),
    (0x15339, 0x0000000000000000),
    (0x153d1, 0x0000000000000000),
    (0x15319, 0x0000000000000000),
    (0x15349, 0x0000000004040404),
];

#[test]
fn native_m5_partial_programs_match_every_write() {
    let (p, controls) = reference();
    let store = build_partial_store_registers(&p, &controls).unwrap();
    let resume = build_partial_resume_registers(&p, &controls).unwrap();
    let load = build_partial_load_registers(&p, &controls).unwrap();
    assert_eq!(store.map(|r| (r.number, r.value)), PARTIAL_STORE);
    assert_eq!(resume.map(|r| (r.number, r.value)), PARTIAL_RESUME);
    assert_eq!(load.map(|r| (r.number, r.value)), PARTIAL_LOAD);
}
