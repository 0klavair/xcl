//! Console descriptor heaps (CBV/SRV/UAV and samplers).
//!
//! On the console a descriptor is GPU-readable memory: 32 bytes per CBV/SRV/UAV slot (T# /
//! V#), 16 per sampler (S#), and a descriptor table in a root argument is a 32-bit GPU address
//! (shaders load it with the high half 0 – amd_fidelityfx_upscaler_xbox.dll, HANDOFF "FSR4").
//! XCL gives console code heaps whose slots live in unified memory (so the table addresses
//! console code computes from the heap start work as they are in the recompiler's `mem`
//! model) and keeps a PC heap beside each: view slot k → PC SRV at 2k, PC UAV at 2k + 1;
//! sampler slot k → PC sampler k. A slot holds the descriptor in the XCL shader ABI
//! (`desc::xcl_image_words`: T# word 0 = 2k; `desc::xcl_sampler_words`: S# word 3 [11:0] = k;
//! V# with its unified-memory address).

use std::sync::Mutex;

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

/// `obj` is a PC heap from `pc_cpu` / `pc_gpu` (increment `inc`; `pc_gpu` 0 when not shader
/// visible) with 2·`n` view descriptors or `n` samplers: give it `n` console slots in
/// unified memory.
pub fn register(obj: usize, n: u32, pc_cpu: u64, pc_gpu: u64, inc: u32, sampler: bool) -> bool {
    let Some(u) = crate::gpu::uma::get() else { return false };
    let bytes = n.max(1) * if sampler { SAMPLER_SLOT } else { SLOT };
    // descriptor tables are 32-bit console addresses: window 0
    let Some(p) = u.alloc_in(0, bytes as usize, 256) else { return false };
    unsafe { std::ptr::write_bytes(p, 0, bytes as usize) };
    HEAPS.lock().unwrap().push(Heap { obj, shadow: p as usize, n, pc_cpu, pc_gpu, inc, sampler });
    true
}

/// The heap object was released for good: its slots go back to unified memory.
pub fn unregister(obj: usize) {
    let gone = {
        let mut heaps = HEAPS.lock().unwrap();
        heaps.iter().position(|h| h.obj == obj).map(|i| heaps.swap_remove(i))
    };
    if let (Some(h), Some(u)) = (gone, crate::gpu::uma::get()) {
        u.free(h.shadow);
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
    pub pc_srv: u64,
    pub pc_uav: u64,
    /// PC GPU handle of the UAV (0: heap not shader visible)
    pub pc_gpu_uav: u64,
    pub sampler: bool,
}

/// The slot behind a console CPU handle (None: not one of XCL's console heaps).
pub fn slot(cpu: u64) -> Option<Slot> {
    let heaps = HEAPS.lock().unwrap();
    let h = heaps.iter().find(|h| cpu >= h.shadow as u64 && cpu < (h.shadow + (h.n * h.slot_bytes()) as usize) as u64)?;
    let k = ((cpu - h.shadow as u64) / h.slot_bytes() as u64) as u32;
    if h.sampler {
        return Some(Slot { at: h.shadow + (k * SAMPLER_SLOT) as usize, index: k, pc_srv: h.pc_cpu + (k * h.inc) as u64, pc_uav: 0, pc_gpu_uav: 0, sampler: true });
    }
    let uav = ((2 * k + 1) * h.inc) as u64;
    Some(Slot {
        at: h.shadow + (k * SLOT) as usize,
        index: 2 * k,
        pc_srv: h.pc_cpu + (2 * k * h.inc) as u64,
        pc_uav: h.pc_cpu + uav,
        pc_gpu_uav: if h.pc_gpu == 0 { 0 } else { h.pc_gpu + uav },
        sampler: false,
    })
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

    #[test]
    fn slots_copy_and_release() {
        let Some(u) = crate::gpu::uma::get() else { return };
        let rel = |p: u64| (p as usize - u.base()) as u32;
        // views: 4 slots over a PC heap at 0x1000 (increment 32, GPU 0x5000); samplers: 2 at 0x9000
        assert!(register(0xA1, 4, 0x1000, 0x5000, 32, false));
        assert!(register(0xA2, 2, 0x9000, 0, 16, true));
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
