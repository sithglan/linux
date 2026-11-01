// SPDX-License-Identifier: GPL-2.0-only OR MIT
//! GPU power budget: native `MaxGPUAbsolutePower` semantics.
//!
//! macOS `AGXAccelerator::setPropertyMaxGPUAbsolutePower` (kext 0x88579b4)
//! calls `AGXFirmware::updatePowerInterfaceTarget(0, mW, reason)`
//! (0x8884440), which stores RegionC+0x84 = mW and RegionC+0x9c = reason and
//! sends no firmware message; GFX1 reads +0x84 live in its power limiter
//! (fw1 0184e4). PPM/CLPC thermal budgets reach the same interface.
//!
//! Linux: the optional in-kernel shim `neo_gpu_thermal` owns the thermal
//! loop and calls back through [`set_budget`]. The value is latched here and
//! published to RegionC at the next submission (an idle GPU draws no power to
//! cap). Without the shim nothing changes: RegionC keeps its encoded default.
use core::ffi::{c_char, c_int, c_void};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use kernel::bindings;

/// Requested budget in mW; 0 = no request (keep the encoded default).
pub(crate) static TARGET_MW: AtomicU32 = AtomicU32::new(0);
pub(crate) static REASON: AtomicU32 = AtomicU32::new(0);
pub(crate) static PUBLISHES: AtomicU64 = AtomicU64::new(0);
static ATTEMPTED: AtomicBool = AtomicBool::new(false);
static REGISTERED: AtomicBool = AtomicBool::new(false);

const REGISTER: &core::ffi::CStr = c"neo_gpu_power_register";
const UNREGISTER: &core::ffi::CStr = c"neo_gpu_power_unregister";

/// Mirrors `struct neo_gpu_power_ops` in neo_gpu_thermal.h.
#[repr(C)]
struct Ops {
    set_budget: Option<unsafe extern "C" fn(*mut c_void, u32, u32) -> c_int>,
}

static OPS: Ops = Ops { set_budget: Some(set_budget) };

type RegisterFn = unsafe extern "C" fn(*const Ops, *mut c_void, u32) -> c_int;
type UnregisterFn = unsafe extern "C" fn(*mut c_void);

/// Called by the shim in process context. Only latches atomics.
unsafe extern "C" fn set_budget(_ctx: *mut c_void, mw: u32, reason: u32) -> c_int {
    // Native updatePowerInterfaceTarget rejects nothing below 0x10000 for
    // interface 0; keep the same bound and refuse 0 (reserved: no request).
    if mw == 0 || mw > 0x10000 {
        return -22;
    }
    REASON.store(reason, Ordering::Relaxed);
    TARGET_MW.store(mw, Ordering::Release);
    0
}

/// True once registration has been tried (loaded or not).
pub(crate) fn attempted() -> bool {
    ATTEMPTED.load(Ordering::Acquire)
}

/// Register once with the shim, if it is loaded. `default_mw` is the live
/// RegionC+0x84 value (the unthrottled budget).
pub(crate) fn register_once(default_mw: u32) {
    if ATTEMPTED.swap(true, Ordering::AcqRel) {
        return;
    }
    // SAFETY: symbol_get of a GPL symbol; a non-null result pins the module.
    let sym = unsafe { bindings::__symbol_get(REGISTER.as_ptr() as *const c_char) };
    if sym.is_null() {
        kernel::pr_info!("G17P power budget: neo_gpu_thermal absent, static RegionC+0x84 = {}\n", default_mw);
        return;
    }
    // SAFETY: the symbol has the RegisterFn C signature (neo_gpu_thermal.h).
    let register: RegisterFn = unsafe { core::mem::transmute(sym) };
    // SAFETY: OPS is 'static; ctx is unused by this driver.
    let ret = unsafe { register(&OPS, core::ptr::null_mut(), default_mw) };
    if ret == 0 {
        REGISTERED.store(true, Ordering::Release);
        kernel::pr_info!("G17P power budget: neo_gpu_thermal registered, default {} mW\n", default_mw);
    } else {
        // SAFETY: balance the symbol_get above.
        unsafe { bindings::__symbol_put(REGISTER.as_ptr() as *const c_char) };
        kernel::pr_warn!("G17P power budget: neo_gpu_thermal register failed {}\n", ret);
    }
}

/// Unregister before the runtime goes away; releases the module reference.
pub(crate) fn unregister() {
    if !REGISTERED.swap(false, Ordering::AcqRel) {
        return;
    }
    // SAFETY: the shim is pinned by the register symbol reference.
    let sym = unsafe { bindings::__symbol_get(UNREGISTER.as_ptr() as *const c_char) };
    if !sym.is_null() {
        // SAFETY: UnregisterFn C signature (neo_gpu_thermal.h).
        let unregister: UnregisterFn = unsafe { core::mem::transmute(sym) };
        unsafe { unregister(core::ptr::null_mut()) };
        unsafe { bindings::__symbol_put(UNREGISTER.as_ptr() as *const c_char) };
    }
    TARGET_MW.store(0, Ordering::Release);
    // SAFETY: balance register_once's symbol_get.
    unsafe { bindings::__symbol_put(REGISTER.as_ptr() as *const c_char) };
}
