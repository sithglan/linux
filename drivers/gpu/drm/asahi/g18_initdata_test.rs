// SPDX-License-Identifier: GPL-2.0-only OR MIT
//! Offline tests for the J873 / G18G / 26A428 init-data profile (plan M1
//! steps 1.1-1.7). Run on the host by `tests/test_g18_initdata.py`.
//!
//! (a) Regression: the layout functions, run over the 25G76 kext size table,
//!     reproduce the literal M5 constants bit for bit; the register-window VA
//!     rule reproduces all 22 native J813 device VAs; and every J700/J813
//!     encoder output hash is the one recorded before the G18G profile existed.
//! (b) G18G: every hw_data offset read by the 26A428 init copier lies on a
//!     field the encoder owns; the relocation bands map every 25G76 copier and
//!     region-C read onto a 26A428 read; and no J813 value survives at an
//!     unrelocated offset (every non-zero output byte is explained).
//!
//! The read lists are the firmware research's scratch `fw/r17.txt`/`r18.txt`
//! (hwreads.py over the loaded 25G76 GFX TEXT 0x1fb18-0x20124 and over
//! armfw_g18g-b000 0x21708-0x21d94) and the region-C loaders (G17
//! 0x3068-0x330c, b000 0x3508-0x37c0), embedded so no scratch tree is needed.

use super::*;

const KERN: u64 = 0xffff_fc20_0000_0000;
const PRIVATE: u64 = KERN + 0x2_0000;
const ROOTS: u64 = KERN + 0x1a_8000;
const SHARED: u64 = KERN + 0xc078_8000;
const FWCTL: u64 = KERN + 0x1c_0000;
const SENTINEL: u64 = KERN + 0xc07e_0000;
const PBDESC: u64 = KERN + 0x15e_0000;
const UMA: u64 = KERN + 0x15e_8000;
const QOS: u64 = KERN + 0x168_0000;
const AON: u64 = KERN + 0x168_4000;
const TIMESTAMP: u64 = 0x1122_3344_5566_7788;

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn completion_aliases() -> [G17PKsmCompletionAliases; M5_25G76_COMPLETION_QUEUE_COUNT] {
    let mut aliases = [G17PKsmCompletionAliases { low: 0, high: 0 }; M5_25G76_COMPLETION_QUEUE_COUNT];
    for (ordinal, alias) in aliases.iter_mut().enumerate() {
        *alias = G17PKsmCompletionAliases {
            low: 0x70_0184_8000 + ordinal as u64 * 0x2_8000,
            high: KERN + 0xc07e_8000 + ordinal as u64 * 0x2_8000,
        };
    }
    aliases
}

fn m5_shared() -> SharedAddresses {
    SharedAddresses {
        region_a: KERN + 0x1b_8000,
        region_c: KERN + 0x1a_0000,
        hw_data_bundle: SHARED,
        hw_data_bundle_alloc: NATIVE_SHARED_CLUSTER_SIZE,
    }
}

fn m5_instances() -> (InstanceAddresses, InstanceAddresses) {
    let primary = InstanceAddresses {
        root: ROOTS,
        main_config: SHARED + NATIVE_PRIMARY_MAIN_BUNDLE_OFFSET as u64,
        status_a: PRIVATE + 0xee40,
        status_b: PRIVATE + 0x340,
        secondary_extras: [0, 0],
    };
    let secondary = InstanceAddresses {
        root: ROOTS + SECONDARY_ROOT_DELTA,
        main_config: SHARED + NATIVE_SECONDARY_MAIN_BUNDLE_OFFSET as u64,
        status_a: PRIVATE + 0xc3100,
        status_b: 0,
        secondary_extras: [primary.status_b + PRIMARY_STATUS_B_CONFIG_OFFSET as u64, PRIVATE + 0x177400],
    };
    (primary, secondary)
}

fn g18_shared() -> SharedAddresses {
    SharedAddresses {
        region_a: KERN + 0x1b_8000,
        region_c: KERN + 0x1a_0000,
        hw_data_bundle: SHARED,
        hw_data_bundle_alloc: G18G_26A428_SHARED_CLUSTER_SIZE,
    }
}

fn g18_instances() -> (InstanceAddresses, InstanceAddresses) {
    let layout = G18G_26A428_PRIVATE_LAYOUT;
    let primary = InstanceAddresses {
        root: ROOTS,
        main_config: SHARED + G18G_26A428_MAIN_BUNDLE_OFFSETS[0] as u64,
        status_a: PRIVATE + layout.roles[0].status_a as u64,
        status_b: PRIVATE + layout.status_b as u64,
        secondary_extras: [0, 0],
    };
    let secondary = InstanceAddresses {
        root: ROOTS + SECONDARY_ROOT_DELTA,
        main_config: SHARED + G18G_26A428_MAIN_BUNDLE_OFFSETS[1] as u64,
        status_a: PRIVATE + layout.roles[1].status_a as u64,
        status_b: 0,
        secondary_extras: [
            PRIVATE + layout.status_config as u64,
            PRIVATE + layout.secondary_extra_1 as u64,
        ],
    };
    (primary, secondary)
}

fn views() -> PrimaryRegionViews {
    PrimaryRegionViews { sentinel_high: SENTINEL, alias_high: [PBDESC, UMA] }
}

// ----------------------------------------------------------------------------------
// (a) Regression
// ----------------------------------------------------------------------------------

#[test]
fn m5_shared_layout_reproduces_the_literal_25g76_constants() {
    let table = KEXT_ALLOCATION_SIZES_25G76;
    let layout = M5_25G76_SHARED_LAYOUT;
    assert_eq!(layout, shared_cluster_layout(&table));
    assert_eq!(
        M5_25G76_SHARED_ALLOCATIONS,
        [
            (layout.hw_data, table[KX_HW_DATA]),
            (layout.block_c18, table[KX_BLOCK_C18]),
            (layout.block_1048, table[KX_BLOCK_1048]),
            (layout.block_e10, table[KX_BLOCK_E10]),
            (layout.policy, table[KX_POLICY]),
            (layout.block_68, table[KX_BLOCK_68]),
            (layout.block_800, table[KX_BLOCK_800]),
        ]
    );
    assert_eq!(M5_25G76_SHARED_PREFIX_SIZE, layout.brn_table);
    assert_eq!(layout.brn_table, layout.block_88);
    assert_eq!(MAIN_REPEATED_REGION_BUNDLE_OFFSET, layout.block_88);
    assert_eq!(
        BUNDLE_VIEW_OFFSETS,
        [layout.block_c18, layout.block_1048, layout.block_e10, layout.block_68, layout.block_800]
    );
    assert_eq!(
        BUNDLE_VIEW_OFFSETS[SECONDARY_EXTRA_VIEW_INDEX] + SECONDARY_EXTRA_VIEW_OFFSET,
        layout.policy
    );
    assert_eq!(WORK_RING_BUNDLE_OFFSETS, work_ring_offsets(&layout));
    assert_eq!(NATIVE_PRIMARY_MAIN_BUNDLE_OFFSET, layout.main_config[0]);
    assert_eq!(NATIVE_SECONDARY_MAIN_BUNDLE_OFFSET, layout.main_config[1]);
    for role in 0..2 {
        assert_eq!(layout.control_ring[role], layout.main_config[role] + MAIN_CONTROL_RING);
    }
    // CONTROL_RING_ENTRIES' documented 0x1ea80 -> 0x22a80 relation.
    assert_eq!(layout.control_ring[0], 0x1ea80);
    assert_eq!(
        layout.control_ring[0] + CONTROL_RING_ENTRIES * CONTROL_RECORD_SIZE,
        layout.main_config[1]
    );
    assert_eq!(layout.end, 0x26f40);
    assert_eq!(NATIVE_SHARED_CLUSTER_SIZE, layout.total);
    assert_eq!(M5_25G76_HW_DATA_SIZE, table[KX_HW_DATA]);
    assert_eq!(M5_25G76_AON_PTD_SIZE, table[KX_AON_PTD]);
    assert_eq!(M5_25G76_STATUS_CONFIG_SIZE, table[KX_STATUS_CONFIG]);
    assert_eq!(M5_25G76_COMPLETION_BACKING_SIZE, 0x20000);

    // The literal pointer offsets of encode_m5_25g76_bundle_policy are the
    // policy block's +0x3c98/+0x6460/+0x651c and its u16 table +0x1c97.
    let mut bundle = vec![0u8; NATIVE_SHARED_CLUSTER_SIZE];
    let state = PRIVATE + 0x177480;
    encode_m5_25g76_bundle_policy(
        M5PolicyBindings { state, aon_ptd: AON, boot_timestamp: TIMESTAMP },
        &mut bundle,
    )
    .unwrap();
    assert_eq!(get_u64(&bundle, layout.policy + 0x3c98), state);
    assert_eq!(get_u64(&bundle, layout.policy + 0x6460), AON);
    assert_eq!(get_u64(&bundle, layout.policy + 0x651c), TIMESTAMP);
    let table16 = layout.policy + M5_25G76_POLICY_U16_TABLE;
    assert_eq!(&bundle[table16..table16 + 4], &[0xff, 0x7f, 0xff, 0x7f]);
    assert_eq!(
        &bundle[table16 + (M5_25G76_POLICY_U16_TABLE_ENTRIES - 1) * 2..table16 + M5_25G76_POLICY_U16_TABLE_ENTRIES * 2],
        &[0xff, 0x7f]
    );
    assert_eq!(bundle[table16 + M5_25G76_POLICY_U16_TABLE_ENTRIES * 2], 0);
    assert_eq!(bundle[table16 - 1], 0);
}

#[test]
fn m5_private_layout_reproduces_the_literal_25g76_constants() {
    let table = KEXT_ALLOCATION_SIZES_25G76;
    let layout = M5_25G76_PRIVATE_LAYOUT;
    assert_eq!(layout, private_cluster_layout(&table));
    assert_eq!(PRIMARY_STATUS_B_STATE_GRID_OFFSET, layout.status_b);
    assert_eq!(PRIMARY_STATUS_B_OBJECT_SIZE, layout.roles[0].status_a - layout.status_b);
    assert_eq!(PRIMARY_STATUS_B_CONFIG_OFFSET, layout.status_config - layout.status_b);
    // The literals used by encode_m5_25g76_private_cluster and m5_resources.
    assert_eq!(layout.status_b, 0x340);
    assert_eq!(layout.roles[0].status_a, 0xee40);
    assert_eq!(layout.roles[1].status_a, 0xc3100);
    assert_eq!(layout.secondary_control_state - WORK_STATE_GRID_OFFSETS[CONTROL_CHANNEL_INDEX], 0x1770c0);
    assert_eq!(layout.secondary_control_state + 2 * CHANNEL_STATE_SPACING, 0x1773e0);
    assert_eq!(layout.control_state + 2 * CHANNEL_STATE_SPACING, 0x320);
    assert_eq!(layout.secondary_extra_1, 0x177400);
    assert_eq!(layout.policy_state, 0x177480);
    assert_eq!(layout.end, 0x177488);
    assert_eq!(M5_25G76_PRIVATE_CLUSTER_SIZE, layout.total);
    for channel in 0..WORK_CHANNEL_COUNT {
        assert_eq!(
            WORK_STATE_GRID_OFFSETS[channel],
            layout.dm_ring_states[WORK_CHANNEL_DM_INDEX[channel]]
        );
    }
    assert_eq!(WORK_STATE_GRID_OFFSETS[CONTROL_CHANNEL_INDEX], layout.control_state);
    for role in layout.roles {
        let a = role.status_a;
        assert_eq!(
            REPORT_STATE_STATUS_A_OFFSETS,
            [
                [role.event_state - a, role.event_ring - a, role.log_state - a],
                [role.trace_state - a, role.trace_ring - a, role.state_30 - a],
            ]
        );
        assert_eq!(REPORT_RING_STATUS_A_OFFSETS, [role.buffer_28800 - a, role.buffer_4800 - a]);
        assert_eq!(PARTIAL_STATE_STATUS_A_OFFSET, role.buffer_79800 - a);
    }
    // The M5 private encoder writes exactly at the derived places.
    let mut private = vec![0xa5u8; M5_25G76_PRIVATE_CLUSTER_SIZE];
    encode_m5_25g76_private_cluster(FWCTL, &mut private).unwrap();
    assert_eq!(get_u32(&private, layout.control_state + 0x20), 1);
    assert_eq!(get_u32(&private, layout.secondary_control_state + 0x20), 1);
    for role in layout.roles {
        assert_eq!(get_u32(&private, role.status_a + STATUS_BLOCK_ONE_OFFSET), 1);
    }
    assert_eq!(get_u32(&private, layout.secondary_extra_1 + 0x10), 2);
    assert_eq!(get_u64(&private, layout.status_b + PRIMARY_STATUS_B_FWCTL_STATE), FWCTL);
    assert_eq!(get_u32(&private, layout.status_b + PRIMARY_STATUS_B_CONFIG_HEADER), 1);
}

fn m5_window_specs() -> [RegisterWindowSpec; 22] {
    M5_25G76_REGISTER_WINDOWS.map(|window| RegisterWindowSpec {
        slot: window.slot,
        phys: window.phys,
        size_10: window.size_10,
        size_14: window.size_14,
        unk_18: window.unk_18,
        flag: window.flag,
    })
}

#[test]
fn m5_register_window_vas_follow_the_allocation_rule() {
    let rebuilt = allocate_register_window_vas(&m5_window_specs());
    assert_eq!(rebuilt, M5_25G76_REGISTER_WINDOWS);
}

/// Output hashes of the unmodified J700/J813 encoders (recorded from the tree
/// before the G18G profile was added; main `+0x250`, the module-parameter
/// controlled log enable, is masked).
const GOLDEN: &[(&str, u64)] = &[
    ("hw_data", 0x5670_d1af_246c_2b4f),
    ("bundle_policy", 0x0730_7593_aaa8_321e),
    ("region_c", 0x5f91_1be1_e7ec_95d1),
    ("private", 0xb56a_eb3c_76de_437e),
    ("aon_ptd", 0x49ac_b6ee_7580_f233),
    ("register_map", 0x72d6_6398_dd1b_5e11),
    ("main_Primary", 0x5cc1_5879_2883_624a),
    ("root_Primary", 0xd066_06a4_52a7_1808),
    ("opening_Primary", 0x8638_9f90_716f_9633),
    ("main_Secondary", 0xb520_6ed3_b447_2ba6),
    ("root_Secondary", 0xcdb1_c409_f884_7f6e),
    ("opening_Secondary", 0x784c_cc69_bb24_720f),
    ("extra1", 0x2968_6894_3cf6_cd84),
    ("j700_region_c", 0xa9d6_78ae_34fa_8ee3),
    ("j700_extra1", 0x8d00_a5af_3b85_2156),
    ("j700_main_Primary", 0x72f5_9416_6a8f_6b5b),
    ("j700_root_Primary", 0xdef2_73c0_6277_bcd0),
    ("j700_main_Secondary", 0xa386_ca1c_977a_9c6e),
    ("j700_root_Secondary", 0x9030_a264_b8db_5d06),
];

fn golden(name: &str) -> u64 {
    GOLDEN.iter().find(|(key, _)| *key == name).unwrap().1
}

#[test]
fn j700_and_j813_encoder_outputs_are_bit_identical() {
    let mut shared = vec![0u8; NATIVE_SHARED_CLUSTER_SIZE];
    encode_m5_25g76_hw_data(completion_aliases(), QOS, &mut shared[..M5_25G76_HW_DATA_SIZE]).unwrap();
    assert_eq!(fnv(&shared[..M5_25G76_HW_DATA_SIZE]), golden("hw_data"));
    encode_m5_25g76_bundle_policy(
        M5PolicyBindings { state: PRIVATE + 0x177480, aon_ptd: AON, boot_timestamp: TIMESTAMP },
        &mut shared,
    )
    .unwrap();
    assert_eq!(fnv(&shared), golden("bundle_policy"));
    let mut region_c = vec![0xa5u8; REGION_C_SIZE];
    encode_region_c_for_profile(RootProfile::J813G17G25G76, &mut region_c).unwrap();
    assert_eq!(fnv(&region_c), golden("region_c"));
    let mut private = vec![0xa5u8; M5_25G76_PRIVATE_CLUSTER_SIZE];
    encode_m5_25g76_private_cluster(FWCTL, &mut private).unwrap();
    assert_eq!(fnv(&private), golden("private"));
    let mut ptd = vec![0xa5u8; M5_25G76_AON_PTD_SIZE];
    encode_m5_25g76_aon_ptd(&mut ptd).unwrap();
    assert_eq!(fnv(&ptd), golden("aon_ptd"));
    let mut map = vec![0xa5u8; HW_REGISTER_MAP_SLOTS * HW_REGISTER_MAP_ENTRY_SIZE];
    encode_m5_25g76_register_map(&mut map).unwrap();
    assert_eq!(fnv(&map), golden("register_map"));

    let addresses = m5_shared();
    let (primary, secondary) = m5_instances();
    let region_views = views();
    for (role, instance, grid, views, name) in [
        (InstanceRole::Primary, &primary, PRIVATE, Some(&region_views), "Primary"),
        (InstanceRole::Secondary, &secondary, PRIVATE + 0x1770c0, None, "Secondary"),
    ] {
        let channels = derive_channel_table(role, grid, instance.status_a, instance.main_config, SHARED);
        for profile in [RootProfile::J700G17P, RootProfile::J813G17G25G76] {
            assert_eq!(
                derive_channel_table_for_profile(profile, role, grid, instance.status_a,
                                                 instance.main_config, SHARED),
                channels
            );
            assert_eq!(profile.root_size(role), root_size(role));
            assert_eq!(profile.control_register_opcode(), Some(CONTROL_OPENING_PRIMARY_REGISTER_OPCODE));
            let mut legacy = vec![0u8; control_opening_size(role)];
            let mut profiled = vec![0xa5u8; control_opening_size(role)];
            encode_control_opening(role, &mut legacy).unwrap();
            encode_control_opening_for_profile(profile, role, &mut profiled).unwrap();
            assert_eq!(legacy, profiled);
        }
        let mut main = vec![0xa5u8; MAIN_CONFIG_SIZE];
        encode_main_config_for_profile(RootProfile::J813G17G25G76, role, &addresses, instance,
                                       &channels, views, &mut main).unwrap();
        main[MAIN_FIRMWARE_LOG_ENABLE..MAIN_FIRMWARE_LOG_ENABLE + 4].fill(0);
        assert_eq!(fnv(&main), golden(&format!("main_{name}")));
        let mut root = vec![0xa5u8; root_size(role)];
        encode_root_for_profile(RootProfile::J813G17G25G76, role, &addresses, instance, &mut root).unwrap();
        assert_eq!(fnv(&root), golden(&format!("root_{name}")));
        check_firmware_root_gates_for_profile(RootProfile::J813G17G25G76, &root).unwrap();
        let mut opening = vec![0xa5u8; control_opening_size(role)];
        encode_control_opening(role, &mut opening).unwrap();
        assert_eq!(fnv(&opening), golden(&format!("opening_{name}")));

        let mut main = vec![0xa5u8; MAIN_CONFIG_SIZE];
        encode_main_config(role, &addresses, instance, &channels, views, &mut main).unwrap();
        main[MAIN_FIRMWARE_LOG_ENABLE..MAIN_FIRMWARE_LOG_ENABLE + 4].fill(0);
        assert_eq!(fnv(&main), golden(&format!("j700_main_{name}")));
        let mut root = vec![0xa5u8; root_size(role)];
        encode_root(role, &addresses, instance, &mut root).unwrap();
        assert_eq!(fnv(&root), golden(&format!("j700_root_{name}")));
    }
    let mut extra = [0xa5u8; STATUS_BLOCK_SIZE];
    encode_secondary_root_extra_1_for_profile(RootProfile::J813G17G25G76, &mut extra).unwrap();
    assert_eq!(fnv(&extra), golden("extra1"));
    let mut region_c = vec![0xa5u8; REGION_C_SIZE];
    encode_region_c(&mut region_c).unwrap();
    assert_eq!(fnv(&region_c), golden("j700_region_c"));
    let mut extra = [0xa5u8; STATUS_BLOCK_SIZE];
    encode_secondary_root_extra_1(&mut extra).unwrap();
    assert_eq!(fnv(&extra), golden("j700_extra1"));
    // The 25G76 word is still the J813 profile's; G18G's is a new constant.
    assert_eq!(RootProfile::J813G17G25G76.version_word(), 0x0c8a_a322_039e_04c0);
    assert_eq!(RootProfile::J700G17P.version_word(), INITDATA_VERSION_WORD);
}

// ----------------------------------------------------------------------------------
// G18G layout, roots and main configuration (plan steps 1.1, 1.2, 1.5, 1.7)
// ----------------------------------------------------------------------------------

#[test]
fn g18g_layouts_match_the_26a428_size_table() {
    let shared = G18G_26A428_SHARED_LAYOUT;
    assert_eq!(
        (shared.hw_data, shared.block_c18, shared.block_1048, shared.block_e10, shared.policy),
        (0, 0x2880, 0x34c0, 0x4540, 0x5380)
    );
    assert_eq!((shared.block_68, shared.block_800, shared.brn_table, shared.block_88),
               (0xcb80, 0xcc00, 0xd400, 0xd400));
    assert_eq!(shared.dm_rings[0], 0xd4c0);
    assert_eq!(shared.main_config, [0x1f4c0, 0x23980]);
    assert_eq!(shared.control_ring, [0x1f980, 0x23e40]);
    assert_eq!((shared.end, shared.total), (0x27e40, 0x28000));
    assert_eq!(G18G_26A428_SHARED_CLUSTER_SIZE, NATIVE_SHARED_CLUSTER_SIZE);
    for channel in 0..WORK_CHANNEL_COUNT {
        assert_eq!(G18G_26A428_WORK_RING_BUNDLE_OFFSETS[channel], WORK_RING_BUNDLE_OFFSETS[channel] + 0xf00);
    }
    assert_eq!(G18G_26A428_SHARED_PREFIX_SIZE, 0xd400);
    assert_eq!(G18G_26A428_BUNDLE_VIEW_OFFSETS, [0x2880, 0x34c0, 0x4540, 0xcb80, 0xcc00]);
    assert_eq!(G18G_26A428_HW_DATA_SIZE, 0x2848);
    assert_eq!(G18G_26A428_POLICY_SIZE, 0x7800);
    assert_eq!(G18G_26A428_REGION_C_ALLOCATION, 0xe80);
    assert!(G18G_26A428_REGION_C_ALLOCATION <= REGION_C_SIZE);

    let private = G18G_26A428_PRIVATE_LAYOUT;
    assert_eq!((private.status_b, private.status_config), (0x340, 0xea80));
    assert_eq!(G18G_26A428_PRIVATE_STATUS_A, [0xf180, 0xc3440]);
    assert_eq!(
        (private.secondary_control_state, private.secondary_extra_1, private.policy_state),
        (0x177700, 0x177740, 0x1777c0)
    );
    assert_eq!((private.end, private.total), (0x1777c8, 0x178000));
    assert_eq!(G18G_26A428_PRIVATE_CLUSTER_SIZE, M5_25G76_PRIVATE_CLUSTER_SIZE);
    assert_eq!(G18G_26A428_PRIVATE_STATE_GRIDS, [0, 0x177400]);
    assert_eq!(G18G_26A428_STATUS_CONFIG_IN_STATUS_B, 0xe740);
    assert_eq!(G18G_26A428_STATUS_B_SPAN, 0xee40);
    // The per-role set keeps its status-A-relative shape, so the report
    // channel offsets are shared with 25G76.
    for (m5, g18) in M5_25G76_PRIVATE_LAYOUT.roles.iter().zip(private.roles) {
        let (a, b) = (m5.status_a, g18.status_a);
        assert_eq!(
            [m5.event_state - a, m5.log_state - a, m5.trace_state - a, m5.state_30 - a,
             m5.event_ring - a, m5.buffer_28800 - a, m5.buffer_79800 - a, m5.trace_ring - a,
             m5.buffer_4800 - a],
            [g18.event_state - b, g18.log_state - b, g18.trace_state - b, g18.state_30 - b,
             g18.event_ring - b, g18.buffer_28800 - b, g18.buffer_79800 - b, g18.trace_ring - b,
             g18.buffer_4800 - b]
        );
    }
    // Everything that follows status B moved by exactly its 0x340 growth.
    assert_eq!(private.roles[0].status_a - M5_25G76_PRIVATE_LAYOUT.roles[0].status_a, 0x340);
    assert_eq!(private.policy_state - M5_25G76_PRIVATE_LAYOUT.policy_state, 0x340);
}

fn g18_root(role: InstanceRole) -> Vec<u8> {
    let (primary, secondary) = g18_instances();
    let instance = if role == InstanceRole::Primary { primary } else { secondary };
    let mut root = vec![0xa5u8; RootProfile::J873G18G26A428.root_size(role)];
    encode_root_for_profile(RootProfile::J873G18G26A428, role, &g18_shared(), &instance, &mut root).unwrap();
    root
}

#[test]
fn g18g_roots_carry_the_26a428_version_word_and_are_0xc8_bytes() {
    assert_eq!(G18G_26A428_INITDATA_VERSION_WORD, 0x0c8b_4339_83a0_04c0);
    assert_eq!(RootProfile::J873G18G26A428.version_word(), G18G_26A428_INITDATA_VERSION_WORD);
    let (primary, secondary) = g18_instances();
    for role in [InstanceRole::Primary, InstanceRole::Secondary] {
        assert_eq!(RootProfile::J873G18G26A428.root_size(role), 0xc8);
        let root = g18_root(role);
        let instance = if role == InstanceRole::Primary { primary } else { secondary };
        assert_eq!(root.len(), 0xc8);
        assert_eq!(get_u64(&root, 0x00), 0x0c8b_4339_83a0_04c0);
        assert_eq!(
            [0x04c0u16, 0x83a0, 0x4339, 0x0c8b],
            [0, 2, 4, 6].map(|index| u16::from_le_bytes(root[index..index + 2].try_into().unwrap()))
        );
        assert_eq!(get_u64(&root, 0x08), g18_shared().region_a);
        assert_eq!(get_u64(&root, 0x10), 0);
        assert_eq!(get_u64(&root, 0x18), instance.main_config);
        assert_eq!(get_u64(&root, 0x20), g18_shared().region_c);
        assert_eq!(get_u32(&root, 0x28), role as u32);
        assert_eq!(get_u32(&root, 0x2c), 1);
        assert_eq!(get_u64(&root, 0xa8), instance.status_a);
        assert_eq!(get_u64(&root, 0xb0), instance.status_b);
        assert_eq!(get_u64(&root, 0xb8), instance.secondary_extras[0]);
        assert_eq!(get_u64(&root, 0xc0), instance.secondary_extras[1]);
        check_firmware_root_gates_for_profile(RootProfile::J873G18G26A428, &root).unwrap();
        // Each firmware generation rejects the other's word.
        assert_eq!(check_firmware_root_gates_for_profile(RootProfile::J813G17G25G76, &root),
                   Err(InitdataError::InterfaceMismatch));
        // The UAT geometry block is shared with 25G76.
        assert_eq!(&root[0x30..0xa8], &{
            let (m5_primary, _) = m5_instances();
            let mut m5 = vec![0u8; ROOT_SIZE_PRIMARY];
            encode_root_for_profile(RootProfile::J813G17G25G76, InstanceRole::Primary, &m5_shared(),
                                    &m5_primary, &mut m5).unwrap();
            m5
        }[0x30..0xa8]);
    }
    // Primary +0xb8/+0xc0 exist (0xc8 bytes) and stay zero.
    let root = g18_root(InstanceRole::Primary);
    assert_eq!(get_u64(&root, 0xb8), 0);
    assert_eq!(get_u64(&root, 0xc0), 0);
    // A 25G76-sized primary root is refused for G18G.
    let mut short = vec![0u8; ROOT_SIZE_PRIMARY];
    assert_eq!(
        encode_root_for_profile(RootProfile::J873G18G26A428, InstanceRole::Primary, &g18_shared(),
                                &primary, &mut short),
        Err(InitdataError::BufferSize)
    );
    let mut m5_word = root.clone();
    put_u64(&mut m5_word, 0, 0x0c8a_a322_039e_04c0);
    assert_eq!(check_firmware_root_gates_for_profile(RootProfile::J873G18G26A428, &m5_word),
               Err(InitdataError::InterfaceMismatch));
    let mut truncated = root[..ROOT_SIZE_PRIMARY].to_vec();
    put_u64(&mut truncated, 0, G18G_26A428_INITDATA_VERSION_WORD);
    assert_eq!(check_firmware_root_gates_for_profile(RootProfile::J873G18G26A428, &truncated),
               Err(InitdataError::BufferSize));
    let mut disabled = root.clone();
    put_u32(&mut disabled, 0x2c, 0);
    assert_eq!(check_firmware_root_gates_for_profile(RootProfile::J873G18G26A428, &disabled),
               Err(InitdataError::HostMappedAllocationsDisabled));
}

fn g18_main(role: InstanceRole) -> Vec<u8> {
    let (primary, secondary) = g18_instances();
    let instance = if role == InstanceRole::Primary { primary } else { secondary };
    let grid = PRIVATE + G18G_26A428_PRIVATE_STATE_GRIDS[role as usize] as u64;
    let channels = derive_channel_table_for_profile(RootProfile::J873G18G26A428, role, grid,
                                                    instance.status_a, instance.main_config, SHARED);
    let region_views = views();
    let views = if role == InstanceRole::Primary { Some(&region_views) } else { None };
    let mut main = vec![0xa5u8; MAIN_CONFIG_SIZE];
    encode_main_config_for_profile(RootProfile::J873G18G26A428, role, &g18_shared(), &instance,
                                   &channels, views, &mut main).unwrap();
    main
}

#[test]
fn g18g_main_configs_use_the_26a428_views_policy_pointer_and_openings() {
    let (primary, secondary) = g18_instances();
    let main = g18_main(InstanceRole::Primary);
    assert_eq!(get_u64(&main, 0x00), SHARED);
    assert_eq!(get_u64(&main, 0x08), SHARED + 0xd400);
    assert_eq!(get_u64(&main, 0x10), SHARED + 0xd400);
    for (index, offset) in [0x2880u64, 0x34c0, 0x4540, 0xcb80, 0xcc00].iter().enumerate() {
        assert_eq!(get_u64(&main, 0x254 + index * 8), SHARED + offset);
    }
    for channel in 0..WORK_CHANNEL_COUNT {
        let entry = channel_table_entry_range(channel).unwrap().start;
        assert_eq!(get_u64(&main, entry + 0x18),
                   SHARED + G18G_26A428_WORK_RING_BUNDLE_OFFSETS[channel] as u64);
        assert_eq!(get_u64(&main, entry),
                   PRIVATE + WORK_STATE_GRID_OFFSETS[channel] as u64);
    }
    let control = channel_table_entry_range(CONTROL_CHANNEL_INDEX).unwrap().start;
    assert_eq!(get_u64(&main, control + 0x18), SHARED + G18G_26A428_CONTROL_RING_BUNDLE_OFFSETS[0] as u64);
    assert_eq!(get_u64(&main, control + 0x18), primary.main_config + MAIN_CONTROL_RING as u64);
    assert_eq!(get_u64(&main, control + 0x10),
               PRIVATE + G18G_26A428_PRIVATE_LAYOUT.control_state as u64 + 0x20);
    // Report channels keep their status-A-relative placement.
    let event = channel_table_entry_range(REPORT_CHANNEL_INDICES[0]).unwrap().start;
    assert_eq!(get_u64(&main, event), primary.status_a + 0x40);
    assert_eq!(get_u64(&main, event + 8), primary.status_a + 0x2c0);
    assert_eq!(get_u32(&main, MAIN_CONTROL_RING), 0x15);
    assert_eq!(get_u32(&main, MAIN_PRIMARY_MASK), 0xff);
    assert_eq!(get_u64(&main, 0x2d0), SENTINEL);
    assert_eq!(get_u64(&main, 0x2e0), PBDESC);
    assert_eq!(get_u64(&main, 0x2f0), UMA);
    assert_eq!(get_u64(&main, 0x2f8), 0);
    // J700-only scalars are absent.
    assert_eq!(get_u32(&main, MAIN_PRIMARY_SCALAR_32C), 0);
    assert_eq!(get_u32(&main, MAIN_PRIMARY_SCALAR_344), 0);

    let main = g18_main(InstanceRole::Secondary);
    for channel in 0..WORK_CHANNEL_COUNT {
        let range = channel_table_entry_range(channel).unwrap();
        assert!(main[range].iter().all(|byte| *byte == 0));
    }
    let control = channel_table_entry_range(CONTROL_CHANNEL_INDEX).unwrap().start;
    assert_eq!(get_u64(&main, control + 0x18), secondary.main_config + MAIN_CONTROL_RING as u64);
    assert_eq!(get_u64(&main, control + 0x10), PRIVATE + 0x177720);
    assert_eq!(get_u32(&main, MAIN_CONTROL_RING), 0x2a);
    assert_eq!(get_u64(&main, G18G_26A428_MAIN_SECONDARY_POLICY_PTR), SHARED + 0x5380);
    // The 25G76 +0x471 pointer and the +0x479 accelerator pair are gone.
    assert!(main[MAIN_SECONDARY_EXTRA_PTR..G18G_26A428_MAIN_SECONDARY_POLICY_PTR].iter().all(|b| *b == 0));
    assert_eq!(get_u32(&main, MAIN_SECONDARY_KIND), 0xc);
    assert_eq!(get_u32(&main, MAIN_PRIMARY_MASK), 0);
    assert_eq!(get_u64(&main, 0x2d8), REGION_VIEW_LOW_ADDRS[0]);
    assert_eq!(get_u64(&main, 0x2e8), REGION_VIEW_LOW_ADDRS[1]);
}

#[test]
fn g18g_device_control_vocabulary_opens_with_0x15_and_never_emits_0x20() {
    let profile = RootProfile::J873G18G26A428;
    assert_eq!(profile.control_opening_opcode(InstanceRole::Primary), 0x15);
    assert_eq!(profile.control_opening_opcode(InstanceRole::Secondary), 0x2a);
    assert_eq!(profile.control_register_opcode(), None);
    for (role, opcode) in [(InstanceRole::Primary, 0x15), (InstanceRole::Secondary, 0x2a)] {
        let mut record = [0xa5u8; CONTROL_RECORD_SIZE];
        encode_control_opening_for_profile(profile, role, &mut record).unwrap();
        assert_eq!(get_u32(&record, 0), opcode);
        assert!(record[4..].iter().all(|byte| *byte == 0));
        let mut wrong = [0u8; 2 * CONTROL_RECORD_SIZE];
        assert_eq!(encode_control_opening_for_profile(profile, role, &mut wrong),
                   Err(InitdataError::BufferSize));
    }
    for opcode in [0x15, 0x2a, 0x30, 0x0a, 0x04, 0x3b] {
        assert!(g18g_26a428_device_control_opcode_valid(opcode), "{opcode:#x}");
    }
    for opcode in [0x00, 0x03, 0x20, 0x19, 0x1c, 0x23, 0x26, 0x28, 0x31, 0x39, 0x3a, 0x3c, 0x40] {
        assert!(!g18g_26a428_device_control_opcode_valid(opcode), "{opcode:#x}");
    }
    assert_eq!(G18G_26A428_DEVICE_CONTROL_OPCODE_IDLE_POWER_OFF, DEVICE_CONTROL_OPCODE_IDLE_POWER_OFF);
    // The 25G76 primary opening opcode reaches an unrelated G18G handler.
    assert_ne!(CONTROL_OPENING_OPCODE_PRIMARY, G18G_26A428_CONTROL_OPENING_OPCODE_PRIMARY);
}

// ----------------------------------------------------------------------------------
// (b) hw_data against the firmware's own reads (plan step 1.4)
// ----------------------------------------------------------------------------------

/// 25G76 init-copier hw_data reads (offset, bytes), in program order.
const G17G_25G76_COPIER_HW_READS: &[(usize, usize)] = &[
    (0x0e90, 16), (0x0ea0, 4), (0x0ea4, 4), (0x0ec8, 4), (0x0ea8, 4), (0x0eac, 4), (0x0eb0, 4),
    (0x0eb4, 4), (0x0ec0, 4), (0x0ec4, 4), (0x0f08, 4), (0x0f0c, 4), (0x0f14, 4), (0x0f18, 4),
    (0x0f1c, 4), (0x0f20, 4), (0x0f48, 4), (0x0ee0, 4), (0x0ed0, 4), (0x0ed4, 4), (0x0ed8, 4),
    (0x0edc, 4), (0x0ef4, 4), (0x0f58, 4), (0x0efc, 4), (0x0f00, 4), (0x0f10, 4), (0x0f80, 4),
    (0x0f88, 4), (0x0f8c, 8), (0x2600, 4), (0x2604, 4), (0x2608, 8), (0x2610, 4), (0x26b4, 4),
    (0x26b8, 4), (0x26bc, 4), (0x26e8, 4), (0x0fc8, 4), (0x1408, 4), (0x1848, 4), (0x1888, 4),
    (0x18c8, 4), (0x1908, 4), (0x1b98, 4), (0x1948, 4), (0x1b9c, 4), (0x194c, 4), (0x1ba0, 4),
    (0x1950, 4), (0x1ba4, 4), (0x1954, 4), (0x1ba8, 4), (0x1958, 4), (0x1bac, 4), (0x195c, 4),
    (0x1bb0, 4), (0x1960, 4), (0x1bb4, 4), (0x1964, 4), (0x1bb8, 4), (0x1968, 4), (0x1bbc, 4),
    (0x196c, 4), (0x1bc0, 4), (0x1970, 4), (0x1bc4, 4), (0x1974, 4), (0x1bc8, 4), (0x1978, 4),
    (0x1bcc, 4), (0x197c, 4), (0x1bd0, 4), (0x1980, 4), (0x1bd4, 4), (0x1984, 4), (0x1a08, 32),
    (0x1a28, 32), (0x19c8, 32), (0x19e8, 32), (0x0fac, 4), (0x0fb0, 8), (0x0f4c, 4),
    (0x0f98, 4), (0x0f9c, 4), (0x0fa0, 4), (0x0fa4, 4), (0x0fa8, 4), (0x0f24, 8), (0x2544, 4),
    (0x2548, 4), (0x2540, 4), (0x254c, 4), (0x2550, 4), (0x2554, 4), (0x2584, 4), (0x2558, 4),
    (0x255c, 4), (0x2588, 4), (0x258c, 8), (0x2594, 4), (0x0f70, 4), (0x0f74, 4), (0x0f78, 4),
    (0x0f7c, 4), (0x2598, 4), (0x259c, 4), (0x25a0, 4), (0x25a4, 4), (0x25a8, 4), (0x25ac, 4),
    (0x26d8, 4), (0x26dc, 4), (0x25b0, 4), (0x26f0, 1), (0x26f1, 1), (0x26ec, 4), (0x08f0, 8),
];

/// 26A428 (b000) init-copier hw_data reads (offset, bytes), in program order.
const G18G_26A428_COPIER_HW_READS: &[(usize, usize)] = &[
    (0x0fb0, 16), (0x0fc0, 4), (0x0fc4, 4), (0x0fe4, 4), (0x0fc8, 4), (0x0fcc, 4), (0x0fd0, 4),
    (0x0fd4, 4), (0x0fdc, 4), (0x0fe0, 4), (0x1024, 4), (0x1028, 4), (0x1030, 4), (0x1034, 4),
    (0x1038, 4), (0x103c, 4), (0x1064, 4), (0x0ffc, 4), (0x0fec, 4), (0x0ff0, 4), (0x0ff4, 4),
    (0x0ff8, 4), (0x1010, 4), (0x1074, 4), (0x1018, 4), (0x101c, 4), (0x102c, 4), (0x109c, 4),
    (0x10a4, 4), (0x10a8, 8), (0x2720, 4), (0x2724, 4), (0x2728, 8), (0x2730, 4), (0x27dc, 4),
    (0x27e0, 4), (0x27e4, 4), (0x2810, 4), (0x1128, 4), (0x1528, 4), (0x1cb4, 4), (0x1a64, 4),
    (0x1cb8, 4), (0x1a68, 4), (0x1cbc, 4), (0x1a6c, 4), (0x1cc0, 4), (0x1a70, 4), (0x1cc4, 4),
    (0x1a74, 4), (0x1cc8, 4), (0x1a78, 4), (0x1ccc, 4), (0x1a7c, 4), (0x1cd0, 4), (0x1a80, 4),
    (0x1cd4, 4), (0x1a84, 4), (0x1cd8, 4), (0x1a88, 4), (0x1cdc, 4), (0x1a8c, 4), (0x1ce0, 4),
    (0x1a90, 4), (0x1ce4, 4), (0x1a94, 4), (0x1ce8, 4), (0x1a98, 4), (0x1cec, 4), (0x1a9c, 4),
    (0x1cf0, 4), (0x1aa0, 4), (0x1b24, 32), (0x1b44, 32), (0x1ae4, 32), (0x1b04, 32),
    (0x10cc, 8), (0x1068, 4), (0x10b8, 4), (0x10bc, 4), (0x10c0, 4), (0x10c4, 4), (0x10c8, 4),
    (0x1040, 8), (0x2660, 4), (0x2664, 4), (0x265c, 4), (0x2668, 4), (0x266c, 4), (0x2670, 4),
    (0x26a0, 4), (0x2674, 4), (0x2678, 4), (0x26a4, 4), (0x26a8, 8), (0x26b0, 4), (0x108c, 4),
    (0x1090, 4), (0x1094, 4), (0x1098, 4), (0x26b4, 4), (0x26b8, 4), (0x26bc, 4), (0x26c0, 4),
    (0x26c4, 4), (0x26c8, 4), (0x2800, 4), (0x2804, 4), (0x26cc, 4), (0x2818, 4), (0x281c, 1),
    (0x281d, 1), (0x2814, 4), (0x10b4, 4), (0x08f0, 8),
];

/// 25G76 copier reads whose b000 counterpart is not a same-shaped read:
/// +0xfac (removed in 26A428), and the six perf-loop base reads, which b000
/// performs at computed addresses (the new two-lane voltage loop).
const G17G_COPIER_READS_WITHOUT_A_B000_TWIN: [usize; 7] =
    [0x0fac, 0x0fc8, 0x1408, 0x1848, 0x1888, 0x18c8, 0x1908];
/// b000 copier reads that no 25G76 read relocates onto: two new fields and
/// the second voltage lane of the core and SRAM rows.
const G18G_COPIER_READS_WITHOUT_A_G17_TWIN: [usize; 4] = [0x10b4, 0x2818, 0x1128, 0x1528];

/// The 26A428 place of a 25G76 hw_data byte: register-map bytes follow their
/// renumbered slot, everything else follows the bands.
fn relocate_hw_data(offset: usize) -> Option<usize> {
    if (HW_REGISTER_MAP_OFFSET..HW_REGISTER_MAP_OFFSET + HW_REGISTER_MAP_SLOTS * HW_REGISTER_MAP_ENTRY_SIZE)
        .contains(&offset)
    {
        let slot = (offset - HW_REGISTER_MAP_OFFSET) / HW_REGISTER_MAP_ENTRY_SIZE;
        let field = (offset - HW_REGISTER_MAP_OFFSET) % HW_REGISTER_MAP_ENTRY_SIZE;
        return G18G_26A428_SLOT_FROM_25G76[slot]
            .map(|slot| HW_REGISTER_MAP_OFFSET + slot as usize * HW_REGISTER_MAP_ENTRY_SIZE + field);
    }
    relocate_by_bands(G18G_26A428_HW_DATA_BANDS, offset)
}

/// Byte ranges of the G18G hw_data object the encoder owns: the relocation
/// targets of every band, the G18G register map, the perf tables, the
/// completion table, the QoS pointer and the new 26A428 fields.
fn g18_hw_data_owned(offset: usize, len: usize) -> bool {
    let end = offset + len;
    if end > G18G_26A428_HW_DATA_SIZE {
        return false;
    }
    (offset..end).all(|byte| {
        (HW_REGISTER_MAP_OFFSET..G18G_26A428_HW_REGISTER_MAP_END).contains(&byte)
            || G18G_26A428_HW_DATA_BANDS.iter().any(|band| {
                let start = (band.start as isize + band.delta) as usize;
                let stop = (band.end as isize + band.delta) as usize;
                (start..stop).contains(&byte)
            })
            || G18G_26A428_HW_NEW_FIELDS.iter().any(|&(field, size)| (field..field + size).contains(&byte))
    })
}

#[test]
fn g18g_copier_reads_land_on_fields_the_encoder_owns() {
    for &(offset, len) in G18G_26A428_COPIER_HW_READS {
        assert!(g18_hw_data_owned(offset, len), "b000 reads hw+{offset:#x} ({len} bytes)");
    }
    // Every 26A428 kext-only field is owned too, and none overlaps another.
    for (index, &(field, size)) in G18G_26A428_HW_NEW_FIELDS.iter().enumerate() {
        assert!(field + size <= G18G_26A428_HW_DATA_SIZE);
        for &(other, other_size) in &G18G_26A428_HW_NEW_FIELDS[index + 1..] {
            assert!(field + size <= other || other + other_size <= field);
        }
    }
    // The highest offset b000 reads (+0x283a) fits the 0x2848 kext size.
    assert!(0x283a + 4 <= G18G_26A428_HW_DATA_SIZE);
    // Band targets never overlap each other or the register map.
    let mut targets: Vec<(usize, usize)> = G18G_26A428_HW_DATA_BANDS
        .iter()
        .map(|band| ((band.start as isize + band.delta) as usize, (band.end as isize + band.delta) as usize))
        .collect();
    targets.push((HW_REGISTER_MAP_OFFSET, G18G_26A428_HW_REGISTER_MAP_END));
    targets.sort();
    for pair in targets.windows(2) {
        assert!(pair[0].1 <= pair[1].0, "overlap {pair:x?}");
    }
    assert!(targets.last().unwrap().1 <= G18G_26A428_HW_DATA_SIZE);
}

#[test]
fn g18g_bands_map_every_25g76_copier_read_onto_a_b000_read() {
    let b000: Vec<(usize, usize)> = G18G_26A428_COPIER_HW_READS.to_vec();
    let mut produced = Vec::new();
    for &(offset, len) in G17G_25G76_COPIER_HW_READS {
        if G17G_COPIER_READS_WITHOUT_A_B000_TWIN.contains(&offset) {
            continue;
        }
        let target = relocate_hw_data(offset).unwrap_or_else(|| panic!("25G76 read {offset:#x} dropped"));
        assert!(b000.contains(&(target, len)), "25G76 read {offset:#x} -> {target:#x} is not a b000 read");
        produced.push((target, len));
    }
    for &(offset, len) in &b000 {
        assert!(
            produced.contains(&(offset, len)) || G18G_COPIER_READS_WITHOUT_A_G17_TWIN.contains(&offset),
            "b000 read {offset:#x} has no 25G76 twin"
        );
    }
    // The removed flag has no place; the perf-loop bases land on the G18G
    // perf tables the encoder fills.
    assert_eq!(relocate_hw_data(0x0fac), None);
    assert_eq!(relocate_hw_data(0x0fc8), Some(G18G_26A428_HW_PERF_FREQUENCY));
    assert_eq!(relocate_hw_data(0x1008), Some(G18G_26A428_HW_PERF_CORE_VOLTAGE));
    assert_eq!(relocate_hw_data(0x1408), Some(G18G_26A428_HW_PERF_SRAM_VOLTAGE));
    assert_eq!(relocate_hw_data(0x1848), Some(G18G_26A428_HW_PERF_SCALE));
    assert_eq!(relocate_hw_data(0x18c8), Some(G18G_26A428_HW_PERF_RELATIVE_A));
    assert_eq!(relocate_hw_data(0x1908), Some(G18G_26A428_HW_PERF_RELATIVE_B));
    assert_eq!(relocate_hw_data(0x19c8), Some(G18G_26A428_HW_PERF_INDEX_MAP));
    // Lane 1 of row 0 of both voltage tables.
    assert_eq!(G18G_26A428_HW_PERF_CORE_VOLTAGE + 4, 0x1128);
    assert_eq!(G18G_26A428_HW_PERF_SRAM_VOLTAGE + 4, 0x1528);
    // The named relocations of the plan: chip id, completion table, QoS.
    assert_eq!(relocate_hw_data(HW_CHIP_ID_OFFSET), Some(G18G_26A428_HW_CHIP_ID_OFFSET));
    assert_eq!(relocate_hw_data(G17P_KSM_COMPLETION_TABLE_OFFSET), Some(G18G_26A428_KSM_COMPLETION_TABLE_OFFSET));
    assert_eq!(relocate_hw_data(G17P_QOS_RESOURCE_LIVE_OFFSET), Some(G18G_26A428_QOS_POINTER_OFFSET));
    assert_eq!(relocate_hw_data(0xe88), Some(0xfa0));
    assert_eq!(relocate_hw_data(0xfc4), Some(G18G_26A428_HW_PERF_STATE_COUNT));
    // Slot 8 (StreamingCodecRegisters) has no 26A428 slot; slot 36 becomes 41.
    assert_eq!(relocate_hw_data(HW_REGISTER_MAP_OFFSET + 8 * HW_REGISTER_MAP_ENTRY_SIZE), None);
    assert_eq!(relocate_hw_data(0xbe8), Some(0xcb0));
}

fn g18_hw_data() -> Vec<u8> {
    let mut hw = vec![0xa5u8; G18G_26A428_HW_DATA_SIZE];
    encode_g18g_26a428_hw_data(completion_aliases(), QOS, &mut hw).unwrap();
    hw
}

#[test]
fn g18g_hw_data_moves_every_j813_value_and_leaves_none_behind() {
    let hw = g18_hw_data();
    // Byte sources, keyed by 26A428 offset.
    let mut expected = vec![None::<u8>; G18G_26A428_HW_DATA_SIZE];
    let mut dropped = Vec::new();
    for &(offset, value) in M5_25G76_HW_PLATFORM_WORDS {
        for (index, byte) in value.to_le_bytes().into_iter().enumerate() {
            if byte == 0 {
                continue;
            }
            match relocate_by_bands(G18G_26A428_HW_DATA_BANDS, offset + index) {
                Some(target) => {
                    assert!(expected[target].is_none());
                    expected[target] = Some(byte);
                }
                None => dropped.push(offset + index),
            }
        }
    }
    assert_eq!(dropped, [0x0fac], "only the removed +0xfac flag has no 26A428 place");

    let mut map = vec![0u8; G18G_26A428_HW_REGISTER_MAP_END - HW_REGISTER_MAP_OFFSET];
    encode_g18g_26a428_register_map(&mut map).unwrap();
    let mut completion = [0u8; 0x80];
    encode_m5_25g76_completion_table(completion_aliases(), &mut completion).unwrap();
    let mut explicit = vec![None::<u8>; G18G_26A428_HW_DATA_SIZE];
    let mut set = |offset: usize, bytes: &[u8]| {
        for (index, byte) in bytes.iter().enumerate() {
            explicit[offset + index] = Some(*byte);
        }
    };
    set(HW_REGISTER_MAP_OFFSET, &map);
    set(G18G_26A428_HW_CHIP_ID_OFFSET, &0x8152u32.to_le_bytes());
    set(G18G_26A428_HW_PERF_STATE_COUNT, &13u32.to_le_bytes());
    set(G18G_26A428_HW_2714_OFFSET, &0x96u32.to_le_bytes());
    // Independent values from both roles in captures 051/053/073.
    for (offset, value) in [(0xfb4, 1u32), (0x10d4, 0x1b), (0x10d8, 4),
                            (0x10dc, 12), (0x283a, 1), (0x283e, 1)] {
        set(offset, &value.to_le_bytes());
    }
    for (state, &(frequency, core)) in J873_GPU_PERF_STATES.iter().enumerate() {
        set(G18G_26A428_HW_PERF_FREQUENCY + state * 4, &frequency.to_le_bytes());
        for lane in 0..16 {
            set(G18G_26A428_HW_PERF_CORE_VOLTAGE + state * 0x40 + lane * 4, &core.to_le_bytes());
            set(G18G_26A428_HW_PERF_SRAM_VOLTAGE + state * 0x40 + lane * 4,
                &J873_GPU_PERF_STATES_SRAM_MV[state].to_le_bytes());
        }
        set(G18G_26A428_HW_PERF_SCALE + state * 4, &0x3f82_8f5cu32.to_le_bytes());
    }
    for index in 0..16u32 {
        set(G18G_26A428_HW_PERF_INDEX_MAP + index as usize * 4, &index.to_le_bytes());
    }
    set(G18G_26A428_KSM_COMPLETION_TABLE_OFFSET, &completion);
    set(G18G_26A428_QOS_POINTER_OFFSET, &QOS.to_le_bytes());

    for (offset, byte) in hw.iter().enumerate() {
        // The two GUESSED relative ladders are checked for shape below.
        let ladder = (G18G_26A428_HW_PERF_RELATIVE_A..G18G_26A428_HW_PERF_RELATIVE_B + 14 * 4).contains(&offset);
        match (explicit[offset], expected[offset]) {
            (Some(value), _) => assert_eq!(*byte, value, "G18G field byte hw+{offset:#x}"),
            (None, Some(value)) => assert_eq!(*byte, value, "relocated J813 byte hw+{offset:#x}"),
            (None, None) if ladder => {}
            (None, None) => assert_eq!(*byte, 0, "unexplained byte hw+{offset:#x} = {byte:#x}"),
        }
    }
    // Relative ladders: 14 states, monotonic, 0 at state 0 and 100 at the top.
    for base in [G18G_26A428_HW_PERF_RELATIVE_A, G18G_26A428_HW_PERF_RELATIVE_B] {
        let ladder: Vec<u32> = (0..14).map(|state| get_u32(&hw, base + state * 4)).collect();
        assert_eq!((ladder[0], ladder[13]), (0, 100));
        assert!(ladder.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(get_u32(&hw, base + 14 * 4), 0);
    }

    // Spot checks of the plan's named moves, and of what now occupies the
    // old 25G76 places.
    assert_eq!(get_u32(&hw, 0xfb0), 0x8152);
    assert_eq!(get_u32(&hw, 0xfb4), 1);
    assert_ne!(get_u32(&hw, 0xe90), 0x8142);
    assert_eq!(get_u32(&hw, 0x2714), 0x96);
    assert_eq!(get_u32(&hw, 0x2710), 1, "25G76 +0x25f4 flag");
    assert_eq!(get_u32(&hw, 0x2720), 1);
    assert_eq!(get_u64(&hw, 0x2728), 1);
    assert_eq!(get_u32(&hw, 0x2730), 0x100);
    assert_eq!(get_u64(&hw, 0x27cc), QOS);
    assert_ne!(get_u64(&hw, G17P_QOS_RESOURCE_LIVE_OFFSET), QOS);
    for ordinal in 0..4 {
        let base = 0x2734 + ordinal * 0x20;
        assert_eq!(get_u64(&hw, base), completion_aliases()[ordinal].low);
        assert_eq!(get_u32(&hw, base + 0x18), ordinal as u32);
        assert_eq!(get_u32(&hw, base + G18G_26A428_KSM_COMPLETION_CURSOR), 0);
    }
    assert_eq!(get_u32(&hw, 0x10e4 + 13 * 4), 1735);
    assert_eq!(get_u32(&hw, 0x1124 + 13 * 0x40), 990);
    assert_eq!(get_u32(&hw, 0x1128 + 13 * 0x40), 990);
    assert_eq!(get_u32(&hw, 0x1524), 775);
    assert_eq!(get_u32(&hw, 0x1528 + 6 * 0x40), 795);
    assert_eq!(get_u32(&hw, 0x1124 + 14 * 0x40), 0, "rows 14 and 15 stay empty");
    for (field, size) in G18G_26A428_HW_NEW_FIELDS {
        if field != G18G_26A428_HW_2714_OFFSET && field != 0x283a && field != 0x283e {
            assert!(hw[field..field + size].iter().all(|byte| *byte == 0));
        }
    }
    // Unchanged and relocated 26A428 constants the kext writes itself.
    assert_eq!(get_u32(&hw, 0xfe4), 1);
    assert_eq!(get_u32(&hw, 0xfec), 24000);
    assert_eq!(get_u64(&hw, 0x1000), 0x1_0000_0001);
    assert_eq!(get_u32(&hw, 0x1020), 0x1f);
    assert_eq!((get_u32(&hw, 0x1050), get_u32(&hw, 0x1054)), (1, 1));
    assert!((0..6).all(|index| get_u64(&hw, 0x26d0 + index * 8) == u64::MAX));
    assert_eq!(get_u32(&hw, 0x27e0), 1);
    assert_eq!(get_u64(&hw, 0x2810), 1);
    assert_eq!(get_u64(&hw, 0x00), 0x6f_0000_0000);
    assert_eq!(get_u64(&hw, 0x20), 0x2ff_ffff_8000);
}

#[test]
fn g18g_register_map_renumbers_rebases_and_zeroes_slots_24_and_41() {
    let hw = g18_hw_data();
    let slot = |index: usize| {
        let base = HW_REGISTER_MAP_OFFSET + index * HW_REGISTER_MAP_ENTRY_SIZE;
        &hw[base..base + HW_REGISTER_MAP_ENTRY_SIZE]
    };
    assert_eq!(G18G_26A428_HW_REGISTER_MAP_END, 0xfa0);
    assert!(slot(G18G_26A428_ZEROED_REGISTER_SLOT).iter().all(|byte| *byte == 0));
    // Both roles in native 26A428 captures 051/053/073 leave DVFMMgpu absent.
    assert!(slot(24).iter().all(|byte| *byte == 0));
    for index in G18G_26A428_REGISTER_FLAG_ONLY_SLOTS {
        let entry = slot(index);
        assert_eq!(get_u32(entry, 0x20), 2);
        assert!(entry[..0x20].iter().all(|byte| *byte == 0), "flag-only slot {index}");
    }
    let mut populated = vec![false; G18G_26A428_HW_REGISTER_MAP_SLOTS];
    for window in G18G_26A428_REGISTER_WINDOWS {
        assert!(!populated[window.slot]);
        populated[window.slot] = true;
        let entry = slot(window.slot);
        assert_eq!(get_u64(entry, 0), window.phys);
        assert_eq!(get_u64(entry, 8), window.device_va);
        assert_eq!(get_u32(entry, 0x10), window.size_10);
        assert_eq!(get_u32(entry, 0x14), window.size_14);
        assert_eq!(get_u64(entry, 0x18), window.unk_18);
        assert_eq!(get_u32(entry, 0x20), window.flag);
        assert!(window.mapped_pages().is_ok(), "slot {}", window.slot);
        assert!(HW_REGISTER_APERTURE.contains(&window.device_va));
    }
    for index in G18G_26A428_REGISTER_FLAG_ONLY_SLOTS {
        assert!(!populated[index]);
    }
    // Mapped pages never overlap.
    let mut spans: Vec<(u64, u64)> = G18G_26A428_REGISTER_WINDOWS
        .iter()
        .map(|window| {
            let (_, va, span) = window.mapped_pages().unwrap();
            (va, va + span)
        })
        .collect();
    spans.sort();
    assert!(spans.windows(2).all(|pair| pair[0].1 <= pair[1].0));
    // Every J813 window that survives keeps its physical page (SGX and GPU
    // PMGR) or moves by exactly the AON rebase; slot 12 (ANE0) is dropped.
    for m5 in M5_25G76_REGISTER_WINDOWS {
        let Some(new_slot) = G18G_26A428_SLOT_FROM_25G76[m5.slot] else {
            assert_eq!(m5.slot, 8);
            continue;
        };
        let Some(g18) = G18G_26A428_REGISTER_WINDOWS.iter().find(|w| w.slot == new_slot as usize) else {
            assert!(matches!(m5.slot, 12 | 24 | 42 | 50), "J813 slot {} lost its window", m5.slot);
            continue;
        };
        if m5.phys >= 0x3_8000_0000 && m5.phys < 0x3_9000_0000 {
            assert_eq!(g18.phys, m5.phys - J873_AON_REBASE, "slot {}", m5.slot);
        } else if m5.slot != 39 {
            assert_eq!(g18.phys, m5.phys, "slot {}", m5.slot);
            assert_eq!(g18.unk_18, m5.unk_18, "slot {}", m5.slot);
        }
    }
    // The 25G76 slot numbering maps injectively onto 26A428 slots.
    let mut seen = vec![false; G18G_26A428_HW_REGISTER_MAP_SLOTS];
    for new_slot in G18G_26A428_SLOT_FROM_25G76.into_iter().flatten() {
        assert!(!seen[new_slot as usize]);
        seen[new_slot as usize] = true;
    }
    assert_eq!(G18G_26A428_SLOT_FROM_25G76[36], Some(G18G_26A428_ZEROED_REGISTER_SLOT as u8));
    // The J873 AIC SW-interrupt page (ADT meta-sw-interrupt 0x301014048).
    assert_eq!(G18G_26A428_REGISTER_WINDOWS[0].phys, 0x3_0101_4048 & !0x3fff);
}

// ----------------------------------------------------------------------------------
// Region C (plan step 1.5)
// ----------------------------------------------------------------------------------

const G17G_25G76_REGION_C_READS: &[(usize, usize)] = &[
    (0x0000, 4), (0x0004, 4), (0x0008, 4), (0x000c, 4), (0x0010, 4), (0x0014, 4), (0x005a, 4),
    (0x001c, 4), (0x0020, 4), (0x0028, 4), (0x002c, 4), (0x0030, 4), (0x0034, 4), (0x003c, 4),
    (0x09c8, 4), (0x09d4, 4), (0x09cc, 4), (0x09d8, 4), (0x0024, 4), (0x09d0, 4), (0x0048, 4),
    (0x0054, 2), (0x0056, 2), (0x0058, 2), (0x004c, 4), (0x0050, 4), (0x005e, 4), (0x006a, 4),
    (0x006e, 4), (0x0066, 4), (0x09b8, 4), (0x09bc, 4), (0x09c0, 4), (0x0e59, 4), (0x0998, 4),
    (0x099c, 4), (0x09a0, 4), (0x09a4, 4), (0x0e49, 16), (0x0e39, 16), (0x09a8, 4), (0x09ac, 4),
    (0x07cc, 4), (0x07d0, 4), (0x07d4, 4), (0x07d8, 4), (0x07dc, 4), (0x07e0, 4), (0x07e4, 4),
    (0x07e8, 4), (0x07c4, 4), (0x07c8, 4), (0x09c4, 4), (0x09dc, 4), (0x0e0c, 4), (0x0e10, 4),
    (0x07f4, 4), (0x0e14, 4), (0x07f8, 4), (0x0e18, 4), (0x0e1c, 1), (0x0e1d, 4), (0x0e21, 4),
    (0x0e25, 4), (0x0e29, 4), (0x0e2d, 4), (0x0e31, 4), (0x0e35, 4), (0x0e5d, 4), (0x0e61, 4),
    (0x0e65, 4), (0x0e69, 4), (0x0e6d, 4),
];

const G18G_26A428_REGION_C_READS: &[(usize, usize)] = &[
    (0x0000, 4), (0x0004, 4), (0x0008, 4), (0x000c, 4), (0x0010, 4), (0x0014, 4), (0x005e, 4),
    (0x001c, 4), (0x0020, 4), (0x0028, 4), (0x002c, 4), (0x0030, 4), (0x0034, 4), (0x0038, 4),
    (0x0040, 4), (0x09cc, 4), (0x09d8, 4), (0x09d0, 4), (0x09dc, 4), (0x0024, 4), (0x09d4, 4),
    (0x004c, 4), (0x0058, 2), (0x005a, 2), (0x005c, 2), (0x0050, 4), (0x0054, 4), (0x0062, 4),
    (0x006e, 4), (0x0072, 4), (0x006a, 4), (0x09bc, 4), (0x09c0, 4), (0x09c4, 4), (0x0e5d, 4),
    (0x099c, 4), (0x09a0, 4), (0x09a4, 4), (0x09a8, 4), (0x0e4d, 16), (0x0e3d, 16), (0x09ac, 4),
    (0x09b0, 4), (0x07d0, 4), (0x07d4, 4), (0x07d8, 4), (0x07dc, 4), (0x07e0, 4), (0x07e4, 4),
    (0x07e8, 4), (0x07ec, 4), (0x07c8, 4), (0x07cc, 4), (0x09c8, 4), (0x09e0, 4), (0x0e10, 4),
    (0x0e14, 4), (0x07f8, 4), (0x0e18, 4), (0x07fc, 4), (0x0e1c, 4), (0x0e20, 1), (0x0e21, 4),
    (0x0e25, 4), (0x0e29, 4), (0x0e2d, 4), (0x0e31, 4), (0x0e35, 4), (0x0e39, 4), (0x0e61, 4),
    (0x0e65, 4), (0x0e69, 4), (0x0e6d, 4), (0x0e71, 4),
];

#[test]
fn g18g_region_c_shifts_by_four_from_0x38_and_carries_the_j873_tunables() {
    // Loader-level proof of the band table.
    let mut produced = Vec::new();
    for &(offset, len) in G17G_25G76_REGION_C_READS {
        let target = relocate_by_bands(G18G_26A428_REGION_C_BANDS, offset).unwrap();
        assert!(G18G_26A428_REGION_C_READS.contains(&(target, len)), "{offset:#x} -> {target:#x}");
        produced.push((target, len));
    }
    let extra: Vec<_> = G18G_26A428_REGION_C_READS.iter().filter(|read| !produced.contains(read)).collect();
    assert_eq!(extra, [&(0x38, 4)]);
    for &(offset, len) in G18G_26A428_REGION_C_READS {
        assert!(offset + len <= G18G_26A428_REGION_C_ALLOCATION);
    }

    let mut m5 = vec![0xa5u8; REGION_C_SIZE];
    encode_region_c_for_profile(RootProfile::J813G17G25G76, &mut m5).unwrap();
    let mut g18 = vec![0xa5u8; REGION_C_SIZE];
    encode_region_c_for_profile(RootProfile::J873G18G26A428, &mut g18).unwrap();
    let mut direct = vec![0x5au8; REGION_C_SIZE];
    encode_g18g_26a428_region_c(&mut direct).unwrap();
    assert_eq!(g18, direct);
    let overrides: Vec<usize> = G18G_26A428_REGION_C_ADT_WORDS
        .iter()
        .flat_map(|&(offset, _)| offset..offset + 4)
        .collect();
    for offset in 0..0xe78 {
        let target = relocate_by_bands(G18G_26A428_REGION_C_BANDS, offset).unwrap();
        if !overrides.contains(&target) {
            assert_eq!(g18[target], m5[offset], "J813 region C +{offset:#x}");
        }
    }
    assert_eq!(get_u32(&g18, 0x38), 0);
    assert_eq!(get_u32(&g18, 0x40), 0x78, "26A428 kext stores 0x78 at +0x40");
    assert_eq!((get_u32(&g18, 0x30), get_u32(&g18, 0x34)), (1, 1));
    assert_eq!(get_u32(&g18, 0x7c8), 1500);
    assert_eq!(get_u32(&g18, 0x7e4), 0x3f4c_cccd);
    assert_eq!(get_u32(&g18, 0x9c), 1300);
    assert_eq!(get_u32(&m5, 0x98), 1200);
    assert_eq!(get_u32(&m5, 0x7c4), 1250, "J813 carried its ADT idleoff-standby-timer here");
    assert_eq!(get_u32(&g18, 0xe5d), get_u32(&m5, 0xe59));
    assert_eq!(get_u32(&g18, 0xe61), 1, "26A428 kext stores 1 at +0xe61");
    assert!(g18[G18G_26A428_REGION_C_ALLOCATION..].iter().all(|byte| *byte == 0));
}

// ----------------------------------------------------------------------------------
// Private cluster (plan step 1.3)
// ----------------------------------------------------------------------------------

#[test]
fn g18g_private_cluster_carries_status_links_producers_and_relocated_config() {
    let layout = G18G_26A428_PRIVATE_LAYOUT;
    let mut private = vec![0xa5u8; G18G_26A428_PRIVATE_CLUSTER_SIZE];
    encode_g18g_26a428_private_cluster(PRIVATE, FWCTL, &mut private).unwrap();
    let status_b = layout.status_b;
    let config = layout.status_config;
    let mut expected = vec![None::<u8>; private.len()];
    let mut set = |offset: usize, bytes: &[u8]| {
        for (index, byte) in bytes.iter().enumerate() {
            assert!(expected[offset + index].is_none() || expected[offset + index] == Some(*byte));
            expected[offset + index] = Some(*byte);
        }
    };
    set(status_b + 4, &1u32.to_le_bytes());
    set(status_b + G18G_26A428_STATUS_B_FWCTL_STATE, &FWCTL.to_le_bytes());
    set(status_b + G18G_26A428_STATUS_B_FWCTL_RING, &(FWCTL + 0x40).to_le_bytes());
    set(status_b + G18G_26A428_STATUS_B_CONFIG_HEADER, &1u32.to_le_bytes());
    set(status_b + G18G_26A428_STATUS_B_LINK_WORD, &0u32.to_le_bytes());
    set(status_b + G18G_26A428_STATUS_B_LINK_PTR,
        &(PRIVATE + (config + G18G_26A428_STATUS_CONFIG_LINK_WORD) as u64).to_le_bytes());
    set(config + G18G_26A428_STATUS_CONFIG_LINK_WORD, &1u32.to_le_bytes());
    set(config + G18G_26A428_STATUS_CONFIG_LINK_PTR,
        &(PRIVATE + (status_b + G18G_26A428_STATUS_B_LINK_WORD) as u64).to_le_bytes());
    let mut relocated = 0;
    for &(offset, value) in M5_25G76_STATUS_CONFIG_WORDS {
        for (index, byte) in value.to_le_bytes().into_iter().enumerate() {
            if byte == 0 {
                continue;
            }
            let target = relocate_by_bands(G18G_26A428_STATUS_CONFIG_BANDS, offset + index)
                .unwrap_or_else(|| panic!("status config +{:#x} has no band", offset + index));
            set(config + target, &[byte]);
            relocated += 1;
        }
    }
    // Every non-zero J813 status-config byte found a band (no drops).
    let nonzero: usize = M5_25G76_STATUS_CONFIG_WORDS
        .iter()
        .map(|(_, value)| value.to_le_bytes().iter().filter(|byte| **byte != 0).count())
        .sum();
    assert_eq!(relocated, nonzero);
    assert_eq!(M5_25G76_STATUS_CONFIG_WORDS.len(), 48);
    for role in layout.roles {
        set(role.status_a + 4, &1u32.to_le_bytes());
    }
    let mut extra = [0u8; STATUS_BLOCK_SIZE];
    encode_secondary_root_extra_1_for_profile(RootProfile::J873G18G26A428, &mut extra).unwrap();
    set(layout.secondary_extra_1, &extra);
    set(0x320, &1u32.to_le_bytes());
    set(0x177720, &1u32.to_le_bytes());
    for (offset, byte) in private.iter().enumerate() {
        assert_eq!(*byte, expected[offset].unwrap_or(0), "private +{offset:#x}");
    }
    // The J813 words keep their meaning at the relocated places.
    assert_eq!(get_u32(&private, config + 0x10), 0x1f);
    assert_eq!(get_u32(&private, config + 0x588 + 0x1c), 1);
    assert_eq!(get_u32(&private, config + 0x620 + 0x18), 0x44c);
    for poll in G18G_26A428_STATUS_CONFIG_SUBMIT_POLLS {
        assert_eq!(get_u32(&private, config + poll), 0);
    }
    for field in [G18G_26A428_STATUS_B_RECOVERY_EPOCH, G18G_26A428_STATUS_B_RECOVERY_STATE] {
        assert_eq!(get_u64(&private, status_b + field), 0);
    }
    // Status config stays inside its 0x6d0 allocation, status B inside 0xe730.
    assert!(G18G_26A428_STATUS_CONFIG_LINK_PTR + 8 <= G18G_26A428_STATUS_CONFIG_SIZE);
    assert!(G18G_26A428_STATUS_B_LINK_PTR + 8 <= G18G_26A428_STATUS_B_SIZE);
    assert_eq!(get_u32(&private, 0x177720), 1);
    assert_eq!(0x177720, G18G_26A428_PRIVATE_STATE_GRIDS[1]
        + WORK_STATE_GRID_OFFSETS[CONTROL_CHANNEL_INDEX] + 2 * CHANNEL_STATE_SPACING);

    // Invalid bindings fail closed before anything is written.
    let mut arena = vec![0xa5u8; G18G_26A428_PRIVATE_CLUSTER_SIZE];
    for (base, fwctl) in [(0, FWCTL), (PRIVATE, 0), (1 << 48, FWCTL), (PRIVATE, PRIVATE + 0x1000)] {
        assert!(encode_g18g_26a428_private_cluster(base, fwctl, &mut arena).is_err());
        assert!(arena.iter().all(|byte| *byte == 0xa5));
    }
    assert_eq!(encode_g18g_26a428_private_cluster(PRIVATE, FWCTL, &mut arena[..0x1000]),
               Err(InitdataError::BufferSize));
}

// ----------------------------------------------------------------------------------
// Perf/policy block (plan step 1.6)
// ----------------------------------------------------------------------------------

#[test]
fn g18g_policy_block_is_zero_except_relocated_and_known_fields() {
    let layout = G18G_26A428_SHARED_LAYOUT;
    let m5 = M5_25G76_SHARED_LAYOUT;
    let state = PRIVATE + G18G_26A428_PRIVATE_LAYOUT.policy_state as u64;
    let mut bundle = vec![0xa5u8; G18G_26A428_SHARED_CLUSTER_SIZE];
    encode_g18g_26a428_bundle_policy(
        M5PolicyBindings { state, aon_ptd: AON, boot_timestamp: TIMESTAMP },
        &mut bundle,
    )
    .unwrap();
    // hw_data and everything from the BRN table on are untouched.
    assert!(bundle[..layout.block_c18].iter().all(|byte| *byte == 0xa5));
    assert!(bundle[G18G_26A428_SHARED_PREFIX_SIZE..].iter().all(|byte| *byte == 0xa5));

    let policy = layout.policy;
    let mut expected = vec![None::<u8>; G18G_26A428_SHARED_PREFIX_SIZE];
    let mut dropped = Vec::new();
    for &(offset, value) in M5_25G76_POLICY_WORDS {
        for (index, byte) in value.to_le_bytes().into_iter().enumerate() {
            if byte == 0 {
                continue;
            }
            if (m5.block_1048..m5.block_e10).contains(&offset) {
                expected[layout.block_1048 + offset - m5.block_1048 + index] = Some(byte);
                continue;
            }
            match relocate_by_bands(G18G_26A428_POLICY_BANDS, offset - m5.policy + index) {
                Some(target) => expected[policy + target] = Some(byte),
                None => dropped.push(offset - m5.policy + index),
            }
        }
    }
    // Only the J813 RT0g/LFCR/LFXG/LPXG descriptor words are dropped.
    assert_eq!(dropped.len(), 18);
    assert!(dropped.iter().all(|offset| (0x6468..0x651c).contains(offset)));
    for index in 0..M5_25G76_POLICY_U16_TABLE_ENTRIES * 2 {
        let target = relocate_by_bands(G18G_26A428_POLICY_BANDS, M5_25G76_POLICY_U16_TABLE + index).unwrap();
        expected[policy + target] = Some(if index % 2 == 0 { 0xff } else { 0x7f });
    }
    let mut set = |offset: usize, bytes: &[u8]| {
        for (index, byte) in bytes.iter().enumerate() {
            expected[policy + offset + index] = Some(*byte);
        }
    };
    set(G18G_26A428_POLICY_STATE_PTR, &state.to_le_bytes());
    set(G18G_26A428_POLICY_AON_PTD_PTR, &AON.to_le_bytes());
    set(G18G_26A428_POLICY_BOOT_TIMESTAMP, &TIMESTAMP.to_le_bytes());
    for (offset, value) in G18G_26A428_POLICY_ADT_WORDS {
        set(offset, &value.to_le_bytes());
    }
    for offset in layout.block_c18..G18G_26A428_SHARED_PREFIX_SIZE {
        assert_eq!(bundle[offset], expected[offset].unwrap_or(0), "bundle +{offset:#x}");
    }

    let word = |offset: usize| get_u32(&bundle, policy + offset);
    assert_eq!(get_u64(&bundle, policy + 0x6c14), TIMESTAMP);
    assert_eq!(get_u64(&bundle, policy + 0x3d30), state);
    assert_eq!(get_u64(&bundle, policy + 0x64d8), AON);
    assert_eq!(word(0xb38), 75);
    assert_eq!((word(0xb6c), word(0xb70)), (0x3f92_e148, 0x3f92_e148));
    assert_eq!((word(0xb7c), word(0xb80)), (0x4092_e148, 0x4092_e148));
    assert_eq!(f32::from_bits(word(0xb6c)), 1.1475);
    assert_eq!(f32::from_bits(word(0xb7c)), 4.59);
    // 26A428 stores tgt - dz at +0xb9c and dz at +0xba4.
    assert_eq!(word(0xb9c), word(0xb38) - word(0xba4));
    assert_eq!((word(0x22d4), word(0x2928), word(0x2e1c)), (1300, 1300, 1300));
    assert_eq!(word(0x0c), 0, "26A428 stores 0x4_0000_0000 at +0xc as a u64");
    assert_eq!(word(0x10), 4);
    assert_eq!(word(0x50), 100);
    assert_eq!(word(0x30cc), 1, "25G76 +0x305c constant 1");
    assert_eq!(get_u32(&bundle, layout.block_1048 + 0xa18), u32::MAX);
    assert_eq!(get_u32(&bundle, layout.block_1048 + 0xa30), u32::MAX);
    // The 0x7800 block ends inside the prefix.
    assert!(policy + G18G_26A428_POLICY_SIZE <= layout.block_68);
    assert!(G18G_26A428_POLICY_BANDS.iter().all(|band| {
        ((band.end as isize + band.delta) as usize) <= G18G_26A428_POLICY_SIZE
    }));

    // Invalid bindings fail closed.
    let mut small = vec![0u8; G18G_26A428_SHARED_PREFIX_SIZE - 1];
    assert!(encode_g18g_26a428_bundle_policy(
        M5PolicyBindings { state, aon_ptd: AON, boot_timestamp: 0 }, &mut small).is_err());
    assert!(encode_g18g_26a428_bundle_policy(
        M5PolicyBindings { state: 0, aon_ptd: AON, boot_timestamp: 0 }, &mut bundle).is_err());
    assert!(encode_g18g_26a428_bundle_policy(
        M5PolicyBindings { state: AON, aon_ptd: AON, boot_timestamp: 0 }, &mut bundle).is_err());
}

// ----------------------------------------------------------------------------------
// Bookkeeping
// ----------------------------------------------------------------------------------

#[test]
fn g18g_unproven_table_names_real_fields() {
    let limit = |object: &str| match object {
        "hw_data" => G18G_26A428_HW_DATA_SIZE,
        "register_map" => G18G_26A428_HW_REGISTER_MAP_SLOTS,
        "policy" => G18G_26A428_POLICY_SIZE,
        "region_c" => G18G_26A428_REGION_C_ALLOCATION,
        "status_b" => G18G_26A428_STATUS_B_SIZE,
        "status_config" => G18G_26A428_STATUS_CONFIG_SIZE,
        "main_config" => MAIN_CONFIG_SIZE,
        "aon_ptd" => G18G_26A428_AON_PTD_SIZE,
        "secondary_extra_1" => KEXT_ALLOCATION_SIZES_26A428[KX_SECONDARY_EXTRA_1],
        other => panic!("unknown object {other}"),
    };
    for row in G18G_26A428_UNPROVEN {
        assert!(row.len > 0 && !row.note.is_empty());
        assert!(row.offset + row.len <= limit(row.object), "{row:?}");
        assert!(matches!(row.evidence, Evidence::Inferred | Evidence::Guessed));
    }
    // Every GUESSED new hw_data field is listed.
    for (field, _) in G18G_26A428_HW_NEW_FIELDS {
        if field == G18G_26A428_HW_2714_OFFSET || field == 0x1078 {
            continue;
        }
        assert!(
            G18G_26A428_UNPROVEN.iter().any(|row| row.object == "hw_data"
                && (row.offset..row.offset + row.len).contains(&field)),
            "hw_data +{field:#x}"
        );
    }
    // Every INFERRED band is covered by a row.
    for band in G18G_26A428_HW_DATA_BANDS.iter().filter(|band| band.evidence == Evidence::Inferred) {
        assert!(G18G_26A428_UNPROVEN.iter().any(|row| row.object == "hw_data" && row.offset == band.start));
    }
}

#[test]
fn g18g_encoders_fail_closed_on_wrong_sizes_and_addresses() {
    let mut short = vec![0u8; G18G_26A428_HW_DATA_SIZE - 1];
    assert_eq!(encode_g18g_26a428_hw_data(completion_aliases(), QOS, &mut short),
               Err(InitdataError::BufferSize));
    let mut m5_sized = vec![0u8; M5_25G76_HW_DATA_SIZE];
    assert_eq!(encode_g18g_26a428_hw_data(completion_aliases(), QOS, &mut m5_sized),
               Err(InitdataError::BufferSize));
    let mut hw = vec![0u8; G18G_26A428_HW_DATA_SIZE];
    assert!(encode_g18g_26a428_hw_data(completion_aliases(), 0, &mut hw).is_err());
    assert_eq!(
        encode_g18g_26a428_hw_data(completion_aliases(), completion_aliases()[1].high, &mut hw),
        Err(InitdataError::PairAliasedObject)
    );
    let mut map = vec![0u8; HW_REGISTER_MAP_SLOTS * HW_REGISTER_MAP_ENTRY_SIZE];
    assert_eq!(encode_g18g_26a428_register_map(&mut map), Err(InitdataError::BufferSize));
    let mut region_c = vec![0u8; G18G_26A428_REGION_C_ALLOCATION];
    assert_eq!(encode_g18g_26a428_region_c(&mut region_c), Err(InitdataError::BufferSize));
    let mut span = vec![0u8; G18G_26A428_STATUS_B_SPAN];
    assert!(encode_g18g_26a428_primary_status_b(FWCTL, FWCTL + 0x40, 0, &mut span).is_err());
    assert!(encode_g18g_26a428_primary_status_b(FWCTL, FWCTL + 0x40, PRIVATE, &mut span[1..]).is_err());
    let mut ptd = vec![0xa5u8; G18G_26A428_AON_PTD_SIZE];
    encode_g18g_26a428_aon_ptd(&mut ptd).unwrap();
    assert_eq!(get_u32(&ptd, 0), 0x6730_5452);
}
