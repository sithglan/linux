// SPDX-License-Identifier: GPL-2.0-only OR MIT
//! A retained, tag-15-only M5 queue registration experiment. No GPU kicks.

use super::{MappedObject, KERNEL_BASE};
use crate::{driver::AsahiDevice, g17_submission, m5_channel, mmu};
use crate::m5_sksm;
use core::sync::atomic::{fence, Ordering};
use kernel::{c_str, io, prelude::*};

const GRAPH_BASE: u64 = KERNEL_BASE + 0xc000_0000;
const ENTRY_HIGH: u64 = KERNEL_BASE + 0x39_0000;
const ENTRY_LOW: u64 = 0x70_005f_0000;
const QUEUE_ID: u8 = 11;
const QUEUE_OFFSET: usize = QUEUE_ID as usize * 0xc0;
const POINTERS: usize = 0x7000;
const JOB_LIST: usize = 0x7100;
const CONTROL: usize = 0x7200;
const ITEMS: usize = 0x8000;
const CONFIG: usize = 0xa800;
// GFX RTKit-3255.160.4 has TEXT at +0 and DATA at +0x4c000.
// Its tag-15 handler 0x122a4..0x12434 installs qid*0x28 at +0x127430.
const FIRMWARE_SLOT: usize = 0x127430 - 0x4c000 + QUEUE_ID as usize * 0x28;
// Tag-15 scheduler binding stores its control pointer at 0x1257e0 + qid*8.
const FIRMWARE_SCHEDULER: usize = 0x1257e0 - 0x4c000 + QUEUE_ID as usize * 8;

pub(super) struct FirmwareDataView {
    map: io::mem::Mem,
    cache_line: usize,
}
// SAFETY: This owns a fixed mapping of reserved RAM, read only through
// bounded volatile accesses. It neither transfers ownership nor writes DATA.
unsafe impl Send for FirmwareDataView {}
// SAFETY: Concurrent read-only volatile observations do not mutate host state.
unsafe impl Sync for FirmwareDataView {}

impl FirmwareDataView {
    pub(super) fn new(dev: &AsahiDevice) -> Result<Self> {
        let resource = dev.as_ref().of_node().ok_or(EINVAL)?
            .reserved_mem_region_to_resource_byname(c_str!("gfx-data"))?;
        if resource.size() != 0x134000 || resource.start() < 0x100_0000_0000
            || resource.start() & 0x3fff != 0
            || resource.start().checked_add(resource.size()).ok_or(EOVERFLOW)? > 0x104_0000_0000
            || 0x127430 - 0x4c000 + 13 * 0x28 > resource.size() as usize
            || 0x1257e0 - 0x4c000 + 13 * 8 > resource.size() as usize
            || 0x178458 - 0x4c000 + 8 > resource.size() as usize {
            return Err(EINVAL);
        }
        let ctr: u64;
        // SAFETY: CTR_EL0 is an architectural read-only cache geometry register.
        unsafe { core::arch::asm!("mrs {ctr}, ctr_el0", ctr = out(reg) ctr,
            options(nomem, nostack, preserves_flags)) };
        let cache_line = 4usize << ((ctr >> 16) & 0xf);
        if cache_line > 0x4000 { return Err(EINVAL); }
        // SAFETY: WB matches reserved System RAM. The host never writes this
        // extent; observations invalidate covering lines before volatile reads.
        let map = unsafe { io::mem::Mem::try_new(resource, io::mem::MemFlag::WB.into()) }?;
        fence(Ordering::SeqCst);
        Ok(Self { map, cache_line })
    }

    fn words<const N: usize>(&self, offset: usize) -> [u64; N] {
        // All callers use constructor-checked queue, scheduler or power extents.
        let line = self.cache_line;
        let start = offset & !(line - 1);
        let end = (offset + N * 8 + line - 1) & !(line - 1);
        for offset in (start..end).step_by(line) {
            // SAFETY: Whole covering lines are inside the retained DATA view.
            // The host never dirties them; do not clean stale data to firmware.
            unsafe { core::arch::asm!("dc ivac, {addr}",
                addr = in(reg) self.map.ptr().add(offset),
                options(nostack, preserves_flags)) };
        }
        // SAFETY: Complete invalidations before volatile observations.
        unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)) };
        let mut out = [0; N];
        for (index, value) in out.iter_mut().enumerate() {
            // SAFETY: The constructor bounds-checks every supported extent.
            *value = unsafe { core::ptr::read_volatile(
                self.map.ptr().add(offset + index * 8).cast::<u64>()) };
        }
        fence(Ordering::Acquire);
        out
    }

    pub(super) fn queue_registration(&self, queue_id: u8) -> Result<([u64; 5], u64)> {
        // TA/fragment own 10/11; application compute owns 12. The constructor
        // bounds both complete tables through the last admitted queue.
        if !(10..=12).contains(&queue_id) { return Err(EINVAL); }
        Ok((self.words::<5>(0x127430 - 0x4c000 + queue_id as usize * 0x28),
            self.words::<1>(0x1257e0 - 0x4c000 + queue_id as usize * 8)[0]))
    }
}

pub(super) struct M5QueueProbe {
    entry_alias: mmu::KernelMapping,
    entry: MappedObject,
    graph: MappedObject,
    firmware: FirmwareDataView,
    bind_scheduler: bool,
}

impl M5QueueProbe {
    pub(super) fn new(dev: &AsahiDevice, uat: &mmu::Uat, bind_scheduler: bool) -> Result<Self> {
        if !uat.is_t8142_geometry() {
            return Err(ENODEV);
        }
        let mut entry = MappedObject::new_wc_at(dev, uat.kernel_vm(), ENTRY_HIGH,
            0x20000, mmu::PROT_FW_PRIV_RW)?;
        let entry_alias = entry.map_alias_at(uat.kernel_lower_vm(), ENTRY_LOW,
            mmu::PROT_GPU_SHARED_RW)?;
        for offset in (0..0x20000).step_by(mmu::UAT_PGSZ) {
            if uat.kernel_vm().translate_iova(ENTRY_HIGH + offset as u64)?
                != uat.kernel_lower_vm().translate_iova(ENTRY_LOW + offset as u64)? {
                return Err(EFAULT);
            }
        }
        let mut graph = MappedObject::new_wc_at(dev, uat.kernel_vm(), GRAPH_BASE,
            0xc000, mmu::PROT_FW_PRIV_RW)?;
        let config = m5_channel::encode_m5_channel_config(m5_channel::M5ChannelConfig {
            ring_low: ENTRY_LOW, ring_high: ENTRY_HIGH, queue_id: QUEUE_ID, priority: 2,
            install_queue: true, gpu_context: 3, event: None,
            channel_control: GRAPH_BASE + CONTROL as u64,
            // Mode 4 additionally binds the owned scheduler control. Neither
            // mode attaches a user context, event machine or shader.
            update_scheduler: bind_scheduler, scheduler_index: 0, channel_stamp_index: 0,
            update_context: false, owner_context: 0, extended_command: false,
        }).map_err(|_| EINVAL)?;
        graph.with_bytes_mut(|out| {
            let reset = g17_submission::encode_m5_25g76_channel_state_reset(
                g17_submission::M5ChannelStateResetValues {
                    uncached_state: GRAPH_BASE + POINTERS as u64,
                    command_pointer_ring: GRAPH_BASE + ITEMS as u64,
                    gpu_va_10: GRAPH_BASE + JOB_LIST as u64,
                    word_48: 0,
                    gpu_va_9c: GRAPH_BASE + CONTROL as u64,
                });
            out[QUEUE_OFFSET..QUEUE_OFFSET + 0xc0].copy_from_slice(&reset);
            // Measured prepared CL_2 channel fields, independent of SKSM ID.
            for (offset, value) in [(0x28, 2u32), (0x2c, 2), (0x40, 2)] {
                out[QUEUE_OFFSET + offset..QUEUE_OFFSET + offset + 4]
                    .copy_from_slice(&value.to_le_bytes());
            }
            out[QUEUE_OFFSET + 0x36..QUEUE_OFFSET + 0x38].fill(0xff);
            out[POINTERS + 0x50..POINTERS + 0x54].fill(0xff);
            out[POINTERS + 0x60..POINTERS + 0x64].copy_from_slice(&0x500u32.to_le_bytes());
            out[JOB_LIST + 8..JOB_LIST + 16]
                .copy_from_slice(&(GRAPH_BASE + JOB_LIST as u64).to_le_bytes());
            // Native M5 scheduler-control initial view. It is retained but
            // attached only by the explicit mode-4 scheduler experiment.
            out[CONTROL..CONTROL + 2].fill(0xff);
            out[CONTROL + 5] = 1;
            out[CONTROL + 0x26] = 2;
            out[CONTROL + 0x33] = 0xff;
            out[CONFIG..CONFIG + config.len()].copy_from_slice(&config);
            out[ITEMS..ITEMS + 8].copy_from_slice(&(GRAPH_BASE + CONFIG as u64).to_le_bytes());
            out[POINTERS + 0x40..POINTERS + 0x44].copy_from_slice(&1u32.to_le_bytes());
            Ok(())
        })?;
        Ok(Self { entry_alias, entry, graph, firmware: FirmwareDataView::new(dev)?,
            bind_scheduler })
    }

    pub(super) fn queue_address(&self) -> u64 {
        GRAPH_BASE + QUEUE_OFFSET as u64
    }

    pub(super) fn prepare_compute_job(&mut self, submit_kick: bool) -> Result {
        use super::program_memory::{DESCRIPTOR_HIGH, DESCRIPTOR_LOW, EVENT};
        if self.inner_counters()? != [1; 3] { return Err(EBUSY); }
        let entry = m5_sksm::prepare_m5_sksm_entry(m5_sksm::M5SksmQueue {
            id: QUEUE_ID, priority: 2, current_stamp: 1, submitted: 0, previous_stamp: None,
        }, m5_sksm::M5SksmOperands {
            barriers: &[], first_predecessor: 0, implicit_dependency_type: 1,
            mcache: None, flag_bit1: false, flag_bit61: false, flag_bit63: false,
            payload: [DESCRIPTOR_HIGH, self.queue_address()],
            qos_byte_32: 3, qos_class_40: 8, qos_word_48: 0xffff,
            event_mask: [0, 0, 8, 0], rce_kind: 0,
            rce: [m5_sksm::M5SksmRce { address: DESCRIPTOR_LOW + 0x40, tag: 37 },
                  m5_sksm::M5SksmRce { address: DESCRIPTOR_LOW + 0x760, tag: 4 },
                  m5_sksm::M5SksmRce { address: 0, tag: 0 },
                  m5_sksm::M5SksmRce { address: 0, tag: 0 }],
            auxiliary: 0x3f_ffff_ffff_ffff,
        }).map_err(|_| EINVAL)?;
        let config = m5_channel::encode_m5_channel_config(m5_channel::M5ChannelConfig {
            ring_low: ENTRY_LOW, ring_high: ENTRY_HIGH, queue_id: QUEUE_ID, priority: 2,
            install_queue: false, gpu_context: 3,
            event: Some(m5_channel::M5ChannelEvent { address: EVENT, timestamp: 1, index: 0 }),
            channel_control: GRAPH_BASE + CONTROL as u64,
            update_scheduler: false, scheduler_index: 0, channel_stamp_index: 0,
            update_context: true, owner_context: 1, extended_command: false,
        }).map_err(|_| EINVAL)?;
        let kick = m5_sksm::encode_m5_sksm_add_kicks(QUEUE_ID, 2, 1, 1).map_err(|_| EINVAL)?;
        self.entry.with_bytes_mut(|out| {
            let range = &mut out[entry.offset as usize..entry.offset as usize + entry.bytes.len()];
            if range.iter().any(|x| *x != 0) { return Err(EBUSY); }
            range.copy_from_slice(&entry.bytes);
            Ok(())
        })?;
        self.graph.with_bytes_mut(|out| {
            out[CONFIG + 0x100..CONFIG + 0x100 + config.len()].copy_from_slice(&config);
            out[CONFIG + 0x200..CONFIG + 0x200 + kick.len()].copy_from_slice(&kick);
            for (index, address) in [DESCRIPTOR_HIGH,
                GRAPH_BASE + CONFIG as u64 + 0x100, GRAPH_BASE + CONFIG as u64 + 0x200]
                .into_iter().enumerate() {
                out[ITEMS + (index + 1) * 8..ITEMS + (index + 2) * 8]
                    .copy_from_slice(&address.to_le_bytes());
            }
            Ok(())
        })?;
        fence(Ordering::SeqCst);
        // Mode7 retires descriptor/configuration only. The prepared SKSM
        // entry stays unpublished because tag14 lies beyond this producer.
        self.graph.write_u32(GRAPH_BASE + POINTERS as u64 + 0x40,
            if submit_kick { 4 } else { 3 })?;
        Ok(())
    }

    pub(super) fn inner_counters(&mut self) -> Result<[u32; 3]> {
        Ok([self.graph.read_u32(GRAPH_BASE + POINTERS as u64)?,
            self.graph.read_u32(GRAPH_BASE + POINTERS as u64 + 0x30)?,
            self.graph.read_u32(GRAPH_BASE + POINTERS as u64 + 0x40)?])
    }

    fn firmware_words<const N: usize>(&self, offset: usize) -> [u64; N] {
        self.firmware.words(offset)
    }

    pub(super) fn firmware_slot(&self) -> [u64; 5] {
        self.firmware_words::<5>(FIRMWARE_SLOT)
    }

    pub(super) fn power_status(&self) -> [u32; 3] {
        // Exact 25G76 reserved DATA observations. These may differ from
        // firmware-private cached state and cannot establish power readiness.
        [self.firmware_words::<1>(0x128868 - 0x4c000)[0] as u32,
         self.firmware_words::<1>(0x1773e8 - 0x4c000)[0] as u32,
         self.firmware_words::<1>(0x177a28 - 0x4c000)[0] as u32]
    }

    pub(super) fn compute_observations(&self) -> [u64; 4] {
        [self.firmware_words::<1>(0x125be8 - 0x4c000)[0],
         self.firmware_words::<1>(0x128868 - 0x4c000)[0],
         self.firmware_words::<1>(0x1773e8 - 0x4c000)[0],
         self.firmware_words::<1>(0x178458 - 0x4c000)[0]]
    }

    pub(super) fn scheduler_status(&mut self) -> Result<Option<[u64; 3]>> {
        if !self.bind_scheduler { return Ok(None); }
        Ok(Some([self.firmware_words::<1>(FIRMWARE_SCHEDULER)[0],
            u64::from(self.graph.read_u32(GRAPH_BASE + CONTROL as u64)?),
            self.graph.read_u64(GRAPH_BASE + CONTROL as u64 + 0x12)?]))
    }

    pub(super) fn scheduler_bound(status: Option<[u64; 3]>) -> bool {
        match status {
            None => true,
            Some([pointer, header, bitmap]) => pointer == GRAPH_BASE + CONTROL as u64
                && header & 0xff != 0xff && (header >> 8) & 0xff != 0xff
                && (header >> 16) & 0xff == 4 && bitmap == 1u64 << QUEUE_ID,
        }
    }

    pub(super) fn is_installed(slot: [u64; 5]) -> bool {
        slot[0] & 1 != 0 && slot[1] == ENTRY_HIGH && slot[2] == ENTRY_LOW
            && slot[3] == 4u64 << 32 && slot[4] as u32 == 2
    }
}
