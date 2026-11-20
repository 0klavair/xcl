//! Real D3D12.X backend: the title gets genuine D3D12 objects from the PC driver.
//!
//! Every object handed to the title is "adopted": its vtable pointer is replaced with
//! an extended copy:
//!
//! ```text
//! ext[-1]                 original vtable (for calling originals)
//! ext[0 .. public)        original PC methods (a few overridden, see below)
//! ext[public .. internal) guard thunks: runtime/DXGI calls → original internal virtual,
//!                         calls from the game image → logged as console-only method
//! ext[internal .. +64)    logging stubs (console-only methods past the end)
//! ```
//!
//! Overrides: QueryInterface (logs unknown IIDs), every Create* on the device (adopts
//! the new object, retries with the PC IID when the console one is unknown, captures
//! shaders, builds fallback pipeline states when console shaders are rejected),
//! CreateRenderTargetView / OMSetRenderTargets / ExecuteCommandLists (presentation).

#![allow(non_snake_case)]

use std::collections::{BTreeSet, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows_sys::core::GUID;

use super::layout::{self, Iface};
use crate::stubs;
use crate::util::{HRESULT, E_INVALIDARG, E_NOINTERFACE, S_FALSE, S_OK};

const DLL: &str = "d3d12_xs.dll";
const EXTRA_SLOTS: usize = 64;
const MAX_SCAN: usize = 512;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Kind {
    Device,
    CommandQueue,
    CommandAllocator,
    CommandList,
    Fence,
    DescriptorHeap,
    Resource,
    Heap,
    PipelineState,
    RootSignature,
    QueryHeap,
    CommandSignature,
    PipelineLibrary,
    StateObject,
}

impl Kind {
    pub fn base(self) -> &'static str {
        match self {
            Kind::Device => "ID3D12Device",
            Kind::CommandQueue => "ID3D12CommandQueue",
            Kind::CommandAllocator => "ID3D12CommandAllocator",
            Kind::CommandList => "ID3D12GraphicsCommandList",
            Kind::Fence => "ID3D12Fence",
            Kind::DescriptorHeap => "ID3D12DescriptorHeap",
            Kind::Resource => "ID3D12Resource",
            Kind::Heap => "ID3D12Heap",
            Kind::PipelineState => "ID3D12PipelineState",
            Kind::RootSignature => "ID3D12RootSignature",
            Kind::QueryHeap => "ID3D12QueryHeap",
            Kind::CommandSignature => "ID3D12CommandSignature",
            Kind::PipelineLibrary => "ID3D12PipelineLibrary",
            Kind::StateObject => "ID3D12StateObject",
        }
    }
}

// ---------------------------------------------------------------------------------------
// statistics for the report
// ---------------------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Serialize, serde::Deserialize)]
pub struct ClassStat {
    pub interface: String,
    pub public_methods: usize,
    pub internal_slots: usize,
}

#[derive(Debug, Default, Clone, Serialize, serde::Deserialize)]
pub struct D3d12Stats {
    pub backend: String,
    pub adapter: String,
    pub classes: Vec<ClassStat>,
    pub unknown_iids: Vec<String>,
    pub fallback_pipelines: u64,
    pub failed_pipelines: u64,
    pub fallback_root_signatures: u64,
    pub presents: u64,
    pub frame_boundaries: u64,
    /// unified memory region opened as a GPU heap (`gpu::uma`): size, bytes in use, buffer VA
    #[serde(default)]
    pub uma_mb: u64,
    #[serde(default)]
    pub uma_used_kb: u64,
    #[serde(default)]
    pub uma_gpu_va: String,
    #[serde(default)]
    pub uma_error: String,
    /// graphics pipelines built from recompiled console shaders (XCL shader ABI root signature)
    #[serde(default)]
    pub translated_pipelines: u64,
    /// why console shaders could not be used (first messages)
    #[serde(default)]
    pub translation_errors: Vec<String>,
}

static ADAPTER: Mutex<String> = Mutex::new(String::new());
static UNKNOWN_IIDS: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
static FALLBACK_PSO: AtomicU64 = AtomicU64::new(0);
static FAILED_PSO: AtomicU64 = AtomicU64::new(0);
static FALLBACK_RS: AtomicU64 = AtomicU64::new(0);
static TRANSLATED_PSO: AtomicU64 = AtomicU64::new(0);
static TRANSLATE_ERRORS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// XCL shader ABI root signature on the backend device (raw, kept for the process lifetime)
static ABI_RS: AtomicUsize = AtomicUsize::new(0);

/// XCL's shader ABI root signature, once a translated pipeline created it.
pub fn abi_rs_cached() -> Option<*mut c_void> {
    match ABI_RS.load(Ordering::Acquire) {
        0 => None,
        p => Some(p as *mut c_void),
    }
}

thread_local! {
    /// the last compute pipeline created on this thread runs a translated console shader
    static LAST_COMPUTE_TRANSLATED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Root signature of the pipelines built from translated console shaders (bind it, then the
/// XCL ABI root UAVs io / res / mem, to draw with them).
pub fn abi_root_signature() -> Option<ID3D12RootSignature> {
    let raw = ABI_RS.load(Ordering::Acquire) as *mut c_void;
    if raw.is_null() {
        return None;
    }
    unsafe { ID3D12RootSignature::from_raw_borrowed(&raw).cloned() }
}
/// UMA as one D3D12 buffer (placed in a heap opened over the process memory): the `mem`
/// of recompiled shaders. Kept for the process lifetime.
static UMA_BUFFER: Mutex<Option<(usize, u64)>> = Mutex::new(None);
static UMA_ERROR: Mutex<String> = Mutex::new(String::new());

/// GPU virtual address of the unified memory buffer (0: not available).
pub fn uma_gpu_va() -> u64 {
    UMA_BUFFER.lock().ok().and_then(|g| g.map(|b| b.1)).unwrap_or(0)
}

unsafe fn open_uma(device: &ID3D12Device) {
    let Some(u) = crate::gpu::uma::get() else {
        *UMA_ERROR.lock().unwrap() = "brak regionu UMA".into();
        return;
    };
    let r = (|| -> Result<(usize, u64), String> {
        let dev3: ID3D12Device3 = device.cast().map_err(|e| format!("ID3D12Device3: {e}"))?;
        let heap: ID3D12Heap = dev3.OpenExistingHeapFromAddress(u.base() as *const c_void).map_err(|e| format!("OpenExistingHeapFromAddress: {e}"))?;
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: u.size() as u64,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS | D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER,
            ..Default::default()
        };
        let mut buf: Option<ID3D12Resource> = None;
        device.CreatePlacedResource(&heap, 0, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut buf).map_err(|e| format!("CreatePlacedResource: {e}"))?;
        let buf = buf.ok_or("CreatePlacedResource")?;
        let va = buf.GetGPUVirtualAddress();
        std::mem::forget(heap);
        Ok((buf.into_raw() as usize, va))
    })();
    match r {
        Ok(b) => {
            crate::xlog_info!("D3D12.X: UMA ({} MB) otwarta jako sterta GPU, bufor pod VA {:#x}", u.size() >> 20, b.1);
            *UMA_BUFFER.lock().unwrap() = Some(b);
        }
        Err(e) => {
            crate::xlog_warn!("D3D12.X: UMA niedostępna dla GPU: {e}");
            *UMA_ERROR.lock().unwrap() = e;
        }
    }
}

pub fn stats() -> D3d12Stats {
    let classes = CLASSES
        .lock()
        .map(|g| {
            g.iter()
                .map(|c| ClassStat { interface: c.iface.name.to_string(), public_methods: c.public, internal_slots: c.scanned - c.public })
                .collect()
        })
        .unwrap_or_default();
    let (presents, boundaries) = super::present::counters();
    D3d12Stats {
        backend: format!("{:?}", super::backend()).to_lowercase(),
        adapter: ADAPTER.lock().map(|a| a.clone()).unwrap_or_default(),
        classes,
        unknown_iids: UNKNOWN_IIDS.lock().map(|s| s.iter().cloned().collect()).unwrap_or_default(),
        fallback_pipelines: FALLBACK_PSO.load(Ordering::Relaxed),
        failed_pipelines: FAILED_PSO.load(Ordering::Relaxed),
        fallback_root_signatures: FALLBACK_RS.load(Ordering::Relaxed),
        presents,
        frame_boundaries: boundaries,
        uma_mb: crate::gpu::uma::get().map(|u| (u.size() >> 20) as u64).unwrap_or(0),
        uma_used_kb: crate::gpu::uma::get().map(|u| (u.used() >> 10) as u64).unwrap_or(0),
        uma_gpu_va: match uma_gpu_va() {
            0 => String::new(),
            v => format!("{v:#x}"),
        },
        uma_error: UMA_ERROR.lock().map(|e| e.clone()).unwrap_or_default(),
        translated_pipelines: TRANSLATED_PSO.load(Ordering::Relaxed),
        translation_errors: TRANSLATE_ERRORS.lock().map(|e| e.clone()).unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------------------
// device creation
// ---------------------------------------------------------------------------------------

static DEVICE: AtomicUsize = AtomicUsize::new(0);

pub fn device_raw() -> *mut c_void {
    DEVICE.load(Ordering::Acquire) as *mut c_void
}

fn adapter_name(desc: &DXGI_ADAPTER_DESC1) -> String {
    let n = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
    String::from_utf16_lossy(&desc.Description[..n])
}

/// Create the process-wide D3D12 device (high-performance GPU → WARP); once – later calls
/// return the existing device's adapter.
pub fn init() -> Result<String, String> {
    if !device_raw().is_null() {
        return Ok(ADAPTER.lock().map(|a| a.clone()).unwrap_or_default());
    }
    unsafe {
        let cfg = crate::state::config();
        if cfg.d3d12_debug {
            let mut dbg: Option<ID3D12Debug> = None;
            if D3D12GetDebugInterface(&mut dbg).is_ok() {
                if let Some(d) = dbg {
                    d.EnableDebugLayer();
                    crate::xlog_info!("D3D12: warstwa debug włączona");
                }
            }
        }
        let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).map_err(|e| format!("DXGI: {e}"))?;
        let mut device: Option<ID3D12Device> = None;
        let mut name = String::new();
        if !cfg.d3d12_adapter.eq_ignore_ascii_case("warp") {
            let mut i = 0;
            while let Ok(adapter) = factory.EnumAdapterByGpuPreference::<IDXGIAdapter1>(i, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE) {
                i += 1;
                let Ok(desc) = adapter.GetDesc1() else { continue };
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                    continue;
                }
                if D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).is_ok() {
                    name = format!("{} ({} MiB VRAM)", adapter_name(&desc), desc.DedicatedVideoMemory >> 20);
                    break;
                }
            }
        }
        if device.is_none() {
            let warp: IDXGIAdapter = factory.EnumWarpAdapter().map_err(|e| format!("WARP: {e}"))?;
            D3D12CreateDevice(&warp, D3D_FEATURE_LEVEL_11_0, &mut device).map_err(|e| format!("D3D12CreateDevice(WARP): {e}"))?;
            name = "WARP (renderer programowy)".into();
        }
        let device = device.ok_or("brak urządzenia")?;
        open_uma(&device);
        let raw = device.into_raw(); // one reference kept for the process lifetime
        adopt(raw, Kind::Device);
        DEVICE.store(raw as usize, Ordering::Release);
        *ADAPTER.lock().unwrap() = name.clone();
        Ok(name)
    }
}

type QiFn = unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
type RefFn = unsafe extern "system" fn(*mut c_void) -> u32;

pub unsafe fn create_device(riid: *const c_void, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return S_FALSE;
    }
    let dev = device_raw();
    if dev.is_null() {
        return E_INVALIDARG;
    }
    if !riid.is_null() {
        let qi: QiFn = std::mem::transmute(*vt(dev));
        let hr = qi(dev, riid as *const GUID, ppv);
        if hr >= 0 {
            return hr;
        }
    }
    let add: RefFn = std::mem::transmute(*vt(dev).add(1));
    add(dev);
    *ppv = dev;
    S_OK
}

// ---------------------------------------------------------------------------------------
// vtable adoption
// ---------------------------------------------------------------------------------------

struct Class {
    orig: usize,
    ext: usize,
    iface: &'static Iface,
    public: usize,
    scanned: usize,
}

static CLASSES: Mutex<Vec<Class>> = Mutex::new(Vec::new());
static EXTS: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

unsafe fn vt(obj: *mut c_void) -> *const usize {
    *(obj as *const *const usize)
}

/// Original function of `slot` for an adopted object.
pub unsafe fn orig_fn(this: *mut c_void, slot: usize) -> usize {
    let ext = vt(this);
    let orig = *ext.sub(1) as *const usize;
    *orig.add(slot)
}

fn module_of(addr: usize) -> usize {
    use windows_sys::Win32::System::LibraryLoader::{
        GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };
    let mut h = std::ptr::null_mut();
    unsafe {
        GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, addr as *const u16, &mut h);
    }
    h as usize
}

fn is_ext(v: usize) -> bool {
    EXTS.lock().map(|g| g.as_ref().map(|s| s.contains(&v)).unwrap_or(false)).unwrap_or(false)
}

/// Give a genuine D3D12 object the extended vtable for `kind`.
pub unsafe fn adopt(obj: *mut c_void, kind: Kind) {
    if obj.is_null() {
        return;
    }
    let v = vt(obj) as usize;
    if is_ext(v) {
        return;
    }
    let ext = class_for(obj, v, kind);
    *(obj as *mut usize) = ext;
}

unsafe fn class_for(obj: *mut c_void, orig: usize, kind: Kind) -> usize {
    if let Some(c) = CLASSES.lock().unwrap().iter().find(|c| c.orig == orig) {
        return c.ext;
    }
    let ot = orig as *const usize;
    // most derived PC interface this object implements
    let chain = layout::chain(kind.base());
    let qi: QiFn = std::mem::transmute(*ot);
    let release: RefFn = std::mem::transmute(*ot.add(2));
    let mut iface = *chain.last().expect("layout chain");
    for i in &chain {
        let iid = layout::parse_guid(i.iid);
        let mut out = std::ptr::null_mut();
        if qi(obj, &iid, &mut out) >= 0 && !out.is_null() {
            release(out);
            iface = i;
            break;
        }
    }
    let public = iface.methods.len();
    let m0 = module_of(*ot);
    let mut scanned = 0;
    while scanned < public + MAX_SCAN && module_of(*ot.add(scanned)) == m0 {
        scanned += 1;
    }
    let scanned = scanned.max(public);
    let total = scanned + EXTRA_SLOTS;
    let mut v: Vec<usize> = Vec::with_capacity(total + 1);
    v.push(orig);
    for i in 0..scanned {
        v.push(*ot.add(i));
    }
    let hook = if kind == Kind::CommandQueue { super::present::on_queue_console_method as usize } else { 0 };
    for i in public..scanned {
        let info = stubs::new_info(DLL, &format!("{}::#{i} (spoza PC)", iface.name), 0);
        info.hook.store(hook, Ordering::Release);
        v[1 + i] = super::guard::make_guard(v[1 + i], info);
    }
    for i in scanned..total {
        let (addr, info) = stubs::make_stub_info(DLL, &format!("{}::#{i} (spoza PC)", iface.name), 0);
        info.hook.store(hook, Ordering::Release);
        v.push(addr);
    }
    for (method, f) in overrides(kind) {
        if let Some(s) = layout::slot(iface, method) {
            if s < public {
                v[1 + s] = f;
            }
        }
    }
    // slots where the console's method differs from the PC one at the same index
    for (slot, f) in console_slots(kind) {
        if slot < public {
            v[1 + slot] = super::guard::make_split(f, v[1 + slot]);
        }
    }
    // XCL_D3D12_TRACE: every public method called from console code is logged
    if std::env::var_os("XCL_D3D12_TRACE").is_some() {
        for i in 0..public {
            let name = format!("{}::{} (#{i})", iface.name, iface.methods.get(i).copied().unwrap_or("?"));
            v[1 + i] = super::guard::make_tracer(v[1 + i], name);
        }
    }
    let leaked: &'static [usize] = Box::leak(v.into_boxed_slice());
    let ext = leaked.as_ptr() as usize + 8;
    EXTS.lock().unwrap().get_or_insert_with(HashSet::new).insert(ext);
    crate::xlog_info!(
        "D3D12.X: {} – {public} metod PC, {} slotów wewnętrznych pod strażnikiem, +{EXTRA_SLOTS} zaślepek",
        iface.name,
        scanned - public
    );
    CLASSES.lock().unwrap().push(Class { orig, ext, iface, public, scanned });
    ext
}

/// Console vtable slots that differ from the PC layout (found on console code calling them):
/// * device #62: `CreateComputePipelineState` with two extra arguments (XboxBvhBuilder.dll
///   passes a D3D12_COMPUTE_PIPELINE_STATE_DESC, NULL, NULL, riid, ppv); PC #62 = CreateStateObject.
/// * device #12 `CreateCommandList` (only with console lists on): console code gets a
///   console-shaped list (D3D12.X inline code writes into it), XCL's own code a PC one.
fn console_slots(kind: Kind) -> Vec<(usize, usize)> {
    match kind {
        Kind::Device => {
            let mut v = vec![(62, dev_create_compute_pso_x as usize)];
            if super::console_list::enabled() {
                v.push((12, dev_create_command_list_x as usize));
            }
            v
        }
        _ => vec![],
    }
}

unsafe extern "system" fn dev_create_command_list_x(this: *mut c_void, mask: usize, ty: usize, alloc: usize, pso: usize, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    let pc_pso = super::console_list::pc_pso(pso as *mut c_void) as usize;
    let hr = dev_create_command_list(this, mask, ty, alloc, pc_pso, riid, ppv);
    if hr >= 0 && !ppv.is_null() && !(*ppv).is_null() {
        *ppv = super::console_list::create_with_allocator(*ppv, alloc as *mut c_void);
    }
    hr
}

unsafe extern "system" fn dev_create_compute_pso_x(this: *mut c_void, desc: *const c_void, a: usize, b: usize, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    if a != 0 || b != 0 {
        crate::xlog_warn!("CreateComputePipelineState (konsola, slot 62): nieznane parametry {a:#x} {b:#x} pominięte");
    }
    let hr = dev_create_compute_pso(this, desc, riid, ppv);
    // console code writes into pipeline objects (D3D12.X inline code): give it a console-shaped one
    if hr >= 0 && super::console_list::enabled() && !ppv.is_null() && !(*ppv).is_null() {
        *ppv = super::console_list::wrap_pso(*ppv, LAST_COMPUTE_TRANSLATED.with(|c| c.get()));
    }
    hr
}

fn overrides(kind: Kind) -> Vec<(&'static str, usize)> {
    let mut v: Vec<(&'static str, usize)> = vec![("QueryInterface", qi_override as usize)];
    match kind {
        Kind::Device => v.extend_from_slice(&[
            ("CreateCommandQueue", dev_create_command_queue as usize),
            ("CreateCommandAllocator", dev_create_command_allocator as usize),
            ("CreateGraphicsPipelineState", dev_create_graphics_pso as usize),
            ("CreateComputePipelineState", dev_create_compute_pso as usize),
            ("CreateCommandList", dev_create_command_list as usize),
            ("CreateDescriptorHeap", dev_create_descriptor_heap as usize),
            ("CreateRootSignature", dev_create_root_signature as usize),
            ("CreateRenderTargetView", dev_create_rtv as usize),
            ("CreateCommittedResource", dev_create_committed_resource as usize),
            ("CreateHeap", dev_create_heap as usize),
            ("CreatePlacedResource", dev_create_placed_resource as usize),
            ("CreateReservedResource", dev_create_reserved_resource as usize),
            ("CreateFence", dev_create_fence as usize),
            ("CreateQueryHeap", dev_create_query_heap as usize),
            ("CreateCommandSignature", dev_create_command_signature as usize),
            ("CreatePipelineLibrary", dev_create_pipeline_library as usize),
            ("CreatePipelineState", dev_create_pipeline_state as usize),
            ("CreateCommandList1", dev_create_command_list1 as usize),
            ("CreateCommittedResource1", dev_create_committed_resource1 as usize),
            ("CreateHeap1", dev_create_heap1 as usize),
            ("CreateReservedResource1", dev_create_reserved_resource1 as usize),
            ("CreateStateObject", dev_create_state_object as usize),
            ("CreateCommittedResource2", dev_create_committed_resource2 as usize),
            ("CreatePlacedResource1", dev_create_placed_resource1 as usize),
            ("CreateCommandQueue1", dev_create_command_queue1 as usize),
        ]),
        Kind::CommandList => v.push(("OMSetRenderTargets", cl_om_set_render_targets as usize)),
        Kind::CommandQueue => v.push(("ExecuteCommandLists", q_execute_command_lists as usize)),
        Kind::CommandAllocator => v.push(("Reset", alloc_reset as usize)),
        _ => {}
    }
    v
}

/// Slot of `method` in the class of an adopted object.
unsafe fn slot_of(this: *mut c_void, method: &str) -> usize {
    let ext = vt(this) as usize;
    let g = CLASSES.lock().unwrap();
    let c = g.iter().find(|c| c.ext == ext).expect("adopted class");
    layout::slot(c.iface, method).expect("method in layout")
}

unsafe extern "system" fn qi_override(this: *mut c_void, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    let f: QiFn = std::mem::transmute(orig_fn(this, 0));
    let hr = f(this, riid, ppv);
    if hr == E_NOINTERFACE && !riid.is_null() {
        let s = layout::guid_string(&*riid);
        if UNKNOWN_IIDS.lock().unwrap().insert(s.clone()) {
            crate::xlog_warn!("D3D12.X: QueryInterface({s}) – interfejs nieznany na PC (konsolowy?)");
        }
    }
    hr
}

fn pc_iid(kind: Kind) -> GUID {
    layout::parse_guid(layout::find(kind.base()).map(|i| i.iid).unwrap_or(""))
}

macro_rules! as_usize {
    ($x:ident) => {
        usize
    };
}

/// Device creation methods ending in (REFIID riid, void** ppv).
macro_rules! creator {
    ($name:ident, $method:literal, $kind:expr, [$($a:ident),*]) => {
        unsafe extern "system" fn $name(this: *mut c_void, $($a: usize,)* riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
            type F = unsafe extern "system" fn(*mut c_void, $(as_usize!($a),)* *const GUID, *mut *mut c_void) -> HRESULT;
            let f: F = std::mem::transmute(orig_fn(this, slot_of(this, $method)));
            let mut hr = f(this, $($a,)* riid, ppv);
            if hr == E_NOINTERFACE {
                let iid = pc_iid($kind);
                hr = f(this, $($a,)* &iid, ppv);
            }
            if hr >= 0 && !ppv.is_null() && !(*ppv).is_null() {
                adopt(*ppv, $kind);
            }
            hr
        }
    };
}

creator!(dev_create_command_queue, "CreateCommandQueue", Kind::CommandQueue, [a1]);
creator!(dev_create_command_queue1, "CreateCommandQueue1", Kind::CommandQueue, [a1, a2]);
creator!(dev_create_command_allocator, "CreateCommandAllocator", Kind::CommandAllocator, [a1]);
creator!(dev_create_command_list, "CreateCommandList", Kind::CommandList, [a1, a2, a3, a4]);
creator!(dev_create_command_list1, "CreateCommandList1", Kind::CommandList, [a1, a2, a3]);
creator!(dev_create_descriptor_heap, "CreateDescriptorHeap", Kind::DescriptorHeap, [a1]);
creator!(dev_create_committed_resource, "CreateCommittedResource", Kind::Resource, [a1, a2, a3, a4, a5]);
creator!(dev_create_committed_resource1, "CreateCommittedResource1", Kind::Resource, [a1, a2, a3, a4, a5, a6]);
creator!(dev_create_committed_resource2, "CreateCommittedResource2", Kind::Resource, [a1, a2, a3, a4, a5, a6]);
creator!(dev_create_placed_resource, "CreatePlacedResource", Kind::Resource, [a1, a2, a3, a4, a5]);
creator!(dev_create_placed_resource1, "CreatePlacedResource1", Kind::Resource, [a1, a2, a3, a4, a5]);
creator!(dev_create_reserved_resource, "CreateReservedResource", Kind::Resource, [a1, a2, a3]);
creator!(dev_create_reserved_resource1, "CreateReservedResource1", Kind::Resource, [a1, a2, a3, a4]);
creator!(dev_create_heap, "CreateHeap", Kind::Heap, [a1]);
creator!(dev_create_heap1, "CreateHeap1", Kind::Heap, [a1, a2]);
creator!(dev_create_fence, "CreateFence", Kind::Fence, [a1, a2]);
creator!(dev_create_query_heap, "CreateQueryHeap", Kind::QueryHeap, [a1]);
creator!(dev_create_command_signature, "CreateCommandSignature", Kind::CommandSignature, [a1, a2]);
creator!(dev_create_pipeline_library, "CreatePipelineLibrary", Kind::PipelineLibrary, [a1, a2]);
creator!(dev_create_state_object, "CreateStateObject", Kind::StateObject, [a1]);

// ---------------------------------------------------------------------------------------
// pipeline states: capture + fallback
// ---------------------------------------------------------------------------------------

struct Dummy {
    vs: Vec<u8>,
    ps: Vec<u8>,
    cs: Vec<u8>,
}

fn compile(src: &str, target: &str) -> Vec<u8> {
    unsafe {
        let mut blob = None;
        let t = format!("{target}\0");
        let r = D3DCompile(
            src.as_ptr() as *const c_void,
            src.len(),
            PCSTR::null(),
            None,
            None,
            PCSTR(b"main\0".as_ptr()),
            PCSTR(t.as_ptr()),
            0,
            0,
            &mut blob,
            None,
        );
        match (r, blob) {
            (Ok(()), Some(b)) => std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec(),
            _ => Vec::new(),
        }
    }
}

/// Stand-in shaders used when the PC driver rejects a console shader.
fn dummy() -> &'static Dummy {
    static D: OnceLock<Dummy> = OnceLock::new();
    D.get_or_init(|| Dummy {
        vs: compile("float4 main(uint id : SV_VertexID) : SV_Position { return float4(0, 0, 0, 1); }", "vs_5_0"),
        ps: compile("float4 main() : SV_Target { return float4(1, 0, 1, 1); }", "ps_5_0"),
        cs: compile("[numthreads(1, 1, 1)] void main() {}", "cs_5_0"),
    })
}

fn bytecode(b: &[u8]) -> D3D12_SHADER_BYTECODE {
    D3D12_SHADER_BYTECODE { pShaderBytecode: b.as_ptr() as *const c_void, BytecodeLength: b.len() }
}

fn note_fallback(what: &str, hr: HRESULT, ok: bool) {
    let n = if ok { FALLBACK_PSO.fetch_add(1, Ordering::Relaxed) } else { FAILED_PSO.fetch_add(1, Ordering::Relaxed) };
    if n < 5 || n % 1000 == 0 {
        if ok {
            crate::xlog_warn!("D3D12.X: {what} odrzucony przez sterownik ({hr:#x}) – użyto zastępczych shaderów (#{})", n + 1);
        } else {
            crate::xlog_error!("D3D12.X: {what} odrzucony ({hr:#x}), także w wersji zastępczej (#{})", n + 1);
        }
    }
}

type PsoFn = unsafe extern "system" fn(*mut c_void, *const c_void, *const GUID, *mut *mut c_void) -> HRESULT;

unsafe fn finish_pso(this: *mut c_void, f: PsoFn, desc: *const c_void, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    let mut hr = f(this, desc, riid, ppv);
    if hr == E_NOINTERFACE {
        let iid = pc_iid(Kind::PipelineState);
        hr = f(this, desc, &iid, ppv);
    }
    if hr >= 0 && !ppv.is_null() && !(*ppv).is_null() {
        adopt(*ppv, Kind::PipelineState);
    }
    hr
}

fn note_translation_error(e: String) {
    let mut g = TRANSLATE_ERRORS.lock().unwrap();
    if g.len() < 5 {
        crate::xlog_warn!("D3D12.X: shadery konsoli w PSO bez tłumaczenia: {e}");
    }
    if g.len() < 20 && !g.contains(&e) {
        g.push(e);
    }
}

unsafe fn shader_bytes<'a>(b: &D3D12_SHADER_BYTECODE) -> Option<&'a [u8]> {
    let (p, n) = (b.pShaderBytecode as usize, b.BytecodeLength);
    (p != 0 && n > 0 && crate::gpu::shader::readable(p, n)).then(|| std::slice::from_raw_parts(p as *const u8, n))
}

/// XCL ABI root signature, created once on the backend device.
unsafe fn abi_rs_raw(this: *mut c_void) -> Option<*mut c_void> {
    let cur = ABI_RS.load(Ordering::Acquire);
    if cur != 0 {
        return Some(cur as *mut c_void);
    }
    let dev = ID3D12Device::from_raw_borrowed(&this)?;
    match crate::gpu::recomp::pso::abi_root_signature(dev, true) {
        Ok(rs) => {
            let raw = rs.into_raw();
            match ABI_RS.compare_exchange(0, raw as usize, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => Some(raw),
                Err(other) => {
                    drop(ID3D12RootSignature::from_raw(raw));
                    Some(other as *mut c_void)
                }
            }
        }
        Err(e) => {
            note_translation_error(format!("root signature ABI XCL: {e}"));
            None
        }
    }
}

/// A graphics pipeline whose VS (and PS) are console shaders: recompiled VS/PS + the XCL
/// shader ABI root signature. None: not console shaders, or not translatable (the caller
/// goes on with the title's description and the stand-in fallback).
/// A compute pipeline with a console CS: `None` = not a console shader, `Some(Err)` = console
/// shader the recompiler could not take, `Some(Ok(hr))` = translated pipeline created (or the
/// driver's verdict on it).
unsafe fn translated_compute_pso(this: *mut c_void, f: PsoFn, desc: *const c_void, riid: *const GUID, ppv: *mut *mut c_void) -> Option<Result<HRESULT, ()>> {
    use crate::gpu::recomp::pso::{console_to_dxil, is_console, Stage};
    if !crate::gpu::shader::readable(desc as usize, std::mem::size_of::<D3D12_COMPUTE_PIPELINE_STATE_DESC>()) {
        return None;
    }
    let d = &*(desc as *const D3D12_COMPUTE_PIPELINE_STATE_DESC);
    let cs = shader_bytes(&d.CS)?;
    if !is_console(cs) {
        return None;
    }
    let dxil = match console_to_dxil(cs, Stage::Compute) {
        Ok(b) => b,
        Err(e) => {
            note_translation_error(format!("CS: {e}"));
            return Some(Err(()));
        }
    };
    let Some(rs) = abi_rs_raw(this) else { return Some(Err(())) };
    let mut copy: D3D12_COMPUTE_PIPELINE_STATE_DESC = std::ptr::read(d);
    copy.pRootSignature = std::mem::transmute::<*mut c_void, std::mem::ManuallyDrop<Option<ID3D12RootSignature>>>(rs);
    copy.CS = bytecode(&dxil);
    copy.CachedPSO = D3D12_CACHED_PIPELINE_STATE::default();
    let hr = finish_pso(this, f, &copy as *const _ as *const c_void, riid, ppv);
    if hr < 0 {
        note_translation_error(format!("sterownik odrzucił obliczeniowy PSO z przetłumaczonym CS ({hr:#x})"));
        return Some(Err(()));
    }
    TRANSLATED_PSO.fetch_add(1, Ordering::Relaxed);
    crate::xlog_info!("D3D12.X: obliczeniowy PSO z przetłumaczonym CS konsoli ({} B DXIL)", dxil.len());
    Some(Ok(hr))
}

unsafe fn translated_graphics_pso(this: *mut c_void, f: PsoFn, desc: *const c_void, riid: *const GUID, ppv: *mut *mut c_void) -> Option<HRESULT> {
    use crate::gpu::recomp::pso::{console_to_dxil, is_console, Stage};
    if !crate::gpu::shader::readable(desc as usize, std::mem::size_of::<D3D12_GRAPHICS_PIPELINE_STATE_DESC>()) {
        return None;
    }
    let d = &*(desc as *const D3D12_GRAPHICS_PIPELINE_STATE_DESC);
    let vs = shader_bytes(&d.VS)?;
    if !is_console(vs) {
        return None;
    }
    if shader_bytes(&d.GS).is_some() || shader_bytes(&d.HS).is_some() || shader_bytes(&d.DS).is_some() {
        note_translation_error("PSO z GS/HS/DS – jeszcze nieobsługiwane".into());
        return None;
    }
    let vs_dxil = match console_to_dxil(vs, Stage::Vertex) {
        Ok(b) => b,
        Err(e) => {
            note_translation_error(format!("VS: {e}"));
            return None;
        }
    };
    let ps_dxil = match shader_bytes(&d.PS) {
        Some(p) if is_console(p) => match console_to_dxil(p, Stage::Pixel) {
            Ok(b) => Some(b),
            Err(e) => {
                note_translation_error(format!("PS: {e}"));
                return None;
            }
        },
        Some(p) => Some(p.to_vec()),
        None => None,
    };
    let rs = abi_rs_raw(this)?;
    let mut copy: D3D12_GRAPHICS_PIPELINE_STATE_DESC = std::ptr::read(d);
    copy.pRootSignature = std::mem::transmute::<*mut c_void, std::mem::ManuallyDrop<Option<ID3D12RootSignature>>>(rs);
    copy.VS = bytecode(&vs_dxil);
    copy.PS = ps_dxil.as_deref().map(bytecode).unwrap_or_default();
    copy.CachedPSO = D3D12_CACHED_PIPELINE_STATE::default();
    let hr = finish_pso(this, f, &copy as *const _ as *const c_void, riid, ppv);
    if hr < 0 {
        note_translation_error(format!("sterownik odrzucił PSO z przetłumaczonymi shaderami ({hr:#x})"));
        return None;
    }
    let n = TRANSLATED_PSO.fetch_add(1, Ordering::Relaxed);
    if n < 5 || n % 1000 == 0 {
        crate::xlog_info!("D3D12.X: graficzny PSO z przetłumaczonych shaderów konsoli (#{})", n + 1);
    }
    Some(hr)
}

unsafe extern "system" fn dev_create_graphics_pso(this: *mut c_void, desc: *const c_void, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    super::null::capture_graphics_desc(desc as *const u8);
    let f: PsoFn = std::mem::transmute(orig_fn(this, slot_of(this, "CreateGraphicsPipelineState")));
    if let Some(hr) = translated_graphics_pso(this, f, desc, riid, ppv) {
        return hr;
    }
    let hr = finish_pso(this, f, desc, riid, ppv);
    if hr >= 0 || !crate::gpu::shader::readable(desc as usize, std::mem::size_of::<D3D12_GRAPHICS_PIPELINE_STATE_DESC>()) {
        return hr;
    }
    let d = dummy();
    let mut copy: D3D12_GRAPHICS_PIPELINE_STATE_DESC = std::ptr::read(desc as *const D3D12_GRAPHICS_PIPELINE_STATE_DESC);
    copy.VS = bytecode(&d.vs);
    copy.PS = bytecode(&d.ps);
    copy.DS = D3D12_SHADER_BYTECODE::default();
    copy.HS = D3D12_SHADER_BYTECODE::default();
    copy.GS = D3D12_SHADER_BYTECODE::default();
    copy.StreamOutput = std::mem::zeroed();
    copy.InputLayout = D3D12_INPUT_LAYOUT_DESC::default();
    copy.CachedPSO = D3D12_CACHED_PIPELINE_STATE::default();
    let hr2 = finish_pso(this, f, &copy as *const _ as *const c_void, riid, ppv);
    note_fallback("graficzny PSO", hr, hr2 >= 0);
    if hr2 >= 0 {
        hr2
    } else {
        hr
    }
}

unsafe extern "system" fn dev_create_compute_pso(this: *mut c_void, desc: *const c_void, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    super::null::capture_compute_desc(desc as *const u8);
    let f: PsoFn = std::mem::transmute(orig_fn(this, slot_of(this, "CreateComputePipelineState")));
    let console = translated_compute_pso(this, f, desc, riid, ppv);
    LAST_COMPUTE_TRANSLATED.with(|c| c.set(matches!(console, Some(Ok(hr)) if hr >= 0)));
    if let Some(Ok(hr)) = console {
        return hr;
    }
    // a console shader the recompiler could not take must not reach the driver: AMD's shader
    // compiler crashes on the stripped DXIL (NOBC bitcode) instead of rejecting it
    let hr = if console.is_some() { E_INVALIDARG } else { finish_pso(this, f, desc, riid, ppv) };
    if hr >= 0 || !crate::gpu::shader::readable(desc as usize, std::mem::size_of::<D3D12_COMPUTE_PIPELINE_STATE_DESC>()) {
        return hr;
    }
    let mut copy: D3D12_COMPUTE_PIPELINE_STATE_DESC = std::ptr::read(desc as *const D3D12_COMPUTE_PIPELINE_STATE_DESC);
    copy.CS = bytecode(&dummy().cs);
    copy.CachedPSO = D3D12_CACHED_PIPELINE_STATE::default();
    let hr2 = finish_pso(this, f, &copy as *const _ as *const c_void, riid, ppv);
    note_fallback("obliczeniowy PSO", hr, hr2 >= 0);
    if hr2 >= 0 {
        hr2
    } else {
        hr
    }
}

/// Rewrite a pipeline state stream with stand-in shaders. None if not possible.
unsafe fn fallback_stream(desc: *const u8) -> Option<(Vec<u8>, [usize; 2])> {
    let size = *(desc as *const usize);
    let base = *(desc.add(8) as *const usize);
    if !crate::gpu::shader::readable(base, size) {
        return None;
    }
    let mut buf = std::slice::from_raw_parts(base as *const u8, size).to_vec();
    let d = dummy();
    let mut off = 0usize;
    while off + 8 <= size {
        let ty = u32::from_le_bytes(buf[off..off + 4].try_into().ok()?);
        let (psize, palign) = super::null::subobject_payload(ty)?;
        let poff = if palign == 8 { 8 } else { 4 };
        let at = off + poff;
        let put = |buf: &mut Vec<u8>, b: &[u8]| {
            buf[at..at + 8].copy_from_slice(&(b.as_ptr() as u64).to_le_bytes());
            buf[at + 8..at + 16].copy_from_slice(&(b.len() as u64).to_le_bytes());
        };
        match ty {
            1 => put(&mut buf, &d.vs),
            2 => put(&mut buf, &d.ps),
            6 => put(&mut buf, &d.cs),
            3 | 4 | 5 => put(&mut buf, &[]),
            24 | 25 => return None, // mesh pipelines need SM6.5 stand-ins
            12 | 19 => buf[at..at + 16].fill(0), // input layout, cached PSO
            7 => buf[at..at + 32].fill(0),        // stream output
            _ => {}
        }
        off += (poff + psize + 7) & !7;
    }
    Some((buf, [size, 0]))
}

unsafe extern "system" fn dev_create_pipeline_state(this: *mut c_void, desc: *const c_void, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    super::null::capture_stream_desc(desc as *const u8);
    let f: PsoFn = std::mem::transmute(orig_fn(this, slot_of(this, "CreatePipelineState")));
    let hr = finish_pso(this, f, desc, riid, ppv);
    if hr >= 0 || !crate::gpu::shader::readable(desc as usize, 16) {
        return hr;
    }
    let Some((buf, _)) = fallback_stream(desc as *const u8) else {
        note_fallback("strumień PSO", hr, false);
        return hr;
    };
    let new_desc: [usize; 2] = [buf.len(), buf.as_ptr() as usize];
    let hr2 = finish_pso(this, f, new_desc.as_ptr() as *const c_void, riid, ppv);
    note_fallback("strumień PSO", hr, hr2 >= 0);
    if hr2 >= 0 {
        hr2
    } else {
        hr
    }
}

unsafe extern "system" fn dev_create_root_signature(this: *mut c_void, mask: u32, blob: *const c_void, len: usize, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    crate::gpu::shader::capture("rootsig", blob as usize, len);
    type F = unsafe extern "system" fn(*mut c_void, u32, *const c_void, usize, *const GUID, *mut *mut c_void) -> HRESULT;
    let f: F = std::mem::transmute(orig_fn(this, slot_of(this, "CreateRootSignature")));
    let mut hr = f(this, mask, blob, len, riid, ppv);
    if hr == E_NOINTERFACE {
        let iid = pc_iid(Kind::RootSignature);
        hr = f(this, mask, blob, len, &iid, ppv);
    }
    if hr < 0 {
        // console root signature rejected → empty one so the title keeps going
        let desc = D3D12_ROOT_SIGNATURE_DESC { Flags: D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT, ..Default::default() };
        let mut ser = None;
        if D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut ser, None).is_ok() {
            if let Some(b) = ser {
                let iid = pc_iid(Kind::RootSignature);
                let hr2 = f(this, mask, b.GetBufferPointer(), b.GetBufferSize(), &iid, ppv);
                if hr2 >= 0 {
                    let n = FALLBACK_RS.fetch_add(1, Ordering::Relaxed);
                    if n < 5 {
                        crate::xlog_warn!("D3D12.X: root signature odrzucona ({hr:#x}) – zastępcza pusta (#{})", n + 1);
                    }
                    hr = hr2;
                }
            }
        }
    }
    if hr >= 0 && !ppv.is_null() && !(*ppv).is_null() {
        adopt(*ppv, Kind::RootSignature);
        if !blob.is_null() {
            super::console_list::note_root_signature(*ppv as usize, std::slice::from_raw_parts(blob as *const u8, len));
        }
    }
    hr
}

// ---------------------------------------------------------------------------------------
// presentation tracking
// ---------------------------------------------------------------------------------------

unsafe extern "system" fn dev_create_rtv(this: *mut c_void, res: *mut c_void, desc: *const c_void, handle: usize) {
    type F = unsafe extern "system" fn(*mut c_void, *mut c_void, *const c_void, usize);
    let f: F = std::mem::transmute(orig_fn(this, slot_of(this, "CreateRenderTargetView")));
    f(this, res, desc, handle);
    super::present::note_rtv(handle, res);
}

unsafe extern "system" fn cl_om_set_render_targets(this: *mut c_void, n: u32, rtvs: *const usize, single: i32, dsv: *const usize) {
    type F = unsafe extern "system" fn(*mut c_void, u32, *const usize, i32, *const usize);
    let f: F = std::mem::transmute(orig_fn(this, slot_of(this, "OMSetRenderTargets")));
    f(this, n, rtvs, single, dsv);
    if n > 0 && !rtvs.is_null() {
        super::present::note_rt0(this as usize, *rtvs);
    }
}

unsafe extern "system" fn alloc_reset(this: *mut c_void) -> HRESULT {
    type F = unsafe extern "system" fn(*mut c_void) -> HRESULT;
    let f: F = std::mem::transmute(orig_fn(this, slot_of(this, "Reset")));
    let hr = f(this);
    if hr >= 0 {
        // the GPU is done with what lists recorded from this allocator used
        super::console_list::allocator_reset(this);
    }
    hr
}

unsafe extern "system" fn q_execute_command_lists(this: *mut c_void, n: u32, lists: *const *mut c_void) {
    type F = unsafe extern "system" fn(*mut c_void, u32, *const *mut c_void);
    let f: F = std::mem::transmute(orig_fn(this, slot_of(this, "ExecuteCommandLists")));
    // console-shaped lists run as the PC lists they were translated into
    let mapped: Vec<*mut c_void>;
    let lists = if n > 0 && !lists.is_null() {
        mapped = std::slice::from_raw_parts(lists, n as usize).iter().map(|&l| super::console_list::pc_list(l).unwrap_or(l)).collect();
        mapped.as_ptr()
    } else {
        lists
    };
    f(this, n, lists);
    if n > 0 && !lists.is_null() {
        super::present::note_execute(this, std::slice::from_raw_parts(lists, n as usize));
    }
}
