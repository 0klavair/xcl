//! Console CBV/SRV/UAV descriptor heaps.
//!
//! On the console a descriptor is GPU-readable memory: 32 bytes per slot (T# / V#), and a
//! descriptor table in a root argument is a 32-bit GPU address (shaders load it with the
//! high half 0 – amd_fidelityfx_upscaler_xbox.dll, HANDOFF "FSR4"). XCL gives console code
//! a heap whose slots live in unified memory (so the table addresses console code computes
//! from the heap start work as they are in the recompiler's `mem` model) and keeps a PC heap
//! with two descriptors per slot beside it: slot k → PC SRV at 2k, PC UAV at 2k + 1. The
//! slot holds the descriptor in the XCL shader ABI (`desc::xcl_image_words`: word 0 = 2k).

use std::sync::Mutex;

pub const SLOT: u32 = 32;

struct Heap {
    obj: usize,
    shadow: usize,
    n: u32,
    pc_cpu: u64,
    inc: u32,
}

static HEAPS: Mutex<Vec<Heap>> = Mutex::new(Vec::new());

/// `obj` is a PC heap with 2·`n` descriptors from `pc_cpu` (increment `inc`): give it `n`
/// console slots in unified memory.
pub fn register(obj: usize, n: u32, pc_cpu: u64, inc: u32) -> bool {
    let Some(u) = crate::gpu::uma::get() else {
        return false;
    };
    let Some(p) = u.alloc((n.max(1) * SLOT) as usize, 256) else {
        return false;
    };
    unsafe { std::ptr::write_bytes(p, 0, (n * SLOT) as usize) };
    HEAPS.lock().unwrap().push(Heap {
        obj,
        shadow: p as usize,
        n,
        pc_cpu,
        inc,
    });
    true
}

/// Start of the console slots of heap `obj` (CPU handle = GPU handle on the console).
pub fn start(obj: usize) -> Option<u64> {
    HEAPS
        .lock()
        .unwrap()
        .iter()
        .find(|h| h.obj == obj)
        .map(|h| h.shadow as u64)
}

/// A console slot: where its descriptor goes and its two PC descriptors.
#[derive(Clone, Copy, Debug)]
pub struct Slot {
    pub at: usize,
    /// PC heap index of the SRV (the UAV is at + 1)
    pub index: u32,
    pub pc_srv: u64,
    pub pc_uav: u64,
}

/// The slot behind a console CPU handle (None: not one of XCL's console heaps).
pub fn slot(cpu: u64) -> Option<Slot> {
    let heaps = HEAPS.lock().unwrap();
    let h = heaps
        .iter()
        .find(|h| cpu >= h.shadow as u64 && cpu < (h.shadow + (h.n * SLOT) as usize) as u64)?;
    let k = ((cpu - h.shadow as u64) / SLOT as u64) as u32;
    Some(Slot {
        at: h.shadow + (k * SLOT) as usize,
        index: 2 * k,
        pc_srv: h.pc_cpu + (2 * k * h.inc) as u64,
        pc_uav: h.pc_cpu + ((2 * k + 1) * h.inc) as u64,
    })
}

pub fn write(s: &Slot, words: &[u32]) {
    let n = words.len().min(8);
    unsafe { std::ptr::copy_nonoverlapping(words.as_ptr(), s.at as *mut u32, n) };
}

/// The descriptor at a console GPU address (low 32 bits, as in a descriptor table), if it
/// is in one of the console heaps.
pub fn words_at(addr32: u32) -> Option<[u32; 8]> {
    let u = crate::gpu::uma::get()?;
    let s = slot((u.base() + addr32 as usize) as u64)?;
    Some(unsafe { std::ptr::read(s.at as *const [u32; 8]) })
}
