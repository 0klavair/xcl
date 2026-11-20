//! End to end without a game executable: real console shaders from the retail pack
//! (`XCL_SAMPLE_RPF`) go through the D3D12.X backend's CreateGraphicsPipelineState hook
//! exactly as a title would hand them over → recompiled VS/PS → D3D12 pipeline → drawn into
//! a texture on the GPU → pixels read back and compared with values computed here from the
//! inputs the tests set explicitly (vertex buffer, user SGPRs, descriptor tables, constants,
//! textures, samplers).
//!
//! The stand-in shaders XCL uses when a console shader cannot be translated draw nothing
//! (degenerate position) or magenta, and the pipeline counters are checked, so these tests
//! cannot pass on the fallback path.

use std::ffi::CStr;
use std::sync::{Mutex, OnceLock};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use xcl::gpu::recomp::runner;
use xcl::gpu::{desc, pack};
use xcl::hle::d3d12x::real;

const SIZE: u32 = 64;
const CLEAR: [f32; 4] = [0.1, 0.2, 0.3, 0.4];

/// The tests share the backend's pipeline counters: one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn f(x: f32) -> u32 {
    x.to_bits()
}

/// Console shader containers by record name (as a title passes them in
/// D3D12_SHADER_BYTECODE) and the backend device. None: no sample pack / DXC / GPU.
fn setup(names: &[&str]) -> Option<(Vec<Vec<u8>>, ID3D12Device)> {
    static PACK: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    let data = PACK.get_or_init(|| std::env::var("XCL_SAMPLE_RPF").ok().and_then(|p| std::fs::read(p).ok())).as_ref();
    let Some(data) = data else {
        println!("XCL_SAMPLE_RPF nie ustawione – pomijam");
        return None;
    };
    if xcl::gpu::recomp::dxc::location().is_err() {
        println!("brak DXC – pomijam");
        return None;
    }
    let recs = pack::scan(data, 0x40);
    let blobs: Vec<Vec<u8>> = names
        .iter()
        .map(|n| {
            let r = recs.iter().find(|r| r.name == *n).unwrap_or_else(|| panic!("brak rekordu {n}"));
            let b = pack::inflate(data, r).unwrap();
            assert!(xcl::gpu::xbox::is_xbox(&b), "{n}: oczekiwany kontener konsoli");
            b
        })
        .collect();
    if xcl::hle::d3d12x::backend() != xcl::hle::d3d12x::Backend::Real {
        println!("brak prawdziwego urządzenia D3D12 – pomijam");
        return None;
    }
    println!("urządzenie backendu: {}", real::stats().adapter);
    let raw = real::device_raw();
    Some((blobs, unsafe { ID3D12Device::from_raw_borrowed(&raw) }.unwrap().clone()))
}

/// CreateGraphicsPipelineState through the backend hook with the title's own (empty)
/// root signature and input layout; asserts the pipeline came from translated shaders.
fn pso_through_hook(dev: &ID3D12Device, vs: &[u8], ps: &[u8], layout: &[(&CStr, DXGI_FORMAT, u32)]) -> ID3D12PipelineState {
    let game_rs: ID3D12RootSignature = unsafe {
        let desc = D3D12_ROOT_SIGNATURE_DESC { Flags: D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT, ..Default::default() };
        let mut ser = None;
        D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut ser, None).unwrap();
        let ser = ser.unwrap();
        dev.CreateRootSignature(0, std::slice::from_raw_parts(ser.GetBufferPointer() as *const u8, ser.GetBufferSize())).unwrap()
    };
    let elems: Vec<D3D12_INPUT_ELEMENT_DESC> = layout
        .iter()
        .map(|(name, fmt, off)| D3D12_INPUT_ELEMENT_DESC {
            SemanticName: windows::core::PCSTR(name.as_ptr() as *const u8),
            Format: *fmt,
            AlignedByteOffset: *off,
            InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
            ..Default::default()
        })
        .collect();
    let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
        pRootSignature: std::mem::ManuallyDrop::new(Some(game_rs)),
        VS: D3D12_SHADER_BYTECODE { pShaderBytecode: vs.as_ptr() as _, BytecodeLength: vs.len() },
        PS: D3D12_SHADER_BYTECODE { pShaderBytecode: ps.as_ptr() as _, BytecodeLength: ps.len() },
        SampleMask: u32::MAX,
        InputLayout: D3D12_INPUT_LAYOUT_DESC { pInputElementDescs: elems.as_ptr(), NumElements: elems.len() as u32 },
        PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
        NumRenderTargets: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        ..Default::default()
    };
    desc.BlendState.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
    desc.RasterizerState.FillMode = D3D12_FILL_MODE_SOLID;
    desc.RasterizerState.CullMode = D3D12_CULL_MODE_NONE;
    desc.RasterizerState.DepthClipEnable = true.into();
    desc.RTVFormats[0] = DXGI_FORMAT_R32G32B32A32_FLOAT;
    let before = real::stats();
    let pso: ID3D12PipelineState = unsafe { dev.CreateGraphicsPipelineState(&desc) }.expect("CreateGraphicsPipelineState przez hak backendu");
    unsafe { std::mem::ManuallyDrop::drop(&mut desc.pRootSignature) };
    let after = real::stats();
    assert_eq!(after.translated_pipelines, before.translated_pipelines + 1, "PSO nie powstał z przetłumaczonych shaderów: {:?}", after.translation_errors);
    assert_eq!(after.fallback_pipelines, before.fallback_pipelines, "użyto zastępczych shaderów");
    pso
}

/// `VS_Rage_TransformPosCpv` (POSITION · world 4×3 · view-projection 4×4, COLOR passed on;
/// s[8:9] = address of a table of two V#s) + `PS_Rage_Untextured` (interpolated COLOR →
/// SV_Target0 through f16): one triangle, interpolated colours checked pixel by pixel.
#[test]
fn retail_vs_ps_through_the_pso_hook_render_and_read_back() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some((blobs, dev)) = setup(&["VS_Rage_TransformPosCpv_f020b20e_Wrapped", "PS_Rage_Untextured_14f8529b_Wrapped"]) else { return };
    let pso = pso_through_hook(&dev, &blobs[0], &blobs[1], &[(c"POSITION", DXGI_FORMAT_R32G32B32_FLOAT, 0), (c"COLOR", DXGI_FORMAT_R32G32B32A32_FLOAT, 12)]);
    let rs = real::abi_root_signature().expect("root signature ABI XCL");

    // explicit inputs: user SGPR s[8:9] → descriptor table at 0x100 with two V#s
    let mut io = vec![0u32; 128];
    io[8] = 0x100;
    io[9] = 0;
    let mut mem = vec![0u32; 0xC00];
    let vsharp = |base: u32| [base, 0, 0x1000, 0];
    mem[0x100 / 4..0x100 / 4 + 4].copy_from_slice(&vsharp(0x1000)); // world (VS reads 0x00..0x3F)
    mem[0x120 / 4..0x120 / 4 + 4].copy_from_slice(&vsharp(0x2000)); // view-projection (0x160..0x19F)
    // world rows = coefficients of x, y, z, then translation: (0.5·x + 0.25, 0.5·y, z)
    let world: [[f32; 4]; 4] = [[0.5, 0.0, 0.0, 0.0], [0.0, 0.5, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.25, 0.0, 0.0, 0.0]];
    for (r, row) in world.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            mem[0x1000 / 4 + 4 * r + c] = f(*v);
        }
    }
    // view-projection: out.k = Σ m[4·i + k]·world_i + m[12 + k]; flips y
    let mut vp = [0.0f32; 16];
    vp[0] = 1.0;
    vp[5] = -1.0;
    vp[10] = 1.0;
    vp[15] = 1.0;
    for (i, v) in vp.iter().enumerate() {
        mem[(0x2000 + 0x160) / 4 + i] = f(*v);
    }
    let corners = [([-1.0f32, -1.0, 0.5], [1.0f32, 0.0, 0.0, 1.0]), ([-1.0, 1.0, 0.5], [0.0, 1.0, 0.0, 1.0]), ([1.0, -1.0, 0.5], [0.0, 0.0, 1.0, 0.5])];
    let mut vertices = Vec::new();
    for (p, c) in &corners {
        vertices.extend_from_slice(p);
        vertices.extend_from_slice(c);
    }
    let px = runner::draw_on(&dev, &pso, &rs, &vertices, 28, &io, &mem, SIZE, CLEAR).expect("rysowanie");

    // expected: the same transform on the CPU → pixel-space triangle → interpolated colour
    let to_px = |p: [f32; 3]| {
        let (wx, wy) = (0.5 * p[0] + 0.25, 0.5 * p[1]);
        let (cx, cy) = (wx, -wy);
        ((cx + 1.0) * 0.5 * SIZE as f32, (1.0 - cy) * 0.5 * SIZE as f32)
    };
    let (a, b, c) = (to_px(corners[0].0), to_px(corners[1].0), to_px(corners[2].0));
    let area = (b.0 - a.0) * (c.1 - a.1) - (c.0 - a.0) * (b.1 - a.1);
    let (mut inside, mut outside) = (0, 0);
    for y in 0..SIZE as usize {
        for x in 0..SIZE as usize {
            let (qx, qy) = (x as f32 + 0.5, y as f32 + 0.5);
            let l1 = ((b.0 - qx) * (c.1 - qy) - (c.0 - qx) * (b.1 - qy)) / area;
            let l2 = ((c.0 - qx) * (a.1 - qy) - (a.0 - qx) * (c.1 - qy)) / area;
            let l3 = 1.0 - l1 - l2;
            let got = px[y * SIZE as usize + x];
            let m = l1.min(l2).min(l3);
            if m > 0.02 {
                let want: Vec<f32> = (0..4).map(|k| l1 * corners[0].1[k] + l2 * corners[1].1[k] + l3 * corners[2].1[k]).collect();
                let ok = (0..4).all(|k| (got[k] - want[k]).abs() < 0.01);
                assert!(ok, "piksel ({x},{y}) wewnątrz: {got:?}, oczekiwane {want:?}");
                inside += 1;
            } else if m < -0.02 {
                assert_eq!(got, CLEAR, "piksel ({x},{y}) poza trójkątem");
                outside += 1;
            }
        }
    }
    println!("sprawdzone piksele: {inside} wewnątrz, {outside} na zewnątrz; trójkąt {a:?} {b:?} {c:?}");
    assert!(inside > 400 && outside > 2500, "za mało sprawdzonych pikseli: {inside} / {outside}");
}

/// Texel (x, y) of the two test textures, RGBA8.
fn texel_a(x: usize, y: usize) -> [u8; 4] {
    [(x * 4) as u8, (y * 4) as u8, if (x / 8 + y / 8) % 2 == 0 { 255 } else { 0 }, 255]
}

fn texel_b(x: usize, y: usize) -> [u8; 4] {
    [(255 - y * 4) as u8, 128, (x * 4) as u8, 200]
}

/// Console T# of a linear (SW_MODE 0) 64×64 8_8_8_8_UNORM 2D texture at `address`.
fn console_tsharp(address: u64, dst_sel: [u32; 4]) -> [u32; 8] {
    let (w1, h1) = (SIZE - 1, SIZE - 1);
    [
        (address >> 8) as u32,
        ((address >> 40) as u32 & 0xFF) | 56 << 20 | (w1 & 3) << 30,
        (w1 >> 2) | h1 << 14,
        dst_sel[0] | dst_sel[1] << 3 | dst_sel[2] << 6 | dst_sel[3] << 9 | 9 << 28,
        0,
        0,
        0,
        0,
    ]
}

/// `VS_Rage_TransformPosCpvTex` + `PS_Rage_Textured` (texture · vertex colour; the T# comes
/// from the descriptor table at s[8:9] + 0x20, the S# is built into the code). Two console
/// T#s → our decoder → D3D12 textures and views; one pipeline, two draws in one command list
/// whose only difference is the descriptor table (left half: texture A, right half:
/// texture B with BGRA swizzle). Checked texel by texel, then that the change took effect.
#[test]
fn retail_textured_ps_decoded_descriptors_and_a_descriptor_change_between_draws() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some((blobs, dev)) = setup(&["VS_Rage_TransformPosCpvTex_1702efec_Wrapped", "PS_Rage_Textured_1702efec_Wrapped"]) else { return };
    let pso = pso_through_hook(
        &dev,
        &blobs[0],
        &blobs[1],
        &[(c"POSITION", DXGI_FORMAT_R32G32B32_FLOAT, 0), (c"COLOR", DXGI_FORMAT_R32G32B32A32_FLOAT, 12), (c"TEXCOORD", DXGI_FORMAT_R32G32_FLOAT, 28)],
    );
    let rs = real::abi_root_signature().expect("root signature ABI XCL");

    // console memory: texel data at the addresses the T#s name, constants, two descriptor tables
    let (addr_a, addr_b) = (0x1_0000u64, 0x2_0000u64);
    let mut mem = vec![0u32; 0x2_4000 / 4];
    let bytes = |mem: &mut Vec<u32>, at: u64, texel: fn(usize, usize) -> [u8; 4]| {
        for y in 0..SIZE as usize {
            for x in 0..SIZE as usize {
                mem[at as usize / 4 + y * SIZE as usize + x] = u32::from_le_bytes(texel(x, y));
            }
        }
    };
    bytes(&mut mem, addr_a, texel_a);
    bytes(&mut mem, addr_b, texel_b);
    let t_a = console_tsharp(addr_a, [4, 5, 6, 7]); // identity
    let t_b = console_tsharp(addr_b, [6, 5, 4, 7]); // r ← z, b ← x
    // the static sampler the PS builds in code: s[0:3] = 0x400, 0xC00000, 0x18F00000, 0
    let s_code = [0x400u32, 0xC0_0000, 0x18F0_0000, 0];

    // our decoder: T# → D3D12 resource + SRV (texel data read from the T#'s address), S# → sampler
    let mut textures = Vec::new();
    for t in [t_a, t_b] {
        let img = desc::image(&t).expect("T#");
        assert_eq!(img.swizzle_mode, 0, "test używa liniowych tekstur");
        let (res, srv) = desc::image_d3d12(&img).expect("format DXGI");
        assert_eq!((res.Width, res.Height, res.Format), (SIZE as u64, SIZE, DXGI_FORMAT_R8G8B8A8_UNORM));
        let at = img.address as usize;
        let data: Vec<u8> = mem[at / 4..at / 4 + (SIZE * SIZE) as usize].iter().flat_map(|w| w.to_le_bytes()).collect();
        textures.push(runner::ViewTexture { res, srv, data, row_bytes: SIZE as usize * 4 });
    }
    let sampler = desc::sampler_d3d12(&s_code, false);
    assert_eq!((sampler.Filter, sampler.AddressU, sampler.MaxAnisotropy), (D3D12_FILTER_ANISOTROPIC, D3D12_TEXTURE_ADDRESS_MODE_WRAP, 4));

    // descriptor tables in the XCL shader ABI form: +0x00 world V#, +0x20 T#, +0x40 view-projection V#
    let vsharp = |base: u32| [base, 0, 0x1000, 0];
    for (table, t, heap_index) in [(0x100usize, t_a, 0u32), (0x200, t_b, 2)] {
        mem[table / 4..table / 4 + 4].copy_from_slice(&vsharp(0x1000));
        mem[(table + 0x20) / 4..(table + 0x20) / 4 + 8].copy_from_slice(&desc::xcl_image_words(&t, heap_index));
        mem[(table + 0x40) / 4..(table + 0x40) / 4 + 4].copy_from_slice(&vsharp(0x2000));
    }
    assert_eq!(desc::xcl_sampler_words(&s_code, 0), s_code, "S# z kodu wskazuje sampler 0");
    // identity world and view-projection
    for (i, v) in [1.0f32, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0].iter().enumerate() {
        mem[0x1000 / 4 + i] = f(*v);
    }
    for (i, v) in [1.0f32, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0].iter().enumerate() {
        mem[(0x2000 + 0x160) / 4 + i] = f(*v);
    }
    // full-screen quad, white, uv = screen position → every pixel centre hits a texel centre
    let mut vertices = Vec::new();
    for (x, y) in [(-1.0f32, -1.0f32), (-1.0, 1.0), (1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
        vertices.extend_from_slice(&[x, y, 0.5, 1.0, 1.0, 1.0, 1.0, (x + 1.0) * 0.5, (1.0 - y) * 0.5]);
    }
    let half = SIZE as i32 / 2;
    let user_data = |table: u32| {
        let mut io = vec![0u32; 128];
        io[8] = table;
        io
    };
    let draws = [
        runner::Draw { sgprs: user_data(0x100), scissor: [0, 0, half, SIZE as i32] },
        runner::Draw { sgprs: user_data(0x200), scissor: [half, 0, SIZE as i32, SIZE as i32] },
    ];
    let px = runner::draw_on_multi(&dev, &pso, &rs, &vertices, 36, &draws, &mem, &textures, &[sampler], SIZE, CLEAR).expect("rysowanie");

    let unorm = |t: [u8; 4]| t.map(|c| c as f32 / 255.0);
    let (mut checked, mut changed) = (0, 0);
    for y in 0..SIZE as usize {
        for x in 0..SIZE as usize {
            let got = px[y * SIZE as usize + x];
            let a = unorm(texel_a(x, y));
            let b = unorm(texel_b(x, y));
            let want = if x < half as usize { a } else { [b[2], b[1], b[0], b[3]] };
            assert!((0..4).all(|k| (got[k] - want[k]).abs() < 0.005), "piksel ({x},{y}): {got:?}, oczekiwane {want:?}");
            checked += 1;
            // right half: the second draw saw the other descriptor, not texture A
            if x >= half as usize && (0..4).any(|k| (got[k] - a[k]).abs() > 0.05) {
                changed += 1;
            }
        }
    }
    let right = (SIZE * SIZE / 2) as usize;
    println!("sprawdzone piksele: {checked}; po zmianie deskryptora różnych od tekstury A: {changed} / {right}");
    assert!(changed > right * 9 / 10, "zmiana deskryptora między draw callami nie zmieniła wyniku: {changed} / {right}");
}
