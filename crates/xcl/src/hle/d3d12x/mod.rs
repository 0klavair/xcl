//! d3d12_xs.dll – D3D12.X.
//!
//! Backends:
//! * `real` (default): genuine PC D3D12 objects from the GPU driver with extended
//!   vtables (PC methods go straight to the driver, console-only slots are guarded and
//!   logged), shader capture, fallback pipeline states, presentation window.
//! * `null`: stand-in objects that render nothing (`graphics = "null"` in xcl.toml or
//!   when no D3D12 device can be created).

#![allow(non_snake_case)]

pub mod guard;
pub mod layout;
pub mod null;
pub mod present;
pub mod real;

use std::ffi::c_void;
use std::sync::OnceLock;

use crate::util::{HRESULT, S_OK};

pub fn exports() -> Vec<(&'static str, usize)> {
    crate::hle_exports![
        D3D12CreateDevice,
        D3D12XboxCreateDevice,
        D3D12GetDebugInterface,
        D3D12SerializeRootSignature,
        D3D12SerializeVersionedRootSignature,
        D3D12CreateRootSignatureDeserializer,
        D3D12CreateVersionedRootSignatureDeserializer,
        D3D12EnableExperimentalFeatures,
        D3D12GetInterface,
        D3D10CreateBlob,
        D3D12XboxSetProcessDebugFlags,
    ]
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    Real,
    Null,
}

/// Decided once: the real backend unless disabled or the driver refuses.
pub fn backend() -> Backend {
    static B: OnceLock<Backend> = OnceLock::new();
    *B.get_or_init(|| {
        let cfg = crate::state::config();
        if cfg.graphics.eq_ignore_ascii_case("null") {
            crate::xlog_info!("D3D12.X: backend 'null' (z konfiguracji)");
            return Backend::Null;
        }
        match real::init() {
            Ok(desc) => {
                crate::xlog_info!("D3D12.X: backend 'real' – {desc}");
                Backend::Real
            }
            Err(e) => {
                crate::xlog_warn!("D3D12.X: nie udało się utworzyć urządzenia D3D12 ({e}) – przełączam na urządzenie zastępcze");
                Backend::Null
            }
        }
    })
}

/// Call a function of the system d3d12.dll by name.
unsafe fn sys(name: &str) -> Option<usize> {
    crate::resolver::system_proc("d3d12.dll", &crate::pe::ImportName::Name { hint: 0, name: name.into() })
}

macro_rules! forward {
    ($name:literal, $ty:ty, $null:expr, ($($a:ident),*)) => {{
        if backend() == Backend::Real {
            if let Some(f) = sys($name) {
                let f: $ty = std::mem::transmute(f);
                return f($($a),*);
            }
        }
        $null($($a),*)
    }};
}

pub unsafe extern "system" fn D3D12CreateDevice(adapter: *mut c_void, fl: u32, riid: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if backend() == Backend::Real {
        return real::create_device(riid, ppv);
    }
    null::D3D12CreateDevice(adapter, fl, riid, ppv)
}

pub unsafe extern "system" fn D3D12XboxCreateDevice(adapter: *mut c_void, params: *const c_void, riid: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if backend() == Backend::Real {
        return real::create_device(riid, ppv);
    }
    null::D3D12XboxCreateDevice(adapter, params, riid, ppv)
}

pub unsafe extern "system" fn D3D12GetDebugInterface(riid: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    forward!("D3D12GetDebugInterface", unsafe extern "system" fn(*const c_void, *mut *mut c_void) -> HRESULT, null::D3D12GetDebugInterface, (riid, ppv))
}

pub unsafe extern "system" fn D3D12SerializeRootSignature(d: *const c_void, v: u32, blob: *mut *mut c_void, err: *mut *mut c_void) -> HRESULT {
    forward!(
        "D3D12SerializeRootSignature",
        unsafe extern "system" fn(*const c_void, u32, *mut *mut c_void, *mut *mut c_void) -> HRESULT,
        null::D3D12SerializeRootSignature,
        (d, v, blob, err)
    )
}

pub unsafe extern "system" fn D3D12SerializeVersionedRootSignature(d: *const c_void, blob: *mut *mut c_void, err: *mut *mut c_void) -> HRESULT {
    forward!(
        "D3D12SerializeVersionedRootSignature",
        unsafe extern "system" fn(*const c_void, *mut *mut c_void, *mut *mut c_void) -> HRESULT,
        null::D3D12SerializeVersionedRootSignature,
        (d, blob, err)
    )
}

pub unsafe extern "system" fn D3D12CreateRootSignatureDeserializer(d: *const c_void, l: usize, r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    forward!(
        "D3D12CreateRootSignatureDeserializer",
        unsafe extern "system" fn(*const c_void, usize, *const c_void, *mut *mut c_void) -> HRESULT,
        null::D3D12CreateRootSignatureDeserializer,
        (d, l, r, ppv)
    )
}

pub unsafe extern "system" fn D3D12CreateVersionedRootSignatureDeserializer(d: *const c_void, l: usize, r: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    forward!(
        "D3D12CreateVersionedRootSignatureDeserializer",
        unsafe extern "system" fn(*const c_void, usize, *const c_void, *mut *mut c_void) -> HRESULT,
        null::D3D12CreateVersionedRootSignatureDeserializer,
        (d, l, r, ppv)
    )
}

pub unsafe extern "system" fn D3D12EnableExperimentalFeatures(n: u32, i: *const c_void, c: *mut c_void, s: *mut u32) -> HRESULT {
    forward!(
        "D3D12EnableExperimentalFeatures",
        unsafe extern "system" fn(u32, *const c_void, *mut c_void, *mut u32) -> HRESULT,
        null::D3D12EnableExperimentalFeatures,
        (n, i, c, s)
    )
}

pub unsafe extern "system" fn D3D12GetInterface(clsid: *const c_void, riid: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    forward!(
        "D3D12GetInterface",
        unsafe extern "system" fn(*const c_void, *const c_void, *mut *mut c_void) -> HRESULT,
        null::D3D12GetInterface,
        (clsid, riid, ppv)
    )
}

pub unsafe extern "system" fn D3D10CreateBlob(size: usize, out: *mut *mut c_void) -> HRESULT {
    null::D3D10CreateBlob(size, out)
}

pub unsafe extern "system" fn D3D12XboxSetProcessDebugFlags(_flags: u32) -> HRESULT {
    S_OK
}
