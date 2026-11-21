//! Console textures from console memory → PC textures. On the console a texture is the
//! memory a T# points at: the title writes it already tiled (GFX10 swizzle, mip chain and
//! slices as addrlib lays them out – its archives store textures that way) and samples it
//! directly. A PC texture has an opaque layout, so every mip level of every slice is detiled
//! (`tiling::Chain`) into a D3D12 subresource and copied in.
//!
//! 2D textures, arrays and cubes, 3D textures (thick and thin modes) and linear ones. What is
//! not here yet: MSAA, the console's pipe configuration for the _X modes (`CONSOLE_PIPES`),
//! and the binding to the console API that creates textures in title memory (and tells when
//! the title rewrote one) – that comes with the game executable.

use super::desc::{self, Image, ImageType};
use super::tiling::{self, Chain, PipeConfig};
use windows::Win32::Graphics::Direct3D12::*;

/// Pipes / packers of the console's GB_ADDR_CONFIG – only the XOR (_X) modes depend on it.
/// Not known yet (to be found on an _X texture from the game); the non-XOR modes the
/// archives use do not need it.
pub const CONSOLE_PIPES: PipeConfig = PipeConfig { pipes_log2: 0, pkrs_log2: 0 };

/// Linear data of one subresource: `depth` slices (3D, else 1) of `rows` rows of `row_bytes`
/// (elements: pixels or 4×4 blocks).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subresource {
    pub rows: usize,
    pub row_bytes: usize,
    pub depth: usize,
    pub data: Vec<u8>,
}

/// Where and how a T#'s texture lies in console memory.
pub struct Placement {
    /// first byte (the T# address rounded down to a block; the rest is the base swizzle)
    pub start: u64,
    pub bytes: usize,
    pub chain: Chain,
    /// array slices (cube faces), or the slices of a 3D texture (`Chain::layers`)
    pub slices: usize,
}

pub fn placement(t: &Image, cfg: PipeConfig) -> Result<Placement, String> {
    if matches!(t.ty, ImageType::D2Msaa | ImageType::D2MsaaArray) {
        return Err(format!("texture {:?} – not yet (MSAA)", t.ty));
    }
    let chain = tiling::image_chain(t, cfg).ok_or_else(|| format!("SW_MODE {} / format {} / {:?} without a tiling pattern", t.swizzle_mode, t.format, t.ty))?;
    let slices = match t.ty {
        ImageType::D1Array | ImageType::D2Array | ImageType::Cube => t.depth.max(1) as usize,
        ImageType::D3 => chain.layers(),
        _ => 1,
    };
    let start = t.address & !(chain.layout.block_bytes as u64 - 1);
    Ok(Placement { start, bytes: chain.slice_bytes * slices, chain, slices })
}

/// The subresources of `t` in D3D12 order (mip + slice × mips) from `mem`, its bytes from
/// `Placement::start` on.
pub fn subresources(t: &Image, mem: &[u8], cfg: PipeConfig) -> Result<Vec<Subresource>, String> {
    let p = placement(t, cfg)?;
    if mem.len() < p.bytes {
        return Err(format!("{} B of memory, but the texture takes {} B", mem.len(), p.bytes));
    }
    let e = 1usize << p.chain.layout.elem_log2;
    if matches!(t.ty, ImageType::D3) {
        // one subresource per level, all its depth slices
        return Ok(p.chain.levels.iter().enumerate().map(|(i, l)| Subresource { rows: l.height, row_bytes: l.width * e, depth: l.depth, data: p.chain.detile_volume(mem, i) }).collect());
    }
    let mut out = Vec::with_capacity(p.slices * p.chain.levels.len());
    for s in 0..p.slices {
        let slice = &mem[s * p.chain.slice_bytes..(s + 1) * p.chain.slice_bytes];
        for (i, l) in p.chain.levels.iter().enumerate() {
            out.push(Subresource { rows: l.height, row_bytes: l.width * e, depth: 1, data: p.chain.detile_level(slice, i) });
        }
    }
    Ok(out)
}

/// The console memory a T#'s texture occupies, when it lies in the unified memory (UMA).
pub fn uma_bytes(t: &Image, cfg: PipeConfig) -> Result<&'static [u8], String> {
    let p = placement(t, cfg)?;
    let u = super::uma::get().ok_or("no UMA")?;
    let (a, n) = (p.start as usize, p.bytes);
    if !u.contains(a) || !u.contains(a + n - 1) {
        return Err(format!("texture {a:#x}+{n:#x} outside UMA"));
    }
    // SAFETY: inside the UMA reservation, which lives for the whole process
    Ok(unsafe { std::slice::from_raw_parts(a as *const u8, n) })
}

fn err(x: windows::core::Error) -> String {
    format!("{x}")
}

/// A PC texture (default heap, left in the shader-resource states) with the console
/// texture's contents: `mem` from `Placement::start` on.
pub unsafe fn create(dev: &ID3D12Device, queue: &ID3D12CommandQueue, t: &Image, mem: &[u8], cfg: PipeConfig) -> Result<ID3D12Resource, String> {
    let subs = subresources(t, mem, cfg)?;
    let (rd, _) = desc::image_d3d12(t).ok_or_else(|| format!("format {} without a DXGI equivalent", t.format))?;
    let mut r: Option<ID3D12Resource> = None;
    dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &rd, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut r)
        .map_err(|x| format!("CreateCommittedResource: {x}"))?;
    let r = r.ok_or("CreateCommittedResource without a resource")?;
    upload(dev, queue, &r, &subs, D3D12_RESOURCE_STATE_COPY_DEST, SHADER_READ)?;
    Ok(r)
}

/// The states `create` leaves a texture in.
pub const SHADER_READ: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATES(D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE.0 | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE.0);

/// Copy `subs` (all subresources, D3D12 order) into `res` on `queue` and wait; `res` is in
/// `before` and ends in `after` (e.g. again after the title rewrote the memory).
pub unsafe fn upload(dev: &ID3D12Device, queue: &ID3D12CommandQueue, res: &ID3D12Resource, subs: &[Subresource], before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) -> Result<(), String> {
    let rd = res.GetDesc();
    let n = subs.len();
    let mut fps = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); n];
    let mut rows = vec![0u32; n];
    let mut total = 0u64;
    dev.GetCopyableFootprints(&rd, 0, n as u32, 0, Some(fps.as_mut_ptr()), Some(rows.as_mut_ptr()), None, Some(&mut total));
    let up = buffer(dev, total)?;
    let mut p = std::ptr::null_mut();
    up.Map(0, None, Some(&mut p)).map_err(err)?;
    for ((s, fp), &nrows) in subs.iter().zip(&fps).zip(&rows) {
        let pitch = fp.Footprint.RowPitch as usize;
        let row = s.row_bytes.min(pitch);
        // depth slices are NumRows rows apart (rows of 4×4 blocks for BCn)
        let slice_pitch = pitch * nrows as usize;
        for z in 0..(fp.Footprint.Depth as usize).min(s.depth) {
            for y in 0..(nrows as usize).min(s.rows) {
                let at = fp.Offset as usize + z * slice_pitch + y * pitch;
                std::ptr::copy_nonoverlapping(s.data[(z * s.rows + y) * s.row_bytes..].as_ptr(), (p as *mut u8).add(at), row);
            }
        }
    }
    up.Unmap(0, None);
    let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).map_err(err)?;
    let list: ID3D12GraphicsCommandList = dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).map_err(err)?;
    if before != D3D12_RESOURCE_STATE_COPY_DEST {
        list.ResourceBarrier(&[transition(res, before, D3D12_RESOURCE_STATE_COPY_DEST)]);
    }
    for (k, fp) in fps.iter().enumerate() {
        let dst = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(res),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: k as u32 },
        };
        let src = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&up), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *fp } };
        list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
    }
    if after != D3D12_RESOURCE_STATE_COPY_DEST {
        list.ResourceBarrier(&[transition(res, D3D12_RESOURCE_STATE_COPY_DEST, after)]);
    }
    list.Close().map_err(err)?;
    queue.ExecuteCommandLists(&[Some(windows::core::Interface::cast(&list).map_err(err)?)]);
    wait(dev, queue)
}

/// Wait until `queue` has run everything submitted so far.
pub unsafe fn wait(dev: &ID3D12Device, queue: &ID3D12CommandQueue) -> Result<(), String> {
    let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(err)?;
    queue.Signal(&fence, 1).map_err(err)?;
    let ev = windows::Win32::System::Threading::CreateEventW(None, false, false, None).map_err(err)?;
    fence.SetEventOnCompletion(1, ev).map_err(err)?;
    windows::Win32::System::Threading::WaitForSingleObject(ev, 10_000);
    let _ = windows::Win32::Foundation::CloseHandle(ev);
    if fence.GetCompletedValue() < 1 {
        return Err(format!("GPU did not finish the texture copy within 10 s ({:?})", dev.GetDeviceRemovedReason()));
    }
    Ok(())
}

unsafe fn buffer(dev: &ID3D12Device, size: u64) -> Result<ID3D12Resource, String> {
    let rd = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size.max(256),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut r: Option<ID3D12Resource> = None;
    dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_UPLOAD, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &rd, D3D12_RESOURCE_STATE_GENERIC_READ, None, &mut r)
        .map_err(|x| format!("texture upload buffer: {x}"))?;
    r.ok_or_else(|| "texture upload buffer".into())
}

fn transition(r: &ID3D12Resource, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: unsafe { std::mem::transmute_copy(r) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use windows::Win32::Graphics::Dxgi::Common::*;

    fn image(format: u32, ty: ImageType, w: u32, h: u32, slices: u32, mips: u32, mode: u32, address: u64) -> Image {
        Image {
            address,
            format,
            width: w,
            height: h,
            depth: slices,
            ty,
            base_level: 0,
            last_level: mips - 1,
            max_mip: mips - 1,
            base_array: 0,
            dst_sel: [4, 5, 6, 7],
            swizzle_mode: mode,
            min_lod: 0.0,
        }
    }

    /// Random linear subresources for `t`, and the console memory they are, tiled.
    fn console_texture(t: &Image) -> (Vec<Subresource>, Vec<u8>) {
        let p = placement(t, CONSOLE_PIPES).unwrap();
        let e = 1usize << p.chain.layout.elem_log2;
        let mut mem = vec![0u8; p.bytes];
        let mut subs = Vec::new();
        let mut n = 0x1234_5678u32;
        let mut random = |len: usize| -> Vec<u8> {
            (0..len)
                .map(|_| {
                    n ^= n << 13;
                    n ^= n >> 17;
                    n ^= n << 5;
                    n as u8
                })
                .collect()
        };
        if matches!(t.ty, ImageType::D3) {
            for (i, l) in p.chain.levels.iter().enumerate() {
                let data = random(l.width * l.height * l.depth * e);
                p.chain.tile_volume(&mut mem, i, &data);
                subs.push(Subresource { rows: l.height, row_bytes: l.width * e, depth: l.depth, data });
            }
            return (subs, mem);
        }
        for s in 0..p.slices {
            let slice = &mut mem[s * p.chain.slice_bytes..(s + 1) * p.chain.slice_bytes];
            for (i, l) in p.chain.levels.iter().enumerate() {
                let data = random(l.width * l.height * e);
                p.chain.tile_level(slice, i, &data);
                subs.push(Subresource { rows: l.height, row_bytes: l.width * e, depth: 1, data });
            }
        }
        (subs, mem)
    }

    #[test]
    fn subresources_of_a_mipped_array_from_console_memory() {
        let rgba8 = desc::gfx10_format(DXGI_FORMAT_R8G8B8A8_UNORM).unwrap();
        let t = image(rgba8, ImageType::D2Array, 96, 40, 3, 6, 5, 0x10_0000 | 0x300);
        let (want, mem) = console_texture(&t);
        let p = placement(&t, CONSOLE_PIPES).unwrap();
        assert_eq!((p.start, p.slices, want.len()), (0x10_0000, 3, 18));
        assert_eq!(subresources(&t, &mem, CONSOLE_PIPES).unwrap(), want);
        assert!(subresources(&t, &mem[..mem.len() - 1], CONSOLE_PIPES).is_err());
    }

    fn device() -> Option<(ID3D12Device, ID3D12CommandQueue)> {
        unsafe {
            let mut dev: Option<ID3D12Device> = None;
            windows::Win32::Graphics::Direct3D12::D3D12CreateDevice(None, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0, &mut dev).ok()?;
            let dev = dev?;
            let queue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }).ok()?;
            Some((dev, queue))
        }
    }

    /// Every subresource of a PC texture, read back (rows of `row_bytes`).
    unsafe fn read_back(dev: &ID3D12Device, queue: &ID3D12CommandQueue, res: &ID3D12Resource, subs: &[Subresource]) -> Vec<Subresource> {
        let rd = res.GetDesc();
        let n = subs.len();
        let mut fps = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); n];
        let mut nrows = vec![0u32; n];
        let mut total = 0u64;
        dev.GetCopyableFootprints(&rd, 0, n as u32, 0, Some(fps.as_mut_ptr()), Some(nrows.as_mut_ptr()), None, Some(&mut total));
        let mut rb: Option<ID3D12Resource> = None;
        let bd = D3D12_RESOURCE_DESC { Dimension: D3D12_RESOURCE_DIMENSION_BUFFER, Width: total, Height: 1, DepthOrArraySize: 1, MipLevels: 1, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR, ..Default::default() };
        dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &bd, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut rb).unwrap();
        let rb = rb.unwrap();
        let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let list: ID3D12GraphicsCommandList = dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).unwrap();
        list.ResourceBarrier(&[transition(res, SHADER_READ, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
        for (k, fp) in fps.iter().enumerate() {
            let src = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(res), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: k as u32 } };
            let dst = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&rb), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *fp } };
            list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        }
        list.ResourceBarrier(&[transition(res, D3D12_RESOURCE_STATE_COPY_SOURCE, SHADER_READ)]);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(windows::core::Interface::cast(&list).unwrap())]);
        wait(dev, queue).unwrap();
        let mut p = std::ptr::null_mut();
        rb.Map(0, None, Some(&mut p)).unwrap();
        let out = subs
            .iter()
            .zip(fps.iter().zip(&nrows))
            .map(|(s, (fp, &nr))| {
                let pitch = fp.Footprint.RowPitch as usize;
                let mut data = Vec::with_capacity(s.depth * s.rows * s.row_bytes);
                for z in 0..s.depth {
                    for y in 0..s.rows {
                        data.extend_from_slice(std::slice::from_raw_parts((p as *const u8).add(fp.Offset as usize + (z * nr as usize + y) * pitch), s.row_bytes));
                    }
                }
                Subresource { rows: s.rows, row_bytes: s.row_bytes, depth: s.depth, data }
            })
            .collect();
        rb.Unmap(0, None);
        out
    }

    /// Tiled console memory → PC texture → read back = the linear data it was tiled from, for
    /// every mip and slice: an RGBA8 array with 4 KB tiles, a BC7 cube with 64 KB tiles, 3D
    /// textures (thick 4 KB / 64 KB, BC1 thick, linear) and a linear 2D texture.
    #[test]
    fn gpu_console_textures_upload_every_mip_and_slice() {
        let Some((dev, queue)) = device() else {
            println!("no D3D12 device – skipping");
            return;
        };
        let rgba8 = desc::gfx10_format(DXGI_FORMAT_R8G8B8A8_UNORM).unwrap();
        let bc7 = desc::gfx10_format(DXGI_FORMAT_BC7_UNORM).unwrap();
        let bc1 = desc::gfx10_format(DXGI_FORMAT_BC1_UNORM).unwrap();
        let rgba16f = desc::gfx10_format(DXGI_FORMAT_R16G16B16A16_FLOAT).unwrap();
        for t in [
            image(rgba8, ImageType::D2Array, 200, 72, 3, 8, 5, 0x40_0000),
            image(bc7, ImageType::Cube, 256, 256, 6, 9, 10, 0x80_0000),
            image(rgba8, ImageType::D3, 40, 24, 20, 6, 5, 0x100_0000),
            image(rgba16f, ImageType::D3, 64, 64, 64, 7, 9, 0x200_0000),
            image(bc1, ImageType::D3, 64, 32, 8, 4, 5, 0x300_0000),
            image(rgba8, ImageType::D3, 33, 17, 5, 3, 0, 0x400_0000),
            image(rgba8, ImageType::D2, 100, 30, 1, 7, 0, 0x500_0000),
        ] {
            let (want, mem) = console_texture(&t);
            unsafe {
                let res = create(&dev, &queue, &t, &mem, CONSOLE_PIPES).unwrap();
                let got = read_back(&dev, &queue, &res, &want);
                for (k, (g, w)) in got.iter().zip(&want).enumerate() {
                    assert!(g == w, "format {} {:?} mode {} subresource {k} ({} × {} rows × {} B) differs", t.format, t.ty, t.swizzle_mode, w.depth, w.rows, w.row_bytes);
                }
            }
        }
    }

    /// Mean |level `k` - level `k - 1` scaled down 2×| per channel (0..255 scale) of a texture
    /// with `ch` channels of `cb` bytes (unorm); None when the sizes do not halve.
    fn mip_error(big: &Subresource, small: &Subresource, ch: usize, cb: usize) -> Option<f64> {
        let e = ch * cb;
        let (bw, bh, sw, sh) = (big.row_bytes / e, big.rows, small.row_bytes / e, small.rows);
        if sw * 2 != bw || sh * 2 != bh {
            return None;
        }
        let get = |d: &[u8], w: usize, x: usize, y: usize, c: usize| -> u32 {
            let o = (y * w + x) * e + c * cb;
            if cb == 1 {
                d[o] as u32
            } else {
                u16::from_le_bytes([d[o], d[o + 1]]) as u32 >> 8
            }
        };
        let mut sum = 0u64;
        for y in 0..sh {
            for x in 0..sw {
                for c in 0..ch {
                    let avg = (get(&big.data, bw, 2 * x, 2 * y, c) + get(&big.data, bw, 2 * x + 1, 2 * y, c) + get(&big.data, bw, 2 * x, 2 * y + 1, c) + get(&big.data, bw, 2 * x + 1, 2 * y + 1, c) + 2) / 4;
                    sum += (avg as i32 - get(&small.data, sw, x, y, c) as i32).unsigned_abs() as u64;
                }
            }
        }
        Some(sum as f64 / (sw * sh * ch) as f64)
    }

    /// The retail TTPF archive (`XCL_SAMPLE_TTPF`): (1) the mip chain model gives the space
    /// every texture takes; (2) on its uncompressed unorm textures each detiled mip level is
    /// close to the level above it scaled down 2× (mip tail levels included), while the same
    /// tail levels read from a wrong place in the tail block (origin 0, 0) are not.
    #[test]
    fn retail_ttpf_textures_detile_with_their_mip_chains() {
        let Ok(path) = std::env::var("XCL_SAMPLE_TTPF") else {
            println!("XCL_SAMPLE_TTPF not set – skipping");
            return;
        };
        let a = Ttpf::open(&path);
        let (mut fit, mut sized, mut unreadable) = (0, 0, 0);
        let rgba8 = desc::gfx10_format(DXGI_FORMAT_R8G8B8A8_UNORM).unwrap();
        let (mut checked, mut close, mut worst) = (0, 0, Vec::new());
        let (mut tail_levels, mut tail_err, mut wrong_err) = (0, 0.0f64, 0.0f64);
        let mut volumes = Vec::new();
        for t in &a.textures {
            let Some((e, side, _)) = ttpf_element(t.dxgi) else { continue };
            let gfx10 = match (e, side) {
                (0, 1) => 1,
                (1, 1) => 7,
                (2, 1) => rgba8,
                (3, 1) => 62,
                (3, 4) => 169,
                _ => 171,
            };
            let ty = match t.dim {
                3 => ImageType::D3,
                2 => ImageType::Cube,
                _ if t.slices > 1 => ImageType::D2Array,
                _ => ImageType::D2,
            };
            let img = image(gfx10, ty, t.w, t.h, t.slices, t.mips, t.mode, 0);
            let p = placement(&img, CONSOLE_PIPES).unwrap();
            sized += 1;
            if p.bytes == t.end - t.start || p.bytes.div_ceil(4096) * 4096 == t.end - t.start {
                fit += 1;
            }
            // volumes of 16-bit float texels (colour-grading LUTs): neighbours along x and z
            // differ little read as thick blocks, a lot read as 2D slices
            if t.dim == 3 && matches!(t.dxgi, 10 | 54) {
                if let Some(mem) = a.read(t.start, t.end) {
                    let thick = subresources(&img, &mem, CONSOLE_PIPES).unwrap().remove(0);
                    let flat = image(gfx10, ImageType::D2Array, t.w, t.h, t.slices, t.mips, t.mode, 0);
                    let as_2d: Vec<u8> = subresources(&flat, &mem, CONSOLE_PIPES).unwrap().into_iter().flat_map(|s| s.data).collect();
                    let (w, h, d, e) = (t.w as usize, t.h as usize, t.slices as usize, 1usize << e);
                    volumes.push((t.index, roughness(&thick.data, w, h, d, e), roughness(&as_2d, w, h, d, e)));
                }
            }
            // unorm channels: R8 / A8, RGBA8 / BGRA8, RGBA16
            let (ch, cb) = match t.dxgi {
                61 | 65 => (1, 1),
                27..=29 | 87 | 90 | 91 => (4, 1),
                11 => (4, 2),
                _ => continue,
            };
            if t.mips < 2 || t.slices != 1 {
                continue;
            }
            let Some(mem) = a.read(t.start, t.start + p.bytes) else {
                unreadable += 1;
                continue;
            };
            let subs = subresources(&img, &mem, CONSOLE_PIPES).unwrap();
            let mut bad = 0.0f64;
            for k in 1..subs.len() {
                let Some(err) = mip_error(&subs[k - 1], &subs[k], ch, cb) else { continue };
                bad = bad.max(err);
                // control: a tail level read from the tail block's corner
                let l = p.chain.levels[k];
                if l.origin != (0, 0) && l.width >= 2 && l.height >= 2 {
                    let mut wrong = p.chain.clone();
                    wrong.levels[k].origin = (0, 0);
                    let ws = Subresource { rows: l.height, row_bytes: subs[k].row_bytes, depth: 1, data: wrong.detile_level(&mem, k) };
                    tail_levels += 1;
                    tail_err += err;
                    wrong_err += mip_error(&subs[k - 1], &ws, ch, cb).unwrap();
                }
            }
            checked += 1;
            if bad < 8.0 {
                close += 1;
            } else if worst.len() < 8 {
                worst.push((t.index, t.w, t.h, t.mips, t.mode, bad));
            }
        }
        let n = tail_levels.max(1) as f64;
        println!("TTPF: the mip chain model size matches for {fit} of {sized} textures");
        println!("TTPF: textures with mips (R8/RGBA8/RGBA16): {close} of {checked} – every mip close to the previous one downscaled (< 8/255), {unreadable} in unreadable blocks; outliers: {worst:?}");
        println!("TTPF: {tail_levels} levels in the mip tail – error {:.2}/255, at the wrong place in the block {:.2}/255", tail_err / n, wrong_err / n);
        println!("TTPF: volumes (index, neighbour difference as thick blocks, as 2D slices): {volumes:?}");
        assert!(fit * 100 >= sized * 99, "mip chain model: {fit} / {sized}");
        assert!(checked >= 5 && close * 100 >= checked * 90, "mips: {close} / {checked}");
        assert!(tail_levels >= 5 && wrong_err > 3.0 * tail_err, "mip tail: {tail_err} vs {wrong_err}");
        assert!(!volumes.is_empty() && volumes.iter().all(|v| v.2 > 3.0 * v.1), "volumes: {volumes:?}");
    }

    /// Mean |difference| between x and z neighbours of the first channel (half floats, `e`-byte
    /// texels) of a `w` × `h` × `d` volume, relative to the mean |value|.
    fn roughness(v: &[u8], w: usize, h: usize, d: usize, e: usize) -> f64 {
        let half = |b: u16| -> f64 {
            let (s, x, m) = ((b >> 15) as i32, ((b >> 10) & 31) as i32, (b & 1023) as f64);
            let v = if x == 0 { m / 1024.0 * 2f64.powi(-14) } else { (1.0 + m / 1024.0) * 2f64.powi(x - 15) };
            if s == 1 { -v } else { v }
        };
        let at = |x: usize, y: usize, z: usize| half(u16::from_le_bytes([v[((z * h + y) * w + x) * e], v[((z * h + y) * w + x) * e + 1]]));
        let (mut diff, mut mag) = (0.0, 0.0);
        for z in 0..d - 1 {
            for y in 0..h {
                for x in 0..w - 1 {
                    let c = at(x, y, z);
                    diff += (at(x + 1, y, z) - c).abs() + (at(x, y, z + 1) - c).abs();
                    mag += c.abs();
                }
            }
        }
        diff / mag.max(1e-9)
    }

    /// (element log2, pixels per side, uncompressed) of the DXGI formats the archive uses
    fn ttpf_element(dxgi: u32) -> Option<(u32, u32, bool)> {
        Some(match dxgi {
            70..=72 | 79..=81 => (3, 4, false),
            73..=78 | 82..=84 | 94..=99 => (4, 4, false),
            10 | 11 => (3, 1, true),
            27..=29 | 87 | 90 | 91 => (2, 1, true),
            54 => (1, 1, true),
            61 | 65 => (0, 1, true),
            _ => return None,
        })
    }

    struct TtpfTexture {
        index: usize,
        w: u32,
        h: u32,
        dxgi: u32,
        mode: u32,
        mips: u32,
        slices: u32,
        /// dword 1 bits 21-23: 1 2D, 2 cube, 3 volume (slices = depth), 5 2D array
        dim: u32,
        start: usize,
        end: usize,
    }

    /// Just enough of a .ttpf archive (see tools/ttpf-viewer/src/archive.rs): its zlib blocks
    /// of one unpacked stream and the texture table.
    struct Ttpf {
        d: Vec<u8>,
        /// (file offset, position in the stream), by position
        blocks: Vec<(usize, usize)>,
        textures: Vec<TtpfTexture>,
        cache: std::cell::RefCell<HashMap<usize, Option<std::rc::Rc<Vec<u8>>>>>,
    }

    impl Ttpf {
        fn open(path: &str) -> Ttpf {
            let d = std::fs::read(path).unwrap();
            let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
            assert_eq!(&d[..4], b"RDS5");
            let total = u32_at(8) as usize | (u32_at(0xc) as usize & 0xff) << 32;
            let at = (u32_at(0x10) as usize) << 8;
            let n = (u32_at(4) as usize).min((at - 0x18) / 8);
            let mut blocks: Vec<(usize, usize)> = (0..n)
                .filter_map(|i| {
                    let v = u64::from_le_bytes(d[0x18 + 8 * i..0x20 + 8 * i].try_into().unwrap());
                    let pos = ((v >> 24) & 0xff_ffff) | ((v >> 48) & 0xf) << 24;
                    (v != 0).then_some((((v & 0xff_ffff) << 8) as usize, (pos << 6) as usize))
                })
                .collect();
            blocks.sort_by_key(|b| b.1);
            let count = u32_at(at + 8) as usize;
            let mut textures: Vec<TtpfTexture> = (0..count)
                .map(|i| {
                    let o = at + 16 + 16 * i;
                    let (a, b, c) = (u32_at(o), u32_at(o + 4), u32_at(o + 8));
                    TtpfTexture {
                        index: i,
                        w: (a & 0x3fff) + 1,
                        h: ((a >> 14) & 0x3fff) + 1,
                        dxgi: (b & 0x3ff) >> 3,
                        // tile mode bits: 0 = 64 KB tiles (SW_64KB_D), else 4 KB (SW_4KB_S)
                        mode: if (b >> 24) & 3 == 0 { 10 } else { 5 },
                        mips: (a >> 28).max(1),
                        slices: ((b >> 10) & 0x7ff) + 1,
                        dim: (b >> 21) & 7,
                        start: (c & 0x1f_ffff) as usize * 4096 + 0x800,
                        end: total,
                    }
                })
                .collect();
            let mut by_start: Vec<usize> = (0..count).collect();
            by_start.sort_by_key(|&i| textures[i].start);
            for k in 0..count.saturating_sub(1) {
                textures[by_start[k]].end = textures[by_start[k + 1]].start;
            }
            Ttpf { d, blocks, textures, cache: Default::default() }
        }

        /// bytes [a, b) of the unpacked stream (None: in a block that is not zlib)
        fn read(&self, a: usize, b: usize) -> Option<Vec<u8>> {
            let mut out = Vec::with_capacity(b - a);
            let mut pos = a;
            while pos < b {
                let i = self.blocks.partition_point(|bl| bl.1 <= pos).checked_sub(1)?;
                let data = self.block(i)?;
                let from = pos - self.blocks[i].1;
                let to = (b - self.blocks[i].1).min(data.len());
                if to <= from {
                    return None;
                }
                out.extend_from_slice(&data[from..to]);
                pos += to - from;
            }
            Some(out)
        }

        fn block(&self, i: usize) -> Option<std::rc::Rc<Vec<u8>>> {
            self.cache
                .borrow_mut()
                .entry(i)
                .or_insert_with(|| {
                    let off = self.blocks[i].0;
                    let next = self.blocks.iter().map(|b| b.0).filter(|&o| o > off).min().unwrap_or(self.d.len());
                    let src = self.d.get(off..next)?;
                    (src.first() == Some(&0x78)).then(|| miniz_oxide::inflate::decompress_to_vec_zlib(src).ok().map(std::rc::Rc::new)).flatten()
                })
                .clone()
        }
    }
}
