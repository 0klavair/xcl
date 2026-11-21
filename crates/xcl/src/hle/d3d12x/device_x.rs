//! The console device's own methods and the PC ones whose arguments differ on the console.
//!
//! The console's ID3D12Device has the PC ID3D12Device methods at slots 0–43; from 44 on its
//! vtable holds console extensions (as observed on Fortnite): CreatePlacedResourceX #44,
//! CreateCommandQueueX #48, SetDebugFlagsX #52, the pipeline-state (de)serialisation
//! #56 / #57 / #66 / #67, CreateGraphicsPipelineStateX #60, CreateSamplerX #61,
//! CreateComputePipelineStateX #62, CreateCommandListX #73, page pools #74 / #75, the frame
//! clock #78–#81 (`frame`), CreatePipelineState #86 (streams, mesh shaders). A title call
//! of any other extension slot is logged and answered S_OK (`real::class_for`) – not run as
//! the PC method at that index.
//!
//! Console queue and list types add DMA (0x10, the copy engine); console queue priorities
//! are mapped onto PC ones.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;
use windows_sys::core::GUID;

use super::real;
use crate::util::{HRESULT, S_OK};

const E_INVALIDARG: HRESULT = 0x8007_0057u32 as i32;
const E_NOTIMPL: HRESULT = 0x8000_4001u32 as i32;
const E_ACCESSDENIED: HRESULT = 0x8007_0005u32 as i32;
const E_FAIL: HRESULT = 0x8000_4005u32 as i32;

/// D3D12_COMMAND_LIST_TYPE of the console's DMA (copy) engine
pub const XBOX_TYPE_DMA: u32 = 0x10;
/// IID of the console's DMA command list (created as a PC graphics command list)
const IID_XBOX_DMA_COMMAND_LIST: GUID = GUID::from_u128(0x30bc77b9_8379_4b25_95de_b8588c42275d);
const IID_GRAPHICS_COMMAND_LIST: GUID = GUID::from_u128(0x5b160d0f_ac1b_4185_8ba8_b3ae42a5a455);
/// D3D12_INDIRECT_ARGUMENT_TYPE of the console's DispatchX
const XBOX_ARG_DISPATCH_X: u32 = 105;

fn guid_bytes(g: &GUID) -> (u32, u16, u16, [u8; 8]) {
    (g.data1, g.data2, g.data3, g.data4)
}

fn once(counter: &AtomicU64, n: u64) -> bool {
    counter.fetch_add(1, Ordering::Relaxed) < n
}

/// a console list / allocator type → the PC one
pub fn pc_list_type(t: u32) -> u32 {
    if t == XBOX_TYPE_DMA {
        D3D12_COMMAND_LIST_TYPE_COPY.0 as u32
    } else {
        t
    }
}

/// D3D12_COMMAND_QUEUE_DESC { type, priority, flags, node mask } of the console → PC
pub fn pc_queue_desc(d: [u32; 4]) -> [u32; 4] {
    let priority = match d[1] {
        0 | 100 | 10000 => d[1], // NORMAL, HIGH, GLOBAL_REALTIME
        _ => 100,
    };
    [pc_list_type(d[0]), priority, d[2], if d[3] == 0 { 0 } else { 1 }]
}

/// device #8 CreateCommandQueue / #48 CreateCommandQueueX(desc, riid, queue)
pub(super) unsafe extern "system" fn create_command_queue_x(this: *mut c_void, desc: *const [u32; 4], riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    let d = if desc.is_null() { [0; 4] } else { pc_queue_desc(desc.read_unaligned()) };
    real::dev_create_command_queue(this, d.as_ptr() as usize, riid, ppv)
}

/// device #9 CreateCommandAllocator(type, riid, allocator)
pub(super) unsafe extern "system" fn create_command_allocator_x(this: *mut c_void, ty: u32, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    real::dev_create_command_allocator(this, pc_list_type(ty) as usize, riid, ppv)
}

unsafe fn create_list(this: *mut c_void, node: u32, ty: u32, alloc: usize, pso: usize, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    let dma = !riid.is_null() && guid_bytes(&*riid) == guid_bytes(&IID_XBOX_DMA_COMMAND_LIST);
    let riid = if dma { &IID_GRAPHICS_COMMAND_LIST as *const GUID } else { riid };
    let (node, ty) = (if node == 0 { 0 } else { 1 }, pc_list_type(ty) as usize);
    if super::console_list::enabled() {
        real::dev_create_command_list_x(this, node, ty, alloc, pso, riid, ppv)
    } else {
        real::dev_create_command_list(this, node, ty, alloc, pso, riid, ppv)
    }
}

/// device #12 CreateCommandList(node mask, type, allocator, initial pipeline, riid, list):
/// console list types and the DMA list's IID
pub(super) unsafe extern "system" fn create_command_list_pc(this: *mut c_void, node: u32, ty: u32, alloc: usize, pso: usize, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    create_list(this, node, ty, alloc, pso, riid, ppv)
}

/// device #73 CreateCommandListX(params { type @0; node mask @8 }, allocator, initial
/// pipeline, riid, list)
pub(super) unsafe extern "system" fn create_command_list_x(this: *mut c_void, params: *const u32, alloc: usize, pso: usize, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    let (ty, node) = if params.is_null() { (0, 1) } else { (params.read_unaligned(), params.add(2).read_unaligned()) };
    create_list(this, node, ty, alloc, pso, riid, ppv)
}

/// device #60 CreateGraphicsPipelineStateX(desc, extension count, extensions, riid, pipeline):
/// the desc is a PC D3D12_GRAPHICS_PIPELINE_STATE_DESC with console shaders; the extensions
/// are not modelled
pub(super) unsafe extern "system" fn create_graphics_pso_x(this: *mut c_void, desc: *const c_void, extensions: u32, _ext: *const c_void, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    static N: AtomicU64 = AtomicU64::new(0);
    if extensions != 0 && once(&N, 1) {
        crate::xlog_info!("D3D12.X: CreateGraphicsPipelineStateX with {extensions} extension(s) – not modelled");
    }
    real::dev_create_graphics_pso(this, desc, riid, ppv)
}

/// device #86 CreatePipelineState(stream desc, riid, pipeline)
pub(super) unsafe extern "system" fn create_pipeline_state_x(this: *mut c_void, desc: *const c_void, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    real::dev_create_pipeline_state(this, desc, riid, ppv)
}

/// device #56 / #66 SerializePipelineStateX: not available in a release title
pub(super) unsafe extern "system" fn serialize_pso_x(_this: *mut c_void, _pso: *mut c_void) -> HRESULT {
    E_ACCESSDENIED
}

/// device #57 / #67 Deserialize{Graphics,Compute}PipelineStateX(state, riid, pipeline): the
/// serialised form is the console driver's; the title creates the pipeline from its desc
pub(super) unsafe extern "system" fn deserialize_pso_x(_this: *mut c_void, _state: *const u8, _riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    static N: AtomicU64 = AtomicU64::new(0);
    if once(&N, 1) {
        crate::xlog_warn!("D3D12.X: DeserializePipelineStateX – console pipeline caches are not supported");
    }
    if !ppv.is_null() {
        *ppv = std::ptr::null_mut();
    }
    E_NOTIMPL
}

/// device #52 SetDebugFlagsX(flags)
pub(super) unsafe extern "system" fn set_debug_flags_x(_this: *mut c_void, _flags: u32) -> HRESULT {
    S_OK
}

static PAGE_POOLS: Mutex<Vec<(u64, u32)>> = Mutex::new(Vec::new());

/// device #74 RegisterPagePoolX(address, pages, index out)
pub(super) unsafe extern "system" fn register_page_pool_x(_this: *mut c_void, va: u64, pages: u32, out: *mut u32) -> HRESULT {
    if va == 0 || pages == 0 {
        return E_INVALIDARG;
    }
    let mut p = PAGE_POOLS.lock().unwrap();
    let i = p.iter().position(|&e| e == (va, pages)).unwrap_or_else(|| {
        p.push((va, pages));
        p.len() - 1
    });
    if !out.is_null() {
        *out = i as u32;
    }
    S_OK
}

/// device #75 UnregisterPagePoolX(index)
pub(super) unsafe extern "system" fn unregister_page_pool_x(_this: *mut c_void, index: u64) -> HRESULT {
    match PAGE_POOLS.lock().unwrap().get_mut(index as usize) {
        Some(e) if e.1 != 0 => {
            *e = (0, 0);
            S_OK
        }
        _ => E_FAIL,
    }
}

/// device #61 CreateSamplerX(desc, destination): the title uses it for anisotropic samplers;
/// the console desc (0x6C bytes) carries the PC fields at other offsets (+0x18 address U / V /
/// W, +0x24 / +0x28 min / max LOD, +0x2C bias, +0x34 max anisotropy, +0x40 comparison); its
/// further anisotropy fields are not modelled
pub(super) unsafe extern "system" fn create_sampler_x(this: *mut c_void, x: *const u8, dest: u64) {
    if x.is_null() || !crate::gpu::shader::readable(x as usize, 0x6C) {
        return;
    }
    let u = |o: usize| (x.add(o) as *const u32).read_unaligned();
    let d = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_ANISOTROPIC,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE(u(0x18) as i32),
        AddressV: D3D12_TEXTURE_ADDRESS_MODE(u(0x1C) as i32),
        AddressW: D3D12_TEXTURE_ADDRESS_MODE(u(0x20) as i32),
        MipLODBias: f32::from_bits(u(0x2C)),
        MaxAnisotropy: u(0x34).clamp(1, 16),
        ComparisonFunc: D3D12_COMPARISON_FUNC(u(0x40) as i32),
        BorderColor: [0.0; 4],
        MinLOD: f32::from_bits(u(0x24)),
        MaxLOD: f32::from_bits(u(0x28)),
    };
    real::dev_create_sampler_x(this, &d, dest);
}

/// the console resource desc (0x38 bytes, D3D12_RESOURCE_DESC) as a PC one: its tile layouts
/// and console flag bits mean nothing to a PC driver
pub fn pc_resource_desc(x: &[u8; 0x38]) -> [u8; 0x38] {
    let mut d = *x;
    let dim = u32::from_le_bytes(d[0..4].try_into().unwrap());
    let flags = u32::from_le_bytes(d[0x30..0x34].try_into().unwrap());
    d[8..16].fill(0); // alignment: the driver's
    let layout = if dim == D3D12_RESOURCE_DIMENSION_BUFFER.0 as u32 { D3D12_TEXTURE_LAYOUT_ROW_MAJOR.0 } else { D3D12_TEXTURE_LAYOUT_UNKNOWN.0 };
    d[0x2C..0x30].copy_from_slice(&(layout as u32).to_le_bytes());
    d[0x30..0x34].copy_from_slice(&(flags & 0xFF).to_le_bytes());
    d
}

/// device #44 CreatePlacedResourceX(address, desc, initial state, clear value, riid, resource):
/// a resource in title memory. A buffer at a 64 KiB-aligned address in unified memory is that
/// memory (placed in its UMA heap); anything else gets memory of its own – a texture's console
/// tiling has no PC equivalent, so what the title writes at the address is not seen there.
pub(super) unsafe extern "system" fn create_placed_resource_x(this: *mut c_void, va: u64, desc: *const u8, state: u32, clear: *const c_void, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    static N: AtomicU64 = AtomicU64::new(0);
    if desc.is_null() || ppv.is_null() || !crate::gpu::shader::readable(desc as usize, 0x38) {
        return E_INVALIDARG;
    }
    let x: [u8; 0x38] = (desc as *const [u8; 0x38]).read_unaligned();
    let d = pc_resource_desc(&x);
    let buffer = u32::from_le_bytes(d[0..4].try_into().unwrap()) == D3D12_RESOURCE_DIMENSION_BUFFER.0 as u32;
    if buffer {
        if let Some(hr) = real::place_at_title_address(this, va as usize, &d, state, clear, riid, ppv) {
            return hr;
        }
    }
    let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
    let hr = real::dev_create_committed_resource(this, &heap as *const _ as usize, 0, d.as_ptr() as usize, state as usize, clear as usize, riid, ppv);
    if hr < 0 || once(&N, 4) {
        let w = |o: usize| u32::from_le_bytes(x[o..o + 4].try_into().unwrap());
        crate::xlog_info!(
            "D3D12.X: CreatePlacedResourceX at {va:#x}: dimension {} {}x{} format {} flags {:#x} – a resource of its own ({hr:#x})",
            w(0),
            u64::from_le_bytes(x[0x10..0x18].try_into().unwrap()),
            w(0x18),
            w(0x20),
            w(0x30)
        );
    }
    hr
}

/// device #38 GetCopyableFootprints: a console resource desc the PC rejects (total stays
/// UINT64_MAX) is asked again as the PC desc XCL creates for it
#[allow(clippy::too_many_arguments)]
pub(super) unsafe extern "system" fn get_copyable_footprints_x(
    this: *mut c_void,
    desc: *const u8,
    first: u32,
    num: u32,
    base: u64,
    layouts: *mut c_void,
    rows: *mut u32,
    row_sizes: *mut u64,
    total: *mut u64,
) {
    type F = unsafe extern "system" fn(*mut c_void, *const u8, u32, u32, u64, *mut c_void, *mut u32, *mut u64, *mut u64);
    let f: F = std::mem::transmute(real::orig_fn(this, 38));
    if desc.is_null() || !crate::gpu::shader::readable(desc as usize, 0x38) {
        return f(this, desc, first, num, base, layouts, rows, row_sizes, total);
    }
    let x: [u8; 0x38] = (desc as *const [u8; 0x38]).read_unaligned();
    let mut probe = u64::MAX;
    let t = if total.is_null() { &mut probe as *mut u64 } else { total };
    *t = u64::MAX;
    f(this, x.as_ptr(), first, num, base, layouts, rows, row_sizes, t);
    if *t == u64::MAX {
        let d = pc_resource_desc(&x);
        f(this, d.as_ptr(), first, num, base, layouts, rows, row_sizes, total);
    }
}

/// device #41 CreateCommandSignature(desc, root signature, riid, signature): the console's
/// DispatchX argument (105) is a PC Dispatch; the root signature only goes with arguments
/// that need one (constants, CBV / SRV / UAV)
pub(super) unsafe extern "system" fn create_command_signature_x(this: *mut c_void, desc: *const u8, root: usize, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    if desc.is_null() || ppv.is_null() {
        return E_INVALIDARG;
    }
    let mut pc = [0u8; 24];
    std::ptr::copy_nonoverlapping(desc, pc.as_mut_ptr(), 24);
    let n = u32::from_le_bytes(pc[4..8].try_into().unwrap()) as usize;
    let args = u64::from_le_bytes(pc[8..16].try_into().unwrap()) as *const [u32; 4];
    let mut list: Vec<[u32; 4]> = if args.is_null() || n > 64 { Vec::new() } else { (0..n).map(|i| args.add(i).read_unaligned()).collect() };
    for a in list.iter_mut() {
        if a[0] == XBOX_ARG_DISPATCH_X {
            a[0] = D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH.0 as u32;
        }
    }
    let needs_root = list.iter().any(|a| (5..=8).contains(&a[0]));
    pc[8..16].copy_from_slice(&(if list.is_empty() { 0 } else { list.as_ptr() as u64 }).to_le_bytes());
    real::dev_create_command_signature(this, pc.as_ptr() as usize, if needs_root { root } else { 0 }, riid, ppv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_types_and_descs_become_pc_ones() {
        assert_eq!(pc_list_type(XBOX_TYPE_DMA), D3D12_COMMAND_LIST_TYPE_COPY.0 as u32);
        assert_eq!(pc_list_type(2), 2);
        assert_eq!(pc_queue_desc([XBOX_TYPE_DMA, 1, 0, 0x10]), [3, 100, 0, 1]);
        assert_eq!(pc_queue_desc([0, 10000, 1, 0]), [0, 10000, 1, 0]);
        let mut x = [0u8; 0x38];
        x[0] = 1; // buffer
        x[8] = 0x40; // alignment
        x[0x2C] = 0x22; // console layout
        x[0x30..0x34].copy_from_slice(&0x1234u32.to_le_bytes());
        let d = pc_resource_desc(&x);
        assert_eq!((d[8], u32::from_le_bytes(d[0x2C..0x30].try_into().unwrap()), u32::from_le_bytes(d[0x30..0x34].try_into().unwrap())), (0, 1, 0x34));
        x[0] = 3; // texture 2D
        assert_eq!(u32::from_le_bytes(pc_resource_desc(&x)[0x2C..0x30].try_into().unwrap()), 0);
    }

    #[test]
    fn page_pools_register_once_and_unregister() {
        unsafe {
            let mut i = u32::MAX;
            assert_eq!(register_page_pool_x(std::ptr::null_mut(), 0x1_0000_0000, 16, &mut i), S_OK);
            let mut j = u32::MAX;
            assert_eq!(register_page_pool_x(std::ptr::null_mut(), 0x1_0000_0000, 16, &mut j), S_OK);
            assert_eq!(i, j);
            assert_eq!(register_page_pool_x(std::ptr::null_mut(), 0, 16, &mut j), E_INVALIDARG);
            assert_eq!(unregister_page_pool_x(std::ptr::null_mut(), i as u64), S_OK);
            assert_eq!(unregister_page_pool_x(std::ptr::null_mut(), i as u64), E_FAIL);
        }
    }
}
