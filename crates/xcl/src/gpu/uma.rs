//! Console unified memory (UMA). On the console CPU and GPU share memory and shaders reach
//! it through plain addresses in SGPRs. XCL keeps the title's graphics memory in regions
//! that D3D12 can open as heaps (`ID3D12Device3::OpenExistingHeapFromAddress`), so the GPU
//! works on them directly.
//!
//! A heap is smaller than 4 GiB, so the memory is up to 4 *windows*: window k is one region
//! at `base + k · 4 GiB` with `base` 16 GiB-aligned. The window of a console GPU address is
//! then `(address >> 32) & 3` and its low 32 bits are the offset in that window's heap – the
//! recompiler's `mem` (window 0), `mem1` … buffers. Allocations never straddle windows.
//!
//! Measured on Windows 11 + RX 9070 (2026-11-20): the heap must cover a fully committed
//! `VirtualAlloc` allocation (a reservation with partial commit is rejected); up to
//! 4095 MiB works (exactly 4 GiB is rejected), opening 4 GiB takes ~0.5 s.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::System::Memory::{VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE};

/// VirtualAlloc: track CPU writes (GetWriteWatch)
const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// allocation granularity (as VirtualAlloc)
pub const GRAIN: usize = 0x1_0000;
/// largest region D3D12 opens as a heap
pub const MAX_SIZE: usize = (4096 << 20) - GRAIN;
/// distance between windows
pub const WINDOW: usize = 1 << 32;
pub const MAX_WINDOWS: usize = 4;

pub struct Uma {
    base: usize,
    windows: usize,
    /// bytes of each window
    size: usize,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// free ranges: offset → length (coalesced)
    free: BTreeMap<usize, usize>,
    /// live allocations: offset → length
    used: BTreeMap<usize, usize>,
}

impl Uma {
    /// Commit `size` bytes (split into as few equal windows as fit, at most 4) at the first
    /// free 16 GiB-aligned address from 16 GiB up.
    pub fn new(size: usize) -> Option<Uma> {
        let size = ((size + GRAIN - 1) & !(GRAIN - 1)).max(GRAIN);
        let n = size.div_ceil(MAX_SIZE).clamp(1, MAX_WINDOWS);
        Uma::with_windows(n, (size.div_ceil(n) + GRAIN - 1) & !(GRAIN - 1))
    }

    /// `windows` windows of `size` bytes each.
    pub fn with_windows(windows: usize, size: usize) -> Option<Uma> {
        let size = ((size + GRAIN - 1) & !(GRAIN - 1)).clamp(GRAIN, MAX_SIZE);
        let windows = windows.clamp(1, MAX_WINDOWS);
        for slot in (4usize..512).step_by(MAX_WINDOWS) {
            let base = slot << 32;
            let mut got = Vec::new();
            for k in 0..windows {
                let want = base + k * WINDOW;
                // write-watched: textures the title keeps here are uploaded again after CPU writes
                let p = unsafe { VirtualAlloc(want as *const std::ffi::c_void, size, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE) };
                if p.is_null() {
                    break;
                }
                got.push(p);
            }
            if got.len() == windows {
                let mut inner = Inner::default();
                for k in 0..windows {
                    inner.free.insert(k * WINDOW, size);
                }
                return Some(Uma { base, windows, size, inner: Mutex::new(inner) });
            }
            for p in got {
                unsafe { VirtualFree(p, 0, MEM_RELEASE) };
            }
        }
        None
    }

    pub fn base(&self) -> usize {
        self.base
    }

    /// usable bytes (all windows)
    pub fn size(&self) -> usize {
        self.size * self.windows
    }

    pub fn windows(&self) -> usize {
        self.windows
    }

    /// (address, bytes) of window `k`
    pub fn window(&self, k: usize) -> (usize, usize) {
        (self.base + k * WINDOW, self.size)
    }

    pub fn window_of(&self, p: usize) -> Option<usize> {
        let k = p.checked_sub(self.base)? / WINDOW;
        (k < self.windows && p - self.window(k).0 < self.size).then_some(k)
    }

    pub fn contains(&self, p: usize) -> bool {
        self.window_of(p).is_some()
    }

    /// Zeroed block of `size` bytes aligned to `align` (at least 64 KiB), first fit (window 0
    /// first).
    pub fn alloc(&self, size: usize, align: usize) -> Option<*mut u8> {
        self.alloc_where(size, align, None)
    }

    /// `alloc` inside window `k` (memory the console addresses with 32 bits, e.g. descriptor
    /// heaps: window 0).
    pub fn alloc_in(&self, k: usize, size: usize, align: usize) -> Option<*mut u8> {
        self.alloc_where(size, align, Some(k))
    }

    fn alloc_where(&self, size: usize, align: usize, window: Option<usize>) -> Option<*mut u8> {
        let size = (size.max(1) + GRAIN - 1) & !(GRAIN - 1);
        let align = align.max(GRAIN).next_power_of_two();
        let mut g = self.inner.lock().unwrap();
        let (start, len, at) = g.free.iter().filter(|(&o, _)| window.is_none_or(|k| o / WINDOW == k)).find_map(|(&o, &l)| {
            let at = ((self.base + o + align - 1) & !(align - 1)) - self.base;
            (at + size <= o + l).then_some((o, l, at))
        })?;
        g.free.remove(&start);
        if at > start {
            g.free.insert(start, at - start);
        }
        if at + size < start + len {
            g.free.insert(at + size, start + len - at - size);
        }
        g.used.insert(at, size);
        drop(g);
        let p = (self.base + at) as *mut u8;
        unsafe { std::ptr::write_bytes(p, 0, size) };
        Some(p)
    }

    /// Release a block from `alloc` (false: not one of ours).
    pub fn free(&self, p: usize) -> bool {
        if !self.contains(p) {
            return false;
        }
        let mut g = self.inner.lock().unwrap();
        let off = p - self.base;
        let Some(orig) = g.used.remove(&off) else { return false };
        let (mut start, mut len) = (off, orig);
        // merge with the free neighbours
        if let Some((&o, &l)) = g.free.range(..off).next_back() {
            if o + l == off {
                g.free.remove(&o);
                start = o;
                len += l;
            }
        }
        if let Some(l) = g.free.remove(&(off + orig)) {
            len += l;
        }
        g.free.insert(start, len);
        true
    }

    /// Bytes handed out.
    pub fn used(&self) -> usize {
        self.inner.lock().unwrap().used.values().sum()
    }
}

impl Drop for Uma {
    fn drop(&mut self) {
        for k in 0..self.windows {
            unsafe {
                VirtualFree(self.window(k).0 as *mut std::ffi::c_void, 0, MEM_RELEASE);
            }
        }
    }
}

static GLOBAL: OnceLock<Option<Uma>> = OnceLock::new();

/// The title's graphics memory (created on first use; `XCL_UMA_MB`, default 2048).
pub fn get() -> Option<&'static Uma> {
    GLOBAL
        .get_or_init(|| {
            let mb = std::env::var("XCL_UMA_MB").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(2048);
            let u = Uma::new(mb << 20);
            match &u {
                Some(u) => crate::xlog_info!("UMA: {} MB of graphics memory at {:#x}, {} window(s) (GPU-accessible without copies)", u.size() >> 20, u.base, u.windows),
                None => crate::xlog_warn!("UMA: could not reserve {mb} MB at a 4 GiB-aligned address – plain allocations"),
            }
            u
        })
        .as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_are_4_gib_apart_and_allocations_stay_inside() {
        let u = Uma::with_windows(2, 4 << 20).unwrap();
        assert_eq!(u.base() % (MAX_WINDOWS * WINDOW), 0, "16 GiB alignment");
        assert_eq!((u.windows(), u.size()), (2, 8 << 20));
        let a = u.alloc(3 << 20, 0).unwrap() as usize;
        let b = u.alloc(3 << 20, 0).unwrap() as usize; // no room left in window 0
        assert_eq!(u.window_of(a), Some(0));
        assert_eq!(u.window_of(b), Some(1));
        assert_eq!(b, u.base() + WINDOW);
        assert_eq!((b >> 32) & 3, 1, "window from the high address bits");
        assert!(u.alloc_in(0, 2 << 20, 0).is_none(), "window 0 full");
        assert!(u.alloc(2 << 20, 0).is_none());
        unsafe { *(b as *mut u32) = 7 };
        assert!(u.free(a) && u.free(b));
        assert!(!u.contains(u.base() + (4 << 20)) && !u.contains(u.base() + 2 * WINDOW));
    }

    #[test]
    fn alloc_free_coalesce() {
        let u = Uma::new(16 << 20).unwrap();
        assert_eq!(u.base() & 0xFFFF_FFFF, 0, "4 GiB alignment");
        let a = u.alloc(100, 0).unwrap() as usize;
        let b = u.alloc(0x2_0000, 0).unwrap() as usize;
        let c = u.alloc(0x1_0000, 0x10_0000).unwrap() as usize;
        assert_eq!(a, u.base());
        assert_eq!(b, u.base() + GRAIN);
        assert_eq!(c % 0x10_0000, 0, "alignment");
        assert_eq!(u.used(), 4 * GRAIN);
        unsafe { *(b as *mut u32) = 0xDEAD };
        assert!(u.free(b) && !u.free(b) && !u.free(12345));
        assert!(u.free(a));
        // a + b coalesced: a 3-grain block fits at the base again, zeroed
        let d = u.alloc(3 * GRAIN, 0).unwrap() as usize;
        assert_eq!(d, u.base());
        assert_eq!(unsafe { *((d + GRAIN) as *const u32) }, 0);
        assert!(u.free(c) && u.free(d));
        assert_eq!(u.used(), 0);
        // everything merged back into one range
        assert!(u.alloc(16 << 20, 0).is_some());
    }
}
