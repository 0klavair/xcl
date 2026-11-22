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
    /// windows `windows`..4 reserved (not committed) as *sparse* windows: the title's own
    /// reservations get their addresses, the GPU sees them through tiled window buffers
    /// (`d3d12x::sparse`)
    sparse: bool,
    /// free ranges of the sparse windows (address → length) and the reservations in them
    sparse_free: Mutex<(BTreeMap<usize, usize>, BTreeMap<usize, usize>)>,
    /// XCL's own pool in window 0 for `Arena` blocks: (start, end, free blocks). The title
    /// cannot take it – a title that commits all of unified memory at start (Unicorn
    /// Overlord: 2046 of 2048 MB) left no room for draw arguments, and draws were skipped.
    pool: Mutex<(usize, usize, Vec<usize>)>,
}

/// bytes of XCL's arena pool (`Uma::new`)
pub const POOL: usize = 64 << 20;

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
        let mut u = Uma::with_windows(n, (size.div_ceil(n) + GRAIN - 1) & !(GRAIN - 1))?;
        u.reserve_sparse_windows();
        u.reserve_pool(POOL);
        Some(u)
    }

    /// XCL's arena pool: `bytes` at the end of window 0 taken now, before the title can (a
    /// quarter of the window at most); the title's allocations still start at the window's
    /// start.
    pub fn reserve_pool(&self, bytes: usize) {
        let bytes = (bytes.min(self.size / 4) / ARENA_BLOCK) * ARENA_BLOCK;
        if bytes == 0 {
            return;
        }
        let at = {
            let mut g = self.inner.lock().unwrap();
            // the free range of window 0 that reaches furthest, its last `bytes`
            let Some((start, len)) = g.free.iter().filter(|(&o, &l)| o / WINDOW == 0 && l >= bytes).max_by_key(|(&o, &l)| o + l).map(|(&o, &l)| (o, l)) else { return };
            let at = (start + len - bytes) & !(GRAIN - 1);
            if at < start {
                return;
            }
            g.free.remove(&start);
            if at > start {
                g.free.insert(start, at - start);
            }
            if at + bytes < start + len {
                g.free.insert(at + bytes, start + len - at - bytes);
            }
            g.used.insert(at, bytes);
            at
        };
        let p = self.base + at;
        unsafe { std::ptr::write_bytes(p as *mut u8, 0, bytes) };
        *self.pool.lock().unwrap() = (p, p + bytes, (p..p + bytes).step_by(ARENA_BLOCK).rev().collect());
    }

    /// A block of `ARENA_BLOCK` bytes from XCL's pool (None: the pool is used up or absent).
    fn pool_block(&self) -> Option<usize> {
        self.pool.lock().unwrap().2.pop()
    }

    /// The windows after the committed ones as one write-watched reservation (sparse windows).
    fn reserve_sparse_windows(&mut self) {
        if self.windows >= MAX_WINDOWS {
            return;
        }
        let start = self.base + self.windows * WINDOW;
        let len = (MAX_WINDOWS - self.windows) * WINDOW;
        let p = unsafe { VirtualAlloc(start as *const std::ffi::c_void, len, MEM_RESERVE | MEM_WRITE_WATCH, PAGE_READWRITE) };
        if p as usize == start {
            self.sparse = true;
            self.sparse_free.lock().unwrap().0.insert(start, len);
        } else if !p.is_null() {
            unsafe { VirtualFree(p, 0, MEM_RELEASE) };
        }
    }

    /// Windows translated shaders address (`mem`, `mem1` …): the committed and the sparse ones.
    pub fn shader_windows(&self) -> usize {
        if self.sparse {
            MAX_WINDOWS
        } else {
            self.windows
        }
    }

    /// Whether `p` is in a sparse window (the title's reservations).
    pub fn is_sparse(&self, p: usize) -> bool {
        self.sparse && p >= self.base + self.windows * WINDOW && p < self.base + MAX_WINDOWS * WINDOW
    }

    /// Whether `p` is in a committed window (XCL's allocations).
    pub fn is_committed(&self, p: usize) -> bool {
        p.checked_sub(self.base).is_some_and(|o| o / WINDOW < self.windows && o % WINDOW < self.size)
    }

    /// Address space for a title reservation of `size` bytes in the sparse windows (64 KiB
    /// aligned, first fit; not committed).
    pub fn sparse_reserve(&self, size: usize) -> Option<usize> {
        let size = (size.max(1) + GRAIN - 1) & !(GRAIN - 1);
        let mut g = self.sparse_free.lock().unwrap();
        let (&at, &len) = g.0.iter().find(|(_, &l)| l >= size)?;
        g.0.remove(&at);
        if len > size {
            g.0.insert(at + size, len - size);
        }
        g.1.insert(at, size);
        Some(at)
    }

    /// The bytes of the sparse reservation starting at `p` (it is gone after this).
    pub fn sparse_release(&self, p: usize) -> Option<usize> {
        let mut g = self.sparse_free.lock().unwrap();
        let size = g.1.remove(&p)?;
        let (mut start, mut len) = (p, size);
        if let Some((&o, &l)) = g.0.range(..p).next_back() {
            if o + l == p {
                g.0.remove(&o);
                start = o;
                len += l;
            }
        }
        if let Some(l) = g.0.remove(&(p + size)) {
            len += l;
        }
        g.0.insert(start, len);
        Some(size)
    }

    /// The sparse reservation holding `p`: (start, bytes).
    pub fn sparse_reservation(&self, p: usize) -> Option<(usize, usize)> {
        let g = self.sparse_free.lock().unwrap();
        g.1.range(..=p).next_back().map(|(&a, &l)| (a, l)).filter(|&(a, l)| p < a + l)
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
                return Some(Uma { base, windows, size, inner: Mutex::new(inner), sparse: false, sparse_free: Mutex::new(Default::default()), pool: Mutex::new((0, 0, Vec::new())) });
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
    /// (address, bytes) of window `k` (a sparse window: all 4 GiB)
    pub fn window(&self, k: usize) -> (usize, usize) {
        (self.base + k * WINDOW, if k < self.windows { self.size } else { WINDOW })
    }

    pub fn window_of(&self, p: usize) -> Option<usize> {
        let k = p.checked_sub(self.base)? / WINDOW;
        (k < self.shader_windows() && p - self.window(k).0 < self.window(k).1).then_some(k)
    }

    pub fn contains(&self, p: usize) -> bool {
        self.window_of(p).is_some()
    }

    /// Whether `p` is memory the title has here: a block handed out in a committed window, or
    /// one of its reservations in a sparse window. The windows' ranges are wide (4 GiB each) and
    /// a PC resource's GPU address can fall in one by number – that address is not the
    /// window's memory (Minecraft's dynamic vertex buffer, a PC upload buffer, was read from an
    /// empty sparse window: nothing drew).
    pub fn is_title_address(&self, p: usize) -> bool {
        if self.is_sparse(p) {
            return self.sparse_reservation(p).is_some();
        }
        if !self.is_committed(p) {
            return false;
        }
        let off = p - self.base;
        let g = self.inner.lock().unwrap();
        g.used.range(..=off).next_back().is_some_and(|(&o, &l)| off < o + l)
    }

    /// Zeroed block of `size` bytes aligned to `align` (at least 64 KiB), first fit (window 0
    /// first).
    pub fn alloc(&self, size: usize, align: usize) -> Option<*mut u8> {
        self.alloc_where(size, align, None, true)
    }

    /// `alloc` for reserved address space: not zeroed (a large reservation is not touched –
    /// what the title commits or maps there it writes; never-used memory is zero anyway).
    pub fn reserve(&self, size: usize, align: usize) -> Option<*mut u8> {
        self.alloc_where(size, align, None, false)
    }

    /// `alloc` inside window `k` (memory the console addresses with 32 bits, e.g. descriptor
    /// heaps: window 0).
    pub fn alloc_in(&self, k: usize, size: usize, align: usize) -> Option<*mut u8> {
        self.alloc_where(size, align, Some(k), true)
    }

    fn alloc_where(&self, size: usize, align: usize, window: Option<usize>, zero: bool) -> Option<*mut u8> {
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
        if zero {
            unsafe { std::ptr::write_bytes(p, 0, size) };
        }
        Some(p)
    }

    /// Release a block from `alloc` (false: not one of ours).
    pub fn free(&self, p: usize) -> bool {
        if !self.contains(p) {
            return false;
        }
        {
            // a block of XCL's pool goes back to the pool
            let mut g = self.pool.lock().unwrap();
            if p >= g.0 && p < g.1 {
                g.2.push(p);
                return true;
            }
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

/// Small pieces of window 0 for XCL's own per-draw data (argument tables, user SGPRs): a
/// block of `ARENA_BLOCK` bytes cut into aligned pieces, a new block when one is used up.
/// `Uma::alloc` hands out whole 64 KiB grains – three of those per draw filled 2 GB within
/// seconds in a scene of thousands of draws (Minecraft's world), and the draws that found
/// no room were skipped (the world flickered). The blocks go to `blocks`; their owner frees
/// them when the GPU is done (a command allocator's reset) and starts a fresh arena.
#[derive(Default)]
pub struct Arena {
    at: usize,
    end: usize,
}

/// block size of an `Arena`
pub const ARENA_BLOCK: usize = 4 * GRAIN;

impl Arena {
    /// `n` bytes at an `align`-aligned address of window 0 (not zeroed), or None when
    /// unified memory is full.
    pub fn take(&mut self, u: &Uma, n: usize, align: usize, blocks: &mut Vec<usize>) -> Option<*mut u8> {
        let align = align.max(16).next_power_of_two();
        let n = n.max(1);
        let at = (self.at + align - 1) & !(align - 1);
        if self.at != 0 && at + n <= self.end {
            self.at = at + n;
            return Some(at as *mut u8);
        }
        if n > ARENA_BLOCK / 2 {
            // a large piece: a block of its own (the current one stays in use)
            let p = u.alloc_in(0, n, align)?;
            blocks.push(p as usize);
            return Some(p);
        }
        // XCL's pool first, then any free part of window 0
        let p = match u.pool_block() {
            Some(p) => p,
            None => u.alloc_in(0, ARENA_BLOCK, GRAIN)? as usize,
        };
        blocks.push(p);
        self.at = p + n;
        self.end = p + ARENA_BLOCK;
        Some(p as *mut u8)
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

    /// Arena blocks come from XCL's pool while the title holds the rest of window 0, and go
    /// back to it when freed.
    #[test]
    fn arena_blocks_come_from_the_pool_when_the_title_took_the_rest() {
        let u = Uma::with_windows(1, 8 << 20).unwrap();
        u.reserve_pool(4 * ARENA_BLOCK);
        // the pool is the window's end: the title's first allocation is still its start
        assert_eq!(u.pool.lock().unwrap().1, u.base() + (8 << 20));
        assert_eq!(u.alloc(GRAIN, GRAIN).map(|p| p as usize), Some(u.base()));
        // the title takes everything else
        let mut title = Vec::new();
        while let Some(p) = u.alloc(GRAIN, GRAIN) {
            title.push(p as usize);
        }
        assert!(u.alloc_in(0, ARENA_BLOCK, GRAIN).is_none());
        let (mut a, mut blocks) = (Arena::default(), Vec::new());
        for _ in 0..4 * (ARENA_BLOCK / 256) {
            a.take(&u, 200, 256, &mut blocks).expect("a piece from the pool");
        }
        assert_eq!(blocks.len(), 4);
        assert!(a.take(&u, 200, 256, &mut blocks).is_none(), "pool used up, the rest is the title's");
        for b in blocks.drain(..) {
            assert!(u.free(b));
        }
        let mut a = Arena::default();
        assert!(a.take(&u, 200, 256, &mut blocks).is_some(), "freed blocks are back in the pool");
    }

    /// Many small pieces share a block (aligned, disjoint); a large one gets its own.
    #[test]
    fn arena_cuts_blocks_into_aligned_pieces() {
        let u = Uma::with_windows(1, 4 << 20).unwrap();
        let (mut a, mut blocks) = (Arena::default(), Vec::new());
        let mut got = Vec::new();
        for k in 0..1000usize {
            let n = 16 + (k % 7) * 40;
            let p = a.take(&u, n, 256, &mut blocks).unwrap() as usize;
            assert_eq!(p % 256, 0);
            assert_eq!(u.window_of(p), Some(0));
            got.push((p, n));
        }
        got.sort_unstable();
        assert!(got.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0), "pieces overlap");
        // 1000 pieces of at most 256 aligned bytes: 1000 · 256 B = 4 blocks, not 1000 grains
        assert_eq!(blocks.len(), (1000 * 256usize).div_ceil(ARENA_BLOCK));
        let big = a.take(&u, ARENA_BLOCK, 256, &mut blocks).unwrap() as usize;
        assert_eq!(blocks.last(), Some(&big));
        for b in blocks {
            assert!(u.free(b));
        }
        assert_eq!(u.used(), 0);
    }

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
        // without XCL's arena pool (`Uma::new` takes it at the start of window 0)
        let u = Uma::with_windows(1, 16 << 20).unwrap();
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

    /// Only handed-out blocks are the title's memory: an address in a window's range that is
    /// not one (a PC GPU address by number) is not.
    #[test]
    fn title_addresses_are_blocks_handed_out() {
        let u = Uma::new(16 << 20).unwrap();
        let a = u.alloc(0x2_0000, 0).unwrap() as usize;
        assert!(u.is_title_address(a) && u.is_title_address(a + 0x1_FFFF));
        assert!(!u.is_title_address(a + 0x2_0000) && !u.is_title_address(u.base() + (8 << 20)));
        assert!(!u.is_title_address(u.base() - 1) && !u.is_title_address(u.base() + (32 << 20)));
        assert!(u.free(a));
        assert!(!u.is_title_address(a));
    }
}
