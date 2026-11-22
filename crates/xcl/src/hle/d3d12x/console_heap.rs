//! Console descriptor heaps (CBV/SRV/UAV and samplers).
//!
//! On the console a descriptor is GPU-readable memory: 32 bytes per CBV/SRV/UAV slot (T# /
//! V#), 16 per sampler (S#), and a descriptor table in a root argument is a 32-bit GPU address
//! (shaders load it with the high half 0 – amd_fidelityfx_upscaler_xbox.dll, HANDOFF "FSR4").
//! XCL gives console code heaps whose slots live in unified memory (so the table addresses
//! console code computes from the heap start work as they are in the recompiler's `mem`
//! model) and keeps PC descriptors beside each: view slot k → an SRV and, next to it, a UAV;
//! sampler slot k → PC sampler k. A slot holds the descriptor in the XCL shader ABI
//! (`desc::xcl_image_words`: T# word 0 = the SRV's index in the shader-visible PC heap;
//! `desc::xcl_sampler_words`: S# word 3 [11:0] = k; V# with its unified-memory address).
//!
//! View descriptors are values on the console: titles create them in a CPU-only heap and copy
//! them into the shader-visible one as memory (the console header's inline
//! CopyDescriptorsSimple; XDefiant copies every texture that way), and a descriptor table may
//! be in any heap. So all console view heaps share one shader-visible PC heap (`Global`):
//! each gets a range there, T# word 0 is an index into it – a T# copied anywhere still names
//! its PC descriptor – and that heap is what lists bind. The title's own PC heap stays as a
//! CPU-only mirror (where views are made, copies come from and UAV clears take their CPU
//! handle). Without the shared heap (no room, not made) a heap keeps its own PC heap: SRV at
//! 2k, UAV at 2k + 1.

use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;

/// bytes of a CBV/SRV/UAV slot (T# / V#) and of a sampler slot (S#)
pub const SLOT: u32 = 32;
pub const SAMPLER_SLOT: u32 = 16;

struct Heap {
    obj: usize,
    shadow: usize,
    n: u32,
    pc_cpu: u64,
    /// 0: not shader visible
    pc_gpu: u64,
    inc: u32,
    sampler: bool,
    /// views in the shared heap: index of slot 0's SRV there, and its CPU / GPU handles
    first: Option<(u32, u64, u64)>,
    /// shader visible as the title made it
    visible: bool,
}

impl Heap {
    fn slot_bytes(&self) -> u32 {
        if self.sampler {
            SAMPLER_SLOT
        } else {
            SLOT
        }
    }
}

static HEAPS: Mutex<Vec<Heap>> = Mutex::new(Vec::new());

/// The shader-visible PC heap of all console view heaps.
struct Global {
    dev: ID3D12Device,
    heap: ID3D12DescriptorHeap,
    cpu: u64,
    gpu: u64,
    inc: u32,
    cap: u32,
    next: u32,
    /// released ranges (first, count)
    free: Vec<(u32, u32)>,
}

// D3D12 descriptor heaps and devices are free-threaded
unsafe impl Send for Global {}

static GLOBAL: Mutex<Option<Global>> = Mutex::new(None);
static GLOBAL_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// descriptors of the shared heap at most (the GPU may take more: OPTIONS19)
const GLOBAL_MAX: u32 = 1 << 21;

/// A first-fit range of `count` in `free` (released ranges), else at `next` below `cap`.
fn take_range(free: &mut Vec<(u32, u32)>, next: &mut u32, cap: u32, count: u32) -> Option<u32> {
    if let Some(i) = free.iter().position(|&(_, n)| n >= count) {
        let (f, n) = free[i];
        if n == count {
            free.swap_remove(i);
        } else {
            free[i] = (f + count, n - count);
        }
        return Some(f);
    }
    if cap - *next < count {
        return None;
    }
    *next += count;
    Some(*next - count)
}

/// `count` descriptors of the shared shader-visible heap (made on first use): the index of
/// the first. None: no shared heap, or no room left in it.
pub unsafe fn reserve_global(dev: &ID3D12Device, count: u32) -> Option<u32> {
    use std::sync::atomic::Ordering::Relaxed;
    let mut g = GLOBAL.lock().unwrap();
    if g.is_none() {
        if GLOBAL_FAILED.load(Relaxed) {
            return None;
        }
        let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS19::default();
        let max = if dev.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS19, &mut o as *mut _ as *mut std::ffi::c_void, std::mem::size_of_val(&o) as u32).is_ok() && o.MaxViewDescriptorHeapSize > 0 {
            o.MaxViewDescriptorHeapSize
        } else {
            1_000_000
        };
        let cap = max.min(GLOBAL_MAX);
        let d = D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, NumDescriptors: cap, Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, NodeMask: 0 };
        let heap: ID3D12DescriptorHeap = match dev.CreateDescriptorHeap(&d) {
            Ok(h) => h,
            Err(e) => {
                GLOBAL_FAILED.store(true, Relaxed);
                crate::xlog_warn!("console descriptor heaps: no shared shader-visible heap of {cap} ({e}) – each keeps its own");
                return None;
            }
        };
        crate::xlog_info!("console descriptor heaps: one shader-visible PC heap of {cap} view descriptors for all of them");
        *g = Some(Global {
            dev: dev.clone(),
            cpu: heap.GetCPUDescriptorHandleForHeapStart().ptr as u64,
            gpu: heap.GetGPUDescriptorHandleForHeapStart().ptr,
            inc: dev.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV),
            heap,
            cap,
            next: 0,
            free: Vec::new(),
        });
    }
    let s = g.as_mut()?;
    let (cap, mut next) = (s.cap, s.next);
    let r = take_range(&mut s.free, &mut next, cap, count);
    s.next = next;
    if r.is_none() {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if N.fetch_add(1, Relaxed) == 0 {
            crate::xlog_warn!("console descriptor heaps: the shared heap ({cap}) has no room for {count} more – that heap keeps its own");
        }
    }
    r
}

/// The shared range starting at `first` (`count` descriptors) is free again.
pub fn release_global(first: u32, count: u32) {
    if let Some(s) = GLOBAL.lock().unwrap().as_mut() {
        s.free.push((first, count));
    }
}

/// The shader-visible PC heap lists bind for view descriptors (None: none shared).
pub fn global_heap() -> Option<ID3D12DescriptorHeap> {
    GLOBAL.lock().unwrap().as_ref().map(|s| s.heap.clone())
}

/// Whether `obj` (a title's view heap) has its descriptors in the shared heap.
pub fn in_global(obj: usize) -> bool {
    HEAPS.lock().unwrap().iter().any(|h| h.obj == obj && h.first.is_some())
}

/// `obj` is a PC heap from `pc_cpu` / `pc_gpu` (increment `inc`; `pc_gpu` 0 when not shader
/// visible) with 2·`n` view descriptors or `n` samplers: give it `n` console slots in
/// unified memory. `first`: its range in the shared heap (`reserve_global`); `visible`: what
/// the title asked for.
pub fn register(obj: usize, n: u32, pc_cpu: u64, pc_gpu: u64, inc: u32, sampler: bool, first: Option<u32>, visible: bool) -> bool {
    let Some(u) = crate::gpu::uma::get() else { return false };
    let bytes = n.max(1) * if sampler { SAMPLER_SLOT } else { SLOT };
    // descriptor tables are 32-bit console addresses: window 0
    let Some(p) = u.alloc_in(0, bytes as usize, 256) else { return false };
    unsafe { std::ptr::write_bytes(p, 0, bytes as usize) };
    {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 48 {
            crate::xlog_info!(
                "console descriptor heap {obj:#x}: {n} {} slots at UMA {:#x}, {}",
                if sampler { "sampler" } else { "view" },
                p as usize - u.base(),
                if visible { "shader visible" } else { "CPU only" }
            );
        }
    }
    let first = first.and_then(|f| {
        let g = GLOBAL.lock().unwrap();
        g.as_ref().map(|s| (f, s.cpu + f as u64 * s.inc as u64, s.gpu + f as u64 * s.inc as u64))
    });
    HEAPS.lock().unwrap().push(Heap { obj, shadow: p as usize, n, pc_cpu, pc_gpu, inc, sampler, first, visible });
    true
}

/// The heap object was released for good: its slots go back to unified memory.
pub fn unregister(obj: usize) {
    let gone = {
        let mut heaps = HEAPS.lock().unwrap();
        heaps.iter().position(|h| h.obj == obj).map(|i| heaps.swap_remove(i))
    };
    if let Some(h) = gone {
        if let Some((f, _, _)) = h.first {
            release_global(f, 2 * h.n);
        }
        if let Some(u) = crate::gpu::uma::get() {
            u.free(h.shadow);
        }
    }
}

/// Start of the console slots of heap `obj` (CPU handle = GPU handle on the console).
pub fn start(obj: usize) -> Option<u64> {
    HEAPS.lock().unwrap().iter().find(|h| h.obj == obj).map(|h| h.shadow as u64)
}

/// A console slot: where its descriptor goes and its PC descriptors.
#[derive(Clone, Copy, Debug)]
pub struct Slot {
    pub at: usize,
    /// views: PC heap index of the SRV (the UAV is at + 1); samplers: of the sampler
    pub index: u32,
    /// PC CPU handles of the SRV / UAV where views are made (the title's heap)
    pub pc_srv: u64,
    pub pc_uav: u64,
    /// PC GPU handle of the UAV (0: heap not shader visible)
    pub pc_gpu_uav: u64,
    pub sampler: bool,
    /// CPU handle of the SRV in the shared heap (0: the heap is not in it)
    pub shared_srv: u64,
}

/// The slot behind a console CPU handle (None: not one of XCL's console heaps).
pub fn slot(cpu: u64) -> Option<Slot> {
    let heaps = HEAPS.lock().unwrap();
    let h = heaps.iter().find(|h| cpu >= h.shadow as u64 && cpu < (h.shadow + (h.n * h.slot_bytes()) as usize) as u64)?;
    let k = ((cpu - h.shadow as u64) / h.slot_bytes() as u64) as u32;
    if h.sampler {
        return Some(Slot { at: h.shadow + (k * SAMPLER_SLOT) as usize, index: k, pc_srv: h.pc_cpu + (k * h.inc) as u64, pc_uav: 0, pc_gpu_uav: 0, sampler: true, shared_srv: 0 });
    }
    let uav = ((2 * k + 1) * h.inc) as u64;
    let (index, pc_gpu_uav, shared_srv) = match h.first {
        Some((f, cpu, gpu)) => (f + 2 * k, gpu + uav, cpu + (2 * k * h.inc) as u64),
        None => (2 * k, if h.pc_gpu == 0 { 0 } else { h.pc_gpu + uav }, 0),
    };
    Some(Slot { at: h.shadow + (k * SLOT) as usize, index, pc_srv: h.pc_cpu + (2 * k * h.inc) as u64, pc_uav: h.pc_cpu + uav, pc_gpu_uav, sampler: false, shared_srv })
}

/// The view just made in slot `s` (its SRV, or with `uav` its UAV) goes to the shared heap.
pub unsafe fn publish(s: &Slot, uav: bool) {
    if s.shared_srv == 0 {
        return;
    }
    if let Some(g) = GLOBAL.lock().unwrap().as_ref() {
        let (dst, src) = if uav { (s.shared_srv + g.inc as u64, s.pc_uav) } else { (s.shared_srv, s.pc_srv) };
        g.dev.CopyDescriptorsSimple(1, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: dst as usize }, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: src as usize }, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    }
}

/// `n` view slots from `first` were copied (in the title's heap): they go to the shared heap.
pub unsafe fn publish_range(first: &Slot, n: u32) {
    if first.shared_srv == 0 || n == 0 {
        return;
    }
    if let Some(g) = GLOBAL.lock().unwrap().as_ref() {
        g.dev.CopyDescriptorsSimple(2 * n, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: first.shared_srv as usize }, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: first.pc_srv as usize }, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    }
}

/// view slot address → the resource its view names (`XCL_TRACE_FRAME` diagnostics)
static RES: Mutex<Option<std::collections::HashMap<usize, usize>>> = Mutex::new(None);

/// The view in slot `s` names `res` (0: none).
pub fn note_resource(s: &Slot, res: usize) {
    if s.sampler || !super::present::trace_frame() {
        return;
    }
    let mut g = RES.lock().unwrap();
    let m = g.get_or_insert_with(Default::default);
    if res == 0 {
        m.remove(&s.at);
    } else {
        m.insert(s.at, res);
    }
}

/// The resource of the view slot at UMA address `at`.
pub fn resource_at(at: usize) -> Option<usize> {
    RES.lock().unwrap().as_ref()?.get(&at).copied()
}

pub fn write(s: &Slot, words: &[u32]) {
    let n = words.len().min(if s.sampler { 4 } else { 8 });
    unsafe { std::ptr::copy_nonoverlapping(words.as_ptr(), s.at as *mut u32, n) };
}

/// Copy a console slot's descriptor to another slot of the same kind, the PC heap index
/// in it (T# word 0, S# word 3 [11:0]) renumbered for the destination. Buffer V# carry
/// their address and stay as they are.
pub fn copy_words(src: &Slot, dst: &Slot) {
    let n = if src.sampler { 4 } else { 8 };
    let mut w = [0u32; 8];
    unsafe { std::ptr::copy_nonoverlapping(src.at as *const u32, w.as_mut_ptr(), n) };
    if src.sampler {
        w[3] = (w[3] & !0xFFF) | (dst.index & 0xFFF);
    } else if w[3] >> 28 >= 8 {
        w[0] = dst.index;
    }
    write(dst, &w[..n]);
    if !src.sampler {
        note_resource(dst, resource_at(src.at).unwrap_or(0));
    }
}

/// `XCL_TRACE_FRAME`: what the slots of each shader-visible view heap hold – images whose
/// PC index is the slot's own (made or copied through the device) or another (copied by the
/// title itself, as memory), buffers, empty.
pub fn census() {
    let heaps = HEAPS.lock().unwrap();
    for h in heaps.iter().filter(|h| !h.sampler) {
        let (mut empty, mut own, mut other, mut buffers) = (0u32, 0u32, 0u32, 0u32);
        let mut examples: Vec<(u32, u32)> = Vec::new();
        let base = h.first.map_or(0, |f| f.0);
        for k in 0..h.n {
            let w = unsafe { std::slice::from_raw_parts((h.shadow + (k * SLOT) as usize) as *const u32, 8) };
            if w.iter().all(|&x| x == 0) {
                empty += 1;
            } else if w[3] >> 28 >= 8 {
                if w[0] == base + 2 * k {
                    own += 1;
                } else {
                    other += 1;
                    if examples.len() < 8 {
                        examples.push((k, w[0]));
                    }
                }
            } else {
                buffers += 1;
            }
        }
        crate::xlog_info!(
            "frame trace: view heap {:#x} ({} slots, {}): {own} images with their own PC index, {other} with another (slot, index: {examples:?}), {buffers} buffers, {empty} empty",
            h.obj,
            h.n,
            if h.visible { "shader visible" } else { "CPU only" }
        );
    }
}

/// The descriptor at a console GPU address (low 32 bits, as in a descriptor table), if it
/// is in one of the console heaps.
pub fn words_at(addr32: u32) -> Option<[u32; 8]> {
    let u = crate::gpu::uma::get()?;
    let s = slot((u.base() + addr32 as usize) as u64)?;
    let mut w = [0u32; 8];
    unsafe { std::ptr::copy_nonoverlapping(s.at as *const u32, w.as_mut_ptr(), if s.sampler { 4 } else { 8 }) };
    Some(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ranges of the shared heap: released ones first (first fit, split), then new ones, and
    /// none past its size.
    #[test]
    fn shared_heap_ranges() {
        let (mut free, mut next) = (Vec::new(), 0u32);
        assert_eq!(take_range(&mut free, &mut next, 100, 40), Some(0));
        assert_eq!(take_range(&mut free, &mut next, 100, 40), Some(40));
        assert_eq!(take_range(&mut free, &mut next, 100, 40), None, "20 left");
        free.push((0, 40));
        assert_eq!(take_range(&mut free, &mut next, 100, 30), Some(0));
        assert_eq!(free, vec![(30, 10)]);
        assert_eq!(take_range(&mut free, &mut next, 100, 20), Some(80));
        assert_eq!(take_range(&mut free, &mut next, 100, 10), Some(30));
        assert!(free.is_empty());
    }

    #[test]
    fn slots_copy_and_release() {
        let Some(u) = crate::gpu::uma::get() else { return };
        let rel = |p: u64| (p as usize - u.base()) as u32;
        // views: 4 slots over a PC heap at 0x1000 (increment 32, GPU 0x5000); samplers: 2 at 0x9000
        assert!(register(0xA1, 4, 0x1000, 0x5000, 32, false, None, true));
        assert!(register(0xA2, 2, 0x9000, 0, 16, true, None, true));
        let (v, sm) = (start(0xA1).unwrap(), start(0xA2).unwrap());
        let s1 = slot(v + 32).unwrap();
        assert_eq!((s1.index, s1.pc_srv, s1.pc_uav, s1.pc_gpu_uav), (2, 0x1040, 0x1060, 0x5060));
        // a 2D T# from slot 1 to slot 3: word 0 = the destination's SRV index
        write(&s1, &[2, 0x0470_0000, 0, 9 << 28, 0, 0, 0, 0]);
        let s3 = slot(v + 96).unwrap();
        copy_words(&s1, &s3);
        assert_eq!(words_at(rel(v + 96)).unwrap()[..4], [6, 0x0470_0000, 0, 9 << 28]);
        // a buffer V# keeps its address
        write(&s1, &[0x1234, 0, 64, 0x3101_6FAC, 0, 0, 0, 0]);
        copy_words(&s1, &s3);
        assert_eq!(words_at(rel(v + 96)).unwrap()[0], 0x1234);
        // samplers: 16-byte slots, S# word 3 [11:0] = sampler index
        let (q0, q1) = (slot(sm).unwrap(), slot(sm + 16).unwrap());
        assert!(q1.sampler && q1.index == 1 && q1.pc_srv == 0x9010);
        write(&q0, &[1, 2, 3, 0xABC0_0000]);
        copy_words(&q0, &q1);
        assert_eq!(words_at(rel(sm + 16)).unwrap()[..4], [1, 2, 3, 0xABC0_0001]);
        assert!(slot(sm + 32).is_none(), "past the last sampler slot");
        unregister(0xA1);
        unregister(0xA2);
        assert!(start(0xA1).is_none() && slot(v).is_none() && slot(sm).is_none());
    }
}
