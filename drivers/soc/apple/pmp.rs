// SPDX-License-Identifier: (GPL-2.0-only OR MIT) AND GPL-2.0-only
#![recursion_limit = "2048"]

//! Apple PMP driver
//!
//! Copyright (C) The Asahi Linux Contributors

use core::mem;

use kernel::{
    bindings,
    device::{
        self,
        Core, //
    },
    devres::Devres,
    driver,
    dma::{
        Coherent,
        CoherentBox,
        Device as _,
        DmaMask, //
    },
    io::{
        mem::{DevresIoMem, IoMem},
        Io, //
    },
    iosys_map::IoSysMapRef,
    kvec,
    new_mutex,
    of,
    platform,
    prelude::*,
    soc::apple::rtkit,
    str::CString,
    sync::{
        aref::ARef,
        Arc,
        Mutex, //
    },
    transmute::{
        AsBytes,
        FromBytes, //
    },
    types::ForeignOwnable, //
    time::{delay::fsleep, Delta},
};

const PMP_MMIO_SIZE: usize = 0x80000;

/// Copies PMP SRAM out word by word (7.3 views have no byte memcpy).
fn sram_read<const N: usize>(
    io: kernel::io::RelaxedMmio<'_, kernel::io::Region<N>>,
    buf: &mut [u8],
    offset: usize,
) -> Result {
    if offset % 4 != 0 {
        return Err(EINVAL);
    }
    for (i, chunk) in buf.chunks_mut(4).enumerate() {
        let word = io.try_read32(offset + i * 4)?.to_le_bytes();
        chunk.copy_from_slice(&word[..chunk.len()]);
    }
    Ok(())
}

/// Copies into PMP SRAM word by word, merging a partial last word.
fn sram_write<const N: usize>(
    io: kernel::io::RelaxedMmio<'_, kernel::io::Region<N>>,
    offset: usize,
    data: &[u8],
) -> Result {
    if offset % 4 != 0 {
        return Err(EINVAL);
    }
    for (i, chunk) in data.chunks(4).enumerate() {
        let off = offset + i * 4;
        let mut word = if chunk.len() < 4 { io.try_read32(off)?.to_le_bytes() } else { [0; 4] };
        word[..chunk.len()].copy_from_slice(chunk);
        io.try_write32(u32::from_le_bytes(word), off)?;
    }
    Ok(())
}

/// `relaxed()` on an `&IoMem`, as the 7.1 API had it: 7.3 has it on the view.
trait RelaxedCompat<'a, const SIZE: usize> {
    fn relaxed(self) -> kernel::io::RelaxedMmio<'a, kernel::io::Region<SIZE>>;
}

impl<'a, const SIZE: usize> RelaxedCompat<'a, SIZE> for &'a IoMem<'_, SIZE> {
    fn relaxed(self) -> kernel::io::RelaxedMmio<'a, kernel::io::Region<SIZE>> {
        use kernel::io::IoBase;
        self.as_view().relaxed()
    }
}

// pmp-telemetry.ko, built with this module: the temperature records of the
// running registry-v2 firmware as an "apple_pmp" hwmon device.
extern "C" {
    fn apple_pmp_telemetry_register(dev: *mut core::ffi::c_void) -> core::ffi::c_int;
    fn apple_pmp_telemetry_stop(dev: *mut core::ffi::c_void);
}
const ASC_MMIO_SIZE: usize = 0x4000;
const BOOTARGS_OFFSET: usize = 0x22c;
const BOOTARGS_SIZE: usize = 0x230;
const CPU_CONTROL: usize = 0x44;
const CPU_RUN: u32 = 0x1 << 4;
const PMP_ENDPOINT: u8 = 0x20;
const OPC_GET_IOVA_TABLE: u64 = 0x10;
const OPC_MALLOC: u64 = 0x12;
const OPC_FREE: u64 = 0x14;
const OPC_SET_BUF: u64 = 0x30;
const OPC_REGISTER_IOREG: u64 = 0x32;
const OPC_SET_IOREG: u64 = 0x34;
const OPC_UPDATE_IOREG_V2: u64 = 0x36;
const OPC_ACK_MASK: u64 = 0x1;
const OPC_SHIFT: u32 = 48;
const MALLOC_SIZE_MASK: u64 = 0xFFFFFF;
const MSG_IOVA_MASK: u64 = 0xFFFFFFFFFFFF;
const SET_IOREG_INDEX_MASK: u64 = 0xFFFF;
const PIO_VM_BASE: u64 = 0xc0000000;
const PIO_GRANULARITY: u64 = 0x1000000;

const fn from_fourcc(b: &[u8; 4]) -> u32 {
    b[3] as u32 | (b[2] as u32) << 8 | (b[1] as u32) << 16 | (b[0] as u32) << 24
}

// Validate the complete table before changing it. The firmware table contains
// mixed-width entries, but the board configuration patched here is all u32.
fn patch_bootarg_table(bytes: &mut [u8], patches: &[(u32, u32)]) -> Result<()> {
    for apply in [false, true] {
        let mut rest = &mut *bytes;
        while !rest.is_empty() {
            if rest.len() < 8 {
                return Err(EINVAL);
            }
            let (header, tail) = rest.split_at_mut(8);
            let key = u32::from_le_bytes(header[..4].try_into().unwrap());
            let size = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
            if size > tail.len() {
                return Err(EINVAL);
            }
            let (value, tail) = tail.split_at_mut(size);
            if let Some((_, replacement)) = patches.iter().find(|(k, _)| *k == key) {
                if size != mem::size_of::<u32>() {
                    return Err(EINVAL);
                }
                if apply {
                    value.copy_from_slice(&replacement.to_le_bytes());
                }
            }
            rest = tail;
        }
    }
    Ok(())
}

struct PmpAllocation {
    addr: u64,
    alloc: Coherent<[u8]>,
}

struct RegistryDescriptor {
    name: [u8; 48],
    size: u32,
    id: u16,
}

impl RegistryDescriptor {
    fn parse(bytes: &[u8]) -> Result<Self> {
        // M5 firmware sends name[48], format[8], unit[8], then three u64s:
        // value length, firmware-assigned ID, and flags. IDs are low16 on wire.
        if bytes.len() < 88 {
            return Err(EINVAL);
        }
        let size = u64::from_le_bytes(bytes[64..72].try_into().unwrap());
        let id = u64::from_le_bytes(bytes[72..80].try_into().unwrap());
        let name: [u8; 48] = bytes[..48].try_into().unwrap();
        if size > u32::MAX as u64 || id > u16::MAX as u64 || !name.contains(&0) {
            return Err(EINVAL);
        }
        Ok(Self {
            name,
            size: size as u32,
            id: id as u16,
        })
    }
}

struct RegistryEntry {
    id: u16,
    value: KVec<u8>,
}

struct IovaTableEntry {
    _host_addr: u64,
    _pio_base: u64,
    _size: u64,
}

// SAFETY: TODO:
unsafe impl AsBytes for IovaTableEntry {}
// SAFETY: TODO:
unsafe impl FromBytes for IovaTableEntry {}

struct PmpState {
    iova_table: Option<Coherent<[IovaTableEntry]>>,
    allocs: KVec<PmpAllocation>,
    value_buf: Option<u64>,
    host_value_buf: Option<u64>,
    ioreg_entries: KVec<u32>,
    registry_entries: KVec<RegistryEntry>,
    registry_requests: u32,
    set_ioreg_count: u32,
    failed: bool,
    // Registry-v2 firmware names buffers by their exact allocation address.
    registry_v2: bool,
}

impl PmpState {
    fn new(registry_v2: bool) -> Result<PmpState> {
        Ok(PmpState {
            iova_table: None,
            allocs: KVec::with_capacity(10, GFP_KERNEL)?,
            value_buf: None,
            host_value_buf: None,
            ioreg_entries: KVec::with_capacity(if registry_v2 { 0 } else { 340 }, GFP_KERNEL)?,
            registry_entries: KVec::with_capacity(if registry_v2 { 340 } else { 0 }, GFP_KERNEL)?,
            registry_requests: 0,
            set_ioreg_count: 0,
            failed: false,
            registry_v2,
        })
    }
    fn find_alloc(&self, addr: u64) -> Option<usize> {
        // Due to how pmp manages memory, iterating in reverse will
        // usually result in us getting the right one on the first try
        for (i, e) in self.allocs.iter().enumerate().rev() {
            if e.addr == addr {
                return Some(i);
            }
        }
        None
    }
    fn get_buf(&mut self, addr: u64) -> Option<&mut [u8]> {
        if self.registry_v2 {
            let idx = self.find_alloc(addr)?;
            // SAFETY: the allocation is owned under the state mutex; the
            // firmware hands it to the host until the request's reply.
            return Some(unsafe { self.allocs[idx].alloc.as_mut() });
        }
        for entry in self.allocs.iter_mut().rev() {
            if let Some(offset) = addr.checked_sub(entry.addr) {
                if offset < entry.alloc.size() as u64 {
                    // SAFETY: the allocation is owned under the state mutex;
                    // an endpoint request hands this buffer to the host until
                    // its reply. No reference survives that reply.
                    return Some(&mut unsafe { entry.alloc.as_mut() }[offset as usize..]);
                }
            }
        }
        None
    }
}

#[pin_data]
struct PmpData {
    dev: ARef<device::Device>,
    t8152: bool,
    registry_v2: bool,
    // The registry-v2 firmware's power context block in PMP SRAM.
    ctx: usize,
    pmp_mmio: DevresIoMem<PMP_MMIO_SIZE>,
    asc_mmio: DevresIoMem<ASC_MMIO_SIZE>,
    #[pin]
    rtkit: Mutex<Option<rtkit::RtKit<PmpData>>>,
    // The registry-v2 RtKit is owned by PmpDriver, avoiding the context's Arc cycle.
    // It drains callbacks before releasing the context and its DMA buffers.
    #[pin]
    registry_rtkit: Mutex<Option<usize>>,
    #[pin]
    stopped: Mutex<bool>,
    #[pin]
    state: Mutex<PmpState>,
}

impl PmpData {
    fn new(dev: &platform::Device<Core<'_>>, t8152: bool, registry_v2: bool, ctx: usize) -> Result<Arc<PmpData>> {
        if t8152 {
            for (name, base, size) in [(c"pmp", 0x300500000, 0xd0000),
                                       (c"asc", 0x300e00000, 0x74000)] {
                let res = dev.resource_by_name(name).ok_or(EINVAL)?;
                if res.start() != base || res.size() != size {
                    return Err(EINVAL);
                }
            }
        }
        let pmp_req = dev.io_request_by_name(c"pmp").ok_or(EINVAL)?;
        let pmp_mmio = pmp_req.iomap_sized::<PMP_MMIO_SIZE>()?.into_devres()?;
        let asc_req = dev.io_request_by_name(c"asc").ok_or(EINVAL)?;
        let asc_mmio = asc_req.iomap_sized::<ASC_MMIO_SIZE>()?.into_devres()?;
        let pmp_state = PmpState::new(registry_v2)?;
        Arc::pin_init(
            try_pin_init!(
                PmpData {
                    dev: dev.as_ref().into(),
                    t8152,
                    registry_v2,
                    ctx,
                    pmp_mmio,
                    asc_mmio,
                    rtkit <- new_mutex!(None),
                    registry_rtkit <- new_mutex!(None),
                    stopped <- new_mutex!(false),
                    state <- new_mutex!(pmp_state)
                }
            ),
            GFP_KERNEL,
        )
    }
    fn start_cpu(&self, dev: &platform::Device<Core<'_>>) -> Result<()> {
        let asc_mmio = self.asc_mmio.access(dev.as_ref())?.relaxed();
        let val = asc_mmio.read32(CPU_CONTROL);
        asc_mmio.write32(val | CPU_RUN, CPU_CONTROL);
        Ok(())
    }
    fn start(&self) -> Result<()> {
        if self.registry_v2 {
            let raw = self.context_handle()?;
            // SAFETY: Probe owns RtKit and firmware has no application
            // endpoint yet; C serializes system messages and power waits.
            unsafe {
                kernel::error::to_result(bindings::apple_rtkit_wake(raw))?;
                return kernel::error::to_result(bindings::apple_rtkit_start_ep(raw, PMP_ENDPOINT));
            }
        }
        let mut guard = self.rtkit.lock();
        let mut rtk = guard.as_mut().as_pin_mut().unwrap();
        rtk.as_mut().wake()?;
        rtk.start_endpoint(PMP_ENDPOINT)
    }
    fn context_handle(&self) -> Result<*mut bindings::apple_rtkit> {
        self.registry_rtkit
            .lock()
            .map(|p| p as *mut bindings::apple_rtkit)
            .ok_or(ENODEV)
    }
    fn cold_preflight(&self, dev: &platform::Device<Core<'_>>) -> Result {
        let io = self.pmp_mmio.access(dev.as_ref())?;
        let asc = self.asc_mmio.access(dev.as_ref())?;
        let ctx = self.ctx;
        // Captured T6050 cold marker. Retained contexts carry live DMA
        // addresses and cannot be cold-reprobed after their buffers retire.
        if asc.read32(CPU_CONTROL) & CPU_RUN != 0
            || io.try_read64(ctx + 0x18)? != 0xfeed1b00
            || io.try_read64(ctx + 0x20)? != 0
            || io.try_read64(ctx + 0x8)? != 0
            || io.try_read64(ctx + 0x10)? != 0
            || io.try_read32(ctx + 0x4)? != 0
            || io.try_read32(ctx)? != 0
        {
            return Err(EBUSY);
        }
        Ok(())
    }
    fn stop_context(&self) -> Result {
        let mut stopped = self.stopped.lock();
        if *stopped {
            return Ok(());
        }
        // SAFETY: Only probe, core unbind, and the registered reboot callback
        // call this method. PmpDriver owns these resources through each call;
        // its first dropped field unregisters and drains the callback.
        let dev = unsafe { self.dev.as_bound() };
        let io = self.pmp_mmio.access(dev)?;
        let ctx = self.ctx;
        let saves = io.try_read32(ctx + 0x4)?;
        let wakes = io.try_read32(ctx)?;
        let raw = self.context_handle()?;
        // Do not hold the reply mutex while waiting: RX callbacks need it
        // to finish outstanding registry exchanges. Ownership remains with
        // the driver, and the stop mutex serializes unbind/reboot.
        unsafe { kernel::error::to_result(bindings::apple_rtkit_idle(raw))? };
        let delta = io.try_read32(ctx + 0x4)?.wrapping_sub(saves);
        if io.try_read64(ctx + 0x18)? != 0xcafe4b0b
            || io.try_read64(ctx + 0x20)? != 0x10024a8
            || io.try_read64(ctx + 0x8)? == 0
            || delta == 0
            || delta >= 1u32 << 31
            || io.try_read32(ctx)? != wakes
        {
            return Err(Error::from_errno(-(bindings::EPROTO as i32)));
        }
        let asc = self.asc_mmio.access(dev)?;
        asc.write32(asc.read32(CPU_CONTROL) & !CPU_RUN, CPU_CONTROL);
        if asc.read32(CPU_CONTROL) & CPU_RUN != 0 {
            return Err(EIO);
        }
        *stopped = true;
        Ok(())
    }
    fn patch_bootargs(&self, dev: &platform::Device<Core<'_>>, patches: &[(u32, u32)]) -> Result<()> {
        let io = self.pmp_mmio.access(dev.as_ref())?.relaxed();
        let offset = io.read32(BOOTARGS_OFFSET) as usize;
        let size = io.read32(BOOTARGS_SIZE) as usize;
        if self.registry_v2 {
            // Bound the allocation as well as the MMIO copy. Firmware supplies both
            // fields, and an invalid table must not reach the writeback below.
            if offset < BOOTARGS_SIZE + mem::size_of::<u32>()
                || offset > io.size()
                || size == 0
                || size > io.size() - offset
            {
                return Err(EINVAL);
            }
            let mut arg_bytes = kvec![0u8; size]?;
            sram_read(io, &mut arg_bytes, offset)?;
            patch_bootarg_table(&mut arg_bytes, patches)?;
            return sram_write(io, offset, &arg_bytes);
        }
        let transfer_size = (size + 3) & !3;
        if offset % 4 != 0 || !(8..=0x4000).contains(&size) ||
            offset.checked_add(transfer_size).ok_or(EINVAL)? > PMP_MMIO_SIZE {
            return Err(EINVAL);
        }
        let mut arg_bytes = kvec![0u8; transfer_size]?;
        if self.t8152 {
            for (i, chunk) in arg_bytes.chunks_exact_mut(4).enumerate() {
                chunk.copy_from_slice(&io.try_read32(offset + i * 4)?.to_le_bytes());
            }
        } else { sram_read(io, &mut arg_bytes, offset)?; }
        let mut seen = kvec![false; patches.len()]?;
        let mut keys = KVec::new();
        let mut idx = 0;
        while idx < size {
            if size - idx < 8 { return Err(EINVAL); }
            let key = u32::from_le_bytes(arg_bytes[idx..idx + 4].try_into().unwrap());
            let len = u32::from_le_bytes(arg_bytes[idx + 4..idx + 8].try_into().unwrap()) as usize;
            if keys.contains(&key) { return Err(EINVAL); }
            keys.push(key, GFP_KERNEL)?;
            idx += 8;
            if len > size - idx { return Err(EINVAL); }
            for (n, (k, v)) in patches.iter().enumerate() {
                if *k != key {
                    continue;
                }
                if ![1, 2, 4, 8].contains(&len) ||
                    (len < 4 && (*v as u64) >= (1u64 << (len * 8))) {
                    return Err(EINVAL);
                }
                arg_bytes[idx..idx + len].copy_from_slice(&(*v as u64).to_le_bytes()[..len]);
                seen[n] = true;
                break;
            }
            idx += len;
        }
        if self.t8152 && seen.iter().any(|v| !v) { return Err(EINVAL); }
        // Only publish after validating the entire blob; never log key material.
        let mut verify = kvec![0u8; transfer_size]?;
        if self.t8152 {
            for (i, chunk) in arg_bytes.chunks_exact(4).enumerate() {
                io.try_write32(u32::from_le_bytes(chunk.try_into().unwrap()), offset + i * 4)?;
            }
            for (i, chunk) in verify.chunks_exact_mut(4).enumerate() {
                chunk.copy_from_slice(&io.try_read32(offset + i * 4)?.to_le_bytes());
            }
        } else {
            sram_write(io, offset, &arg_bytes)?;
            sram_read(io, &mut verify, offset)?;
        }
        if verify != arg_bytes { return Err(EIO); }
        dev_info!(self.dev, "PMP bootargs validated and read back: {} fields\n", patches.len());
        Ok(())
    }
    fn get_iova_table(&self) -> Result<u64> {
        let mut state = self.state.lock();
        if state.iova_table.is_some() {
            dev_err!(self.dev, "Asked for iova table with existing buffer");
            return Err(EIO);
        }
        let node = self.dev.fwnode().ok_or(EIO)?;
        let mut pio_base = PIO_VM_BASE;
        let prop_name = c"apple,pio-ranges";
        if !node.property_present(prop_name) {
            return Ok((OPC_GET_IOVA_TABLE | OPC_ACK_MASK) << OPC_SHIFT);
        }
        let n_entries = node.property_count_elem::<u64>(prop_name)? / 2;
        let ranges = node
            .property_read_array_vec::<u64>(prop_name, n_entries * 2)?
            .required_by(&self.dev)?;
        // SAFETY: TODO: ensure self.dev is bound
        let bound_dev = unsafe { self.dev.as_bound() };
        let mut table = CoherentBox::<[IovaTableEntry]>::zeroed_slice(bound_dev, 170, GFP_KERNEL)?;

        let domain = unsafe { bindings::iommu_get_domain_for_dev(self.dev.as_raw()) };
        for i in 0..n_entries {
            let host_addr = ranges[i * 2];
            let size = ranges[i * 2 + 1];
            unsafe {
                let err = bindings::iommu_map(
                    domain,
                    pio_base as usize,
                    host_addr,
                    size as usize,
                    (bindings::IOMMU_READ | bindings::IOMMU_WRITE | bindings::IOMMU_MMIO) as i32,
                    bindings::GFP_KERNEL,
                );
                if err != 0 {
                    return Err(Error::from_errno(err));
                }
            }
            table[i] = IovaTableEntry {
                _host_addr: host_addr,
                _pio_base: pio_base,
                _size: size,
            };
            pio_base += PIO_GRANULARITY;
        }
        let table: Coherent<[IovaTableEntry]> = table.into();
        let msg = (OPC_GET_IOVA_TABLE | OPC_ACK_MASK) << OPC_SHIFT | table.dma_address();
        state.iova_table = Some(table);
        Ok(msg)
    }
    fn malloc(&self, size: u64) -> Result<u64> {
        // Registry-v2 keeps its own allocation path unchanged.
        if (!self.registry_v2 && size == 0) || (self.t8152 && size > 0x1000000) {
            return Err(EINVAL);
        }
        // SAFETY: TODO: ensure self.dev is bound
        let bound_dev = unsafe { self.dev.as_bound() };
        let iomem = Coherent::<u8>::zeroed_slice(bound_dev, size as usize, GFP_KERNEL)?;
        let mut state = self.state.lock();
        let addr = iomem.dma_address();
        let msg = (OPC_MALLOC | OPC_ACK_MASK) << OPC_SHIFT | addr;
        state.allocs.push(
            PmpAllocation {
                addr: addr,
                alloc: iomem,
            },
            GFP_KERNEL,
        )?;
        Ok(msg)
    }
    fn free(&self, addr: u64) -> Result<u64> {
        let mut state = self.state.lock();
        if let Some(idx) = state.find_alloc(addr) {
            // Registry-v2 firmware may free and set its value buffers again.
            if self.registry_v2 {
                if state.value_buf == Some(addr) {
                    state.value_buf = None;
                }
                if state.host_value_buf == Some(addr) {
                    state.host_value_buf = None;
                }
            }
            state.allocs.swap_remove(idx);
        } else {
            dev_err!(
                self.dev,
                "Attempted to free memory that was not allocated {}",
                addr
            );
            return Err(EIO);
        }
        let msg = (OPC_FREE | OPC_ACK_MASK) << OPC_SHIFT;
        Ok(msg)
    }
    fn set_buf(&self, addr: u64) -> Result<u64> {
        let mut state = self.state.lock();
        if state.value_buf.is_some() {
            dev_err!(self.dev, "Setting a buffer when one exists");
            return Err(EIO);
        }
        let ptr_buf = if let Some(s) = state.get_buf(addr) {
            s
        } else {
            dev_err!(self.dev, "Unable to find buffer");
            return Err(EIO);
        };
        // Registry-v2 firmware also passes the host value buffer.
        let pointers_size = if self.registry_v2 { 16 } else { mem::size_of::<u64>() };
        if ptr_buf.len() < pointers_size {
            dev_err!(self.dev, "Buffer too small");
            return Err(EIO);
        }
        let ptr = u64::from_le_bytes(ptr_buf[..8].try_into().unwrap());
        let host_ptr = if self.registry_v2 {
            Some(u64::from_le_bytes(ptr_buf[8..16].try_into().unwrap()))
        } else {
            None
        };
        if state.get_buf(ptr).is_none() { return Err(EINVAL); }
        if let Some(host_ptr) = host_ptr {
            if state.get_buf(host_ptr).is_none() {
                return Err(EINVAL);
            }
            state.host_value_buf = Some(host_ptr);
        }
        state.value_buf = Some(ptr);
        let msg = (OPC_SET_BUF | OPC_ACK_MASK) << OPC_SHIFT;
        Ok(msg)
    }
    fn register_ioreg(&self, addr: u64) -> Result<u64> {
        let mut state = self.state.lock();
        let msg_buf = if let Some(s) = state.get_buf(addr) {
            s
        } else {
            dev_err!(self.dev, "Unable to find buffer");
            return Err(EIO);
        };
        if msg_buf.len() < 0x44 {
            dev_err!(self.dev, "Buffer too small");
            return Err(EIO);
        }
        let mut size = u32::from_le_bytes(msg_buf[0x40..0x44].try_into().unwrap());
        if size == 0 {
            let mut name_vec = KVec::with_capacity(0x31, GFP_KERNEL)?;
            name_vec.extend_from_slice(&msg_buf[0..0x30], GFP_KERNEL)?;
            name_vec.push(0, GFP_KERNEL)?;
            let name_str = CStr::from_bytes_until_nul(&name_vec).unwrap();
            let name_str = CString::try_from_fmt(fmt!("apple,tunable-{name_str}"))?;
            let node = self.dev.fwnode().ok_or(EIO)?;
            if state.value_buf.is_none() {
                dev_err!(self.dev, "Value buf not set");
                return Err(EIO);
            }
            let val_buf_addr = state.value_buf.unwrap();
            let val_buf = if let Some(s) = state.get_buf(val_buf_addr) {
                s
            } else {
                dev_err!(self.dev, "Unable to find value buffer");
                return Err(EIO);
            };
            if node.property_present(&name_str) {
                let len = node.property_count_elem::<u8>(&name_str)?;
                let data = node
                    .property_read_array_vec::<u8>(&name_str, len)?
                    .required_by(&self.dev)?;
                if len > val_buf.len() || len > u32::MAX as usize {
                    return Err(EINVAL);
                }
                val_buf[0..len].copy_from_slice(&data);
                size = len as u32;
            } else {
                dev_info!(self.dev, "unknown property {:?}", name_str);
            }
        }
        // Native T8152 only inserts entries backed by data. A missing property
        // replies with zero and does not consume a registry index.
        if !self.t8152 || size != 0 {
            state.ioreg_entries.push(size, GFP_KERNEL)?;
        }
        state.registry_requests += 1;
        let index = if self.t8152 { 0 } else { state.ioreg_entries.len() as u64 };
        if self.t8152 {
            dev_info!(self.dev, "PMP property request={} size={:#x}\n", state.registry_requests, size);
        }
        let msg = (OPC_REGISTER_IOREG | OPC_ACK_MASK) << OPC_SHIFT | (index << 32) | size as u64;
        Ok(msg)
    }
    fn set_ioreg(&self, index: u64) -> Result<u64> {
        let mut state = self.state.lock();
        let len = *state
            .ioreg_entries
            .get(index as usize)
            .ok_or(EIO)? as u64;
        state.set_ioreg_count += 1;
        if self.t8152 {
            // 26A428 ApplePMPv2::handleRegistryRemoveEntryReq handles 0x34.
            // Removing index3 shifts the later native index26 by one. Keep
            // this correction scoped to the captured T8152 protocol.
            let _ = state.ioreg_entries.remove(index as usize);
            dev_info!(self.dev, "PMP remove property index={} size={:#x}\n", index, len);
        }
        let msg = (OPC_SET_IOREG | OPC_ACK_MASK) << OPC_SHIFT | len;
        Ok(msg)
    }
    fn register_ioreg_v2(&self, addr: u64) -> Result<u64> {
        let mut state = self.state.lock();
        let request = state.get_buf(addr).ok_or(EINVAL)?;
        // The firmware has transferred this allocation for the request and
        // waits for our response before reusing it.
        let descriptor = RegistryDescriptor::parse(request)?;
        if state.registry_entries.iter().any(|e| e.id == descriptor.id) {
            return Err(EINVAL);
        }
        let value_addr = state.value_buf.ok_or(EINVAL)?;
        let value_buf = state.get_buf(value_addr).ok_or(EINVAL)?;
        let value = if descriptor.size == 0 {
            let name = CStr::from_bytes_until_nul(&descriptor.name).map_err(|_| EINVAL)?;
            let property = CString::try_from_fmt(fmt!("apple,tunable-{name}"))?;
            let node = self.dev.fwnode().ok_or(EIO)?;
            if !node.property_present(&property) {
                return Ok((OPC_REGISTER_IOREG | OPC_ACK_MASK) << OPC_SHIFT);
            }
            let len = node.property_count_elem::<u8>(&property)?;
            if len > value_buf.len() || len > u32::MAX as usize {
                return Err(EINVAL);
            }
            let value = node
                .property_read_array_vec::<u8>(&property, len)?
                .required_by(&self.dev)?;
            // This is the negotiated shared value buffer; firmware consumes
            // the supplied bytes only after our acknowledgement.
            value_buf[..len].copy_from_slice(&value);
            value
        } else {
            let len = descriptor.size as usize;
            if len > value_buf.len() {
                return Err(EINVAL);
            }
            // As above, firmware waits for the acknowledgement before
            // reusing the shared value buffer.
            let mut value = KVec::with_capacity(len, GFP_KERNEL)?;
            value.extend_from_slice(&value_buf[..len], GFP_KERNEL)?;
            value
        };
        let size = value.len() as u64;
        if size != 0 {
            state.registry_entries.push(
                RegistryEntry {
                    id: descriptor.id,
                    value,
                },
                GFP_KERNEL,
            )?;
        }
        // The ID came from the firmware descriptor; the reply carries length.
        Ok((OPC_REGISTER_IOREG | OPC_ACK_MASK) << OPC_SHIFT | size)
    }
    fn change_ioreg_v2(&self, id: u16, remove: bool) -> Result<u64> {
        let mut state = self.state.lock();
        let opcode = if remove { 0x35 } else { 0x37 };
        let Some(index) = state.registry_entries.iter().position(|e| e.id == id) else {
            return Ok(opcode << OPC_SHIFT);
        };
        let size = state.registry_entries[index].value.len();
        if remove {
            state.registry_entries.swap_remove(index);
        } else {
            let addr = state.value_buf.ok_or(EINVAL)?;
            let alloc_index = state.find_alloc(addr).ok_or(EINVAL)?;
            let PmpState {
                allocs,
                registry_entries,
                ..
            } = &mut *state;
            let buffer = &allocs[alloc_index].alloc;
            if size > buffer.size() {
                return Err(EINVAL);
            }
            // SAFETY: Firmware waits for this update's reply before reusing
            // the shared buffer, and the stored length is bounds-checked.
            registry_entries[index]
                .value
                .copy_from_slice(unsafe { &buffer.as_ref()[..size] });
        }
        Ok(opcode << OPC_SHIFT | size as u64)
    }
    fn recv_message(&self, msg: u64) -> Result<()> {
        let opc = (msg >> OPC_SHIFT) & 0xFF;
        let reply = match opc {
            OPC_GET_IOVA_TABLE => self.get_iova_table()?,
            OPC_MALLOC => self.malloc(msg & MALLOC_SIZE_MASK)?,
            OPC_FREE => self.free(msg & MSG_IOVA_MASK)?,
            OPC_SET_BUF => self.set_buf(msg & MSG_IOVA_MASK)?,
            OPC_REGISTER_IOREG if self.registry_v2 => {
                self.register_ioreg_v2(msg & MSG_IOVA_MASK)?
            }
            OPC_REGISTER_IOREG => self.register_ioreg(msg & MSG_IOVA_MASK)?,
            OPC_SET_IOREG if self.registry_v2 => self.change_ioreg_v2(msg as u16, true)?,
            OPC_SET_IOREG => self.set_ioreg(msg & SET_IOREG_INDEX_MASK)?,
            OPC_UPDATE_IOREG_V2 if self.registry_v2 => self.change_ioreg_v2(msg as u16, false)?,
            _ => {
                dev_err!(self.dev, "Got unknown message {}", msg);
                return Err(EIO);
            }
        };
        if self.registry_v2 {
            let guard = self.registry_rtkit.lock();
            let raw = guard.ok_or(ENODEV)? as *mut bindings::apple_rtkit;
            // SAFETY: The driver's owner drains C RX before releasing this
            // context; C mailbox TX is internally serialized.
            return unsafe {
                kernel::error::to_result(bindings::apple_rtkit_send_message(
                    raw,
                    PMP_ENDPOINT,
                    reply,
                    core::ptr::null_mut(),
                    false,
                ))
            };
        }
        let mut rtk_guard = self.rtkit.lock();
        let rtk = rtk_guard.as_mut().as_pin_mut().unwrap();
        rtk.send_message(PMP_ENDPOINT, reply)?;
        Ok(())
    }
}

unsafe impl Send for PmpData {}
unsafe impl Sync for PmpData {}

struct NoBuffer;
impl rtkit::Buffer for NoBuffer {
    fn iova(&self) -> Result<usize> {
        unreachable!()
    }
    fn buf(&mut self) -> Result<IoSysMapRef<'_, u8>> {
        unreachable!()
    }
}

#[vtable]
impl rtkit::Operations for PmpData {
    type Data = Arc<PmpData>;
    type Buffer = NoBuffer;

    fn recv_message(data: <Self::Data as ForeignOwnable>::Borrowed<'_>, _ep: u8, msg: u64) {
        let ret = data.recv_message(msg);
        if let Err(e) = ret {
            data.state.lock().failed = true;
            dev_err!(data.dev, "Failed to handle rtkit message, error: {:?}", e);
        }
    }

    fn crashed(data: <Self::Data as ForeignOwnable>::Borrowed<'_>, _crashlog: Option<&[u8]>) {
        data.state.lock().failed = true;
        // SAFETY: the device is live while its RTKit callbacks run.
        unsafe { apple_pmp_telemetry_stop(data.dev.as_raw().cast()) };
        dev_err!(data.dev, "PMP firmware crashed");
    }
}

struct PmpReboot {
    block: bindings::notifier_block,
    data: Arc<PmpData>,
}

unsafe impl Send for PmpReboot {}

unsafe extern "C" fn pmp_reboot(
    block: *mut bindings::notifier_block,
    _event: usize,
    _arg: *mut core::ffi::c_void,
) -> core::ffi::c_int {
    // SAFETY: This exact stable KBox block is registered while its driver
    // owns RtKit. Unregister drains callbacks before that owner is dropped.
    let state = unsafe { &*kernel::container_of!(block, PmpReboot, block) };
    match state.data.stop_context() {
        Ok(()) => bindings::NOTIFY_OK as i32,
        Err(e) => {
            dev_err!(state.data.dev, "PMP reboot stop failed: {:?}", e);
            bindings::NOTIFY_BAD as i32
        }
    }
}

#[allow(dead_code)]
struct PmpOwner {
    reboot: Option<PmpRebootRegistration>,
    // Field order drains RX before releasing the context's final Arc.
    registry_rtkit: Option<rtkit::RtKit<PmpData>>,
    data: Arc<PmpData>,
}

impl Drop for PmpOwner {
    fn drop(&mut self) {
        if self.registry_rtkit.is_some() {
            // Also covers failure to allocate the core's driver-data box
            // after probe constructed this owner and started firmware.
            if let Err(e) = self.data.stop_context() {
                dev_err!(
                    self.data.dev,
                    "PMP stop failed; retaining DMA until platform reset: {:?}",
                    e
                );
                self.registry_rtkit
                    .as_mut()
                    .unwrap()
                    .retain_shared_buffers_on_drop();
                core::mem::forget(self.data.clone());
            }
        }
    }
}

struct PmpRebootRegistration(KBox<PmpReboot>);

impl Drop for PmpRebootRegistration {
    fn drop(&mut self) {
        // Unregister holds no stop/reply mutex. The chain joins callbacks
        // before the boxed block and its context reference are freed.
        let ret = unsafe { bindings::unregister_reboot_notifier(&mut self.0.block) };
        assert_eq!(ret, 0, "PMP notifier could not be drained");
    }
}

/// Per-SoC configuration.
struct PmpHwConfig {
    /// Use the separately qualified T8152 firmware protocol.
    t8152: bool,
    /// DMA mask to set before the firmware allocates anything, or `None` to
    /// keep the default.
    dma_mask: Option<DmaMask>,
    /// Use the registry-v2 firmware protocol (T6050).
    registry_v2: bool,
    /// Offset of the registry-v2 firmware's power context block in PMP SRAM:
    /// the wake and save counts, then the cold marker (0xfeed1b00) or, once
    /// the context is saved, 0xcafe4b0b and the resume address at +0x18.
    ctx: usize,
}

const HW_CFG_DEFAULT: PmpHwConfig = PmpHwConfig {
    t8152: false,
    dma_mask: None,
    registry_v2: false,
    ctx: 0,
};
const HW_CFG_T8152: PmpHwConfig = PmpHwConfig {
    t8152: true,
    dma_mask: None,
    registry_v2: false,
    ctx: 0,
};

/// T8140: the DART translates 42 bits and its DMA window starts at 1 TiB.
const HW_CFG_T8140: PmpHwConfig = PmpHwConfig {
    t8152: false,
    dma_mask: Some(DmaMask::new::<42>()),
    registry_v2: false,
    ctx: 0,
};

const HW_CFG_T6050: PmpHwConfig = PmpHwConfig {
    t8152: false,
    dma_mask: Some(DmaMask::new::<48>()),
    registry_v2: true,
    ctx: 0x7bcc0,
};

/// T8142 (M5, J813): the T6050 registry-v2 firmware with its power context
/// block lower in SRAM.
const HW_CFG_T8142: PmpHwConfig = PmpHwConfig {
    ctx: 0x54ec0,
    ..HW_CFG_T6050
};

kernel::of_device_table!(
    OF_TABLE,
    &'static PmpHwConfig,
    [
        (of::DeviceId::new(c"apple,t8140-pmp-v2"), &HW_CFG_T8140),
        // T8142 (J813) runs the M5 family's registry-v2 firmware, as T6050
        // does: it sends OPC_UPDATE_IOREG_V2 and takes the board/PMC boot
        // arguments.
        (of::DeviceId::new(c"apple,t8142-pmp-v2"), &HW_CFG_T8142),
        (of::DeviceId::new(c"apple,t8152-pmp-v2"), &HW_CFG_T8152),
        (of::DeviceId::new(c"apple,t6000-pmp-v2"), &HW_CFG_DEFAULT),
        (of::DeviceId::new(c"apple,t6050-pmp-v2"), &HW_CFG_T6050),
    ]
);

/// Driver for the SoCs whose PMP firmware keeps its DMA owned until reboot.
#[allow(dead_code)]
struct PmpDriver(PmpOwner);

impl platform::Driver for PmpDriver {
    type IdInfo = &'static PmpHwConfig;
    type Data<'bound> = Self;

    const OF_ID_TABLE: Option<of::IdTable<Self::IdInfo>> = Some(&OF_TABLE);
    // Firmware retains coherent allocations. Manual detach has no qualified stop
    // handshake, including on the older profile; preserve ownership until reboot.
    fn suppress_bind_attrs() -> bool {
        true
    }

    fn probe<'bound>(pdev: &'bound platform::Device<Core<'_>>, info: Option<&'bound Self::IdInfo>) -> impl PinInit<Self::Data<'bound>, Error> + 'bound {
        // Allocate the platform driver's storage before starting firmware. An
        // allocation failure after CPU_RUN must not release the device's devres.
        pin_init::pin_init_scope(move || Ok(PmpOwner::probe_inner(pdev, info).map(Self)))
    }
}

/// Driver used on T6050, whose registry-v2 firmware stops on unbind and reboot.
#[allow(dead_code)]
struct PmpUnbindableDriver(PmpOwner);

impl platform::Driver for PmpUnbindableDriver {
    type IdInfo = &'static PmpHwConfig;
    type Data<'bound> = Self;

    const OF_ID_TABLE: Option<of::IdTable<Self::IdInfo>> = Some(&OF_TABLE);

    fn probe<'bound>(pdev: &'bound platform::Device<Core<'_>>, info: Option<&'bound Self::IdInfo>) -> impl PinInit<Self::Data<'bound>, Error> + 'bound {
        PmpOwner::probe_inner(pdev, info).map(Self)
    }

    fn unbind(_dev: &platform::Device<Core<'_>>, this: Pin<&Self>) {
        if this.0.data.registry_v2 {
            if let Err(e) = this.0.data.stop_context() {
                dev_err!(this.0.data.dev, "PMP unbind stop failed: {:?}", e);
            }
        }
    }
}

impl PmpOwner {
    fn probe_inner(pdev: &platform::Device<Core<'_>>, info: Option<&&'static PmpHwConfig>) -> Result<Self> {
        let cfg = *info.ok_or(ENODEV)?;
        let t8152 = cfg.t8152;
        let registry_v2 = cfg.registry_v2;
        if let Some(mask) = cfg.dma_mask {
            // SAFETY: No DMA allocations or mappings exist yet.
            unsafe { pdev.dma_set_mask_and_coherent(mask)? };
        }
        let dev: ARef<device::Device> = pdev.as_ref().into();
        if registry_v2 {
            // Deferred-probe timeout can otherwise allow DMA to fall back to
            // physical addresses when the required DART provider failed.
            // PMP messages carry a 48-bit DMA address. The inherited DART
            // aperture starts above 32 bits; the IOMMU constrains this mask to
            // that aperture. The mask is set before any RTKit allocation.
            // SAFETY: The platform device is live throughout probe.
            if unsafe { bindings::iommu_get_domain_for_dev(dev.as_raw()) }.is_null() {
                dev_err!(
                    dev,
                    "PMP requires an attached IOMMU domain before CPU start"
                );
                return Err(EPROBE_DEFER);
            }
        }
        let data = PmpData::new(pdev, t8152, registry_v2, cfg.ctx)?;
        if registry_v2 {
            data.cold_preflight(pdev)?;
        }
        let node = dev.fwnode().ok_or(EIO)?;
        let dvid = node
            .property_read(c"apple,dram-vendor-id")
            .required_by(&dev)?;
        let bdid = node.property_read(c"apple,board-id").required_by(&dev)?;
        if t8152 {
            if bdid != 0x24 || node.property_present(c"apple,pio-ranges") {
                return Err(EINVAL);
            }
            let profile: CString = node.property_read(c"apple,firmware-profile").required_by(&dev)?;
            if profile.to_bytes() != b"j873-26A428-pmp-reference074" { return Err(EINVAL); }
            let pmc: u32 = node.property_read(c"apple,pmc").required_by(&dev)?;
            let pmgr: u32 = node.property_read(c"apple,pmc-pmgr").required_by(&dev)?;
            let latency: u32 = node.property_read(c"apple,pmc-max-bwr-latency-us").required_by(&dev)?;
            if pmc != 1 || pmgr != 31 || latency != 1_000_000 { return Err(EINVAL); }
            if data.asc_mmio.access(pdev.as_ref())?.read32(CPU_CONTROL) & CPU_RUN != 0 {
                return Err(EBUSY);
            }
            // The exact DART profile has already admitted only SID6 on J873.
            // SAFETY: no DMA allocations or firmware callbacks exist yet.
            unsafe { pdev.dma_set_mask_and_coherent(DmaMask::new::<42>())?; }
            let domain = unsafe { bindings::iommu_get_domain_for_dev(dev.as_raw()) };
            if domain.is_null() { return Err(ENODEV); }
            let probe = Coherent::<u8>::zeroed_slice(pdev.as_ref(), 0x4000, GFP_KERNEL)?;
            let iova = probe.dma_address();
            let translated = unsafe { bindings::iommu_iova_to_phys(domain, iova) };
            if !(0x10040000000..0x10080000000).contains(&iova) || translated == 0 {
                return Err(EIO);
            }
            drop(probe);
            if unsafe { bindings::iommu_iova_to_phys(domain, iova) } != 0 { return Err(EIO); }
            dev_info!(dev, "PMP SID6 DMA map/unmap translation check passed\n");
            data.patch_bootargs(pdev, &[
                (from_fourcc(b"BDID"), bdid), (from_fourcc(b"DVID"), dvid),
                (from_fourcc(b"PMC_"), pmc), (from_fourcc(b"PMCV"), pmgr & 1),
                (from_fourcc(b"PMCB"), (pmgr >> 3) & 1), (from_fourcc(b"PMCM"), latency),
            ])?;
        } else {
            let mut patches = KVec::with_capacity(9, GFP_KERNEL)?;
            patches.push((from_fourcc(b"BDID"), bdid), GFP_KERNEL)?;
            patches.push((from_fourcc(b"DVID"), dvid), GFP_KERNEL)?;
            // Optional values are supplied by the bootloader's board profile. Do
            // not replace firmware defaults when a property is absent.
            for (property, key) in [
                (c"apple,dram-capacity", b"DCAP"),
                (c"apple,dram-channel-disable", b"DCHD"),
                (c"apple,pmc", b"PMC_"),
                (c"apple,pmc-msg-disabled", b"PMCX"),
                (c"apple,soc-chip-variant", b"CVAR"),
            ] {
                // Preserve the existing T6000 boot profile. The extra board/PMC
                // fields belong to the registry-v2 firmware ABI.
                if !registry_v2 && key != b"DCAP" {
                    continue;
                }
                if node.property_present(property) {
                    let value = node.property_read::<u32>(property).required_by(&dev)?;
                    patches.push((from_fourcc(key), value), GFP_KERNEL)?;
                }
            }
            if registry_v2 && node.property_present(c"apple,pmc-pmgr") {
                let value = node
                    .property_read::<u32>(c"apple,pmc-pmgr")
                    .required_by(&dev)?;
                patches.push((from_fourcc(b"PMCV"), value & 1), GFP_KERNEL)?;
                patches.push((from_fourcc(b"PMCB"), (value >> 3) & 1), GFP_KERNEL)?;
            }
            data.patch_bootargs(pdev, &patches)?;
        }
        let rtkit = rtkit::RtKit::<PmpData>::new(&dev, None, 0, data.clone())?;
        if registry_v2 {
            *data.registry_rtkit.lock() = Some(rtkit.as_raw() as usize);
            let mut owner = PmpOwner {
                reboot: None,
                registry_rtkit: Some(rtkit),
                data,
            };
            owner.data.start_cpu(pdev)?;
            owner.data.start()?;
            // SAFETY: the bound PMP device; the telemetry is device-managed.
            let ret = unsafe { apple_pmp_telemetry_register(dev.as_raw().cast()) };
            if ret != 0 {
                dev_warn!(dev, "PMP telemetry not registered: {}\n", ret);
            }
            let mut reboot = KBox::new(
                PmpReboot {
                    block: bindings::notifier_block {
                        notifier_call: Some(pmp_reboot),
                        next: core::ptr::null_mut(),
                        priority: -10,
                    },
                    data: owner.data.clone(),
                },
                GFP_KERNEL,
            )?;
            // SAFETY: The boxed notifier has a stable address until its
            // registration guard unregisters and drains the callback chain.
            let ret = unsafe { bindings::register_reboot_notifier(&mut reboot.block) };
            if ret != 0 {
                return Err(Error::from_errno(ret));
            }
            owner.reboot = Some(PmpRebootRegistration(reboot));
            return Ok(owner);
        }
        *data.rtkit.lock() = Some(rtkit);
        if t8152 {
            // Keep callback code, devres and DMA owners resident even on timeout.
            // The RtKit context owns a second Arc to data; no detached allocations.
            unsafe { bindings::__module_get((*kernel::module::this_module::<crate::LocalModule>()).as_ptr()) };
            let result = data.start_cpu(pdev).and_then(|_| data.start()).and_then(|_| {
                for _ in 0..500 {
                    {
                        let state = data.state.lock();
                        if state.failed { return Err(EIO); }
                        if state.registry_requests == 40 && state.set_ioreg_count >= 2 {
                            dev_info!(dev, "PMP reference074 property exchange completed; owners retained until reboot\n");
                            return Ok(());
                        }
                    }
                    fsleep(Delta::from_millis(10));
                }
                Err(ETIMEDOUT)
            });
            if let Err(e) = result {
                dev_err!(dev, "PMP startup failed {:?}; retaining bound DMA owners until reboot\n", e);
                data.state.lock().failed = true;
            }
            return Ok(PmpOwner {
                reboot: None,
                registry_rtkit: None,
                data,
            });
        }
        data.start_cpu(pdev)?;
        data.start()?;
        Ok(PmpOwner {
            reboot: None,
            registry_rtkit: None,
            data,
        })
    }
}

fn machine_is_compatible(compat: &CStr) -> bool {
    let compats = [compat.as_char_ptr(), core::ptr::null()];
    // SAFETY: `compats` is a NULL-terminated array of NUL-terminated strings
    // that lives for the duration of the call.
    unsafe { bindings::of_machine_compatible_match(compats.as_ptr()) }
}

/// The one "apple_pmp" platform driver registered on this machine.
#[allow(dead_code)]
enum PmpRegistration {
    Retained(Pin<KBox<driver::Registration<platform::Adapter<PmpDriver>>>>),
    Unbindable(Pin<KBox<driver::Registration<platform::Adapter<PmpUnbindableDriver>>>>),
}

struct PmpModule {
    _registration: PmpRegistration,
}

impl kernel::Module for PmpModule {
    fn init(module: &'static ThisModule) -> Result<Self> {
        let name = <Self as kernel::ModuleMetadata>::NAME;
        // Manual bind/unbind is a T6050 feature: its registry-v2 firmware has
        // a verified stop. Every other SoC keeps the DMA owned until reboot.
        let registration = if machine_is_compatible(c"apple,t6050") {
            PmpRegistration::Unbindable(KBox::pin_init(
                driver::Registration::new(name, module),
                GFP_KERNEL,
            )?)
        } else {
            PmpRegistration::Retained(KBox::pin_init(
                driver::Registration::new(name, module),
                GFP_KERNEL,
            )?)
        };
        Ok(Self {
            _registration: registration,
        })
    }
}

module! {
    type: PmpModule,
    name: "apple_pmp",
    description: "Apple Power Management Processor",
    license: "GPL",
}
