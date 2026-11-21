//! The console's shader compiler as a test generator for the recompiler: HLSL compiled by the
//! console's `dxcompiler_xs.dll` (DXIL + the native RDNA2 code in XDAT → XBCH → FSSM) goes
//! through XCL's RDNA2 → HLSL translation and runs on the PC GPU; the same HLSL compiled by the
//! PC compiler runs next to it, and the two must write the same memory.
//!
//! The compiler is the console's own file and stays outside the repo: the tests run only when
//! `XCL_SAMPLE_DXC` names `dxcompiler_xs.dll` (with its `xbsc_xs.dll` next to it), e.g. from
//! the system directory of a console OS image.

use std::ffi::c_void;
use windows::core::{Interface, GUID, HRESULT, PCWSTR};
use windows::Win32::Graphics::Direct3D::Dxc::{DxcBuffer, IDxcBlob, IDxcCompiler3, IDxcResult, CLSID_DxcCompiler, DXC_CP_UTF8};
use xcl::gpu::recomp::{dxc, pso, runner};
use xcl::hle::d3d12x::console_list;

type CreateInstance = unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

/// The console compiler (None: `XCL_SAMPLE_DXC` not set).
fn console_compiler() -> Option<IDxcCompiler3> {
    let path = std::env::var("XCL_SAMPLE_DXC").ok()?;
    let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
    unsafe {
        // its back end is loaded by name when it compiles: loaded first from the same directory
        // (LOAD_WITH_ALTERED_SEARCH_PATH)
        let dir = std::path::Path::new(&path).parent().map(|d| d.to_path_buf()).unwrap_or_default();
        for name in ["xbsc_xs.dll", "newbe_xs.dll"] {
            let w: Vec<u16> = dir.join(name).to_string_lossy().encode_utf16().chain([0]).collect();
            windows_sys::Win32::System::LibraryLoader::LoadLibraryExW(w.as_ptr(), std::ptr::null_mut(), 0x8);
        }
        let h = windows_sys::Win32::System::LibraryLoader::LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), 0x8);
        assert!(!h.is_null(), "LoadLibraryExW({path}) failed");
        let f = windows_sys::Win32::System::LibraryLoader::GetProcAddress(h, b"DxcCreateInstance\0".as_ptr()).expect("DxcCreateInstance");
        let create: CreateInstance = std::mem::transmute(f);
        let mut p = std::ptr::null_mut();
        let hr = create(&CLSID_DxcCompiler, &IDxcCompiler3::IID, &mut p);
        assert!(hr.is_ok() && !p.is_null(), "DxcCreateInstance: {hr:?}");
        Some(IDxcCompiler3::from_raw(p))
    }
}

/// HLSL → the console compiler's container
fn console_compile(c: &IDxcCompiler3, src: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let args: Vec<Vec<u16>> = args.iter().map(|a| a.encode_utf16().chain([0]).collect()).collect();
    let ptrs: Vec<PCWSTR> = args.iter().map(|a| PCWSTR(a.as_ptr())).collect();
    let buf = DxcBuffer { Ptr: src.as_ptr() as *const c_void, Size: src.len(), Encoding: DXC_CP_UTF8.0 };
    unsafe {
        let r: IDxcResult = c.Compile(&buf, Some(&ptrs), None).map_err(|e| format!("Compile: {e}"))?;
        let status = r.GetStatus().map_err(|e| format!("{e}"))?;
        let errors = r
            .GetErrorBuffer()
            .ok()
            .filter(|b| !b.GetBufferPointer().is_null() && b.GetBufferSize() > 0)
            .map(|b| String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())).trim_end_matches('\0').to_string())
            .unwrap_or_default();
        if status.is_err() {
            return Err(format!("console dxc: {errors}"));
        }
        let blob: IDxcBlob = r.GetResult().map_err(|e| format!("{e}"))?;
        Ok(std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()).to_vec())
    }
}

/// The parts of a DXBC container (fourcc, size)
fn parts(blob: &[u8]) -> Vec<(String, usize)> {
    let u = |o: usize| u32::from_le_bytes(blob[o..o + 4].try_into().unwrap()) as usize;
    if blob.len() < 32 || &blob[0..4] != b"DXBC" {
        return Vec::new();
    }
    (0..u(28))
        .map(|i| {
            let o = u(32 + 4 * i);
            (String::from_utf8_lossy(&blob[o..o + 4]).into_owned(), u(o + 4))
        })
        .collect()
}

/// The buffer every kernel works on: inputs, then outputs (byte offsets).
const INTS: u32 = 0;
const FLOATS: u32 = 0x1000;
const OUT: u32 = 0x2000;
const BUF_BYTES: u32 = 0x4000;
/// thread groups of 64 threads
const GROUPS: u32 = 4;

/// The kernel source around `body`: one RWByteAddressBuffer `buf` – a root UAV on the console
/// (its address in the root argument array), u2 (the runner's flat memory) on the PC.
fn kernel(body: &str) -> String {
    format!(
        r#"#ifdef XCL_CONSOLE
RWByteAddressBuffer buf : register(u0);
#define ROOT [RootSignature("UAV(u0)")]
#else
RWByteAddressBuffer buf : register(u2);
#define ROOT
#endif
#define OUT_AT(i) ({OUT}u + 4u * (i))
groupshared uint lds[64];
uint in_u(uint i) {{ return buf.Load({INTS}u + (i & 1023u) * 4u); }}
float in_f(uint i) {{ return asfloat(buf.Load({FLOATS}u + (i & 1023u) * 4u)); }}
void out_u(uint i, uint v) {{ buf.Store(OUT_AT(i), v); }}
void out_f(uint i, float v) {{ out_u(i, asuint(v)); }}
ROOT [numthreads(64, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID, uint3 gtid : SV_GroupThreadID, uint3 gid : SV_GroupID) {{
{body}
}}
"#
    )
}

/// How the two runs' words must agree.
#[derive(Clone, Copy, PartialEq)]
enum Compare {
    Exact,
    /// up to the sign of zero (also of the halves of a word: packed f16): the console's
    /// compiler multiplies by constants with v_mul_legacy_f32 where the HLSL allows it (DXIL
    /// fast math: no signed zeros), and there -0 × c = +0 – on the PC -0
    SignOfZero,
    /// floats, relative error 2e-5 (the console's transcendental and division code)
    Float,
}
use Compare::*;

/// (name, body, how the outputs are compared)
const KERNELS: &[(&str, &str, Compare)] = &[
    (
        "integer arithmetic",
        "uint a = in_u(tid.x), b = in_u(tid.x + 256u), c = in_u(tid.x + 512u); uint o = tid.x * 8u;
         out_u(o, a * b + c);
         out_u(o + 1u, (a >> (b & 31u)) ^ (c << 3u));
         out_u(o + 2u, countbits(a) + firstbithigh(b) * 100u + firstbitlow(c) * 10000u);
         out_u(o + 3u, reversebits(a));
         out_u(o + 4u, (uint)((int)a / max(1, (int)(b & 0xFFFFu))));
         out_u(o + 5u, a % max(1u, b & 0xFFFu));
         out_u(o + 6u, min(a, b) + (uint)max((int)b, (int)c));
         out_u(o + 7u, (uint)(((uint64_t)a * b) >> 32));",
        Exact,
    ),
    (
        "conversions",
        "float x = in_f(tid.x) * 1000.0f; uint a = in_u(tid.x); uint o = tid.x * 8u;
         out_u(o, (uint)(int)x);
         out_f(o + 1u, (float)a);
         out_f(o + 2u, (float)(int)a);
         out_u(o + 3u, f32tof16(x) | f32tof16(-x * 0.001f) << 16);
         out_f(o + 4u, f16tof32(a));
         out_f(o + 5u, floor(x) + ceil(x * 0.5f) * 3.0f);
         out_f(o + 6u, round(x * 0.37f));
         out_f(o + 7u, saturate(x * 0.001f) + trunc(x));",
        SignOfZero,
    ),
    (
        "float math",
        "float x = in_f(tid.x), y = in_f(tid.x + 256u); uint o = tid.x * 8u;
         out_f(o, x * y + 1.0f);
         out_f(o + 1u, sqrt(abs(x)));
         out_f(o + 2u, rsqrt(abs(x) + 1.0f));
         out_f(o + 3u, exp2(x * 0.5f));
         out_f(o + 4u, log2(abs(y) + 1.0f));
         out_f(o + 5u, sin(x) + cos(y));
         out_f(o + 6u, x / (y + 20.0f));
         out_f(o + 7u, frac(x * 3.7f) + min(x, y) - max(x, y));",
        Float,
    ),
    (
        "loops and branches per lane",
        "uint a = in_u(tid.x); uint n = a & 63u; uint acc = tid.x;
         for (uint i = 0u; i < n; i++) {
             acc = acc * 31u + i;
             if ((acc & 7u) == 3u) continue;
             if (acc > 0xF0000000u) break;
             acc ^= in_u(i + tid.x);
         }
         if (a & 1u) acc += 5u; else acc = ~acc;
         out_u(tid.x * 8u, acc);
         out_u(tid.x * 8u + 1u, n);",
        Exact,
    ),
    (
        "groupshared memory and barriers",
        "lds[gtid.x] = in_u(tid.x);
         GroupMemoryBarrierWithGroupSync();
         uint s = 0u;
         for (uint i = 0u; i < 64u; i++) s += lds[(gtid.x + i) & 63u] * (i + 1u);
         GroupMemoryBarrierWithGroupSync();
         lds[63u - gtid.x] = s;
         GroupMemoryBarrierWithGroupSync();
         out_u(tid.x * 8u, lds[gtid.x]);
         out_u(tid.x * 8u + 1u, gid.x);",
        Exact,
    ),
    (
        "atomics",
        "uint a = in_u(tid.x); uint old;
         buf.InterlockedAdd(OUT_AT(a & 15u), 1u, old);
         buf.InterlockedMax(OUT_AT(16u + (a >> 28)), a, old);
         buf.InterlockedXor(OUT_AT(32u + (tid.x & 7u)), a);
         buf.InterlockedOr(OUT_AT(40u), 1u << (a & 31u));
         buf.InterlockedMax(OUT_AT(41u), a | 0x80000000u);",
        Exact,
    ),
    (
        "64-bit integers",
        "uint64_t a = ((uint64_t)in_u(tid.x) << 32) | in_u(tid.x + 256u);
         uint64_t b = ((uint64_t)in_u(tid.x + 512u) << 32) | in_u(tid.x + 768u);
         uint64_t r = (a * b + (a >> 7)) ^ (b << 13);
         int64_t d = (int64_t)a / (int64_t)((b & 0xFFFFFFull) | 1ull);
         uint o = tid.x * 8u;
         out_u(o, (uint)r); out_u(o + 1u, (uint)(r >> 32));
         out_u(o + 2u, (uint)d); out_u(o + 3u, (uint)((uint64_t)d >> 32));
         out_u(o + 4u, (uint)min(a, b)); out_u(o + 5u, (uint)(max(a, b) >> 32));",
        Exact,
    ),
];

/// The inputs: 1024 integers (varied bit patterns), 1024 floats (-10 … 10 and a few special
/// values).
fn inputs() -> Vec<u32> {
    let mut v = vec![0u32; (BUF_BYTES / 4) as usize];
    let mut x = 0x2545_F491u32;
    for i in 0..1024 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        v[(INTS / 4) as usize + i] = match i % 16 {
            0 => 0,
            1 => u32::MAX,
            2 => 0x8000_0000,
            3 => i as u32,
            _ => x,
        };
        let f = match i % 32 {
            0 => 0.0f32,
            1 => -0.0,
            2 => 1.0,
            3 => 1e-30,
            _ => (x >> 8) as f32 / (1u32 << 24) as f32 * 20.0 - 10.0,
        };
        v[(FLOATS / 4) as usize + i] = f.to_bits();
    }
    v
}

/// One kernel through the console compiler + XCL's translation and through the PC compiler:
/// the output part of the buffer after each.
fn run_both(c: &IDxcCompiler3, body: &str) -> Result<(Vec<u32>, Vec<u32>), String> {
    let src = kernel(body);
    let blob = console_compile(c, &src, &["-E", "main", "-T", "cs_6_6", "-enable-16bit-types", "-D", "XCL_CONSOLE=1"])?;
    let translated = pso::console_to_dxil(&blob, pso::Stage::Compute, 0).map_err(|e| format!("translation: {e}"))?;
    // the console run: the buffer at 0x1_0000, its address the one root argument – in user
    // SGPRs or in a table at 0x100, as XCL passes them on a console list's dispatch
    let (table_at, base) = (0x100usize, 0x1_0000usize);
    let layout = console_list::layout_from_blob(&blob).ok_or("no RTS0")?;
    let args = (base as u64).to_le_bytes();
    let data = inputs();
    let mut mem = vec![0u32; (base + BUF_BYTES as usize) / 4];
    for (k, b) in args[layout.inline as usize..].iter().enumerate() {
        mem[(table_at + k) / 4] |= (*b as u32) << (8 * (k % 4));
    }
    mem[base / 4..].copy_from_slice(&data);
    let sgprs = console_list::user_data(&layout, &args, table_at as u64);
    let console = runner::dispatch(&translated.dxil, [GROUPS, 1, 1], &sgprs, &mem)?;
    let pc_dxil = dxc::compile(&src, "main", "cs_6_6", &["-enable-16bit-types"])?;
    let pc = runner::dispatch(&pc_dxil, [GROUPS, 1, 1], &[], &data)?;
    let out = |m: &[u32], at: usize| m[(at + OUT as usize) / 4..(at + BUF_BYTES as usize) / 4].to_vec();
    Ok((out(&console, base), out(&pc, 0)))
}

/// Every kernel of `KERNELS`: the console's code as XCL runs it writes what the PC compile of
/// the same source writes.
#[test]
fn console_compiled_kernels_match_the_pc_compile() {
    let Some(c) = console_compiler() else { return };
    if runner::adapter().is_err() {
        return;
    }
    let mut failed = Vec::new();
    for &(name, body, how) in KERNELS {
        match run_both(&c, body) {
            Err(e) => failed.push(format!("{name}: {e}")),
            Ok((console, pc)) => {
                let zero = |w: u32| {
                    let w = if w == 0x8000_0000 { 0 } else { w };
                    let half = |h: u32| if h == 0x8000 { 0 } else { h };
                    half(w & 0xFFFF) | half(w >> 16) << 16
                };
                let same = |a: u32, b: u32| match how {
                    _ if a == b => true,
                    Exact => false,
                    SignOfZero => zero(a) == zero(b),
                    Float => {
                        let (x, y) = (f32::from_bits(a), f32::from_bits(b));
                        (x.is_nan() && y.is_nan()) || (x - y).abs() <= 2e-5 * y.abs().max(1.0)
                    }
                };
                let bad: Vec<usize> = (0..pc.len()).filter(|&i| !same(console[i], pc[i])).collect();
                println!("{name}: {} of {} words differ", bad.len(), pc.len());
                if let Some(&i) = bad.first() {
                    let mut outputs: Vec<usize> = bad.iter().map(|i| i % 8).collect();
                    outputs.dedup();
                    outputs.sort();
                    outputs.dedup();
                    failed.push(format!("{name}: {} words differ (outputs {outputs:?}), e.g. word {i} (thread {}, output {}): console {:#x}, PC {:#x}", bad.len(), i / 8, i % 8, console[i], pc[i]));
                }
            }
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// Kernel `XCL_KERNEL` (index in `KERNELS`) through the console compiler: its parts, its
/// hardware interface, its disassembly; with `XCL_KERNEL_HLSL` set the translated HLSL.
#[test]
#[ignore]
fn print_a_console_compile() {
    let Some(c) = console_compiler() else { return };
    let which = std::env::var("XCL_KERNEL").ok().and_then(|k| k.parse::<usize>().ok()).unwrap_or(0);
    let src = kernel(KERNELS[which].1);
    let blob = console_compile(&c, &src, &["-E", "main", "-T", "cs_6_6", "-D", "XCL_CONSOLE=1"]).unwrap_or_else(|e| panic!("{e}"));
    println!("{}: {} bytes, parts {:?}", KERNELS[which].0, blob.len(), parts(&blob));
    match xcl::gpu::xbox::parse(&blob) {
        Some(sh) => {
            for f in &sh.fssm {
                println!("FSSM: stages {:?}, programs {}", f.stages.iter().map(|s| xcl::gpu::xbox::hw_stage_name(s.hw)).collect::<Vec<_>>(), f.programs.len());
                if let Some(st) = f.stages.first() {
                    println!("  {:?}", xcl::gpu::recomp::CsInterface::from_descriptor(&f.data, st.offset));
                }
                if let Some(p) = f.programs.first() {
                    for l in xcl::gpu::rdna2::disassemble(&f.data[p.offset..p.offset + p.len], 0, true) {
                        println!("  {:4x}: {}", l.pc, l.text);
                    }
                }
            }
        }
        None => println!("not a console container"),
    }
    match pso::console_to_hlsl(&blob, pso::Stage::Compute) {
        Ok((h, _)) => {
            println!("translated: {} bytes of HLSL", h.len());
            if std::env::var("XCL_KERNEL_HLSL").is_ok() {
                for (i, l) in h.lines().enumerate() {
                    println!("{:4} {l}", i + 1);
                }
            }
        }
        Err(e) => println!("translation: {e}"),
    }
}

/// Where the console puts root arguments: a few root signatures, the first instructions of
/// each compiled kernel and its user SGPR count.
#[test]
#[ignore]
fn print_root_argument_placement() {
    let Some(c) = console_compiler() else { return };
    let cases = [
        ("UAV only", "UAV(u0)", "RWByteAddressBuffer o : register(u0);", "o.Store(tid.x * 4u, tid.x);"),
        ("2 constants, UAV", "RootConstants(num32BitConstants=2, b0), UAV(u0)", "RWByteAddressBuffer o : register(u0); cbuffer k : register(b0) { uint k0; uint k1; };", "o.Store(tid.x * 4u, k0 + k1 * tid.x);"),
        ("UAV, 2 constants", "UAV(u0), RootConstants(num32BitConstants=2, b0)", "RWByteAddressBuffer o : register(u0); cbuffer k : register(b0) { uint k0; uint k1; };", "o.Store(tid.x * 4u, k0 + k1 * tid.x);"),
        ("CBV, UAV", "CBV(b0), UAV(u0)", "RWByteAddressBuffer o : register(u0); cbuffer k : register(b0) { uint k0; uint k1; };", "o.Store(tid.x * 4u, k0 + k1 * tid.x);"),
        ("SRV, UAV", "SRV(t0), UAV(u0)", "RWByteAddressBuffer o : register(u0); ByteAddressBuffer i : register(t0);", "o.Store(tid.x * 4u, i.Load(tid.x * 4u));"),
        ("table UAV", "DescriptorTable(UAV(u0))", "RWByteAddressBuffer o : register(u0);", "o.Store(tid.x * 4u, tid.x);"),
        ("20 constants, UAV", "RootConstants(num32BitConstants=20, b0), UAV(u0)", "RWByteAddressBuffer o : register(u0); cbuffer k : register(b0) { uint4 k[5]; };", "o.Store(tid.x * 4u, k[0].x + k[4].w * tid.x);"),
        ("10 constants, UAV", "RootConstants(num32BitConstants=10, b0), UAV(u0)", "RWByteAddressBuffer o : register(u0); cbuffer k : register(b0) { uint4 k[2]; uint2 k2; };", "o.Store(tid.x * 4u, k[0].x + k2.y * tid.x);"),
        ("11 constants, UAV", "RootConstants(num32BitConstants=11, b0), UAV(u0)", "RWByteAddressBuffer o : register(u0); cbuffer k : register(b0) { uint4 k[2]; uint3 k2; };", "o.Store(tid.x * 4u, k[0].x + k2.z * tid.x);"),
        ("UAV, 11 constants", "UAV(u0), RootConstants(num32BitConstants=11, b0)", "RWByteAddressBuffer o : register(u0); cbuffer k : register(b0) { uint4 k[2]; uint3 k2; };", "o.Store(tid.x * 4u, k[0].x + k2.z * tid.x);"),
        ("6 UAVs", "UAV(u0), UAV(u1), UAV(u2), UAV(u3), UAV(u4), UAV(u5)", "RWByteAddressBuffer o0 : register(u0); RWByteAddressBuffer o1 : register(u1); RWByteAddressBuffer o2 : register(u2); RWByteAddressBuffer o3 : register(u3); RWByteAddressBuffer o4 : register(u4); RWByteAddressBuffer o5 : register(u5);", "o0.Store(tid.x * 4u, 1u); o5.Store(tid.x * 4u, 2u); o3.Store(tid.x * 4u, 3u);"),
    ];
    for (name, rs, decls, body) in cases {
        let src = format!("{decls}\n[RootSignature(\"{rs}\")]\n[numthreads(64, 1, 1)]\nvoid main(uint3 tid : SV_DispatchThreadID) {{ {body} }}\n");
        let blob = match console_compile(&c, &src, &["-E", "main", "-T", "cs_6_6"]) {
            Ok(b) => b,
            Err(e) => {
                println!("{name}: {e}");
                continue;
            }
        };
        let Some(sh) = xcl::gpu::xbox::parse(&blob) else { continue };
        for f in &sh.fssm {
            let cs = f.stages.first().and_then(|st| xcl::gpu::recomp::CsInterface::from_descriptor(&f.data, st.offset));
            println!("{name} [{rs}]: user SGPRs {:?}", cs.map(|c| c.user_sgprs));
            if let Some(p) = f.programs.first() {
                for l in xcl::gpu::rdna2::disassemble(&f.data[p.offset..p.offset + p.len], 0, true).iter().skip(2).take(14) {
                    println!("    {}", l.text);
                }
            }
        }
    }
}
