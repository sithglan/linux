// SPDX-License-Identifier: GPL-2.0-only OR MIT

#![cfg_attr(not(test), allow(dead_code))]

//! Byte-confirmed G17 GPU RTKit wire fields.
//!
//! This module is a pure codec. It does not allocate memory, start an ASC,
//! send a mailbox message, or infer any MMIO or power transition. It encodes
//! only the GPU-specific wire form that the driver itself produces/consumes:
//! the init-data doorbell, the SKSM kick word, the GPU endpoint set, and the
//! runtime dispatch fields.
//!
//! The management endpoint 0 protocol (HELLO version negotiation, EPMAP,
//! STARTEP, AP/IOP power states) is *not* the GPU driver's responsibility: it
//! is owned by the generic `apple-rtkit` core (`drivers/soc/apple/rtkit.c`),
//! which the driver already wraps via `kernel::soc::apple::rtkit`. That is why
//! the generic core owns those replies.  Its discovery-only API now exposes
//! the unmodified HELLO and raw endpoint bitmap without starting system
//! endpoints or changing AP/IOP power state.  This module applies the
//! G17P-specific admission rule to that snapshot: both roles must advertise
//! exact HELLO `0x0c/0x0c` and endpoints `{0, 1, 0x20, 0x21, 0x23}`.
//!
//! The GPU-specific constants below come from matching host and G17S firmware
//! disassembly (verified 2026-08-25 against `GFX.a010.macho` init-data parser
//! at VA 0xfffffc000002127c: tag mask 0xbf..==0x81.. @0x212b0, address prefix
//! 0xfffff.. @0x212c4). The encoded doorbell is byte-identical to the shipping
//! AGX2 transport in `gpu.rs` (`MSG_INIT = 0x81 << 48`, `INIT_DATA_MASK =
//! (1 << 44) - 1`), so this codec can drive the same live path.

/// Receive-side validation and the proved field encoders are available.
pub(crate) const GROUNDED_CODEC_AVAILABLE: bool = true;
/// The GPU-specific wire codec is complete and byte-identical to the shipping
/// AGX2 transport, and the management protocol it relies on (owned by the
/// generic `apple-rtkit` core) negotiates the G17 firmware's advertised
/// interface version 0xc. What still requires a live M5 firmware ACK — that
/// the firmware actually accepts the handoff and cold-boots — is gated by the
/// boot-lifecycle gates in `g17_boot`, not by this codec.
pub(crate) const EXECUTABLE_TRANSPORT_AVAILABLE: bool = APPLE_RTKIT_CORE_ACCEPTS_G17;

/// Generic `apple-rtkit` core supported interface-version window, mirrored
/// from `drivers/soc/apple/rtkit.c` (`APPLE_RTKIT_MIN/MAX_SUPPORTED_VERSION`).
pub(crate) const APPLE_RTKIT_CORE_MIN_VERSION: u16 = 11;
pub(crate) const APPLE_RTKIT_CORE_MAX_VERSION: u16 = 12;

/// Whether the generic core can negotiate the G17 firmware's advertised RTKit
/// interface version. This is the only management-layer fact the GPU codec
/// depends on; the rest of EP0 (HELLO/EPMAP framing) is the core's ABI.
pub(crate) const APPLE_RTKIT_CORE_ACCEPTS_G17: bool = INTERFACE_VERSION
    >= APPLE_RTKIT_CORE_MIN_VERSION
    && INTERFACE_VERSION <= APPLE_RTKIT_CORE_MAX_VERSION;

/// The host-served endpoint set in both recovered G17 firmware roles.
pub(crate) const EXPECTED_ENDPOINTS: [u8; 4] = [0x00, 0x01, 0x20, 0x21];

pub(crate) const ENDPOINT_MANAGEMENT: u8 = 0x00;
pub(crate) const ENDPOINT_CRASHLOG: u8 = 0x01;
pub(crate) const ENDPOINT_GFX_MESSAGES: u8 = 0x20;
pub(crate) const ENDPOINT_GFX_INTERRUPTS: u8 = 0x21;
/// Firmware-to-firmware control endpoint observed on T8140 (both instances).
///
/// It may legitimately appear in an instance's endpoint map, but it carries
/// peer traffic between the two firmware instances: the host must **never**
/// start it, ack it, or serve messages on it. It is permitted-but-unserved in
/// [`validate_endpoint_set`].
pub(crate) const ENDPOINT_FIRMWARE_PEER: u8 = 0x23;

/// Exact T8140/G17P discovery map for each independent firmware role.
pub(crate) const EXPECTED_G17P_DISCOVERY_ENDPOINTS: [u8; 5] = [
    ENDPOINT_MANAGEMENT,
    ENDPOINT_CRASHLOG,
    ENDPOINT_GFX_MESSAGES,
    ENDPOINT_GFX_INTERRUPTS,
    ENDPOINT_FIRMWARE_PEER,
];

/// Exact raw 256-bit EPMAP returned by both T8140/G17P firmware roles.
pub(crate) const EXPECTED_G17P_DISCOVERY_EPMAP: [u64; 4] = [
    (1u64 << ENDPOINT_MANAGEMENT)
        | (1u64 << ENDPOINT_CRASHLOG)
        | (1u64 << ENDPOINT_GFX_MESSAGES)
        | (1u64 << ENDPOINT_GFX_INTERRUPTS)
        | (1u64 << ENDPOINT_FIRMWARE_PEER),
    0,
    0,
    0,
];

const MANAGEMENT_TYPE_SHIFT: u32 = 52;
const MANAGEMENT_TYPE_MASK: u8 = 0x0f;

/// HELLO is generic management type one.
pub(crate) const MANAGEMENT_TYPE_HELLO: u8 = 0x01;
const GENERIC_MANAGEMENT_TYPE_MASK: u64 = 0xff << MANAGEMENT_TYPE_SHIFT;
const HELLO_MIN_VERSION_MASK: u64 = 0xffff;
const HELLO_MAX_VERSION_MASK: u64 = 0xffff << 16;
const HELLO_ALLOWED_MASK: u64 =
    GENERIC_MANAGEMENT_TYPE_MASK | HELLO_MIN_VERSION_MASK | HELLO_MAX_VERSION_MASK;

/// Byte-confirmed management type used by the endpoint-map path.
pub(crate) const MANAGEMENT_TYPE_EPMAP: u8 = 0x08;
/// Observed management type in the start-endpoints/AP-power path.
///
/// The value is exact; its complete payload grammar and semantic power-state
/// name are not yet proven, so this module provides no encoder for it.
pub(crate) const MANAGEMENT_TYPE_START_ENDPOINTS_PATH: u8 = 0x0b;
/// Literal observed in the same path. Its semantic state name is unknown.
pub(crate) const OBSERVED_START_ENDPOINTS_PAYLOAD: u16 = 0x8000;

/// Both 16-bit interface values are compared against this exact value.
///
/// Their positions inside HELLO are not exposed because those positions are
/// not byte-confirmed for G17.
pub(crate) const INTERFACE_VERSION: u16 = 0x000c;

const INITDATA_TAG: u64 = 0x0081_0000_0000_0000;
const INITDATA_TAG_CHECK_MASK: u64 = 0x00bf_0000_0000_0000;
const INITDATA_ADDRESS_MASK: u64 = 0x0000_0fff_ffff_ffff;
const INITDATA_ADDRESS_PREFIX: u64 = 0xffff_f000_0000_0000;
const RECOVERY_INSPECTION_TAG: u64 = 0x0086_0000_0000_0000;
const RECOVERY_INSPECTION_GENERATION_MASK: u64 = (1u64 << 44) - 1;
const DEVICE_CONTROL_TAG: u64 = 0x0084_0000_0000_0000;
/// `AGXArmFirmware::powerTransitionEnd(AGFIFirmwareRole, bool)` closes the GPU
/// power-on transition by sending this note to EACH firmware role.
///
/// KEXT_PROVEN, 25G83 @0xfffffe00088936a4. Both send sites are identical:
///
///     ldr   x0, [x19, #6616]  /  [x19, #6672]   ; the two role objects
///     ldr   x8, [obj + 0x8a8]                   ; sendAsyncNoteToFirmware
///     mov   x1, #0x89000000000000               ; tag 0x89, payload 0
///     mov   w2, #0x1                            ; gated=true
///     blraa x8, x16
///
/// MEASURED on hardware (m1n1 hv capture 20260904-001852Z): it is the LAST
/// EP-0x21 message of bring-up, sent once per role right after the initdata
/// handshake completes and 11.4 seconds before the first work doorbell:
///
///     57.95ms  role0 -> ep=0x21 0x0089000000000000
///     59.31ms  role1 -> ep=0x21 0x0089000000000000
///     ...
///   11425.12ms role0 -> ep=0x21 0x0083000000000000   <- first doorbell
///
/// This driver has never sent it. Its EP-0x21 vocabulary was 0x81 initdata,
/// 0x83 doorbell, 0x84 device control and 0x86 recovery inspection only.
const POWER_TRANSITION_END_TAG: u64 = 0x0089_0000_0000_0000;

/// Encode the role's power-transition-end note. The payload is zero in every
/// captured instance and the kext materialises the bare tag with a single
/// `movz x1, #0x89, lsl #48`.
pub(crate) const fn encode_power_transition_end() -> u64 {
    POWER_TRANSITION_END_TAG
}
const DEVICE_CONTROL_PRIMARY_MODE: u64 = 0x11;

const RUNTIME_TYPE_SHIFT: u32 = 44;
const RUNTIME_TYPE_MASK: u64 = 0xffff;
const RUNTIME_TYPE_VALID_MASK: u32 = 0x13ff_ffbe;

const KICK_STAMP_INDEX_MASK: u64 = 0xff;
const KICK_STAMP_SHIFT: u32 = 8;
const KICK_TIMESTAMP_MASK: u64 = 0x0000_00ff_ffff_ffff;
const KICK_QUEUE_ID_SHIFT: u32 = 40;
const KICK_QUEUE_ID_MASK: u8 = 0x7f;
const KICK_RESERVED_MASK: u64 = 0xffff_8000_0000_0000;
const KICK_QUEUE_RECORD_BASE: u32 = 0x10;
const KICK_QUEUE_RECORD_STRIDE: u32 = 0x20;

/// Refusals made by the evidence-backed codec.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum WireError {
    UnexpectedEndpointSet,
    UnsupportedInterface,
    AddressOutOfRange,
    InvalidInitdataTag,
    InvalidRuntimeType,
    QueueIdOutOfRange,
    UnmodeledBits,
}

/// Raw facts exported by the generic discovery-only RTKit client.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveryObservation<'a> {
    pub(crate) raw_hello: u64,
    pub(crate) hello_min_version: u16,
    pub(crate) hello_max_version: u16,
    pub(crate) endpoints: &'a [u64; 4],
}

/// Return whether an endpoint is instantiated by the recovered G17 roles.
pub(crate) const fn endpoint_is_present(endpoint: u8) -> bool {
    endpoint == ENDPOINT_MANAGEMENT
        || endpoint == ENDPOINT_CRASHLOG
        || endpoint == ENDPOINT_GFX_MESSAGES
        || endpoint == ENDPOINT_GFX_INTERRUPTS
        || endpoint == ENDPOINT_FIRMWARE_PEER
}

/// Return whether the host may start/serve an endpoint. EP 0x23 exists but is
/// firmware-to-firmware only.
pub(crate) const fn endpoint_is_host_served(endpoint: u8) -> bool {
    endpoint == ENDPOINT_MANAGEMENT
        || endpoint == ENDPOINT_CRASHLOG
        || endpoint == ENDPOINT_GFX_MESSAGES
        || endpoint == ENDPOINT_GFX_INTERRUPTS
}

/// Require every host-served endpoint `{0x00, 0x01, 0x20, 0x21}` to be
/// present exactly once.
///
/// Ordering is irrelevant; duplicates and unknown endpoints (e.g. 0x22) are
/// rejected. The firmware-to-firmware endpoint 0x23 is *permitted* in the map
/// (T8140 advertises it) but confers nothing: the host must never start or
/// serve it ([`ENDPOINT_FIRMWARE_PEER`]).
pub(crate) fn validate_endpoint_set(endpoints: &[u8]) -> Result<(), WireError> {
    let mut seen = 0u8;
    let mut peer_seen = false;
    for endpoint in endpoints {
        if *endpoint == ENDPOINT_FIRMWARE_PEER {
            if peer_seen {
                return Err(WireError::UnexpectedEndpointSet);
            }
            peer_seen = true;
            continue;
        }
        let Some(index) = EXPECTED_ENDPOINTS
            .iter()
            .position(|expected| expected == endpoint)
        else {
            return Err(WireError::UnexpectedEndpointSet);
        };
        let bit = 1u8 << index;
        if seen & bit != 0 {
            return Err(WireError::UnexpectedEndpointSet);
        }
        seen |= bit;
    }

    if seen == (1 << EXPECTED_ENDPOINTS.len()) - 1 {
        Ok(())
    } else {
        Err(WireError::UnexpectedEndpointSet)
    }
}

/// Admit one raw T8140/G17P role discovery.
///
/// The raw HELLO must contain only management type one and the two generic
/// version fields, both exactly `0x0c`.  The endpoint map is exact, including
/// firmware-to-firmware EP 0x23; unlike [`validate_endpoint_set`], this live
/// admission does not permit the peer endpoint to be absent.
pub(crate) fn validate_g17p_discovery(
    observation: &DiscoveryObservation<'_>,
) -> Result<(), WireError> {
    if observation.raw_hello & !HELLO_ALLOWED_MASK != 0
        || ((observation.raw_hello & GENERIC_MANAGEMENT_TYPE_MASK) >> MANAGEMENT_TYPE_SHIFT)
            != MANAGEMENT_TYPE_HELLO as u64
    {
        return Err(WireError::UnsupportedInterface);
    }

    let raw_min = (observation.raw_hello & HELLO_MIN_VERSION_MASK) as u16;
    let raw_max = ((observation.raw_hello & HELLO_MAX_VERSION_MASK) >> 16) as u16;
    if raw_min != observation.hello_min_version || raw_max != observation.hello_max_version {
        return Err(WireError::UnsupportedInterface);
    }
    validate_interface_versions(raw_min, raw_max)?;

    if observation.endpoints != &EXPECTED_G17P_DISCOVERY_EPMAP {
        return Err(WireError::UnexpectedEndpointSet);
    }

    Ok(())
}

/// Require independent, exact discoveries from both T8140 firmware roles.
pub(crate) fn validate_g17p_dual_discovery(
    gfx: &DiscoveryObservation<'_>,
    gfx1: &DiscoveryObservation<'_>,
) -> Result<(), WireError> {
    validate_g17p_discovery(gfx)?;
    validate_g17p_discovery(gfx1)
}

/// Extract the byte-confirmed EP0 management type from bits 55:52.
pub(crate) const fn management_type(message: u64) -> u8 {
    ((message >> MANAGEMENT_TYPE_SHIFT) as u8) & MANAGEMENT_TYPE_MASK
}

/// Accept only the interface pair checked by the recovered firmware.
pub(crate) const fn validate_interface_versions(first: u16, second: u16) -> Result<(), WireError> {
    if first == INTERFACE_VERSION && second == INTERFACE_VERSION {
        Ok(())
    } else {
        Err(WireError::UnsupportedInterface)
    }
}

/// Encode the G17 init-data handoff exactly as the recovered host does.
///
/// The host instruction truncates to 44 bits. Linux instead rejects an
/// address unless it is already in low-44-bit or firmware-canonical form.
pub(crate) const fn encode_initdata_doorbell(firmware_va: u64) -> Result<u64, WireError> {
    let high = firmware_va & !INITDATA_ADDRESS_MASK;
    if high != 0 && high != INITDATA_ADDRESS_PREFIX {
        return Err(WireError::AddressOutOfRange);
    }

    Ok(INITDATA_TAG | (firmware_va & INITDATA_ADDRESS_MASK))
}

/// Encode `AGXArmFirmware::stopFirmwareForRecoveryInspection`'s role-0 note.
/// Shipping macOS inserts the low 44 bits of the host-accepted recovery
/// generation into a type-6 EP21 message before it resumes recovery state 1.
pub(crate) const fn encode_recovery_inspection(generation: u64) -> u64 {
    RECOVERY_INSPECTION_TAG | (generation & RECOVERY_INSPECTION_GENERATION_MASK)
}

/// Notify the primary firmware that one device-control record was published.
/// The opening UMA release uses native submit mode one, which produces this
/// single role-0 EP21 word and does not request a synchronous completion wait.
pub(crate) const fn encode_primary_device_control() -> u64 {
    DEVICE_CONTROL_TAG | DEVICE_CONTROL_PRIMARY_MODE
}

/// Decode an init-data handoff using the exact firmware tag check.
///
/// Bit 54 is intentionally ignored by the firmware's `0x00bf...` mask. The
/// returned address is canonicalized exactly as the firmware does.
pub(crate) const fn decode_initdata_doorbell(message: u64) -> Result<u64, WireError> {
    if message & INITDATA_TAG_CHECK_MASK != INITDATA_TAG {
        return Err(WireError::InvalidInitdataTag);
    }

    Ok(INITDATA_ADDRESS_PREFIX | (message & INITDATA_ADDRESS_MASK))
}

/// Extract the G17 EP 0x21 dispatch field from bits 59:44.
pub(crate) const fn runtime_message_type(message: u64) -> u16 {
    ((message >> RUNTIME_TYPE_SHIFT) & RUNTIME_TYPE_MASK) as u16
}

/// Apply the byte-exact firmware validity mask to a runtime type.
pub(crate) const fn validate_runtime_message_type(message_type: u16) -> Result<(), WireError> {
    if message_type < 32 && (RUNTIME_TYPE_VALID_MASK & (1u32 << message_type)) != 0 {
        Ok(())
    } else {
        Err(WireError::InvalidRuntimeType)
    }
}

/// Proven fields in the host-produced SKSM kick word.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct KickWord {
    pub(crate) stamp_index: u8,
    pub(crate) stamp: u32,
    pub(crate) queue_id: u8,
    /// Bits 39:0, matching the firmware's printed timestamp value.
    pub(crate) timestamp: u64,
    /// QID bit 6, used by firmware to select a valid-queue mask word.
    pub(crate) mask_selector: u8,
}

impl KickWord {
    /// Encode only the exact host-produced fields; bits 63:47 remain zero.
    pub(crate) const fn encode(
        queue_id: u8,
        stamp: u32,
        stamp_index: u8,
    ) -> Result<u64, WireError> {
        if queue_id > KICK_QUEUE_ID_MASK {
            return Err(WireError::QueueIdOutOfRange);
        }

        Ok((stamp_index as u64 & KICK_STAMP_INDEX_MASK)
            | ((stamp as u64) << KICK_STAMP_SHIFT)
            | ((queue_id as u64) << KICK_QUEUE_ID_SHIFT))
    }

    /// Decode a host-produced kick, refusing the unmodeled upper 17 bits.
    pub(crate) const fn decode_strict(word: u64) -> Result<Self, WireError> {
        if word & KICK_RESERVED_MASK != 0 {
            return Err(WireError::UnmodeledBits);
        }

        Ok(Self {
            stamp_index: (word & KICK_STAMP_INDEX_MASK) as u8,
            stamp: ((word >> KICK_STAMP_SHIFT) & u32::MAX as u64) as u32,
            queue_id: ((word >> KICK_QUEUE_ID_SHIFT) as u8) & KICK_QUEUE_ID_MASK,
            timestamp: word & KICK_TIMESTAMP_MASK,
            mask_selector: ((word >> 46) & 1) as u8,
        })
    }
}

/// Byte offset of one firmware queue record: `0x10 + QID * 0x20`.
pub(crate) const fn kick_queue_record_offset(queue_id: u8) -> Result<u32, WireError> {
    if queue_id > KICK_QUEUE_ID_MASK {
        Err(WireError::QueueIdOutOfRange)
    } else {
        Ok(KICK_QUEUE_RECORD_BASE + queue_id as u32 * KICK_QUEUE_RECORD_STRIDE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_set_is_exact_and_order_independent() {
        assert!(endpoint_is_present(ENDPOINT_MANAGEMENT));
        assert!(endpoint_is_present(ENDPOINT_CRASHLOG));
        assert!(endpoint_is_present(ENDPOINT_GFX_MESSAGES));
        assert!(endpoint_is_present(ENDPOINT_GFX_INTERRUPTS));
        assert!(!endpoint_is_present(0x22));
        assert_eq!(validate_endpoint_set(&EXPECTED_ENDPOINTS), Ok(()));
        assert_eq!(validate_endpoint_set(&[0x21, 0x00, 0x20, 0x01]), Ok(()));
        assert_eq!(
            validate_endpoint_set(&[0x00, 0x01, 0x02, 0x03]),
            Err(WireError::UnexpectedEndpointSet)
        );
        assert_eq!(
            validate_endpoint_set(&[0x00, 0x01, 0x22, 0x23]),
            Err(WireError::UnexpectedEndpointSet)
        );
        assert_eq!(
            validate_endpoint_set(&[0x00, 0x00, 0x20, 0x21]),
            Err(WireError::UnexpectedEndpointSet)
        );
    }

    #[test]
    fn firmware_peer_endpoint_is_permitted_but_never_host_served() {
        // T8140 advertises the firmware-to-firmware endpoint 0x23; a map
        // that includes it is valid...
        assert!(endpoint_is_present(ENDPOINT_FIRMWARE_PEER));
        assert_eq!(
            validate_endpoint_set(&[0x00, 0x01, 0x20, 0x21, 0x23]),
            Ok(())
        );
        assert_eq!(
            validate_endpoint_set(&[0x23, 0x21, 0x00, 0x20, 0x01]),
            Ok(())
        );
        // ...but it substitutes for nothing and must appear at most once.
        assert_eq!(
            validate_endpoint_set(&[0x00, 0x01, 0x20, 0x23]),
            Err(WireError::UnexpectedEndpointSet)
        );
        assert_eq!(
            validate_endpoint_set(&[0x00, 0x01, 0x20, 0x21, 0x23, 0x23]),
            Err(WireError::UnexpectedEndpointSet)
        );
        // The host must never start or serve it.
        assert!(!endpoint_is_host_served(ENDPOINT_FIRMWARE_PEER));
        for served in EXPECTED_ENDPOINTS {
            assert!(endpoint_is_host_served(served));
        }
    }

    #[test]
    fn management_type_field_is_bits_55_52() {
        assert_eq!(
            management_type(0x0080_0000_0000_1234),
            MANAGEMENT_TYPE_EPMAP
        );
        assert_eq!(
            management_type(0x00b0_0000_0000_8000),
            MANAGEMENT_TYPE_START_ENDPOINTS_PATH
        );
        assert_eq!(OBSERVED_START_ENDPOINTS_PAYLOAD, 0x8000);
    }

    #[test]
    fn only_interface_version_twelve_is_accepted() {
        assert_eq!(validate_interface_versions(0x0c, 0x0c), Ok(()));
        assert_eq!(
            validate_interface_versions(0x0b, 0x0c),
            Err(WireError::UnsupportedInterface)
        );
        assert_eq!(
            validate_interface_versions(0x0c, 0x0d),
            Err(WireError::UnsupportedInterface)
        );
    }

    #[test]
    fn g17p_discovery_requires_exact_raw_hello_and_peer_endpoint() {
        const RAW_HELLO_0C_0C: u64 =
            ((MANAGEMENT_TYPE_HELLO as u64) << MANAGEMENT_TYPE_SHIFT) | (0x0c << 16) | 0x0c;
        assert_eq!(EXPECTED_G17P_DISCOVERY_EPMAP[0].count_ones(), 5);
        for endpoint in EXPECTED_G17P_DISCOVERY_ENDPOINTS {
            assert_ne!(
                EXPECTED_G17P_DISCOVERY_EPMAP[endpoint as usize / 64]
                    & (1u64 << (endpoint as usize % 64)),
                0
            );
        }
        let exact = DiscoveryObservation {
            raw_hello: RAW_HELLO_0C_0C,
            hello_min_version: 0x0c,
            hello_max_version: 0x0c,
            endpoints: &EXPECTED_G17P_DISCOVERY_EPMAP,
        };
        assert_eq!(validate_g17p_discovery(&exact), Ok(()));

        let mut missing_peer_epmap = EXPECTED_G17P_DISCOVERY_EPMAP;
        missing_peer_epmap[0] &= !(1u64 << ENDPOINT_FIRMWARE_PEER);
        let missing_peer = DiscoveryObservation {
            endpoints: &missing_peer_epmap,
            ..exact
        };
        assert_eq!(
            validate_g17p_discovery(&missing_peer),
            Err(WireError::UnexpectedEndpointSet)
        );

        let reserved_hello_bit = DiscoveryObservation {
            raw_hello: RAW_HELLO_0C_0C | (1 << 40),
            ..exact
        };
        assert_eq!(
            validate_g17p_discovery(&reserved_hello_bit),
            Err(WireError::UnsupportedInterface)
        );

        let decoded_does_not_match_raw = DiscoveryObservation {
            hello_max_version: 0x0b,
            ..exact
        };
        assert_eq!(
            validate_g17p_discovery(&decoded_does_not_match_raw),
            Err(WireError::UnsupportedInterface)
        );
    }

    #[test]
    fn g17p_dual_discovery_rejects_either_bad_role() {
        const RAW_HELLO_0C_0C: u64 =
            ((MANAGEMENT_TYPE_HELLO as u64) << MANAGEMENT_TYPE_SHIFT) | (0x0c << 16) | 0x0c;
        let exact = DiscoveryObservation {
            raw_hello: RAW_HELLO_0C_0C,
            hello_min_version: 0x0c,
            hello_max_version: 0x0c,
            endpoints: &EXPECTED_G17P_DISCOVERY_EPMAP,
        };
        assert_eq!(validate_g17p_dual_discovery(&exact, &exact), Ok(()));

        let mut bad_epmap = EXPECTED_G17P_DISCOVERY_EPMAP;
        bad_epmap[0] &= !(1u64 << ENDPOINT_FIRMWARE_PEER);
        bad_epmap[0] |= 1u64 << 0x22;
        let bad_role = DiscoveryObservation {
            endpoints: &bad_epmap,
            ..exact
        };
        assert_eq!(
            validate_g17p_dual_discovery(&bad_role, &exact),
            Err(WireError::UnexpectedEndpointSet)
        );
        assert_eq!(
            validate_g17p_dual_discovery(&exact, &bad_role),
            Err(WireError::UnexpectedEndpointSet)
        );
    }

    #[test]
    fn g17_interface_version_is_inside_the_generic_core_window() {
        // The firmware advertises 0xc; the generic apple-rtkit core supports
        // [11, 12]. This is what makes the transport executable without the
        // GPU driver owning the HELLO/EPMAP framing.
        assert_eq!(INTERFACE_VERSION, 0x0c);
        assert!(INTERFACE_VERSION >= APPLE_RTKIT_CORE_MIN_VERSION);
        assert!(INTERFACE_VERSION <= APPLE_RTKIT_CORE_MAX_VERSION);
        assert!(APPLE_RTKIT_CORE_ACCEPTS_G17);
        assert!(EXECUTABLE_TRANSPORT_AVAILABLE);
    }

    #[test]
    fn doorbell_matches_the_shipping_agx2_wire_form() {
        // gpu.rs: MSG_INIT = 0x81 << 48, INIT_DATA_MASK = (1 << 44) - 1.
        const MSG_INIT: u64 = 0x81 << 48;
        const INIT_DATA_MASK: u64 = (1 << 44) - 1;
        for va in [0x1000u64, 0x0123_4567_89ab, 0x0fff_ffff_ffff] {
            assert_eq!(
                encode_initdata_doorbell(va).unwrap(),
                MSG_INIT | (va & INIT_DATA_MASK)
            );
        }
    }

    #[test]
    fn recovery_inspection_keeps_the_native_tag_and_low_44_bit_generation() {
        assert_eq!(encode_recovery_inspection(0), 0x0086_0000_0000_0000);
        assert_eq!(encode_recovery_inspection(1), 0x0086_0000_0000_0001);
        assert_eq!(
            encode_recovery_inspection(0xffff_f123_4567_89ab),
            0x0086_0123_4567_89ab
        );
    }

    #[test]
    fn primary_device_control_uses_native_mode_one() {
        assert_eq!(encode_primary_device_control(), 0x0084_0000_0000_0011);
    }

    #[test]
    fn observed_management_types_are_stable_under_the_core_mask() {
        // The core uses GENMASK(59, 52); this codec reads bits [55:52]. Every
        // observed EP0 type (HELLO=1, HELLO_REPLY=2, EPMAP=8, AP power=0xb)
        // fits in 4 bits, so both interpretations agree on real messages.
        for ty in [
            1u8,
            2,
            MANAGEMENT_TYPE_EPMAP,
            MANAGEMENT_TYPE_START_ENDPOINTS_PATH,
        ] {
            assert!(ty <= 0x0f, "observed EP0 type must fit the 4-bit subfield");
            let msg = (ty as u64) << MANAGEMENT_TYPE_SHIFT;
            // Both the 4-bit subfield read and the core's 8-bit read agree.
            assert_eq!(management_type(msg), ty);
            assert_eq!(((msg >> 52) & 0xff) as u8, ty);
        }
    }

    #[test]
    fn initdata_doorbell_uses_a_44_bit_address() {
        for address in [0x1000, 0x0123_4567_89ab, 0x0fff_ffff_ffff] {
            let message = encode_initdata_doorbell(address).unwrap();
            assert_eq!(message, INITDATA_TAG | address);
            assert_eq!(
                decode_initdata_doorbell(message),
                Ok(INITDATA_ADDRESS_PREFIX | address)
            );
        }

        assert_eq!(
            encode_initdata_doorbell(INITDATA_ADDRESS_PREFIX | 0x2000),
            Ok(INITDATA_TAG | 0x2000)
        );
        assert_eq!(
            encode_initdata_doorbell(1 << 44),
            Err(WireError::AddressOutOfRange)
        );
    }

    #[test]
    fn initdata_decoder_matches_the_firmware_mask() {
        assert_eq!(
            decode_initdata_doorbell(0x00c1_0000_0000_1000),
            Ok(INITDATA_ADDRESS_PREFIX | 0x1000)
        );
        assert_eq!(
            decode_initdata_doorbell(0x0091_0000_0000_1000),
            Err(WireError::InvalidInitdataTag)
        );
        assert_eq!(
            decode_initdata_doorbell(0),
            Err(WireError::InvalidInitdataTag)
        );
    }

    #[test]
    fn runtime_dispatch_field_and_validity_mask_are_exact() {
        assert_eq!(runtime_message_type(0x0001_1000_0000_0000), 0x11);
        for valid in [1, 5, 7, 0x11, 25, 28] {
            assert_eq!(validate_runtime_message_type(valid), Ok(()));
        }
        for invalid in [0, 6, 26, 27, 29, 31, 32, u16::MAX] {
            assert_eq!(
                validate_runtime_message_type(invalid),
                Err(WireError::InvalidRuntimeType)
            );
        }
    }

    #[test]
    fn kick_word_round_trips_every_proven_field() {
        let word = KickWord::encode(0x7f, 0x89ab_cdef, 0x42).unwrap();
        assert_eq!(word, 0x0000_7f89_abcd_ef42);
        assert_eq!(
            KickWord::decode_strict(word),
            Ok(KickWord {
                stamp_index: 0x42,
                stamp: 0x89ab_cdef,
                queue_id: 0x7f,
                timestamp: 0x89ab_cdef42,
                mask_selector: 1,
            })
        );
        assert_eq!(
            KickWord::encode(0x80, 0, 0),
            Err(WireError::QueueIdOutOfRange)
        );
        assert_eq!(
            KickWord::decode_strict(1 << 47),
            Err(WireError::UnmodeledBits)
        );
    }

    #[test]
    fn queue_record_offsets_use_the_firmware_stride() {
        assert_eq!(kick_queue_record_offset(0), Ok(0x10));
        assert_eq!(kick_queue_record_offset(1), Ok(0x30));
        assert_eq!(kick_queue_record_offset(0x7f), Ok(0x0ff0));
        assert_eq!(
            kick_queue_record_offset(0x80),
            Err(WireError::QueueIdOutOfRange)
        );
    }
}
