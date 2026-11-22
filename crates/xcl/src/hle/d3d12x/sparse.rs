//! The title's own memory, seen by the GPU: sparse windows.
//!
//! A console title keeps GPU data in memory it reserves and commits itself (XDefiant: one
//! 8 GB reservation of 64 KiB pages it commits in steps and places buffers in) – on the
//! console all of it is GPU-visible at the same addresses. XCL gives such reservations
//! addresses in the *sparse windows* of the unified-memory block (`gpu::uma`: windows after
//! the committed one, reserved only), so translated shaders reach them through `mem1`…`mem3`
//! like any unified memory. On the GPU each sparse window is a reserved (tiled) buffer of
//! 4 GiB whose 64 KiB tiles are mapped onto video-memory heaps as the title commits pages.
//! The CPU's view stays the title's memory: what the CPU wrote (write-watch) is copied into
//! the tiles before the title's next ExecuteCommandLists, and its queues wait for that copy.
//! Only the bytes the CPU changed go (64-byte chunks against a hash of what was sent last,
//! `changed_chunks`): the rest of a written page may hold what the GPU wrote there (counters,
//! indirect arguments) and must not be overwritten with the CPU's stale view of it.
//! Buffers the title places there get reserved resources over the same tiles, so PC-side
//! copies, views and barriers see the same memory. GPU writes are not copied back to the
//! CPU's view (not needed by rendering so far).

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::Mutex;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows_sys::Win32::System::Memory::{VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_DECOMMIT, PAGE_READWRITE};

const TILE: usize = 0x1_0000;
/// tiles of a video-memory pool heap (64 MiB)
const POOL_TILES: u32 = 1024;
/// bytes of the upload staging buffer
const STAGING: usize = 64 << 20;
/// bytes of the GPU→CPU readback staging buffer
const READBACK_STAGING: usize = 16 << 20;

struct State {
    dev: ID3D12Device,
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    value: u64,
    /// fence signalled on title queues so `queue` waits before copying GPU writes back to CPU
    rb_fence: ID3D12Fence,
    rb_value: u64,
    /// (window start, reserved buffer) of each sparse window
    windows: Vec<(usize, ID3D12Resource)>,
    pools: Vec<ID3D12Heap>,
    free: Vec<(u32, u32)>,
    /// tile address → (pool, tile in it)
    mapped: HashMap<usize, (u32, u32)>,
    /// pool tiles also used by placed buffers (not freed while they are)
    pinned: HashMap<(u32, u32), u32>,
    /// placed buffers → the pool tiles they use
    buffers: HashMap<usize, Vec<(u32, u32)>>,
    /// reused tiles to clear: (address, bytes)
    to_zero: Vec<(usize, usize)>,
    staging: Option<(ID3D12Resource, *mut u8)>,
    readback: Option<(ID3D12Resource, *mut u8)>,
    alloc: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    /// the fence value each title queue last waited for
    waited: HashMap<usize, u64>,
    /// 4 KiB page → hash of each 64-byte chunk the GPU got from the CPU (absent: all zero)
    sent: HashMap<usize, Box<[u64; CHUNKS]>>,
    /// `mapped` as sorted (start, end) runs (None: to be made again)
    runs: Option<Vec<(usize, usize)>>,
    /// 4 KiB pages of mapped tiles the title has not committed (a tile mapped for part of it,
    /// a page decommitted inside one): no memory to read there
    holes: HashSet<usize>,
}

const PAGE: usize = 0x1000;
const CHUNK: usize = 64;
const CHUNKS: usize = PAGE / CHUNK;

/// hash of 64 bytes
fn chunk_hash(c: &[u8]) -> u64 {
    let mut h = 0x243F_6A88_85A3_08D3u64;
    for w in c.chunks_exact(8) {
        h = (h ^ u64::from_le_bytes(w.try_into().unwrap())).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29);
    }
    h
}

fn zero_hashes() -> [u64; CHUNKS] {
    [chunk_hash(&[0u8; CHUNK]); CHUNKS]
}

/// The byte ranges (offset, length) of `page` whose 64-byte chunks differ from `sent`
/// (updated to the page's chunks).
pub fn changed_chunks(page: &[u8], sent: &mut [u64; CHUNKS]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (k, c) in page.chunks_exact(CHUNK).enumerate() {
        let h = chunk_hash(c);
        if h == sent[k] {
            continue;
        }
        sent[k] = h;
        match out.last_mut() {
            Some(last) if last.0 + last.1 == k * CHUNK => last.1 += CHUNK,
            _ => out.push((k * CHUNK, CHUNK)),
        }
    }
    out
}

// D3D12 objects are free-threaded; the staging pointer is only used under the lock
unsafe impl Send for State {}

static STATE: Mutex<Option<State>> = Mutex::new(None);
/// commits before the device existed
static EARLY: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

fn buffer_desc(bytes: u64) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    }
}

/// The sparse windows' buffers on `dev`: `(window_index, buffer_raw, gpu_va)` for each
/// sparse window in `gpu::uma` (empty: no sparse windows).
pub unsafe fn init(dev: &ID3D12Device) -> Vec<(usize, usize, u64)> {
    let Some(u) = crate::gpu::uma::get() else { return Vec::new() };
    let mut out = Vec::new();
    let mut windows = Vec::new();
    for k in 0..u.shader_windows() {
        if !u.is_sparse_window(k) {
            continue;
        }
        let (base, size) = u.window(k);
        let mut r: Option<ID3D12Resource> = None;
        if let Err(e) = dev.CreateReservedResource(&buffer_desc(size as u64), D3D12_RESOURCE_STATE_COMMON, None, &mut r) {
            crate::xlog_warn!("sparse window {k}: CreateReservedResource: {e}");
            return Vec::new();
        }
        let Some(r) = r else { return Vec::new() };
        out.push((k, r.clone().into_raw() as usize, r.GetGPUVirtualAddress()));
        windows.push((base, r));
    }
    if out.is_empty() {
        return out;
    }
    let made = (|| -> windows::core::Result<State> {
        let queue: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default())?;
        let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        let rb_fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
        let list: ID3D12GraphicsCommandList = dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None)?;
        list.Close()?;
        Ok(State {
            dev: dev.clone(),
            queue,
            fence,
            value: 0,
            rb_fence,
            rb_value: 0,
            windows,
            pools: Vec::new(),
            free: Vec::new(),
            mapped: HashMap::new(),
            pinned: HashMap::new(),
            buffers: HashMap::new(),
            to_zero: Vec::new(),
            staging: None,
            readback: None,
            alloc,
            list,
            waited: HashMap::new(),
            sent: HashMap::new(),
            runs: None,
            holes: HashSet::new(),
        })
    })();
    match made {
        Ok(s) => {
            crate::xlog_info!("D3D12.X: {} sparse window(s) for the title's own memory (tiled buffers on video memory)", out.len());
            *STATE.lock().unwrap() = Some(s);
            for (a, n) in std::mem::take(&mut *EARLY.lock().unwrap()) {
                committed(a, n);
            }
            out
        }
        Err(e) => {
            crate::xlog_warn!("sparse windows: {e}");
            Vec::new()
        }
    }
}

impl State {
    fn window(&self, a: usize) -> Option<(usize, &ID3D12Resource)> {
        self.windows.iter().find(|(b, _)| a >= *b && a < *b + crate::gpu::uma::WINDOW).map(|(b, r)| (*b, r))
    }

    unsafe fn take_tile(&mut self) -> Option<((u32, u32), bool)> {
        if let Some(t) = self.free.pop() {
            return Some((t, true));
        }
        let desc = D3D12_HEAP_DESC {
            SizeInBytes: POOL_TILES as u64 * TILE as u64,
            Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
            Alignment: 0,
            Flags: D3D12_HEAP_FLAG_DENY_RT_DS_TEXTURES | D3D12_HEAP_FLAG_DENY_NON_RT_DS_TEXTURES,
        };
        let mut heap: Option<ID3D12Heap> = None;
        if let Err(e) = super::title_textures::own_work(|| self.dev.CreateHeap(&desc, &mut heap)) {
            crate::xlog_warn!("sparse windows: no video memory for more tiles ({e})");
            return None;
        }
        let h = self.pools.len() as u32;
        self.pools.push(heap?);
        self.free.extend((1..POOL_TILES).rev().map(|t| (h, t)));
        Some(((h, 0), false))
    }

    /// map `tiles` (tile addresses) of `res` (starting at `res_base`) onto pool tiles
    unsafe fn map_tiles(&self, res: &ID3D12Resource, res_base: usize, tiles: &[(usize, (u32, u32))]) {
        for &(a, (h, t)) in tiles {
            let start = D3D12_TILED_RESOURCE_COORDINATE { X: ((a - res_base) / TILE) as u32, ..Default::default() };
            let region = D3D12_TILE_REGION_SIZE { NumTiles: 1, UseBox: false.into(), ..Default::default() };
            let (flags, n) = (D3D12_TILE_RANGE_FLAG_NONE, 1u32);
            self.queue.UpdateTileMappings(res, 1, Some(&start), Some(&region), &self.pools[h as usize], 1, Some(&flags), Some(&t), Some(&n), D3D12_TILE_MAPPING_FLAG_NONE);
        }
    }

    unsafe fn unmap_tiles(&self, res: &ID3D12Resource, res_base: usize, tiles: &[usize]) {
        for &a in tiles {
            let start = D3D12_TILED_RESOURCE_COORDINATE { X: ((a - res_base) / TILE) as u32, ..Default::default() };
            let region = D3D12_TILE_REGION_SIZE { NumTiles: 1, UseBox: false.into(), ..Default::default() };
            let flags = D3D12_TILE_RANGE_FLAG_NULL;
            self.queue.UpdateTileMappings(res, 1, Some(&start), Some(&region), None, 1, Some(&flags), None, None, D3D12_TILE_MAPPING_FLAG_NONE);
        }
    }

    /// tiles of `[a, a + n)` get video memory (mapped in their window)
    unsafe fn ensure_mapped(&mut self, a: usize, n: usize) {
        let first = a & !(TILE - 1);
        let mut new: Vec<(usize, (u32, u32))> = Vec::new();
        let mut t = first;
        while t < a + n {
            if !self.mapped.contains_key(&t) && self.window(t).is_some() {
                let Some((tile, reused)) = self.take_tile() else { break };
                self.mapped.insert(t, tile);
                self.runs = None;
                if reused {
                    self.to_zero.push((t, TILE));
                }
                new.push((t, tile));
            }
            t += TILE;
        }
        // per window
        let mut by_window: HashMap<usize, Vec<(usize, (u32, u32))>> = HashMap::new();
        for x in new {
            if let Some((b, _)) = self.window(x.0) {
                by_window.entry(b).or_default().push(x);
            }
        }
        for (b, tiles) in by_window {
            if let Some((_, r)) = self.window(b) {
                let r = r.clone();
                self.map_tiles(&r, b, &tiles);
            }
        }
        self.signal();
    }

    unsafe fn signal(&mut self) {
        self.value += 1;
        let _ = self.queue.Signal(&self.fence, self.value);
    }

    unsafe fn wait_idle(&self) {
        if self.fence.GetCompletedValue() < self.value {
            if let Ok(ev) = windows::Win32::System::Threading::CreateEventW(None, false, false, None) {
                let _ = self.fence.SetEventOnCompletion(self.value, ev);
                windows::Win32::System::Threading::WaitForSingleObject(ev, 5000);
                let _ = windows::Win32::Foundation::CloseHandle(ev);
            }
        }
    }

    unsafe fn staging(&mut self) -> Option<(ID3D12Resource, *mut u8)> {
        if self.staging.is_none() {
            let mut r: Option<ID3D12Resource> = None;
            let mut d = buffer_desc(STAGING as u64);
            d.Flags = D3D12_RESOURCE_FLAG_NONE;
            self.dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_UPLOAD, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &d, D3D12_RESOURCE_STATE_GENERIC_READ, None, &mut r).ok()?;
            let r = r?;
            let mut p = std::ptr::null_mut();
            r.Map(0, None, Some(&mut p)).ok()?;
            self.staging = Some((r, p as *mut u8));
        }
        self.staging.clone()
    }

    unsafe fn readback_buf(&mut self) -> Option<(ID3D12Resource, *mut u8)> {
        if self.readback.is_none() {
            let mut r: Option<ID3D12Resource> = None;
            let mut d = buffer_desc(READBACK_STAGING as u64);
            d.Flags = D3D12_RESOURCE_FLAG_NONE;
            self.dev
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() },
                    D3D12_HEAP_FLAG_NONE,
                    &d,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut r,
                )
                .ok()?;
            let r = r?;
            let mut p = std::ptr::null_mut();
            r.Map(0, None, Some(&mut p)).ok()?;
            self.readback = Some((r, p as *mut u8));
        }
        self.readback.clone()
    }

    /// Update `sent` hashes for all 64-byte chunks overlapping `[va, va + len)` so the CPU
    /// copy-back we just wrote to `va` is not re-uploaded by the next `before_execute`.
    unsafe fn sync_sent_range(&mut self, va: usize, len: usize) {
        if len == 0 {
            return;
        }
        let mut pg = va & !(PAGE - 1);
        let end = va + len;
        while pg < end {
            if !self.holes.contains(&pg) {
                let bytes = std::slice::from_raw_parts(pg as *const u8, PAGE);
                let sent = self.sent.entry(pg).or_insert_with(|| Box::new(zero_hashes()));
                let lo = va.saturating_sub(pg) / CHUNK;
                let hi = (end.min(pg + PAGE) - pg).div_ceil(CHUNK).min(CHUNKS);
                for k in lo..hi {
                    sent[k] = chunk_hash(&bytes[k * CHUNK..(k + 1) * CHUNK]);
                }
            }
            pg += PAGE;
        }
    }

    /// Copy `ranges` (address, bytes) from sparse VRAM back into the CPU view and refresh
    /// `sent` hashes. Caller has already ordered `self.queue` after any GPU writes if needed.
    unsafe fn readback_ranges(&mut self, ranges: &[(usize, usize)]) {
        let Some((rb, p)) = self.readback_buf() else { return };
        let mut pending: Vec<(usize, usize, usize)> = Vec::new(); // (va, rb_off, chunk)
        let mut used = 0usize;
        let mut open = false;
        let flush_and_copy = |s: &mut State, open: &mut bool, pending: &mut Vec<(usize, usize, usize)>, used: &mut usize, p: *mut u8| {
            s.flush(open);
            for &(dst, off, len) in pending.iter() {
                // copy only committed (non-hole) bytes
                let mut at = dst;
                let mut src_off = off;
                let end = dst + len;
                while at < end {
                    let pg = at & !(PAGE - 1);
                    let step = (pg + PAGE).min(end) - at;
                    if !s.holes.contains(&pg) {
                        std::ptr::copy_nonoverlapping(p.add(src_off), at as *mut u8, step);
                    }
                    at += step;
                    src_off += step;
                }
                s.sync_sent_range(dst, len);
            }
            pending.clear();
            *used = 0;
        };
        for &(a, n) in ranges {
            if n == 0 {
                continue;
            }
            let mut done = 0usize;
            while done < n {
                let cur = a + done;
                let tile = cur & !(TILE - 1);
                let Some((b, r)) = self.window(cur).map(|(b, r)| (b, r.clone())) else { break };
                let to_tile_end = (tile + TILE) - cur;
                if !self.mapped.contains_key(&tile) {
                    done += (n - done).min(to_tile_end);
                    continue;
                }
                if used == READBACK_STAGING {
                    flush_and_copy(self, &mut open, &mut pending, &mut used, p);
                }
                let to_win_end = (b + crate::gpu::uma::WINDOW) - cur;
                let chunk = (n - done).min(READBACK_STAGING - used).min(to_tile_end).min(to_win_end);
                if !open {
                    self.wait_idle();
                    let _ = self.alloc.Reset();
                    let _ = self.list.Reset(&self.alloc, None);
                    open = true;
                }
                self.list.CopyBufferRegion(&rb, used as u64, &r, (cur - b) as u64, chunk as u64);
                pending.push((cur, used, chunk));
                used += chunk;
                done += chunk;
            }
        }
        if open {
            flush_and_copy(self, &mut open, &mut pending, &mut used, p);
        }
    }

    /// copy `runs` (address, bytes) of the CPU's view into the tiles (`zero`: clear instead)
    unsafe fn upload(&mut self, runs: &[(usize, usize, bool)]) {
        let Some((staging, p)) = self.staging() else { return };
        let mut used = 0usize;
        let mut open = false;
        for &(a, n, zero) in runs {
            let mut done = 0;
            while done < n {
                if used == STAGING {
                    self.flush(&mut open);
                    used = 0;
                }
                let Some((b, r)) = self.window(a + done).map(|(b, r)| (b, r.clone())) else { break };
                let to_win_end = (b + crate::gpu::uma::WINDOW) - (a + done);
                let chunk = (n - done).min(STAGING - used).min(to_win_end);
                if !open {
                    self.wait_idle();
                    let _ = self.alloc.Reset();
                    let _ = self.list.Reset(&self.alloc, None);
                    open = true;
                }
                if zero {
                    std::ptr::write_bytes(p.add(used), 0, chunk);
                } else {
                    std::ptr::copy_nonoverlapping((a + done) as *const u8, p.add(used), chunk);
                }
                self.list.CopyBufferRegion(&r, (a + done - b) as u64, &staging, used as u64, chunk as u64);
                used += chunk;
                done += chunk;
            }
        }
        if open {
            self.flush(&mut open);
        }
    }

    unsafe fn flush(&mut self, open: &mut bool) {
        if !*open {
            return;
        }
        *open = false;
        if self.list.Close().is_ok() {
            if let Ok(l) = self.list.cast::<ID3D12CommandList>() {
                super::title_textures::own_work(|| {
                    self.queue.ExecuteCommandLists(&[Some(l)]);
                });
            }
        }
        self.signal();
        // the staging buffer is written again only after this
        self.wait_idle();
    }
}

/// Allocate a zeroed, write-watched block in the sparse VRAM windows (never straddling a
/// 4 GiB window boundary when `size <= 4 GiB`). Commits the CPU pages and maps VRAM tiles
/// (or queues them until the D3D12 device exists). Returns `(address, rounded_size)`.
pub unsafe fn alloc(size: usize, align: usize) -> Option<(usize, usize)> {
    let u = crate::gpu::uma::get()?;
    let (addr, rounded) = u.sparse_alloc(size, align)?;
    if VirtualAlloc(addr as *const c_void, rounded, MEM_COMMIT, PAGE_READWRITE).is_null() {
        u.sparse_release(addr);
        return None;
    }
    committed(addr, rounded);
    Some((addr, rounded))
}

/// Allocate a zeroed, write-watched block strictly inside sparse window `window` (e.g.
/// window 0 for 32-bit-addressed descriptor heaps). Returns `(address, rounded_size)`.
pub unsafe fn alloc_in(window: usize, size: usize, align: usize) -> Option<(usize, usize)> {
    let u = crate::gpu::uma::get()?;
    let (addr, rounded) = u.sparse_alloc_in(window, size, align)?;
    if VirtualAlloc(addr as *const c_void, rounded, MEM_COMMIT, PAGE_READWRITE).is_null() {
        u.sparse_release(addr);
        return None;
    }
    committed(addr, rounded);
    Some((addr, rounded))
}

/// Release a sparse allocation from `alloc` / `alloc_in` (false: not a sparse allocation).
pub unsafe fn free(p: usize) -> bool {
    let Some(u) = crate::gpu::uma::get() else { return false };
    if !u.is_sparse(p) {
        return false;
    }
    let Some(len) = u.sparse_release(p) else { return false };
    decommitted(p, len);
    VirtualFree(p as *mut c_void, len, MEM_DECOMMIT);
    true
}

/// After `queue.ExecuteCommandLists`: copy GPU-written sparse ranges (e.g. `Write32/64`,
/// `FillMemoryWith32BitValueX`, `CopyMemoryToMemoryX`) back to their CPU view so CPU reads
/// and spin-waits see the GPU writes without clobbering `sent` hashes.
pub unsafe fn readback_after_execute(queue: &ID3D12CommandQueue, ranges: &[(usize, usize)]) {
    if ranges.is_empty() {
        return;
    }
    let mut g = STATE.lock().unwrap();
    let Some(s) = g.as_mut() else { return };
    let mut filtered: Vec<(usize, usize)> = ranges.iter().copied().filter(|&(a, n)| n > 0 && s.window(a).is_some()).collect();
    if filtered.is_empty() {
        return;
    }
    filtered.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(filtered.len());
    for (a, n) in filtered {
        match merged.last_mut() {
            Some(last) if a <= last.0.saturating_add(last.1) => {
                let end = (last.0 + last.1).max(a + n);
                last.1 = end - last.0;
            }
            _ => merged.push((a, n)),
        }
    }
    s.rb_value += 1;
    let v = s.rb_value;
    if queue.Signal(&s.rb_fence, v).is_ok() {
        let _ = s.queue.Wait(&s.rb_fence, v);
    }
    s.readback_ranges(&merged);
}

/// Synchronize `[va, va + len)` in a sparse window for a CPU read (`ID3D12Resource::Map` with
/// a non-empty read range): flushes pending tile clears and unsubmitted CPU-changed chunks
/// first so CPU writes are preserved, then copies the range back from VRAM into CPU memory.
pub unsafe fn readback_now(va: usize, len: usize) {
    if len == 0 {
        return;
    }
    let mut g = STATE.lock().unwrap();
    let Some(s) = g.as_mut() else { return };
    if s.window(va).is_none() {
        return;
    }
    let zeroed = std::mem::take(&mut s.to_zero);
    if !zeroed.is_empty() {
        for &(a, n) in &zeroed {
            for pg in (a..a + n).step_by(PAGE) {
                s.sent.remove(&pg);
            }
        }
        let zruns: Vec<(usize, usize, bool)> = zeroed.into_iter().map(|(a, n)| (a, n, true)).collect();
        s.upload(&zruns);
    }
    // Preserve any unsubmitted CPU writes on pages overlapping `[va, va + len)`
    let mut dirty_runs: Vec<(usize, usize, bool)> = Vec::new();
    let mut pg = va & !(PAGE - 1);
    let end = va + len;
    while pg < end {
        let tile = pg & !(TILE - 1);
        if s.mapped.contains_key(&tile) && !s.holes.contains(&pg) {
            let bytes = std::slice::from_raw_parts(pg as *const u8, PAGE);
            let sent = s.sent.entry(pg).or_insert_with(|| Box::new(zero_hashes()));
            for (o, n) in changed_chunks(bytes, sent) {
                match dirty_runs.last_mut() {
                    Some(last) if last.0 + last.1 == pg + o => last.1 += n,
                    _ => dirty_runs.push((pg + o, n, false)),
                }
            }
        }
        pg += PAGE;
    }
    if !dirty_runs.is_empty() {
        s.upload(&dirty_runs);
    }
    s.readback_ranges(&[(va, len)]);
}

/// The title committed `[a, a + n)` in a sparse window. The pages of tiles mapped for it that
/// lie outside the range are not committed (holes); the range's pages are.
pub unsafe fn committed(a: usize, n: usize) {
    let mut g = STATE.lock().unwrap();
    let Some(s) = g.as_mut() else {
        EARLY.lock().unwrap().push((a, n));
        return;
    };
    let tiles: Vec<usize> = (a & !(TILE - 1)..a + n).step_by(TILE).filter(|t| !s.mapped.contains_key(t)).collect();
    s.ensure_mapped(a, n);
    for t in tiles.into_iter().filter(|t| s.mapped.contains_key(t)) {
        s.holes.extend((t..t + TILE).step_by(PAGE).filter(|&pg| pg + PAGE <= a || pg >= a + n));
    }
    for pg in (a & !(PAGE - 1)..a + n).step_by(PAGE) {
        s.holes.remove(&pg);
    }
}

/// The title decommitted `[a, a + n)`: its whole tiles give their video memory back (unless a
/// placed buffer still uses it).
pub unsafe fn decommitted(a: usize, n: usize) {
    let mut g = STATE.lock().unwrap();
    let Some(s) = g.as_mut() else {
        EARLY.lock().unwrap().retain(|&(x, m)| x + m <= a || x >= a + n);
        return;
    };
    let first = (a + TILE - 1) & !(TILE - 1);
    let tiles: Vec<usize> = (first..(a + n) & !(TILE - 1)).step_by(TILE).filter(|t| s.mapped.contains_key(t)).collect();
    let mut by_window: HashMap<usize, Vec<usize>> = HashMap::new();
    for &t in &tiles {
        if let Some((b, _)) = s.window(t) {
            by_window.entry(b).or_default().push(t);
        }
    }
    for (b, ts) in by_window {
        if let Some((_, r)) = s.window(b) {
            let r = r.clone();
            s.unmap_tiles(&r, b, &ts);
        }
    }
    // pages of tiles that stay mapped: holes, their video memory cleared (sent: absent = zero;
    // the title finds zeros there when it commits them again)
    for pg in (a & !(PAGE - 1)..a + n).step_by(PAGE) {
        s.sent.remove(&pg);
        if s.mapped.contains_key(&(pg & !(TILE - 1))) && !tiles.contains(&(pg & !(TILE - 1))) && s.holes.insert(pg) {
            s.to_zero.push((pg, PAGE));
        }
    }
    for &t in &tiles {
        for pg in (t..t + TILE).step_by(PAGE) {
            s.holes.remove(&pg);
        }
    }
    for t in tiles {
        if let Some(tile) = s.mapped.remove(&t) {
            s.runs = None;
            if !s.pinned.contains_key(&tile) {
                s.free.push(tile);
            }
        }
    }
    s.signal();
}

impl State {
    /// a pool tile no window maps and no buffer uses goes back to the free list
    fn release_if_unused(&mut self, tile: (u32, u32)) {
        if !self.pinned.contains_key(&tile) && !self.mapped.values().any(|&x| x == tile) {
            self.free.push(tile);
        }
    }
}

/// The sparse windows' tiles with video memory as sorted (start, end) runs: where a CPU write
/// has to reach the GPU.
pub fn mapped_ranges() -> Vec<(usize, usize)> {
    let mut g = STATE.lock().unwrap();
    let Some(s) = g.as_mut() else { return Vec::new() };
    if s.runs.is_none() {
        let mut tiles: Vec<usize> = s.mapped.keys().copied().collect();
        tiles.sort_unstable();
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for t in tiles {
            match runs.last_mut() {
                Some(last) if last.1 == t => last.1 = t + TILE,
                _ => runs.push((t, t + TILE)),
            }
        }
        s.runs = Some(runs);
    }
    s.runs.clone().unwrap_or_default()
}

/// time spent and pages found by the write-watch scans (`refresh_once`), for the timing line
static WATCH: [std::sync::atomic::AtomicU64; 2] = [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];
/// [lock wait, chunk scan, upload] in µs, runs uploaded
static PHASES: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

pub fn note_watch(t: std::time::Duration, pages: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    WATCH[0].fetch_add(t.as_micros() as u64, Relaxed);
    WATCH[1].fetch_add(pages as u64, Relaxed);
}

/// Before the title's ExecuteCommandLists on `queue`: the pages the CPU wrote (`dirty`, 4 KiB
/// pages from the write-watch, sorted) and the reused tiles are copied into video memory, and
/// `queue` waits for that and for new tile mappings.
pub unsafe fn before_execute(queue: &ID3D12CommandQueue, dirty: &[usize], page: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    let t0 = std::time::Instant::now();
    let mut g = STATE.lock().unwrap();
    let Some(s) = g.as_mut() else { return };
    let t_locked = std::time::Instant::now();
    PHASES[0].fetch_add((t_locked - t0).as_micros() as u64, Relaxed);
    let zeroed = std::mem::take(&mut s.to_zero);
    for &(a, n) in &zeroed {
        for pg in (a..a + n).step_by(PAGE) {
            s.sent.remove(&pg);
        }
    }
    let zero_runs: Vec<(usize, usize, bool)> = zeroed.into_iter().map(|(a, n)| (a, n, true)).collect();
    let mut dirty_runs: Vec<(usize, usize, bool)> = Vec::new();
    let mut i = 0;
    while i < dirty.len() {
        let a = dirty[i];
        let mut n = page;
        i += 1;
        while i < dirty.len() && dirty[i] == a + n {
            n += page;
            i += 1;
        }
        // only what has video memory (committed tiles of a sparse window)
        let mut x = a;
        while x < a + n {
            let t = x & !(TILE - 1);
            let end = (t + TILE).min(a + n);
            if s.mapped.contains_key(&t) {
                // per page: only the chunks the CPU changed since they were sent
                let mut pg = x & !(PAGE - 1);
                while pg < end {
                    if s.holes.contains(&pg) {
                        pg += PAGE;
                        continue;
                    }
                    let bytes = std::slice::from_raw_parts(pg as *const u8, PAGE);
                    let sent = s.sent.entry(pg).or_insert_with(|| Box::new(zero_hashes()));
                    for (o, n) in changed_chunks(bytes, sent) {
                        match dirty_runs.last_mut() {
                            Some(last) if last.0 + last.1 == pg + o => last.1 += n,
                            _ => dirty_runs.push((pg + o, n, false)),
                        }
                    }
                    pg += PAGE;
                }
            }
            x = end;
        }
    }
    let t_scanned = std::time::Instant::now();
    PHASES[1].fetch_add((t_scanned - t_locked).as_micros() as u64, Relaxed);
    PHASES[3].fetch_add((zero_runs.len() + dirty_runs.len()) as u64, Relaxed);
    // Flush tile clears before CPU writes so overlapping CopyBufferRegion destinations have no WAW hazard.
    if !zero_runs.is_empty() {
        s.upload(&zero_runs);
    }
    if !dirty_runs.is_empty() {
        s.upload(&dirty_runs);
    }
    PHASES[2].fetch_add(t_scanned.elapsed().as_micros() as u64, Relaxed);
    {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        static CALLS: AtomicU64 = AtomicU64::new(0);
        static BYTES: AtomicU64 = AtomicU64::new(0);
        static MICROS: AtomicU64 = AtomicU64::new(0);
        let bytes: usize = zero_runs.iter().chain(dirty_runs.iter()).map(|r| r.1).sum();
        let n = CALLS.fetch_add(1, Relaxed) + 1;
        let b = BYTES.fetch_add(bytes as u64, Relaxed) + bytes as u64;
        let us = MICROS.fetch_add(t0.elapsed().as_micros() as u64, Relaxed) + t0.elapsed().as_micros() as u64;
        if n.is_power_of_two() && n >= 16 {
            crate::xlog_info!("sparse windows: {n} submits, {} MB of CPU writes uploaded, {} ms spent, {} tiles mapped", b >> 20, us / 1000, s.mapped.len());
            let ms = |x: &std::sync::atomic::AtomicU64| x.load(Relaxed) / 1000;
            crate::xlog_info!(
                "sparse windows timing: write-watch {} ms ({} dirty pages), lock wait {} ms, chunk scan {} ms, upload {} ms ({} runs)",
                ms(&WATCH[0]),
                WATCH[1].load(Relaxed),
                ms(&PHASES[0]),
                ms(&PHASES[1]),
                ms(&PHASES[2]),
                PHASES[3].load(Relaxed)
            );
        }
    }
    let q = queue.as_raw() as usize;
    if s.value > 0 && s.waited.get(&q).copied().unwrap_or(0) < s.value {
        let _ = queue.Wait(&s.fence, s.value);
        s.waited.insert(q, s.value);
    }
}

/// A buffer the title places at `va` in a sparse window (`bytes` long, `desc` the PC
/// description): a reserved resource over the window's tiles there, starting at the 64 KiB
/// below `va` (returned: its distance from there).
pub unsafe fn place_buffer(this: *mut c_void, va: usize, desc: &D3D12_RESOURCE_DESC, state: u32, riid: *const windows_sys::core::GUID, ppv: *mut *mut c_void) -> Option<(crate::util::HRESULT, usize)> {
    let mut g = STATE.lock().unwrap();
    let s = g.as_mut()?;
    let start = va & !(TILE - 1);
    let delta = va - start;
    let bytes = desc.Width as usize;
    s.window(start)?;
    s.window(va + bytes - 1)?;
    let mut d = *desc;
    d.Width = (delta + bytes) as u64;
    d.Alignment = 0;
    d.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    // Reserved buffers only accept NONE or ALLOW_UNORDERED_ACCESS (strip console flags and ALLOW_CROSS_ADAPTER).
    d.Flags &= D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
    if state & (D3D12_RESOURCE_STATE_UNORDERED_ACCESS.0 as u32 | D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE.0 as u32) != 0 {
        d.Flags |= D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
    }
    let hr = super::real::dev_create_reserved_resource(this, &d as *const _ as usize, state as usize, 0, riid, ppv);
    if hr < 0 || ppv.is_null() || (*ppv).is_null() {
        return None;
    }
    // its tiles: the window's video memory at the same addresses
    s.ensure_mapped(start, delta + bytes);
    let tiles: Vec<(usize, (u32, u32))> = (start..va + bytes).step_by(TILE).filter_map(|t| s.mapped.get(&t).map(|&x| (t, x))).collect();
    let raw = *ppv;
    let res = ID3D12Resource::from_raw_borrowed(&raw)?.clone();
    s.map_tiles(&res, start, &tiles);
    for &(_, tile) in &tiles {
        *s.pinned.entry(tile).or_default() += 1;
    }
    s.buffers.insert(raw as usize, tiles.iter().map(|x| x.1).collect());
    s.signal();
    Some((hr, delta))
}

/// A placed buffer is gone: its tiles are not pinned by it any more.
pub fn forget_buffer(res: usize) {
    let mut g = STATE.lock().unwrap();
    let Some(s) = g.as_mut() else { return };
    let Some(tiles) = s.buffers.remove(&res) else { return };
    for tile in tiles {
        if let Some(c) = s.pinned.get_mut(&tile) {
            *c -= 1;
            if *c == 0 {
                s.pinned.remove(&tile);
                s.release_if_unused(tile);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the 64-byte chunks the CPU changed are sent again – the rest of the page may hold
    /// what the GPU wrote.
    #[test]
    fn only_changed_chunks_are_sent() {
        let mut page = vec![0u8; PAGE];
        let mut sent = zero_hashes();
        assert!(changed_chunks(&page, &mut sent).is_empty(), "a zero page is what the GPU has");
        page[10] = 1;
        page[200] = 2;
        page[255] = 3;
        page[4000] = 4;
        assert_eq!(changed_chunks(&page, &mut sent), vec![(0, 64), (192, 64), (3968, 64)]);
        assert!(changed_chunks(&page, &mut sent).is_empty(), "nothing changed since");
        page[256] = 5; // next to the chunk at 192: one run
        page[200] = 9;
        assert_eq!(changed_chunks(&page, &mut sent), vec![(192, 128)]);
    }
}
