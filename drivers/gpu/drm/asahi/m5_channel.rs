// SPDX-License-Identifier: GPL-2.0-only OR MIT

#![cfg_attr(not(test), allow(dead_code))]

//! T8142/25G76 channel configuration, separate from Neo's HAL200 path.
//!
//! Native AGXCLChannelSKSM::submitBuffer inlines this packed 0x86-byte
//! record at 0xfffffe000898488c..0x8984bd8. The controlled native sequence
//! is a selector-3 register program, this tag-15 record, then a tag-14 kick.
//! These builders do not map memory, install a queue or publish a producer.
//! Every pointer and event identifier must belong to the retained session.

pub(crate) const M5_CHANNEL_CONFIG_BYTES: usize = 0x86;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum M5ChannelError {
    QueueId,
    Priority,
    Address,
    Context,
    DataMaster,
}

/// Optional TA parameter-buffer event, separate from the first event tuple.
/// The native TA writer at 25G76 0x8a249ac..0x8a249e4 obtains these values
/// from its retained PM manager and command descriptor.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct M5ParameterBufferEvent {
    pub(crate) address: u64,
    pub(crate) timestamp: u64,
    /// TA descriptor +0x1218, independently selected by the owned PB table.
    pub(crate) hardware_buffer_id: u32,
    /// Native PM manager +0x79c. This is not the SKSM queue ID.
    pub(crate) manager_id: u32,
}

/// Optional retained event-machine object, with the native timestamp and
/// index. None emits the native disabled branch's full-width sentinels.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct M5ChannelEvent {
    pub(crate) address: u64,
    pub(crate) timestamp: u64,
    pub(crate) index: u32,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct M5ChannelConfig {
    pub(crate) ring_low: u64,
    pub(crate) ring_high: u64,
    pub(crate) queue_id: u8,
    pub(crate) priority: u8,
    /// Native descriptor +0x181 bit 0. Mode 3 validates the first queue
    /// installation on M5; an already-installed native capture has 0.
    pub(crate) install_queue: bool,
    /// Native descriptor +0x144, independently matched to the UAT context.
    pub(crate) gpu_context: u8,
    pub(crate) event: Option<M5ChannelEvent>,
    /// AGXCommandQueue +0x8b8: retained scheduler/control object.
    pub(crate) channel_control: u64,
    /// Native descriptor +0x2dc bit 0.
    pub(crate) update_scheduler: bool,
    /// AGXCommandQueue +0x898 object's +0x10. Not the GPU context ID.
    pub(crate) scheduler_index: u32,
    /// getStampIndex on descriptor +0x170. Not a fixed target constant.
    pub(crate) channel_stamp_index: u32,
    /// Native descriptor +0x180 bit 0.
    pub(crate) update_context: bool,
    /// Descriptor +0x58 owner's virtual context value. Distinct from UAT.
    pub(crate) owner_context: u32,
    /// Native channel-state +0x84 is nonzero.
    pub(crate) extended_command: bool,
}

fn firmware_address(address: u64) -> bool {
    address >= 0xffff_fc00_0000_0000 && address & 7 == 0
}

fn put32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn encode_m5_channel_config(
    p: M5ChannelConfig,
) -> Result<[u8; M5_CHANNEL_CONFIG_BYTES], M5ChannelError> {
    encode_m5_channel_config_for(p, 2, 2, None, None)
}

/// The two selectors are independent: native TA/fragment use 0/1 at +0x22
/// while both retain 2 at +0x5e. Changing both by stage breaks that contract.
/// gpu_state_update retains the separate +0x26 flag/+0x2a value pair;
/// the native TA writer copies descriptor +0xe41/+0xe48 to these fields.
pub(crate) fn encode_m5_channel_config_for(
    p: M5ChannelConfig,
    data_master: u32,
    channel_data_master: u32,
    gpu_state_update: Option<u64>,
    parameter_buffer: Option<M5ParameterBufferEvent>,
) -> Result<[u8; M5_CHANNEL_CONFIG_BYTES], M5ChannelError> {
    if data_master > 2
        || channel_data_master > 2
        || (parameter_buffer.is_some() && data_master != 0)
    {
        return Err(M5ChannelError::DataMaster);
    }
    if p.queue_id > 127 {
        return Err(M5ChannelError::QueueId);
    }
    if p.priority > 3 {
        return Err(M5ChannelError::Priority);
    }
    if p.gpu_context == 0 {
        return Err(M5ChannelError::Context);
    }
    if p.ring_low == 0
        || p.ring_low >= (1 << 42)
        || p.ring_low & 0x3fff != 0
        || !firmware_address(p.ring_high)
        || p.ring_high & 0x3fff != 0
        || !firmware_address(p.channel_control)
    {
        return Err(M5ChannelError::Address);
    }
    if let Some(event) = p.event {
        if !firmware_address(event.address) {
            return Err(M5ChannelError::Address);
        }
    }
    if let Some(event) = parameter_buffer {
        if !firmware_address(event.address) {
            return Err(M5ChannelError::Address);
        }
    }
    let mut bytes = [0; M5_CHANNEL_CONFIG_BYTES];
    put32(&mut bytes, 0, 15);
    put64(&mut bytes, 8, p.ring_low);
    put64(&mut bytes, 0x10, p.ring_high);
    bytes[0x18..0x1a].copy_from_slice(&u16::from(p.queue_id).to_le_bytes());
    for (offset, value) in [
        (0x1a, u32::from(p.install_queue)),
        (0x1e, u32::from(p.priority)),
        (0x22, data_master),
        (0x32, u32::from(p.gpu_context)),
        (0x52, u32::from(p.update_scheduler)),
        (0x56, p.scheduler_index),
        (0x5a, p.channel_stamp_index),
        (0x5e, channel_data_master),
        (0x62, u32::from(p.update_context)),
        (0x66, p.owner_context),
        (0x6a, u32::from(p.extended_command)),
    ] {
        put32(&mut bytes, offset, value);
    }
    if let Some(value) = gpu_state_update {
        put32(&mut bytes, 0x26, 1);
        put64(&mut bytes, 0x2a, value);
    }
    match p.event {
        Some(event) => {
            put64(&mut bytes, 0x36, event.address);
            put64(&mut bytes, 0x3e, event.timestamp);
            put32(&mut bytes, 0x46, event.index);
        }
        None => {
            put64(&mut bytes, 0x3e, u64::MAX);
            put32(&mut bytes, 0x46, u32::MAX);
        }
    }
    put64(&mut bytes, 0x4a, p.channel_control);
    if let Some(event) = parameter_buffer {
        put64(&mut bytes, 0x6e, event.address);
        put64(&mut bytes, 0x76, event.timestamp);
        put32(&mut bytes, 0x7e, event.hardware_buffer_id);
        put32(&mut bytes, 0x82, event.manager_id);
    } else {
        bytes[0x76..0x86].fill(0xff);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> M5ChannelConfig {
        M5ChannelConfig {
            ring_low: 0x7000000000,
            ring_high: 0xfffffc2000400000,
            queue_id: 11,
            priority: 2,
            install_queue: false,
            gpu_context: 3,
            event: None,
            channel_control: 0xfffffc20c07c00c0,
            update_scheduler: true,
            scheduler_index: 0x12345678,
            channel_stamp_index: 0xabcdef01,
            update_context: true,
            owner_context: 1,
            extended_command: false,
        }
    }

    #[test]
    fn disabled_events_use_full_width_sentinels_and_indices_do_not_truncate() {
        let bytes = encode_m5_channel_config(config()).unwrap();
        assert_eq!(&bytes[0x36..0x3e], &[0; 8]);
        assert_eq!(&bytes[0x3e..0x4a], &[0xff; 12]);
        assert_eq!(&bytes[0x6e..0x76], &[0; 8]);
        assert_eq!(&bytes[0x76..], &[0xff; 16]);
        assert_eq!(&bytes[0x56..0x5a], &0x12345678u32.to_le_bytes());
        assert_eq!(&bytes[0x5a..0x5e], &0xabcdef01u32.to_le_bytes());
    }

    #[test]
    fn first_bind_changes_only_the_install_field() {
        let p = config();
        let mut expected = encode_m5_channel_config(p).unwrap();
        expected[0x1a] = 1;
        assert_eq!(
            encode_m5_channel_config(M5ChannelConfig {
                install_queue: true,
                ..p
            })
            .unwrap(),
            expected
        );
    }

    #[test]
    fn graphics_selectors_and_parameter_event_are_independent() {
        let p = config();
        let event = M5ParameterBufferEvent {
            address: 0xfffffc2000600000,
            timestamp: 0x27d7,
            hardware_buffer_id: 4,
            manager_id: 11,
        };
        let ta = encode_m5_channel_config_for(p, 0, 2, Some(0), Some(event)).unwrap();
        let mut expected = encode_m5_channel_config(p).unwrap();
        expected[0x22..0x26].fill(0);
        expected[0x26] = 1;
        expected[0x6e..0x76].copy_from_slice(&event.address.to_le_bytes());
        expected[0x76..0x7e].copy_from_slice(&event.timestamp.to_le_bytes());
        expected[0x7e..0x82].copy_from_slice(&event.hardware_buffer_id.to_le_bytes());
        expected[0x82..0x86].copy_from_slice(&event.manager_id.to_le_bytes());
        assert_eq!(ta, expected);
        let mut fragment = encode_m5_channel_config(p).unwrap();
        fragment[0x22] = 1;
        assert_eq!(
            encode_m5_channel_config_for(p, 1, 2, None, None).unwrap(),
            fragment
        );
        for (primary, channel) in [(3, 2), (0, 3)] {
            assert_eq!(
                encode_m5_channel_config_for(p, primary, channel, None, None),
                Err(M5ChannelError::DataMaster)
            );
        }
        assert_eq!(
            encode_m5_channel_config_for(p, 1, 2, None, Some(event)),
            Err(M5ChannelError::DataMaster)
        );
        assert_eq!(
            encode_m5_channel_config_for(
                p,
                0,
                2,
                None,
                Some(M5ParameterBufferEvent {
                    address: 0x580004000,
                    ..event
                })
            ),
            Err(M5ChannelError::Address)
        );
    }

    #[test]
    fn invalid_queues_contexts_and_addresses_are_rejected() {
        let p = config();
        for bad in [
            M5ChannelConfig { queue_id: 128, ..p },
            M5ChannelConfig { priority: 4, ..p },
            M5ChannelConfig {
                gpu_context: 0,
                ..p
            },
            M5ChannelConfig {
                ring_low: 1 << 42,
                ..p
            },
            M5ChannelConfig {
                ring_high: p.ring_high + 8,
                ..p
            },
            M5ChannelConfig {
                channel_control: 0x580004000,
                ..p
            },
        ] {
            assert!(encode_m5_channel_config(bad).is_err());
        }
    }
}
