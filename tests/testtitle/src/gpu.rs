//! D3D12.X checks of the test title. The device comes from `d3d12_xs.dll` (XCL);
//! the `windows` crate is only used for D3D12 types and vtable calls – exactly what a
//! console title compiled against a PC-compatible D3D12.X layout would do.

use std::ffi::c_void;
use std::mem::{transmute_copy, ManuallyDrop};

use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

type HRESULT = i32;

#[link(name = "d3d12_xs", kind = "raw-dylib")]
extern "system" {
    fn D3D12CreateDevice(adapter: *mut c_void, fl: u32, riid: *const c_void, dev: *mut *mut c_void) -> HRESULT;
    fn D3D12SerializeRootSignature(desc: *const c_void, v: u32, blob: *mut *mut c_void, err: *mut *mut c_void) -> HRESULT;
}

#[link(name = "d3dcompiler_47", kind = "raw-dylib")]
extern "system" {
    fn D3DCompile(
        src: *const u8,
        len: usize,
        name: *const u8,
        defines: *const c_void,
        include: *const c_void,
        entry: *const u8,
        target: *const u8,
        flags1: u32,
        flags2: u32,
        code: *mut *mut c_void,
        errors: *mut *mut c_void,
    ) -> HRESULT;
}

#[link(name = "kernel32", kind = "raw-dylib")]
extern "system" {
    fn CreateEventW(sa: *const c_void, manual: i32, initial: i32, name: *const u16) -> *mut c_void;
    fn WaitForSingleObject(h: *mut c_void, ms: u32) -> u32;
}

macro_rules! ensure {
    ($c:expr, $($m:tt)*) => { if !$c { return Err(format!($($m)*)); } };
}

/// A command list method at its console slot: from SetDescriptorHeaps (PC 28) on, the
/// console list has the PC methods 4 slots further (the query methods come first).
unsafe fn xslot<F: Copy>(list: &ID3D12GraphicsCommandList, slot: usize) -> F {
    transmute_copy(&*(*(list.as_raw() as *const *const usize)).add(slot))
}

type OmSetRenderTargets = unsafe extern "system" fn(*mut c_void, u32, *const D3D12_CPU_DESCRIPTOR_HANDLE, i32, *const D3D12_CPU_DESCRIPTOR_HANDLE);
type ClearRtv = unsafe extern "system" fn(*mut c_void, D3D12_CPU_DESCRIPTOR_HANDLE, *const [f32; 4], u32, *const RECT);
type SetRootSignature = unsafe extern "system" fn(*mut c_void, *mut c_void);
const X_SET_GRAPHICS_ROOT_SIGNATURE: usize = 34;
const X_OM_SET_RENDER_TARGETS: usize = 50;
const X_CLEAR_RENDER_TARGET_VIEW: usize = 52;

fn e(x: windows::core::Error) -> String {
    format!("{x}")
}

fn device() -> Result<ID3D12Device, String> {
    unsafe {
        let mut p = std::ptr::null_mut();
        let hr = D3D12CreateDevice(std::ptr::null_mut(), 0xC000, &ID3D12Device::IID as *const _ as *const c_void, &mut p);
        ensure!(hr >= 0 && !p.is_null(), "D3D12CreateDevice hr={hr:#x}");
        Ok(ID3D12Device::from_raw(p))
    }
}

fn compile(src: &str, target: &str) -> Result<ID3DBlob, String> {
    unsafe {
        let mut blob = std::ptr::null_mut();
        let t = format!("{target}\0");
        let hr = D3DCompile(src.as_ptr(), src.len(), std::ptr::null(), std::ptr::null(), std::ptr::null(),
            b"main\0".as_ptr(), t.as_ptr(), 0, 0, &mut blob, std::ptr::null_mut());
        ensure!(hr == 0 && !blob.is_null(), "D3DCompile({target}) hr={hr:#x}");
        Ok(ID3DBlob::from_raw(blob))
    }
}

fn bc(b: &ID3DBlob) -> D3D12_SHADER_BYTECODE {
    unsafe { D3D12_SHADER_BYTECODE { pShaderBytecode: b.GetBufferPointer(), BytecodeLength: b.GetBufferSize() } }
}

fn root_signature(dev: &ID3D12Device) -> Result<ID3D12RootSignature, String> {
    unsafe {
        let desc = D3D12_ROOT_SIGNATURE_DESC { Flags: D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT, ..Default::default() };
        let (mut blob, mut err) = (std::ptr::null_mut(), std::ptr::null_mut());
        let hr = D3D12SerializeRootSignature(&desc as *const _ as *const c_void, 1, &mut blob, &mut err);
        ensure!(hr == 0 && !blob.is_null(), "D3D12SerializeRootSignature hr={hr:#x}");
        let blob = ID3DBlob::from_raw(blob);
        let bytes = std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize());
        dev.CreateRootSignature(0, bytes).map_err(e)
    }
}

fn pso_desc(rs: &ID3D12RootSignature, vs: D3D12_SHADER_BYTECODE, ps: D3D12_SHADER_BYTECODE) -> D3D12_GRAPHICS_PIPELINE_STATE_DESC {
    let mut d = D3D12_GRAPHICS_PIPELINE_STATE_DESC::default();
    d.pRootSignature = unsafe { transmute_copy(rs) };
    d.VS = vs;
    d.PS = ps;
    d.BlendState.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
    d.SampleMask = u32::MAX;
    d.RasterizerState.FillMode = D3D12_FILL_MODE_SOLID;
    d.RasterizerState.CullMode = D3D12_CULL_MODE_NONE;
    d.RasterizerState.DepthClipEnable = true.into();
    d.PrimitiveTopologyType = D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE;
    d.NumRenderTargets = 1;
    d.RTVFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
    d.SampleDesc = DXGI_SAMPLE_DESC { Count: 1, Quality: 0 };
    d
}

fn wait(queue: &ID3D12CommandQueue, fence: &ID3D12Fence, value: u64) -> Result<(), String> {
    unsafe {
        queue.Signal(fence, value).map_err(e)?;
        let ev = CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
        fence.SetEventOnCompletion(value, windows::Win32::Foundation::HANDLE(ev)).map_err(e)?;
        ensure!(WaitForSingleObject(ev, 5000) == 0, "GPU fence {value} not reached");
        ensure!(fence.GetCompletedValue() >= value, "fence value {}", fence.GetCompletedValue());
        Ok(())
    }
}

pub fn queue_fence() -> Result<(), String> {
    unsafe {
        let dev = device()?;
        let queue: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).map_err(e)?;
        let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(e)?;
        wait(&queue, &fence, 3)?;
        wait(&queue, &fence, 4)
    }
}

/// Fake console container: unknown part "XBOX" with RDNA2 code
/// (s_mov_b32 s0, s1 ; v_add_f32 v5, v1, v2 ; s_endpgm).
fn console_container() -> Vec<u8> {
    let code: [u8; 12] = [0x01, 0x03, 0x80, 0xbe, 0x01, 0x05, 0x0a, 0x06, 0x00, 0x00, 0x81, 0xbf];
    let mut xb = vec![0u8; 36];
    xb[0..4].copy_from_slice(b"DXBC");
    xb[28..32].copy_from_slice(&1u32.to_le_bytes());
    xb[32..36].copy_from_slice(&36u32.to_le_bytes());
    xb.extend_from_slice(b"XBOX");
    xb.extend_from_slice(&(code.len() as u32).to_le_bytes());
    xb.extend_from_slice(&code);
    let total = xb.len() as u32;
    xb[24..28].copy_from_slice(&total.to_le_bytes());
    xb
}

fn le(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

/// Console shader in the retail layout: DXBC whose DXIL part carries no bitcode ("NOBC")
/// but XDAT → XBCH (zlib) → FSSM with native code. The code uses v_mac_f32, which only
/// the console GPU has (removed in retail RDNA2):
/// v_mac_f32 v6, s40, v21 ; s_endpgm
fn xbox_native_container() -> Vec<u8> {
    // FSSM: code pointer at +0xC0/+0xC8 (relative to +0x10), PS hardware-stage descriptor at +0x100
    let code: [u8; 8] = [0x28, 0x2a, 0x0c, 0x3e, 0x00, 0x00, 0x81, 0xbf];
    let mut fssm = vec![0u8; 0x308];
    fssm[0..4].copy_from_slice(b"FSSM");
    fssm[4..8].copy_from_slice(&0x101u32.to_le_bytes());
    fssm[0xC0..0xC8].copy_from_slice(&0x2f8u64.to_le_bytes());
    fssm[0xC8..0xD0].copy_from_slice(&(code.len() as u64).to_le_bytes());
    fssm[0x100..0x108].copy_from_slice(&0x0000_00C0_0002_0105u64.to_le_bytes());
    fssm.extend_from_slice(&code);
    let size = (fssm.len() - 0x10) as u32;
    fssm[8..12].copy_from_slice(&size.to_le_bytes());
    let z = miniz_oxide::deflate::compress_to_vec_zlib(&fssm, 6);
    let mut xbch = b"XBCH".to_vec();
    for x in [3, z.len() as u32, 0, fssm.len() as u32, 0] {
        le(&mut xbch, x);
    }
    xbch.extend_from_slice(&z);
    // DXIL part: program header (24 B), XDAT, entries, NOBC placeholder
    let hash = [0x5au8; 16];
    let table = 0x18 + 12;
    let data_at = table + 2 * 12;
    let mut d = Vec::new();
    for x in [0x66u32, 0, u32::from_le_bytes(*b"DXIL"), 0x106, 0, 4] {
        le(&mut d, x);
    }
    d.extend_from_slice(b"XDAT");
    for x in [2, table as u32, 0x12, data_at as u32, 16, 0x0d, (data_at + 16) as u32, xbch.len() as u32] {
        le(&mut d, x);
    }
    d.extend_from_slice(&hash);
    d.extend_from_slice(&xbch);
    while d.len() % 4 != 0 {
        d.push(0);
    }
    let nobc = d.len();
    d.extend_from_slice(b"NOBC");
    let dwords = (d.len() / 4) as u32;
    d[4..8].copy_from_slice(&dwords.to_le_bytes());
    d[16..20].copy_from_slice(&((nobc - 8) as u32).to_le_bytes());
    // DXBC with a single DXIL part
    let mut v = vec![0u8; 36];
    v[0..4].copy_from_slice(b"DXBC");
    v[20..24].copy_from_slice(&1u32.to_le_bytes());
    v[28..32].copy_from_slice(&1u32.to_le_bytes());
    v[32..36].copy_from_slice(&36u32.to_le_bytes());
    v.extend_from_slice(b"DXIL");
    le(&mut v, d.len() as u32);
    v.extend_from_slice(&d);
    let total = v.len() as u32;
    v[24..28].copy_from_slice(&total.to_le_bytes());
    v
}

pub fn xbox_native_pso() -> Result<(), String> {
    unsafe {
        let dev = device()?;
        let rs = root_signature(&dev)?;
        let vs = compile("float4 main(uint id : SV_VertexID) : SV_Position { return float4(id, 0, 0, 1); }", "vs_5_0")?;
        let xb = xbox_native_container();
        let ps = D3D12_SHADER_BYTECODE { pShaderBytecode: xb.as_ptr() as *const c_void, BytecodeLength: xb.len() };
        let pso: Result<ID3D12PipelineState, _> = dev.CreateGraphicsPipelineState(&pso_desc(&rs, bc(&vs), ps));
        ensure!(pso.is_ok(), "CreateGraphicsPipelineState with a retail-format console shader failed: {:?}", pso.err());
        Ok(())
    }
}

pub fn pso_capture_and_fallback() -> Result<(), String> {
    unsafe {
        let dev = device()?;
        let rs = root_signature(&dev)?;
        let vs = compile("float4 main(uint id : SV_VertexID) : SV_Position { return float4(id, 0, 0, 1); }", "vs_5_0")?;
        let xb = console_container();
        let console_ps = D3D12_SHADER_BYTECODE { pShaderBytecode: xb.as_ptr() as *const c_void, BytecodeLength: xb.len() };
        let desc = pso_desc(&rs, bc(&vs), console_ps);
        // the PC driver rejects the console PS; XCL must substitute a stand-in shader
        let pso: Result<ID3D12PipelineState, _> = dev.CreateGraphicsPipelineState(&desc);
        ensure!(pso.is_ok(), "CreateGraphicsPipelineState with a console shader failed: {:?}", pso.err());
        Ok(())
    }
}

/// Console presentation as a title does it: a 60 Hz frame clock (device #78 / #79), the
/// origin wait that hands out the frame token (#80), a cleared 640×360 back buffer handed to
/// `PresentX` (queue #30) with that token, then the frame's statistics (#81).
pub fn present_x_frame() -> Result<(), String> {
    unsafe {
        let dev = device()?;
        let queue: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).map_err(e)?;
        let slot = |obj: *mut c_void, i: usize| *(*(obj as *const *const usize)).add(i);
        let d = dev.as_raw();
        let set_interval: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, u32, u32) -> HRESULT = std::mem::transmute(slot(d, 78));
        let schedule: unsafe extern "system" fn(*mut c_void, u32, u32, *const u8, u32) -> HRESULT = std::mem::transmute(slot(d, 79));
        let wait_event: unsafe extern "system" fn(*mut c_void, u32, u32, *const u8, u32, *mut u64) -> HRESULT = std::mem::transmute(slot(d, 80));
        let statistics: unsafe extern "system" fn(*mut c_void, u64, u32, *mut u32, *mut u8) -> HRESULT = std::mem::transmute(slot(d, 81));
        ensure!(set_interval(d, std::ptr::null_mut(), 16667, 1, 0) >= 0, "SetFrameIntervalX");
        ensure!(schedule(d, 0, 0, std::ptr::null(), 0) >= 0, "ScheduleFrameEventX (origin)");
        let mut token = 0u64;
        let hr = wait_event(d, 0, 500, std::ptr::null(), 0, &mut token);
        ensure!(hr == 0 && token != 0, "WaitFrameEventX hr={hr:#x} token={token}");

        let (w, h) = (640u32, 360u32);
        let tex = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: w as u64,
            Height: h,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let mut bb: Option<ID3D12Resource> = None;
        dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &tex, D3D12_RESOURCE_STATE_RENDER_TARGET, None, &mut bb)
            .map_err(e)?;
        let bb = bb.ok_or("no back buffer")?;
        let heap: ID3D12DescriptorHeap = dev
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV, NumDescriptors: 1, ..Default::default() })
            .map_err(e)?;
        let start: unsafe extern "system" fn(*mut c_void) -> usize = std::mem::transmute(slot(heap.as_raw(), 9));
        let rtv = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: start(heap.as_raw()) };
        dev.CreateRenderTargetView(&bb, None, rtv);
        let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).map_err(e)?;
        let list: ID3D12GraphicsCommandList = dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).map_err(e)?;
        xslot::<ClearRtv>(&list, X_CLEAR_RENDER_TARGET_VIEW)(list.as_raw(), rtv, &[0.1, 0.5, 0.9, 1.0], 0, std::ptr::null());
        // consoles want the back buffer in COMMON / PRESENT for PresentX
        list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: transmute_copy(&bb),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: D3D12_RESOURCE_STATE_RENDER_TARGET,
                    StateAfter: D3D12_RESOURCE_STATE_PRESENT,
                }),
            },
        }]);
        list.Close().map_err(e)?;
        queue.ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>().map_err(e)?)]);

        // plane 0: { u64 frame token; u32 resource count @8; ID3D12Resource** @0x10 }
        let resources = [bb.as_raw()];
        let mut plane = [0u8; 0x40];
        plane[..8].copy_from_slice(&token.to_le_bytes());
        plane[8..12].copy_from_slice(&1u32.to_le_bytes());
        plane[0x10..0x18].copy_from_slice(&(resources.as_ptr() as u64).to_le_bytes());
        let present: unsafe extern "system" fn(*mut c_void, u32, *const u8, *const u8) -> HRESULT = std::mem::transmute(slot(queue.as_raw(), 30));
        let hr = present(queue.as_raw(), 1, plane.as_ptr(), std::ptr::null());
        ensure!(hr == 0, "PresentX hr={hr:#x}");
        let (mut n, mut stats) = (1u32, [0u8; 0x40]);
        let hr = statistics(d, token, 0, &mut n, stats.as_mut_ptr());
        ensure!(hr == 0 && n == 1, "GetFrameStatisticsX hr={hr:#x} count={n}");
        let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(e)?;
        queue.Signal(&fence, 1).map_err(e)?;
        wait(&queue, &fence, 1)?;
        ensure!(set_interval(d, std::ptr::null_mut(), 0, 0, 0) >= 0, "stopping the frame clock");
        Ok(())
    }
}

pub fn render_triangle() -> Result<(), String> {
    unsafe {
        let dev = device()?;
        let queue: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).map_err(e)?;
        let rs = root_signature(&dev)?;
        let vs = compile(
            "float4 main(uint id : SV_VertexID) : SV_Position { float2 p[3] = { float2(-0.5,-0.5), float2(0.0,0.5), float2(0.5,-0.5) }; return float4(p[id], 0, 1); }",
            "vs_5_0",
        )?;
        let ps = compile("float4 main() : SV_Target { return float4(0, 1, 0, 1); }", "ps_5_0")?;
        let pso: ID3D12PipelineState = dev.CreateGraphicsPipelineState(&pso_desc(&rs, bc(&vs), bc(&ps))).map_err(e)?;

        let (w, h) = (64u32, 64u32);
        let tex = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: w as u64,
            Height: h,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let clear = D3D12_CLEAR_VALUE { Format: DXGI_FORMAT_R8G8B8A8_UNORM, Anonymous: D3D12_CLEAR_VALUE_0 { Color: [0.0, 0.0, 1.0, 1.0] } };
        let mut rt: Option<ID3D12Resource> = None;
        dev.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
            D3D12_HEAP_FLAG_NONE,
            &tex,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            Some(&clear),
            &mut rt,
        )
        .map_err(e)?;
        let rt = rt.ok_or("no render target")?;
        let heap: ID3D12DescriptorHeap = dev
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV, NumDescriptors: 1, ..Default::default() })
            .map_err(e)?;
        // D3D12.X: the heap start handle comes back in rax (the PC method returns it through
        // a hidden pointer) – console code calls slot 9 like this
        let start: unsafe extern "system" fn(*mut c_void) -> usize = transmute_copy(&(*(*(heap.as_raw() as *const *const usize)).add(9)));
        let rtv = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: start(heap.as_raw()) };
        dev.CreateRenderTargetView(&rt, None, rtv);

        let mut layout = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut total = 0u64;
        dev.GetCopyableFootprints(&tex, 0, 1, 0, Some(&mut layout), None, None, Some(&mut total));
        let buf_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: total,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut rb: Option<ID3D12Resource> = None;
        dev.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() },
            D3D12_HEAP_FLAG_NONE,
            &buf_desc,
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
            &mut rb,
        )
        .map_err(e)?;
        let rb = rb.ok_or("no readback buffer")?;

        let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).map_err(e)?;
        let list: ID3D12GraphicsCommandList = dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, &pso).map_err(e)?;
        // console slots (OMSetRenderTargets #50, ClearRenderTargetView #52, SetGraphicsRootSignature #34)
        xslot::<OmSetRenderTargets>(&list, X_OM_SET_RENDER_TARGETS)(list.as_raw(), 1, &rtv, 0, std::ptr::null());
        xslot::<ClearRtv>(&list, X_CLEAR_RENDER_TARGET_VIEW)(list.as_raw(), rtv, &[0.0, 0.0, 1.0, 1.0], 0, std::ptr::null());
        list.RSSetViewports(&[D3D12_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: w as f32, Height: h as f32, MinDepth: 0.0, MaxDepth: 1.0 }]);
        list.RSSetScissorRects(&[RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 }]);
        xslot::<SetRootSignature>(&list, X_SET_GRAPHICS_ROOT_SIGNATURE)(list.as_raw(), rs.as_raw());
        list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        list.DrawInstanced(3, 1, 0, 0);
        let barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: transmute_copy(&rt),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: D3D12_RESOURCE_STATE_RENDER_TARGET,
                    StateAfter: D3D12_RESOURCE_STATE_COPY_SOURCE,
                }),
            },
        };
        list.ResourceBarrier(&[barrier]);
        let dst = D3D12_TEXTURE_COPY_LOCATION {
            pResource: transmute_copy(&rb),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: layout },
        };
        let src = D3D12_TEXTURE_COPY_LOCATION {
            pResource: transmute_copy(&rt),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        };
        list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        list.Close().map_err(e)?;
        queue.ExecuteCommandLists(&[Some(list.cast().map_err(e)?)]);
        let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(e)?;
        wait(&queue, &fence, 1)?;

        let mut p = std::ptr::null_mut();
        rb.Map(0, None, Some(&mut p)).map_err(e)?;
        let pitch = layout.Footprint.RowPitch as usize;
        let px = |x: usize, y: usize| -> [u8; 4] {
            let o = y * pitch + x * 4;
            let s = std::slice::from_raw_parts((p as *const u8).add(o), 4);
            [s[0], s[1], s[2], s[3]]
        };
        let center = px(32, 32);
        let corner = px(1, 1);
        rb.Unmap(0, None);
        ensure!(center == [0, 255, 0, 255], "centre pixel {center:?} (expected green triangle)");
        ensure!(corner == [0, 0, 255, 255], "corner pixel {corner:?} (expected blue clear colour)");
        Ok(())
    }
}
