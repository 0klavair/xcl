//! The console command list's own methods.
//!
//! The console's ID3D12GraphicsCommandList keeps the PC methods (some moved:
//! `real::console_remap`) and holds console extensions from #60 on (as observed on Fortnite):
//! CopyMemoryToMemoryX #60, FillMemoryWith{32,64,128}BitValueX #61–#63, depth bounds #68,
//! Write{32,64}BitValueBottomOfPipeX #69 / #70, DispatchX #78, DrawIndexedX #81,
//! CopyResourceX #84, SetPredicationBufferX #85, Write{32,64}BitValueTopOfPipeX #88 / #89,
//! Write32BitValueBottomOfShaderX #90. A title call of any other extension (waits on memory,
//! GDS, KickoffX, depth bias, hints…) is logged and does nothing (`real::class_for`).
//!
//! Occlusion queries may nest on the console; the PC allows one active per list. When one
//! begins while another is active, the outer one ends there on the PC and resolves as visible;
//! the inner one is a real query (Fortnite nests its per-object queries in a dummy outer one).
//!
//! Console GPU addresses in unified memory are the title's own addresses; memory operations
//! go to that window's buffer (`real::uma_window_buffer`), which is in COMMON between them.
//! An index buffer at such an address is given to the PC as the window buffer's GPU address.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{transmute_copy, ManuallyDrop};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::real;

const PC_GET_TYPE: usize = 8;
const PC_RESET: usize = 10;
const PC_DRAW_INDEXED: usize = 13;
const PC_COPY_BUFFER_REGION: usize = 15;
const PC_COPY_TEXTURE_REGION: usize = 16;
const PC_COPY_RESOURCE: usize = 17;
const PC_RESOURCE_BARRIER: usize = 26;
const PC_IA_SET_INDEX_BUFFER: usize = 43;
const PC_BEGIN_QUERY: usize = 52;
const PC_END_QUERY: usize = 53;
const PC_RESOLVE_QUERY_DATA: usize = 54;
const PC_SET_PREDICATION: usize = 55;
const PC_OM_SET_DEPTH_BOUNDS: usize = 62;
const PC_WRITE_BUFFER_IMMEDIATE: usize = 66;

/// Write64BitValueBottomOfPipeX flags 1 / 2, Write64BitValueTopOfPipeX flags 4 / 8: the value
/// written is a GPU / system timestamp
const BOTTOM_TIMESTAMPS: u32 = 0x3;
const TOP_TIMESTAMPS: u32 = 0xC;
/// fills larger than this are not done (one pattern buffer per value, up to this size)
const MAX_FILL: u64 = 256 << 20;

fn once(counter: &AtomicU64, n: u64) -> bool {
    counter.fetch_add(1, Ordering::Relaxed) < n
}

/// a PC method of the list (its runtime's own)
unsafe fn pc<F: Copy>(list: *mut c_void, slot: usize) -> F {
    transmute_copy(&real::orig_fn(list, slot))
}

unsafe fn list_type(list: *mut c_void) -> D3D12_COMMAND_LIST_TYPE {
    pc::<unsafe extern "system" fn(*mut c_void) -> D3D12_COMMAND_LIST_TYPE>(list, PC_GET_TYPE)(list)
}

unsafe fn transition(list: *mut c_void, res: usize, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) {
    let b = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: transmute_copy(&res),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    };
    pc::<unsafe extern "system" fn(*mut c_void, u32, *const D3D12_RESOURCE_BARRIER)>(list, PC_RESOURCE_BARRIER)(list, 1, &b);
}

/// `op` with the window buffer `res` as a copy destination (back to COMMON after it)
unsafe fn as_copy_dest(list: *mut c_void, res: usize, op: impl FnOnce()) {
    transition(list, res, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST);
    op();
    transition(list, res, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON);
}

unsafe fn copy_buffer_region(list: *mut c_void, dst: usize, dst_off: u64, src: usize, src_off: u64, n: u64) {
    pc::<unsafe extern "system" fn(*mut c_void, usize, u64, usize, u64, u64)>(list, PC_COPY_BUFFER_REGION)(list, dst, dst_off, src, src_off, n);
}

fn warn(counter: &AtomicU64, what: &str, why: std::fmt::Arguments) {
    if once(counter, 4) {
        crate::xlog_warn!("D3D12.X: {what}: {why} – not done");
    }
}

/// command list #60 CopyMemoryToMemoryX(destination, source, size, flags)
pub(super) unsafe extern "system" fn copy_memory_x(this: *mut c_void, dst: u64, src: u64, size: u64, _flags: u32) {
    static N: AtomicU64 = AtomicU64::new(0);
    const W: &str = "CopyMemoryToMemoryX";
    if size == 0 || !size.is_multiple_of(4) {
        return warn(&N, W, format_args!("size {size:#x} is not a non-zero multiple of 4"));
    }
    let (Some((d, doff)), Some((s, soff))) = (real::uma_window_buffer(dst as usize, size as usize), real::uma_window_buffer(src as usize, size as usize)) else {
        return warn(&N, W, format_args!("{src:#x} → {dst:#x} ({size:#x} bytes) is not in unified memory"));
    };
    if d == s && dst < src + size && src < dst + size {
        return warn(&N, W, format_args!("{src:#x} → {dst:#x} ({size:#x} bytes) overlap"));
    }
    if d == s {
        // one window: the buffer is the source and the destination (the ranges do not overlap)
        as_copy_dest(this, d, || copy_buffer_region(this, d, doff, s, soff, size));
        return;
    }
    transition(this, s, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE);
    as_copy_dest(this, d, || copy_buffer_region(this, d, doff, s, soff, size));
    transition(this, s, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON);
}

unsafe fn device_of(list: *mut c_void) -> Option<ID3D12Device> {
    let mut dev: Option<ID3D12Device> = None;
    ID3D12GraphicsCommandList::from_raw_borrowed(&list)?.GetDevice(&mut dev).ok()?;
    dev
}

/// upload buffers holding a repeated 16-byte pattern: (pattern, buffer, bytes)
static PATTERNS: Mutex<Vec<([u8; 16], ID3D12Resource, u64)>> = Mutex::new(Vec::new());

/// A buffer of at least `n` bytes repeating `pat` (the device's upload heap).
unsafe fn pattern_buffer(list: *mut c_void, pat: [u8; 16], n: u64) -> Option<usize> {
    let mut cache = PATTERNS.lock().unwrap();
    if let Some((_, b, _)) = cache.iter().find(|(p, _, len)| *p == pat && *len >= n) {
        return Some(b.as_raw() as usize);
    }
    // a larger one replaces an older pattern buffer only when nothing may still read it: kept
    if cache.len() >= 32 {
        return None;
    }
    let dev = device_of(list)?;
    let len = n.next_power_of_two().max(0x1_0000);
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: len,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut b: Option<ID3D12Resource> = None;
    dev.CreateCommittedResource(
        &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_UPLOAD, ..Default::default() },
        D3D12_HEAP_FLAG_NONE,
        &desc,
        D3D12_RESOURCE_STATE_GENERIC_READ,
        None,
        &mut b,
    )
    .ok()?;
    let b = b?;
    let mut p: *mut c_void = std::ptr::null_mut();
    b.Map(0, None, Some(&mut p)).ok()?;
    for chunk in std::slice::from_raw_parts_mut(p as *mut u8, len as usize).chunks_exact_mut(16) {
        chunk.copy_from_slice(&pat);
    }
    b.Unmap(0, None);
    let raw = b.as_raw() as usize;
    cache.push((pat, b, len));
    Some(raw)
}

unsafe fn fill(list: *mut c_void, va: u64, size: u64, pat: [u8; 16], unit: u64, what: &str) {
    static N: AtomicU64 = AtomicU64::new(0);
    if size == 0 || !size.is_multiple_of(unit) || size > MAX_FILL {
        return warn(&N, what, format_args!("size {size:#x} (a non-zero multiple of {unit} up to {MAX_FILL:#x} is filled)"));
    }
    let Some((d, off)) = real::uma_window_buffer(va as usize, size as usize) else {
        return warn(&N, what, format_args!("{va:#x} ({size:#x} bytes) is not in unified memory"));
    };
    let Some(src) = pattern_buffer(list, pat, size) else {
        return warn(&N, what, format_args!("no pattern buffer for {pat:x?}"));
    };
    as_copy_dest(list, d, || copy_buffer_region(list, d, off, src, 0, size));
}

fn pattern(bytes: &[u8]) -> [u8; 16] {
    let mut p = [0u8; 16];
    for (i, b) in p.iter_mut().enumerate() {
        *b = bytes[i % bytes.len()];
    }
    p
}

/// command list #61 FillMemoryWith32BitValueX(address, size, value)
pub(super) unsafe extern "system" fn fill32_x(this: *mut c_void, va: u64, size: u64, value: u32) {
    fill(this, va, size, pattern(&value.to_le_bytes()), 4, "FillMemoryWith32BitValueX");
}

/// command list #62 FillMemoryWith64BitValueX(address, size, value)
pub(super) unsafe extern "system" fn fill64_x(this: *mut c_void, va: u64, size: u64, value: u64) {
    fill(this, va, size, pattern(&value.to_le_bytes()), 8, "FillMemoryWith64BitValueX");
}

/// command list #63 FillMemoryWith128BitValueX(address, size, value*)
pub(super) unsafe extern "system" fn fill128_x(this: *mut c_void, va: u64, size: u64, value: *const [u8; 16]) {
    if !value.is_null() {
        fill(this, va, size, value.read_unaligned(), 16, "FillMemoryWith128BitValueX");
    }
}

/// Values written by the GPU at `va` (WriteBufferImmediate: MARKER_IN = when the command is
/// reached, MARKER_OUT = when the work before it is done).
unsafe fn write(list: *mut c_void, va: u64, words: &[u32], mode: D3D12_WRITEBUFFERIMMEDIATE_MODE, what: &str) {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = 4 * words.len() as u64;
    if !va.is_multiple_of(n) {
        return warn(&N, what, format_args!("{va:#x} is not {n}-byte aligned"));
    }
    let (Some((d, _)), Some(at)) = (real::uma_window_buffer(va as usize, n as usize), real::uma_va_of(va as usize)) else {
        return warn(&N, what, format_args!("{va:#x} is not in unified memory"));
    };
    let params: Vec<D3D12_WRITEBUFFERIMMEDIATE_PARAMETER> = words.iter().enumerate().map(|(i, &w)| D3D12_WRITEBUFFERIMMEDIATE_PARAMETER { Dest: at + 4 * i as u64, Value: w }).collect();
    let modes = vec![mode; words.len()];
    type F = unsafe extern "system" fn(*mut c_void, u32, *const D3D12_WRITEBUFFERIMMEDIATE_PARAMETER, *const D3D12_WRITEBUFFERIMMEDIATE_MODE);
    as_copy_dest(list, d, || pc::<F>(list, PC_WRITE_BUFFER_IMMEDIATE)(list, params.len() as u32, params.as_ptr(), modes.as_ptr()));
}

/// timestamp queries (one heap per device, used round-robin)
static TIMESTAMPS: Mutex<Option<HashMap<usize, ID3D12QueryHeap>>> = Mutex::new(None);
const TIMESTAMP_SLOTS: u32 = 4096;
static NEXT_TIMESTAMP: AtomicU32 = AtomicU32::new(0);

/// The GPU timestamp (the PC queue's ticks) written at `va` when the work before it is done.
unsafe fn timestamp(list: *mut c_void, va: u64, what: &str) {
    static N: AtomicU64 = AtomicU64::new(0);
    let ty = list_type(list);
    if ty != D3D12_COMMAND_LIST_TYPE_DIRECT && ty != D3D12_COMMAND_LIST_TYPE_COMPUTE {
        return warn(&N, what, format_args!("a timestamp on a type-{} list", ty.0));
    }
    let Some((d, off)) = real::uma_window_buffer(va as usize, 8).filter(|_| va.is_multiple_of(8)) else {
        return warn(&N, what, format_args!("{va:#x} is not 8-byte aligned in unified memory"));
    };
    let Some(dev) = device_of(list) else { return };
    let heap = {
        let mut m = TIMESTAMPS.lock().unwrap();
        let m = m.get_or_insert_with(HashMap::new);
        if let std::collections::hash_map::Entry::Vacant(slot) = m.entry(dev.as_raw() as usize) {
            let mut h: Option<ID3D12QueryHeap> = None;
            let desc = D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: TIMESTAMP_SLOTS, NodeMask: 0 };
            match dev.CreateQueryHeap(&desc, &mut h) {
                Ok(()) if h.is_some() => slot.insert(h.unwrap()),
                _ => return warn(&N, what, format_args!("no timestamp query heap")),
            };
        }
        m[&(dev.as_raw() as usize)].as_raw() as usize
    };
    let i = NEXT_TIMESTAMP.fetch_add(1, Ordering::Relaxed) % TIMESTAMP_SLOTS;
    pc::<unsafe extern "system" fn(*mut c_void, usize, D3D12_QUERY_TYPE, u32)>(list, PC_END_QUERY)(list, heap, D3D12_QUERY_TYPE_TIMESTAMP, i);
    type Resolve = unsafe extern "system" fn(*mut c_void, usize, D3D12_QUERY_TYPE, u32, u32, usize, u64);
    as_copy_dest(list, d, || pc::<Resolve>(list, PC_RESOLVE_QUERY_DATA)(list, heap, D3D12_QUERY_TYPE_TIMESTAMP, i, 1, d, off));
}

/// command list #69 Write32BitValueBottomOfPipeX(address, value, flush, flags)
pub(super) unsafe extern "system" fn write32_bottom_x(this: *mut c_void, va: u64, value: u32, _flush: u32, _flags: u32) {
    write(this, va, &[value], D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_OUT, "Write32BitValueBottomOfPipeX");
}

/// command list #70 Write64BitValueBottomOfPipeX(address, value, flush, flags)
pub(super) unsafe extern "system" fn write64_bottom_x(this: *mut c_void, va: u64, value: u64, _flush: u32, flags: u32) {
    const W: &str = "Write64BitValueBottomOfPipeX";
    if flags & BOTTOM_TIMESTAMPS != 0 {
        return timestamp(this, va, W);
    }
    write(this, va, &[value as u32, (value >> 32) as u32], D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_OUT, W);
}

/// command list #88 Write32BitValueTopOfPipeX(address, value, flags)
pub(super) unsafe extern "system" fn write32_top_x(this: *mut c_void, va: u64, value: u32, _flags: u32) {
    write(this, va, &[value], D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_IN, "Write32BitValueTopOfPipeX");
}

/// command list #89 Write64BitValueTopOfPipeX(address, value, flags)
pub(super) unsafe extern "system" fn write64_top_x(this: *mut c_void, va: u64, value: u64, flags: u32) {
    const W: &str = "Write64BitValueTopOfPipeX";
    if flags & TOP_TIMESTAMPS != 0 {
        return timestamp(this, va, W);
    }
    write(this, va, &[value as u32, (value >> 32) as u32], D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_IN, W);
}

/// command list #90 Write32BitValueBottomOfShaderX(address, value, stage)
pub(super) unsafe extern "system" fn write32_bottom_of_shader_x(this: *mut c_void, va: u64, value: u32, _stage: u32) {
    write(this, va, &[value], D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_OUT, "Write32BitValueBottomOfShaderX");
}

/// command list #68 OMSetDepthBounds(min, max) (PC #62)
pub(super) unsafe extern "system" fn set_depth_bounds_x(this: *mut c_void, min: f32, max: f32) {
    pc::<unsafe extern "system" fn(*mut c_void, f32, f32)>(this, PC_OM_SET_DEPTH_BOUNDS)(this, min, max);
}

/// command list #78 DispatchX(x, y, z, flags)
pub(super) unsafe extern "system" fn dispatch_x(this: *mut c_void, x: u32, y: u32, z: u32, _flags: u32) {
    super::bind::dispatch(this, x, y, z);
}

/// command list #84 CopyResourceX(destination, source, flags); in place (the console's
/// compression operations) there is nothing to do on PC
pub(super) unsafe extern "system" fn copy_resource_flags_x(this: *mut c_void, dst: usize, src: usize, _flags: u32) {
    if dst != src {
        copy_resource_x(this, dst, src);
    }
}

/// command list #85 SetPredicationBufferX({ u64 address; u32 operation }*)
pub(super) unsafe extern "system" fn set_predication_buffer_x(this: *mut c_void, desc: *const u64) {
    static N: AtomicU64 = AtomicU64::new(0);
    type F = unsafe extern "system" fn(*mut c_void, usize, u64, D3D12_PREDICATION_OP);
    let f = pc::<F>(this, PC_SET_PREDICATION);
    if desc.is_null() {
        return f(this, 0, 0, D3D12_PREDICATION_OP_EQUAL_ZERO);
    }
    let (va, op) = (desc.read_unaligned(), (desc.add(1) as *const u32).read_unaligned());
    let Some((b, off)) = real::uma_window_buffer(va as usize, 8).filter(|_| va.is_multiple_of(8)) else {
        return warn(&N, "SetPredicationBufferX", format_args!("{va:#x} is not 8-byte aligned in unified memory"));
    };
    transition(this, b, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_PREDICATION);
    f(this, b, off, D3D12_PREDICATION_OP(op as i32));
    transition(this, b, D3D12_RESOURCE_STATE_PREDICATION, D3D12_RESOURCE_STATE_COMMON);
}

/// the index buffer each list has bound (PC addresses), for DrawIndexedX to restore
static INDEX_BUFFERS: Mutex<Option<HashMap<usize, D3D12_INDEX_BUFFER_VIEW>>> = Mutex::new(None);

/// a console GPU address as the PC's (unified memory: the window buffer's)
fn pc_va(va: u64) -> u64 {
    real::uma_va_of(va as usize).unwrap_or(va)
}

unsafe fn set_index_buffer(list: *mut c_void, view: Option<&D3D12_INDEX_BUFFER_VIEW>) {
    let f = pc::<unsafe extern "system" fn(*mut c_void, *const D3D12_INDEX_BUFFER_VIEW)>(list, PC_IA_SET_INDEX_BUFFER);
    f(list, view.map_or(std::ptr::null(), |v| v as *const _));
}

/// command list #47 IASetIndexBuffer(view) (PC #43): the address is the title's
pub(super) unsafe extern "system" fn ia_set_index_buffer_x(this: *mut c_void, view: *const D3D12_INDEX_BUFFER_VIEW) {
    let mut m = INDEX_BUFFERS.lock().unwrap();
    let m = m.get_or_insert_with(HashMap::new);
    if view.is_null() {
        m.remove(&(this as usize));
        return set_index_buffer(this, None);
    }
    let mut v = view.read_unaligned();
    v.BufferLocation = pc_va(v.BufferLocation);
    m.insert(this as usize, v);
    set_index_buffer(this, Some(&v));
}

/// command list #81 DrawIndexedX(index address, index format, index count, flags): one
/// instance of `count` indices read from that address; the bound index buffer stays
pub(super) unsafe extern "system" fn draw_indexed_x(this: *mut c_void, va: u64, format: DXGI_FORMAT, count: u32, _flags: u32) {
    static N: AtomicU64 = AtomicU64::new(0);
    let stride = match format {
        DXGI_FORMAT_R16_UINT => 2,
        DXGI_FORMAT_R32_UINT => 4,
        f => return warn(&N, "DrawIndexedX", format_args!("index format {}", f.0)),
    };
    if count == 0 {
        return;
    }
    let view = D3D12_INDEX_BUFFER_VIEW { BufferLocation: pc_va(va), SizeInBytes: count.saturating_mul(stride), Format: format };
    set_index_buffer(this, Some(&view));
    pc::<unsafe extern "system" fn(*mut c_void, u32, u32, u32, i32, u32)>(this, PC_DRAW_INDEXED)(this, count, 1, 0, 0, 0);
    let bound = INDEX_BUFFERS.lock().unwrap().as_ref().and_then(|m| m.get(&(this as usize)).copied());
    set_index_buffer(this, bound.as_ref());
}

/// a query: (heap, index)
type Query = (usize, u32);

/// One list's occlusion queries: the one active on the PC and the outer ones it ended early.
#[derive(Default)]
struct Occlusion {
    active: Option<Query>,
    ended_early: Vec<Query>,
}

impl Occlusion {
    /// Begin `q` on the PC; Some(outer): end that one on the PC first.
    fn begin(&mut self, q: Query) -> Option<Query> {
        let outer = self.active.replace(q);
        if let Some(o) = outer {
            self.ended_early.push(o);
        }
        outer
    }

    /// Whether the PC ends `q` (not: an outer one already ended – it resolves as visible).
    fn end(&mut self, q: Query) -> bool {
        if self.active == Some(q) {
            self.active = None;
            return true;
        }
        match self.ended_early.iter().position(|e| *e == q) {
            Some(i) => {
                self.ended_early.swap_remove(i);
                false
            }
            None => true,
        }
    }
}

static OCCLUSION: Mutex<Option<HashMap<usize, Occlusion>>> = Mutex::new(None);
/// outer queries ended early, and the result they resolve to
static VISIBLE: Mutex<Option<HashMap<Query, u64>>> = Mutex::new(None);
/// what such an OCCLUSION query reports (BINARY_OCCLUSION: 1)
const VISIBLE_SAMPLES: u64 = 0x1_0000;

fn is_occlusion(ty: D3D12_QUERY_TYPE) -> bool {
    ty == D3D12_QUERY_TYPE_OCCLUSION || ty == D3D12_QUERY_TYPE_BINARY_OCCLUSION
}

type QueryFn = unsafe extern "system" fn(*mut c_void, usize, D3D12_QUERY_TYPE, u32);

/// command list #10 Reset(allocator, initial pipeline): no query is active after it
pub(super) unsafe extern "system" fn reset_x(this: *mut c_void, alloc: usize, pso: usize) -> i32 {
    if let Some(m) = OCCLUSION.lock().unwrap().as_mut() {
        m.remove(&(this as usize));
    }
    super::bind::reset(this, alloc);
    // the PC runtime writes its own vtable into a copy list again on Reset: keep XCL's
    let ext = *(this as *const usize);
    let hr = pc::<unsafe extern "system" fn(*mut c_void, usize, usize) -> i32>(this, PC_RESET)(this, alloc, pso);
    if *(this as *const usize) != ext {
        *(this as *mut usize) = ext;
    }
    hr
}

/// the D3D12_RESOURCE_STATES bits the PC knows
const PC_STATES: u32 = 0x1 | 0x2 | 0x4 | 0x8 | 0x10 | 0x20 | 0x40 | 0x80 | 0x100 | 0x200 | 0x400 | 0x800 | 0x1000 | 0x2000 | 0xF_0000 | 0x20_0000 | 0x40_0000 | 0x80_0000 | 0x100_0000;

/// #26 ResourceBarrier(count, barriers): console barriers (D3D12_RESOURCE_BARRIER, 32 bytes) as
/// PC ones – state bits the PC does not know are dropped (logged: the console's own), a
/// transition left with nothing to do goes, aliasing barriers cover any resources
pub(super) unsafe extern "system" fn resource_barrier_x(this: *mut c_void, n: u32, barriers: *const [u32; 8]) {
    static N: AtomicU64 = AtomicU64::new(0);
    if barriers.is_null() || n == 0 {
        return;
    }
    let mut out: Vec<[u32; 8]> = Vec::with_capacity(n as usize);
    for k in 0..n as usize {
        let mut b = barriers.add(k).read_unaligned();
        let (ty, flags) = (b[0], b[1]);
        if ty > 2 || flags > 3 {
            if N.fetch_add(1, Ordering::Relaxed) < 16 {
                crate::xlog_warn!("D3D12.X: ResourceBarrier: console barrier type {ty} flags {flags:#x} dropped ({:x?})", b);
            }
            continue;
        }
        if ty == 1 {
            // aliasing (resource before +8, after +16): on the PC only between placed / reserved
            // resources, and the console's texture memory is mostly XCL's committed resources
            // (the list would fail to Close) – an aliasing barrier for any resources instead
            b[2..6].fill(0);
        }
        if ty == 0 {
            // transition: resource (+8), subresource (+16), before (+20), after (+24)
            let (before, after) = (b[5], b[6]);
            if (before | after) & !PC_STATES != 0 && N.fetch_add(1, Ordering::Relaxed) < 16 {
                crate::xlog_warn!("D3D12.X: ResourceBarrier: console state bits {:#x} → {:#x} dropped", before & !PC_STATES, after & !PC_STATES);
            }
            b[5] &= PC_STATES;
            b[6] &= PC_STATES;
            if b[5] == b[6] {
                continue;
            }
        }
        out.push(b);
    }
    if !out.is_empty() {
        pc::<unsafe extern "system" fn(*mut c_void, u32, *const [u32; 8])>(this, PC_RESOURCE_BARRIER)(this, out.len() as u32, out.as_ptr());
    }
}

/// DMA list #9 Close: as on the PC; a failure is logged (the title stops on it)
pub(super) unsafe extern "system" fn dma_close_x(this: *mut c_void) -> i32 {
    let hr = pc::<unsafe extern "system" fn(*mut c_void) -> i32>(this, 9)(this);
    if hr < 0 {
        let ty = pc::<unsafe extern "system" fn(*mut c_void) -> i32>(this, PC_GET_TYPE)(this);
        crate::xlog_warn!("D3D12.X: DMA list Close → {:#x} (PC list type {ty})", hr as u32);
    }
    hr
}

/// a copy location with the offset of a buffer placed below its console address added
unsafe fn pc_location(l: *const D3D12_TEXTURE_COPY_LOCATION) -> Option<D3D12_TEXTURE_COPY_LOCATION> {
    let mut l = std::ptr::read(l.as_ref()?);
    if l.Type == D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT {
        l.Anonymous.PlacedFootprint.Offset += real::buffer_delta(transmute_copy::<_, usize>(&l.pResource));
    }
    Some(l)
}

/// CopyBufferRegion(dst, dst offset, src, src offset, bytes) (PC #15; DMA list #11)
pub(super) unsafe extern "system" fn copy_buffer_region_x(this: *mut c_void, dst: usize, dst_off: u64, src: usize, src_off: u64, n: u64) {
    pc::<unsafe extern "system" fn(*mut c_void, usize, u64, usize, u64, u64)>(this, PC_COPY_BUFFER_REGION)(this, dst, dst_off + real::buffer_delta(dst), src, src_off + real::buffer_delta(src), n)
}

/// CopyTextureRegion(dst, x, y, z, src, box) (PC #16; DMA list #12)
pub(super) unsafe extern "system" fn copy_texture_region_x(this: *mut c_void, dst: *const D3D12_TEXTURE_COPY_LOCATION, x: u32, y: u32, z: u32, src: *const D3D12_TEXTURE_COPY_LOCATION, bx: *const D3D12_BOX) {
    let (Some(d), Some(s)) = (pc_location(dst), pc_location(src)) else { return };
    pc::<unsafe extern "system" fn(*mut c_void, *const D3D12_TEXTURE_COPY_LOCATION, u32, u32, u32, *const D3D12_TEXTURE_COPY_LOCATION, *const D3D12_BOX)>(this, PC_COPY_TEXTURE_REGION)(this, &d, x, y, z, &s, bx);
    std::mem::forget((d, s)); // borrowed resource pointers
}

/// CopyResource(dst, src) (PC #17; DMA list #13): buffers placed below their console address
/// are copied as their own bytes (the PC resources also hold what lies before them)
pub(super) unsafe extern "system" fn copy_resource_x(this: *mut c_void, dst: usize, src: usize) {
    let (dd, sd) = (real::buffer_delta(dst), real::buffer_delta(src));
    if dd == 0 && sd == 0 {
        return pc::<unsafe extern "system" fn(*mut c_void, usize, usize)>(this, PC_COPY_RESOURCE)(this, dst, src);
    }
    let n = real::console_buffer(dst).or(real::console_buffer(src)).map_or(0, |b| b.1 as u64);
    copy_buffer_region_x(this, dst, 0, src, 0, n);
}

/// command list #28 BeginQuery(heap, type, index) (PC #52)
pub(super) unsafe extern "system" fn begin_query_x(this: *mut c_void, heap: usize, ty: D3D12_QUERY_TYPE, index: u32) {
    static N: AtomicU64 = AtomicU64::new(0);
    if is_occlusion(ty) {
        let outer = OCCLUSION.lock().unwrap().get_or_insert_with(HashMap::new).entry(this as usize).or_default().begin((heap, index));
        if let Some((h, i)) = outer {
            if once(&N, 1) {
                crate::xlog_info!("D3D12.X: nested occlusion query – the outer one ends there and reports visible, the inner one counts");
            }
            pc::<QueryFn>(this, PC_END_QUERY)(this, h, ty, i);
        }
        if let Some(m) = VISIBLE.lock().unwrap().as_mut() {
            m.remove(&(heap, index));
        }
    }
    pc::<QueryFn>(this, PC_BEGIN_QUERY)(this, heap, ty, index);
}

/// command list #29 EndQuery(heap, type, index) (PC #53)
pub(super) unsafe extern "system" fn end_query_x(this: *mut c_void, heap: usize, ty: D3D12_QUERY_TYPE, index: u32) {
    if is_occlusion(ty) {
        let forward = OCCLUSION.lock().unwrap().get_or_insert_with(HashMap::new).entry(this as usize).or_default().end((heap, index));
        if !forward {
            let v = if ty == D3D12_QUERY_TYPE_BINARY_OCCLUSION { 1 } else { VISIBLE_SAMPLES };
            VISIBLE.lock().unwrap().get_or_insert_with(HashMap::new).insert((heap, index), v);
            return;
        }
    }
    pc::<QueryFn>(this, PC_END_QUERY)(this, heap, ty, index);
}

/// command list #30 ResolveQueryData(heap, type, first, count, destination, offset) (PC #54):
/// outer occlusion queries ended early are written over the resolved data (after it, in COPY_DEST as
/// ResolveQueryData wants the destination)
pub(super) unsafe extern "system" fn resolve_query_data_x(this: *mut c_void, heap: usize, ty: D3D12_QUERY_TYPE, first: u32, count: u32, dest: usize, offset: u64) {
    type F = unsafe extern "system" fn(*mut c_void, usize, D3D12_QUERY_TYPE, u32, u32, usize, u64);
    pc::<F>(this, PC_RESOLVE_QUERY_DATA)(this, heap, ty, first, count, dest, offset);
    if !is_occlusion(ty) {
        return;
    }
    let visible: Vec<(u32, u64)> = match VISIBLE.lock().unwrap().as_ref() {
        Some(m) if !m.is_empty() => (first..first.saturating_add(count)).filter_map(|i| m.get(&(heap, i)).map(|&v| (i, v))).collect(),
        _ => return,
    };
    let dest = dest as *mut c_void;
    let Some(res) = ID3D12Resource::from_raw_borrowed(&dest).filter(|_| !visible.is_empty()) else { return };
    let at = res.GetGPUVirtualAddress() + offset;
    let params: Vec<D3D12_WRITEBUFFERIMMEDIATE_PARAMETER> = visible
        .iter()
        .flat_map(|&(i, v)| {
            let a = at + 8 * (i - first) as u64;
            [D3D12_WRITEBUFFERIMMEDIATE_PARAMETER { Dest: a, Value: v as u32 }, D3D12_WRITEBUFFERIMMEDIATE_PARAMETER { Dest: a + 4, Value: (v >> 32) as u32 }]
        })
        .collect();
    let modes = vec![D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_OUT; params.len()];
    type W = unsafe extern "system" fn(*mut c_void, u32, *const D3D12_WRITEBUFFERIMMEDIATE_PARAMETER, *const D3D12_WRITEBUFFERIMMEDIATE_MODE);
    pc::<W>(this, PC_WRITE_BUFFER_IMMEDIATE)(this, params.len() as u32, params.as_ptr(), modes.as_ptr());
}

#[cfg(test)]
mod tests {
    #[test]
    fn nested_occlusion_queries_end_the_outer_one_early() {
        let mut o = super::Occlusion::default();
        assert_eq!(o.begin((1, 0)), None);
        assert_eq!(o.begin((1, 1)), Some((1, 0)), "the outer one ends on the PC");
        assert!(o.end((1, 1)), "inner: a real query");
        // more per-object queries inside the same outer one
        assert_eq!(o.begin((1, 2)), None);
        assert!(o.end((1, 2)));
        assert!(!o.end((1, 0)), "outer: already ended");
        // overlapping, not nested: A, B, end A, end B
        assert_eq!(o.begin((1, 3)), None);
        assert_eq!(o.begin((1, 4)), Some((1, 3)));
        assert!(!o.end((1, 3)) && o.end((1, 4)));
        assert!(o.active.is_none() && o.ended_early.is_empty());
        assert!(o.end((1, 9)), "a query not begun here: the PC's call");
    }

    #[test]
    fn fill_patterns_repeat_the_value() {
        assert_eq!(super::pattern(&0x1122_3344u32.to_le_bytes())[..8], [0x44, 0x33, 0x22, 0x11, 0x44, 0x33, 0x22, 0x11]);
        assert_eq!(super::pattern(&[7u8; 16]), [7u8; 16]);
    }
}
