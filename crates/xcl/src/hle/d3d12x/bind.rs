//! Root arguments of console code, bound when it dispatches with a translated pipeline.
//!
//! A translated console shader reads its root arguments the console's way (user SGPRs s4…,
//! the rest from a table at s[0:1]: `console_list::user_data`) and memory through XCL's
//! shader ABI (u0 io, u1 res, u2… unified memory). So on a command list of console code the
//! compute root signature and root arguments are kept here – packed by the root signature's
//! layout – instead of going to the PC list, and each Dispatch with a translated pipeline
//! binds the ABI root signature, writes the user SGPRs (`io`) and the argument table to
//! unified memory and dispatches. That memory stays until the list's allocator is reset.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

use super::console_list::{self, Layout, Param};

#[derive(Default)]
struct State {
    layout: Option<Arc<Layout>>,
    args: Vec<u8>,
    pso: usize,
    allocator: usize,
    /// unified memory of this recording (io, argument tables)
    uma: Vec<usize>,
}

static STATES: Mutex<Option<HashMap<usize, State>>> = Mutex::new(None);

fn with<R>(list: *mut c_void, f: impl FnOnce(&mut State) -> R) -> R {
    f(STATES.lock().unwrap().get_or_insert_with(HashMap::new).entry(list as usize).or_default())
}

/// The list was reset onto `allocator`: the previous recording's memory is the previous
/// allocator's until that one is reset.
pub(super) fn reset(list: *mut c_void, allocator: usize) {
    let (old, blocks) = with(list, |s| {
        let old = std::mem::replace(&mut s.allocator, allocator);
        (old, std::mem::take(&mut s.uma))
    });
    console_list::hold_until_reset(old, blocks);
}

/// The list is gone.
pub(super) fn forget(list: *mut c_void) {
    if let Some(s) = STATES.lock().unwrap().as_mut().and_then(|m| m.remove(&(list as usize))) {
        console_list::hold_until_reset(s.allocator, s.uma);
    }
}

/// bytes of parameter `i` at constant `offset`
fn put(list: *mut c_void, i: u32, offset: u32, bytes: &[u8]) {
    with(list, |s| {
        let Some(at) = s.layout.as_ref().and_then(|l| l.params.get(i as usize)).map(|p| match *p {
            Param::Constants { at, .. } => at as usize + 4 * offset as usize,
            Param::Descriptor { at } | Param::Table { at } => at as usize,
        }) else {
            return;
        };
        if let Some(t) = s.args.get_mut(at..at + bytes.len()) {
            t.copy_from_slice(bytes);
        }
    })
}

/// #33 SetComputeRootSignature (PC #29)
pub(super) unsafe extern "system" fn set_compute_root_signature(this: *mut c_void, rs: *mut c_void) {
    let layout = console_list::layout_of(rs as usize);
    with(this, |s| {
        s.args = vec![0u8; layout.as_ref().map_or(0, |l| l.size as usize)];
        s.layout = layout;
    });
}

/// #35 SetComputeRootDescriptorTable (PC #31): the table's console address, 32 bits
pub(super) unsafe extern "system" fn set_compute_root_table(this: *mut c_void, i: u32, handle: u64) {
    put(this, i, 0, &(handle as u32).to_le_bytes());
}

/// #37 SetComputeRoot32BitConstant (PC #33)
pub(super) unsafe extern "system" fn set_compute_root_constant(this: *mut c_void, i: u32, value: u32, offset: u32) {
    put(this, i, offset, &value.to_le_bytes());
}

/// #39 SetComputeRoot32BitConstants (PC #35)
pub(super) unsafe extern "system" fn set_compute_root_constants(this: *mut c_void, i: u32, n: u32, data: *const u32, offset: u32) {
    if !data.is_null() {
        let bytes: Vec<u8> = std::slice::from_raw_parts(data, n as usize).iter().flat_map(|w| w.to_le_bytes()).collect();
        put(this, i, offset, &bytes);
    }
}

/// #41 / #43 / #45 SetComputeRoot{ConstantBuffer,ShaderResource,UnorderedAccess}View (PC #37 /
/// #39 / #41): the 64-bit GPU address
pub(super) unsafe extern "system" fn set_compute_root_descriptor(this: *mut c_void, i: u32, va: u64) {
    put(this, i, 0, &va.to_le_bytes());
}

/// #25 SetPipelineState: noted, then bound as on the PC
pub(super) unsafe extern "system" fn set_pipeline_state(this: *mut c_void, pso: *mut c_void) {
    with(this, |s| s.pso = pso as usize);
    super::real::bind_pipeline(this, pso);
}

/// #14 Dispatch: with a translated pipeline, on XCL's ABI with the kept root arguments
pub(super) unsafe extern "system" fn dispatch(this: *mut c_void, x: u32, y: u32, z: u32) {
    let pso = with(this, |s| s.pso);
    let Some(list) = ID3D12GraphicsCommandList::from_raw_borrowed(&this) else { return };
    if !super::real::is_translated_compute(pso) {
        // a console pipeline without a translation: nothing to run (its root arguments were
        // not given to the PC list either)
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 4 {
            crate::xlog_warn!("D3D12.X: Dispatch with a pipeline that has no translated console shader ({pso:#x}) skipped");
        }
        return;
    }
    if let Err(e) = translated_dispatch(this, list, pso, x, y, z) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 4 {
            crate::xlog_warn!("D3D12.X: Dispatch with a translated pipeline skipped: {e}");
        }
    }
}

unsafe fn translated_dispatch(this: *mut c_void, list: &ID3D12GraphicsCommandList, pso: usize, x: u32, y: u32, z: u32) -> Result<(), String> {
    let u = crate::gpu::uma::get().ok_or("no unified memory")?;
    let windows = super::real::uma_window_vas();
    let first = *windows.first().ok_or("unified memory is not open on the GPU")?;
    let (layout, args) = with(this, |s| (s.layout.clone(), s.args.clone()));
    let mut owned = Vec::new();
    let mut copy = |bytes: &[u8]| -> Result<usize, String> {
        let p = u.alloc_in(0, bytes.len().max(16), 256).ok_or("unified memory is full")?; // 32-bit console addresses: window 0
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        owned.push(p as usize);
        Ok(p as usize)
    };
    let inline = layout.as_ref().map_or(0, |l| l.inline as usize).min(args.len());
    let table_at = copy(&args[inline..])?;
    let mut io = match layout.as_ref() {
        Some(l) => console_list::user_data(l, &args, table_at as u64),
        None => vec![table_at as u32, (table_at >> 32) as u32],
    };
    io.resize(crate::gpu::recomp::KERNEL_SGPRS, 0);
    let io_at = copy(&io.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>())?;
    let res_at = copy(&[0u8; 256])?;
    with(this, |s| s.uma.extend(owned));
    let va = |p: usize| super::real::uma_va_of(p).unwrap_or(first);
    // a pipeline still being built in the background is waited for
    let real = super::pso_async::resolve_wait(pso);
    let rs = super::real::pso_root_signature(real).or_else(|| super::real::abi_rs_cached().map(|p| p as usize)).ok_or("no XCL ABI root signature")?;
    let rs = rs as *mut c_void;
    let rs = ID3D12RootSignature::from_raw_borrowed(&rs).ok_or("root signature")?;
    list.SetComputeRootSignature(rs);
    list.SetComputeRootUnorderedAccessView(0, va(io_at));
    list.SetComputeRootUnorderedAccessView(1, va(res_at));
    for k in 0..4 {
        list.SetComputeRootUnorderedAccessView(2 + k as u32, windows.get(k).copied().unwrap_or(first));
    }
    let real = real as *mut c_void;
    let pc = ID3D12PipelineState::from_raw_borrowed(&real).ok_or("pipeline")?;
    list.SetPipelineState(pc);
    list.Dispatch(x, y, z);
    Ok(())
}
