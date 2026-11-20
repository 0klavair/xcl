//! Real console DLLs from the game package (`XCL_SAMPLE_DLLS` = their folder; skipped when
//! unset) through XCL's loader: mapped, imports resolved (HLE / Windows / the game's own
//! DLLs), TLS + DllMain run – then their code is called.

use std::path::PathBuf;
use std::sync::OnceLock;

use xcl::modules::MappedModule;
use xcl::pe::ExportTarget;

fn setup() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = PathBuf::from(std::env::var_os("XCL_SAMPLE_DLLS")?);
        cap_memory(8 << 30);
        xcl::stubs::init_db(&Default::default());
        xcl::hle::init(&xcl::config::Config::default());
        xcl::resolver::set_search_dirs(vec![dir.clone()]);
        let _ = xcl::eh::install_pc_hook();
        xcl::eh::install_crash_handler();
        Some(dir)
    })
    .as_ref()
}

/// Limit this test process's committed memory: a runaway shader compile (DXC / driver) on
/// a big translated program once exhausted the machine's commit and took the user's session
/// down – with the cap only the test fails.
fn cap_memory(bytes: usize) {
    use windows_sys::Win32::System::JobObjects::*;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        info.ProcessMemoryLimit = bytes;
        let ok = SetInformationJobObject(job, JobObjectExtendedLimitInformation, &info as *const _ as *const _, std::mem::size_of_val(&info) as u32) != 0
            && AssignProcessToJobObject(job, GetCurrentProcess()) != 0;
        if !ok {
            eprintln!("cap_memory: nie udało się ustawić limitu pamięci");
        }
    }
}

fn load(dir: &std::path::Path, name: &str) -> std::sync::Arc<MappedModule> {
    xcl::loader::load_game_dll(&dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn proc(m: &MappedModule, name: &str) -> usize {
    match m.exports.by_name.get(name) {
        Some(ExportTarget::Rva(r)) => m.base + *r as usize,
        other => panic!("{}!{name}: {other:?}", m.name),
    }
}

#[test]
fn console_crt_dlls_load_and_run() {
    let Some(dir) = setup() else { return };
    unsafe {
        let vcrt = load(dir, "vcruntime140.dll");
        let memcpy: extern "C" fn(*mut u8, *const u8, usize) -> *mut u8 = std::mem::transmute(proc(&vcrt, "memcpy"));
        let src: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
        let mut dst = vec![0u8; 1000];
        memcpy(dst.as_mut_ptr(), src.as_ptr(), 1000);
        assert_eq!(dst, src);

        let cpp = load(dir, "msvcp140.dll");
        let hw: extern "C" fn() -> u32 = std::mem::transmute(proc(&cpp, "_Thrd_hardware_concurrency"));
        let cores = std::thread::available_parallelism().unwrap().get() as u32;
        assert_eq!(hw(), cores, "console msvcp140 _Thrd_hardware_concurrency");
        // std::mutex storage (80 B on x64), plain mutex
        let init: extern "C" fn(*mut u8, i32) = std::mem::transmute(proc(&cpp, "_Mtx_init_in_situ"));
        let lock: extern "C" fn(*mut u8) -> i32 = std::mem::transmute(proc(&cpp, "_Mtx_lock"));
        let unlock: extern "C" fn(*mut u8) -> i32 = std::mem::transmute(proc(&cpp, "_Mtx_unlock"));
        let destroy: extern "C" fn(*mut u8) = std::mem::transmute(proc(&cpp, "_Mtx_destroy_in_situ"));
        let mut mtx = vec![0u8; 256];
        init(mtx.as_mut_ptr(), 1);
        // two threads bump a counter under the console mutex
        let (m, counter) = (mtx.as_mut_ptr() as usize, Box::leak(Box::new(0u64)) as *mut u64 as usize);
        let workers: Vec<_> = (0..2)
            .map(|_| {
                std::thread::spawn(move || {
                    for _ in 0..20000 {
                        lock(m as *mut u8);
                        *(counter as *mut u64) += 1;
                        unlock(m as *mut u8);
                    }
                })
            })
            .collect();
        workers.into_iter().for_each(|w| w.join().unwrap());
        assert_eq!(*(counter as *const u64), 40000);
        destroy(mtx.as_mut_ptr());

        let omp = load(dir, "vcomp140.dll");
        let procs: extern "C" fn() -> i32 = std::mem::transmute(proc(&omp, "omp_get_num_procs"));
        assert_eq!(procs() as u32, cores, "console vcomp140 omp_get_num_procs");

        let concrt = load(dir, "concrt140.dll");
        let n: extern "C" fn() -> u32 = std::mem::transmute(proc(&concrt, "?GetProcessorCount@Concurrency@@YAIXZ"));
        assert_eq!(n(), cores, "console concrt140 GetProcessorCount");
    }
}

#[test]
fn other_console_dlls_map_and_initialise() {
    let Some(dir) = setup() else { return };
    for name in ["msvcp140_1.dll", "msvcp140_2.dll", "msvcp140_atomic_wait.dll", "msvcp140_codecvt_ids.dll", "vccorlib140.dll", "vcruntime140_1.dll", "Bink2Scarlett.dll", "XCurl.dll", "libHttpClient.GDK.dll", "amd_fidelityfx_loader_xbox.dll", "amd_fidelityfx_upscaler_xbox.dll", "XboxBvhBuilder.dll"] {
        let m = load(dir, name);
        assert!(m.initialized.load(std::sync::atomic::Ordering::Acquire), "{name}: DllMain");
    }
}

#[repr(C)]
struct GeometryDesc {
    ty: u32,
    flags: u32,
    transform: u64,
    index_format: u32,
    vertex_format: u32,
    index_count: u32,
    vertex_count: u32,
    index_buffer: u64,
    vertex_buffer: u64,
    vertex_stride: u64,
}

#[repr(C)]
struct BuildInputs {
    ty: u32,
    flags: u32,
    num_descs: u32,
    layout: u32,
    descs: u64,
}

/// Worst-case result size the console builder reports, as measured: a 256-byte header,
/// n leaves, max(n - 1, 1) box32 nodes (128 B), a 4-byte parent pointer per 64 bytes of
/// nodes, rounded up to 1 KB. Leaves: triangle 64 B (bottom level), instance 128 B (top).
fn console_bvh_size(n: u64, leaf: u64) -> u64 {
    let nodes = n * leaf + (n.max(2) - 1) * 128;
    (256 + nodes + nodes / 64 * 4).div_ceil(1024) * 1024
}

/// The console's own BVH builder sizes acceleration structures on the CPU: result / scratch
/// bytes for bottom levels of n triangles and top levels of n instances (XCL_PRINT=1 prints).
/// The sizes pin down the node sizes of the console format (`console_bvh_size`).
#[test]
fn xbox_bvh_builder_prebuild_sizes() {
    let Some(dir) = setup() else { return };
    let m = load(dir, "XboxBvhBuilder.dll");
    let f: extern "system" fn(*mut u8, *const BuildInputs, *const u8, *mut [u64; 3]) = unsafe {
        std::mem::transmute(proc(&m, "?GetRaytracingAccelerationStructurePrebuildInfo@XboxBvhBuilder@@QEAAXPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS@@PEBUD3D12XBOX_XDXR_STREAM_DESC@@PEAUD3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO@@@Z"))
    };
    // the method only reads settings from the object; a zeroed one = defaults
    let mut this = vec![0u8; 0x10000];
    let print = std::env::var_os("XCL_PRINT").is_some();
    let mut size = |inputs: &BuildInputs| {
        let mut out = [0u64; 3];
        f(this.as_mut_ptr(), inputs, std::ptr::null(), &mut out);
        out
    };
    for n in [1u32, 2, 3, 4, 5, 8, 16, 64, 256, 1024, 4096] {
        let g = GeometryDesc { ty: 0, flags: 1, transform: 0, index_format: 0, vertex_format: 6, index_count: 0, vertex_count: 3 * n, index_buffer: 0, vertex_buffer: 0x1000, vertex_stride: 12 };
        let r = size(&BuildInputs { ty: 1, flags: 0, num_descs: 1, layout: 0, descs: &g as *const GeometryDesc as u64 });
        if print {
            println!("BLAS {n:5} tri: result {:8} scratch {:8} update {:8}  ({:.1} B/tri)", r[0], r[1], r[2], r[0] as f64 / n as f64);
        }
        assert_eq!(r[0], console_bvh_size(n as u64, 64), "bottom level, {n} triangles");
    }
    for n in [1u32, 2, 4, 16, 256, 4096] {
        let r = size(&BuildInputs { ty: 0, flags: 0, num_descs: n, layout: 0, descs: 0x10000 });
        if print {
            println!("TLAS {n:5} inst: result {:8} scratch {:8} update {:8}  ({:.1} B/inst)", r[0], r[1], r[2], r[0] as f64 / n as f64);
        }
        assert_eq!(r[0], console_bvh_size(n as u64, 128), "top level, {n} instances");
    }
}

/// The console's own Bink 2 library decodes a movie from the game (`XCL_SAMPLE_BK2`) on the
/// CPU: open, decode every frame to 32-bit pixels, the picture must change and not be flat.
/// `XCL_BINK_DUMP=<file.ppm>` saves the middle frame.
#[test]
fn bink2_console_dll_decodes_a_game_movie() {
    let Some(dir) = setup() else { return };
    let Some(movie) = std::env::var_os("XCL_SAMPLE_BK2") else { return };
    let m = load(dir, "Bink2Scarlett.dll");
    unsafe {
        let open: extern "C" fn(*const u8, u32) -> *mut u32 = std::mem::transmute(proc(&m, "BinkOpen"));
        let error: extern "C" fn() -> *const std::ffi::c_char = std::mem::transmute(proc(&m, "BinkGetError"));
        let do_frame: extern "C" fn(*mut u32) -> i32 = std::mem::transmute(proc(&m, "BinkDoFrame"));
        let next: extern "C" fn(*mut u32) = std::mem::transmute(proc(&m, "BinkNextFrame"));
        let copy: extern "C" fn(*mut u32, *mut u8, i32, u32, u32, u32, u32) -> i32 = std::mem::transmute(proc(&m, "BinkCopyToBuffer"));
        let close: extern "C" fn(*mut u32) = std::mem::transmute(proc(&m, "BinkClose"));
        let path = format!("{}\0", movie.to_string_lossy());
        let b = open(path.as_ptr(), 0);
        assert!(!b.is_null(), "BinkOpen: {:?}", std::ffi::CStr::from_ptr(error()));
        let (w, h, frames) = (*b, *b.add(1), *b.add(2));
        assert!(w >= 16 && h >= 16 && frames > 0, "{w}x{h}, {frames} frames");
        let mut px = vec![0u8; (w * h * 4) as usize];
        let (mut changes, mut prev, mut mid) = (0, vec![], vec![]);
        for f in 0..frames {
            do_frame(b);
            copy(b, px.as_mut_ptr(), (w * 4) as i32, h, 0, 0, 3); // BINKSURFACE32
            if f > 0 && px != prev {
                changes += 1;
            }
            if f == frames / 2 {
                mid = px.clone();
            }
            prev.clone_from(&px);
            if f + 1 < frames {
                next(b);
            }
        }
        close(b);
        // the middle frame has a picture: many distinct colours, not one flat fill
        let mut colours: Vec<u32> = mid.chunks(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], 0])).collect();
        colours.sort_unstable();
        colours.dedup();
        if let Some(out) = std::env::var_os("XCL_BINK_DUMP") {
            let mut ppm = format!("P6 {w} {h} 255\n").into_bytes();
            mid.chunks(4).for_each(|c| ppm.extend([c[2], c[1], c[0]]));
            std::fs::write(out, ppm).unwrap();
        }
        println!("Bink: {w}x{h}, {frames} klatek, zmian między klatkami {changes}, kolorów w środkowej {}", colours.len());
        assert!(changes > frames / 4, "frames change: {changes} of {frames}");
        assert!(colours.len() > 500, "middle frame: {} colours", colours.len());
    }
}

#[repr(C)]
struct BuildDesc {
    dest: u64,
    inputs: BuildInputs,
    source: u64,
    scratch: u64,
}

/// Page-aligned zeroed memory (the builder uses aligned vector loads).
fn pages(n: usize) -> &'static mut [u8] {
    use windows_sys::Win32::System::Memory::*;
    unsafe {
        let p = VirtualAlloc(std::ptr::null(), n, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) as *mut u8;
        assert!(!p.is_null());
        std::slice::from_raw_parts_mut(p, n)
    }
}

/// Game geometry from `core_bvh4.rpf` (`XCL_SAMPLE_BVH4` = the package): one physical chunk
/// (zlib at 0xa4e26070) holds at +0x100 a compressed bottom level (tag 0x81020006: tree,
/// boxes and primitive pairing, no vertices). The console builder's own CPU `Rehydrate…X`
/// expands it with a synthetic vertex buffer (vertex i = (i, -i, i / 2)); the output must be the
/// GPURT gfx10 layout XCL emulates: root @0x30, box16 / box32 nodes, triangle pairs
/// (v0..v3, primitives @52 / @56, triangle id @60) with every primitive exactly once.
#[test]
fn xbox_bvh_builder_rehydrates_game_geometry() {
    let Some(dir) = setup() else { return };
    let Some(pack) = std::env::var_os("XCL_SAMPLE_BVH4") else { return };
    let m = load(dir, "XboxBvhBuilder.dll");
    let chunk = {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(pack).unwrap();
        f.seek(SeekFrom::Start(0xa4e26070)).unwrap();
        let mut z = vec![0u8; 1 << 20];
        f.read_exact(&mut z).unwrap();
        let mut d = miniz_oxide::inflate::core::DecompressorOxide::new();
        let mut out = vec![0u8; 1 << 20];
        let flags = miniz_oxide::inflate::core::inflate_flags::TINFL_FLAG_PARSE_ZLIB_HEADER | miniz_oxide::inflate::core::inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
        let (st, _, n) = miniz_oxide::inflate::core::decompress(&mut d, &z, &mut out, 0, flags);
        assert_eq!(st, miniz_oxide::inflate::TINFLStatus::Done);
        out.truncate(n);
        out
    };
    assert_eq!(&chunk[0x100..0x104], &[0x06, 0x00, 0x02, 0x81], "compressed bottom level tag");
    let src = pages(chunk.len() + 0x1000);
    src[..chunk.len()].copy_from_slice(&chunk);
    let hdr = |at: usize| u32::from_le_bytes(src[0x100 + at..0x104 + at].try_into().unwrap());
    let (prims, tri_nodes) = (hdr(4), hdr(0xC) as usize);
    let scale = [0, 1, 2].map(|k| (2.0f64).powi(src[0x100 + 0x3C + k] as i32 - 127));
    let dest = pages(16 << 20);
    const VERTS: usize = 1 << 16;
    let vb = pages(VERTS * 12);
    for i in 0..VERTS {
        for (k, c) in [i as f32, -(i as f32), i as f32 / 2.0].iter().enumerate() {
            vb[12 * i + 4 * k..12 * i + 4 * k + 4].copy_from_slice(&c.to_le_bytes());
        }
    }
    // primitives carry their geometry's index; identical geometries cover any of them
    let geoms: Vec<GeometryDesc> = (0..1024)
        .map(|_| GeometryDesc { ty: 0, flags: 1, transform: 0, index_format: 0, vertex_format: 6, index_count: 0, vertex_count: VERTS as u32, index_buffer: 0, vertex_buffer: vb.as_ptr() as u64, vertex_stride: 12 })
        .collect();
    let desc = BuildDesc {
        dest: dest.as_ptr() as u64,
        inputs: BuildInputs { ty: 1, flags: 0, num_descs: geoms.len() as u32, layout: 0, descs: geoms.as_ptr() as u64 },
        source: src.as_ptr() as u64 + 0x100,
        scratch: 0,
    };
    let mut this = vec![0u8; 0x10000];
    let rehydrate: extern "system" fn(*mut u8, *mut u8, *const BuildDesc) -> i32 = unsafe {
        std::mem::transmute(proc(&m, "?RehydrateRaytracingAccelerationStructureX@XboxBvhBuilder@@QEAAJPEAUID3D12Device11@@PEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@@Z"))
    };
    assert_eq!(rehydrate(this.as_mut_ptr(), std::ptr::null_mut(), &desc), 0, "Rehydrate HRESULT");
    if let Some(out) = std::env::var_os("XCL_BVH_DUMP") {
        let used = dest.iter().rposition(|&b| b != 0).map_or(0, |p| p + 1);
        std::fs::write(out, &dest[..used]).unwrap();
    }
    let word = |at: usize| u32::from_le_bytes(dest[at..at + 4].try_into().unwrap());
    assert_eq!(word(0) >> 24 & 1, 0, "rehydrated: compressed bit cleared");
    let root = word(0x30);
    assert!(matches!(root & 7, 4 | 5), "root {root:#x} is a box");
    // vertex index from the synthetic position (positions come out scaled per axis)
    let f = |at: usize| f32::from_bits(word(at)) as f64;
    let vertex = |at: usize| -> usize {
        let i = f(at) / scale[0];
        let node = at & !63;
        assert!(i.fract() == 0.0 && f(at + 4) == -i * scale[1] && f(at + 8) == i / 2.0 * scale[2], "vertex @{at:#x}: node {:?} words {:x?}", (0..12).map(|k| f(node + 4 * k) / scale[k % 3]).collect::<Vec<_>>(), (12..16).map(|k| word(node + 4 * k)).collect::<Vec<_>>());
        i as usize
    };
    // @48 = geometry << 8 | second triangle << 7 | opaque; primitive indices count within their
    // geometry, so (geometry, primitive) is unique
    let (mut seen_prims, mut seen_nodes, mut boxes) = (std::collections::BTreeMap::<(u32, usize), u32>::new(), std::collections::BTreeSet::new(), [0usize; 2]);
    let mut todo = vec![root];
    while let Some(n) = todo.pop() {
        let at = ((n & !7) as usize) << 3;
        match n & 7 {
            4 | 5 => {
                boxes[(n & 7) as usize - 4] += 1;
                todo.extend((0..4).map(|k| word(at + 4 * k)).filter(|&c| c != u32::MAX));
            }
            t @ 0..=1 => {
                seen_nodes.insert(at);
                let id = word(at + 60) >> (8 * t);
                let (isrc, jsrc) = ((id & 3) as usize, (id >> 2 & 3) as usize);
                if t == 0 {
                    // @48 bit 7: a second triangle – the game's shader then tests the node again
                    // as type 1 (`v76 = [node+48] >> 7`, `next = node | 1`)
                    if word(at + 48) & 0x80 != 0 {
                        todo.push(n | 1);
                    }
                    let p = word(at + 52) as usize;
                    *seen_prims.entry((word(at + 48) >> 8, p)).or_default() += 1;
                    let v = [vertex(at), vertex(at + 12), vertex(at + 24)];
                    let mut sorted = v;
                    sorted.sort_unstable();
                    assert_eq!(sorted, [3 * p, 3 * p + 1, 3 * p + 2], "triangle 0 of node {n:#x}");
                    assert_eq!((v[isrc], v[jsrc]), (3 * p + 1, 3 * p + 2), "I / J source of node {n:#x}");
                } else {
                    let q = word(at + 56) as usize;
                    *seen_prims.entry((word(at + 48) >> 8, q)).or_default() += 1;
                    // triangle 1 = (v1, v3, v2): only v3 is its own vertex
                    let v3 = vertex(at + 36);
                    assert!((3 * q..3 * q + 3).contains(&v3), "triangle 1 of node {n:#x}");
                    if v3 == 3 * q + 1 {
                        assert_eq!(isrc, 1, "I source of node {n:#x}");
                    }
                    if v3 == 3 * q + 2 {
                        assert_eq!(jsrc, 1, "J source of node {n:#x}");
                    }
                }
            }
            t => panic!("unexpected node type {t} ({n:#x})"),
        }
    }
    println!("Rehydrate: {} węzłów trójkątów, box32 {}, box16 {}", seen_nodes.len(), boxes[1], boxes[0]);
    assert_eq!(seen_nodes.len(), tri_nodes, "triangle nodes = header @0x0C");
    assert!(seen_prims.values().all(|&c| c == 1), "every (geometry, primitive) exactly once");
    // every geometry's primitives 0..n are all there
    let mut per_geometry = std::collections::BTreeMap::<u32, Vec<usize>>::new();
    for &(g, p) in seen_prims.keys() {
        per_geometry.entry(g).or_default().push(p);
    }
    for (g, ps) in &per_geometry {
        assert!(ps.iter().enumerate().all(|(i, &p)| i == p), "geometry {g}: primitives 0..{} without gaps", ps.len());
    }
    let counts: Vec<usize> = per_geometry.values().map(Vec::len).collect();
    println!("geometrie: {} z trójkątami {counts:?} (razem {}; pole nagłówka @0x04 = {prims})", counts.len(), seen_prims.len());
}

/// Probe: the console BVH builder constructed on XCL's D3D12.X device (real backend) – what
/// it asks the device for (root signatures, compute pipelines from its console shaders) and
/// whether it survives. `XCL_PROBE_BVH_DEVICE=1` runs it.
#[test]
fn xbox_bvh_builder_on_xcl_device_probe() {
    let Some(dir) = setup() else { return };
    if std::env::var_os("XCL_PROBE_BVH_DEVICE").is_none() {
        return;
    }
    let m = load(dir, "XboxBvhBuilder.dll");
    unsafe {
        // IID_ID3D12Device
        let iid: [u8; 16] = [0xf1, 0x19, 0x98, 0x18, 0xb6, 0x1d, 0x57, 0x4b, 0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7];
        let mut dev: *mut std::ffi::c_void = std::ptr::null_mut();
        let hr = xcl::hle::d3d12x::D3D12XboxCreateDevice(std::ptr::null_mut(), std::ptr::null(), iid.as_ptr() as _, &mut dev);
        println!("D3D12XboxCreateDevice {hr:#x} {dev:?}");
        assert!(hr >= 0 && !dev.is_null());
        let ctor: extern "system" fn(*mut u8, *mut std::ffi::c_void, bool) -> *mut u8 = std::mem::transmute(proc(&m, "??0XboxBvhBuilder@@QEAA@PEAUID3D12Device11@@_N@Z"));
        let this = pages(0x10000);
        ctor(this.as_mut_ptr(), dev, false);
        println!("XboxBvhBuilder skonstruowany");
        if std::env::var_os("XCL_PROBE_BVH_RECORD").is_none() {
            return;
        }
        // record (not execute) a bottom-level build of 64 triangles into a command list made
        // on XCL's device; buffers in the unified memory (console GPU address = CPU address)
        use windows::core::Interface;
        use windows::Win32::Graphics::Direct3D12::*;
        let device = ID3D12Device::from_raw_borrowed(&dev).unwrap();
        let alloc: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).unwrap();
        let uma = xcl::gpu::uma::get().expect("UMA");
        let n = 64u32;
        let vb = uma.alloc(n as usize * 36, 256).unwrap();
        for (i, v) in std::slice::from_raw_parts_mut(vb as *mut f32, n as usize * 9).iter_mut().enumerate() {
            *v = (i % 9) as f32 * 0.1 + (i / 9) as f32;
        }
        let geom = GeometryDesc { ty: 0, flags: 1, transform: 0, index_format: 0, vertex_format: 6, index_count: 0, vertex_count: 3 * n, index_buffer: 0, vertex_buffer: vb as u64, vertex_stride: 12 };
        let inputs = BuildInputs { ty: 1, flags: 0, num_descs: 1, layout: 0, descs: &geom as *const GeometryDesc as u64 };
        let prebuild: extern "system" fn(*mut u8, *const BuildInputs, *const u8, *mut [u64; 3]) = std::mem::transmute(proc(&m, "?GetRaytracingAccelerationStructurePrebuildInfo@XboxBvhBuilder@@QEAAXPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS@@PEBUD3D12XBOX_XDXR_STREAM_DESC@@PEAUD3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO@@@Z"));
        let mut sizes = [0u64; 3];
        prebuild(this.as_mut_ptr(), &inputs, std::ptr::null(), &mut sizes);
        let dest = uma.alloc(sizes[0] as usize, 256).unwrap();
        let scratch = uma.alloc(sizes[1] as usize, 256).unwrap();
        println!("rozmiary: wynik {} B, scratch {} B; dest {dest:?}, scratch {scratch:?}", sizes[0], sizes[1]);
        let desc = BuildDesc { dest: dest as u64, inputs, source: 0, scratch: scratch as u64 };
        let build: extern "system" fn(*mut u8, *mut std::ffi::c_void, *const BuildDesc, u32, *const u8) = std::mem::transmute(proc(
            &m,
            "?BuildRaytracingAccelerationStructure@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList4@@PEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@IPEBUD3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC@@@Z",
        ));
        build(this.as_mut_ptr(), list.as_raw(), &desc, 0, std::ptr::null());
        list.Close().unwrap();
        println!("budowa BLAS nagrana na liście poleceń (niewykonana)");
    }
}

/// The memory cap of `setup` holds: committing 9 GB in this process fails.
#[test]
fn memory_cap_is_active() {
    if setup().is_none() {
        return;
    }
    use windows_sys::Win32::System::Memory::*;
    unsafe {
        let p = VirtualAlloc(std::ptr::null(), 9 << 30, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if !p.is_null() {
            VirtualFree(p, 0, MEM_RELEASE);
        }
        assert!(p.is_null(), "a 9 GB commit went through – the job memory cap is not active");
    }
}

// ---------------------------------------------------------------------------------------
// Fake console command list: what D3D12.X code inlined into a title writes into the list
// object and which methods it calls (`XCL_PROBE_BVH_FAKELIST=1`).
// ---------------------------------------------------------------------------------------

static FAKE_SLOT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static FAKE_CALLS: std::sync::Mutex<Vec<(u32, [usize; 6])>> = std::sync::Mutex::new(Vec::new());
static FAKE_BUMP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

extern "system" fn fake_method(this: usize, a: usize, b: usize, c: usize, d: usize, e: usize) -> usize {
    let slot = FAKE_SLOT.load(std::sync::atomic::Ordering::Relaxed);
    FAKE_CALLS.lock().unwrap().push((slot, [this, a, b, c, d, e]));
    match slot {
        // AllocateUploadMemory(size, alignment) → CPU pointer (UMA: = GPU address)
        148 => {
            let align = b.max(16);
            let at = FAKE_BUMP.load(std::sync::atomic::Ordering::Relaxed).div_ceil(align) * align;
            FAKE_BUMP.store(at + a, std::sync::atomic::Ordering::Relaxed);
            at
        }
        _ => 0,
    }
}

/// vtable of `n` thunks: `mov dword [FAKE_SLOT], slot ; jmp fake_method`
fn fake_vtable(n: usize) -> Vec<usize> {
    use windows_sys::Win32::System::Memory::*;
    let per = 32usize;
    unsafe {
        let code = VirtualAlloc(std::ptr::null(), n * per, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) as *mut u8;
        (0..n)
            .map(|i| {
                let mut c = vec![0x48, 0xB8];
                c.extend_from_slice(&(&FAKE_SLOT as *const _ as u64).to_le_bytes()); // mov rax, &FAKE_SLOT
                c.extend_from_slice(&[0xC7, 0x00]);
                c.extend_from_slice(&(i as u32).to_le_bytes()); // mov dword [rax], i
                c.extend_from_slice(&[0x48, 0xB8]);
                c.extend_from_slice(&(fake_method as *const () as usize as u64).to_le_bytes()); // mov rax, fake_method
                c.extend_from_slice(&[0xFF, 0xE0]); // jmp rax
                assert!(c.len() <= per);
                let p = code.add(i * per);
                std::ptr::copy_nonoverlapping(c.as_ptr(), p, c.len());
                p as usize
            })
            .collect()
    }
}

#[test]
fn xbox_bvh_builder_records_into_a_fake_console_list() {
    let Some(dir) = setup() else { return };
    if std::env::var_os("XCL_PROBE_BVH_FAKELIST").is_none() {
        return;
    }
    let m = load(dir, "XboxBvhBuilder.dll");
    unsafe {
        let iid: [u8; 16] = [0xf1, 0x19, 0x98, 0x18, 0xb6, 0x1d, 0x57, 0x4b, 0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7];
        let mut dev: *mut std::ffi::c_void = std::ptr::null_mut();
        assert!(xcl::hle::d3d12x::D3D12XboxCreateDevice(std::ptr::null_mut(), std::ptr::null(), iid.as_ptr() as _, &mut dev) >= 0);
        let ctor: extern "system" fn(*mut u8, *mut std::ffi::c_void, bool) -> *mut u8 = std::mem::transmute(proc(&m, "??0XboxBvhBuilder@@QEAA@PEAUID3D12Device11@@_N@Z"));
        let this = pages(0x10000);
        ctor(this.as_mut_ptr(), dev, false);
        let uma = xcl::gpu::uma::get().expect("UMA");
        // the fake list: vtable, then a zeroed console-sized body; +0x18 / +0x30 = record buffer
        let vt = Box::leak(fake_vtable(600).into_boxed_slice());
        let list = pages(0x10000);
        let records = uma.alloc(1 << 20, 256).unwrap() as usize;
        let lp = list.as_mut_ptr();
        let w = |at: usize, v: usize| (lp.add(at) as *mut usize).write(v);
        w(0, vt.as_ptr() as usize);
        w(0x18, records);
        w(0x30, records + (1 << 20));
        FAKE_BUMP.store(uma.alloc(4 << 20, 4096).unwrap() as usize, std::sync::atomic::Ordering::Relaxed);
        let snapshot = list.to_vec();
        // a bottom level of 64 triangles
        let n = 64u32;
        let vb = uma.alloc(n as usize * 36, 256).unwrap();
        for (i, v) in std::slice::from_raw_parts_mut(vb as *mut f32, n as usize * 9).iter_mut().enumerate() {
            *v = (i % 9) as f32 * 0.1 + (i / 9) as f32;
        }
        let geom = GeometryDesc { ty: 0, flags: 1, transform: 0, index_format: 0, vertex_format: 6, index_count: 0, vertex_count: 3 * n, index_buffer: 0, vertex_buffer: vb as u64, vertex_stride: 12 };
        let inputs = BuildInputs { ty: 1, flags: 0, num_descs: 1, layout: 0, descs: &geom as *const GeometryDesc as u64 };
        let dest = uma.alloc(13312, 256).unwrap();
        let scratch = uma.alloc(23872, 256).unwrap();
        let desc = BuildDesc { dest: dest as u64, inputs, source: 0, scratch: scratch as u64 };
        println!("vb {vb:?} dest {dest:?} scratch {scratch:?} rekordy {records:#x} upload od {:#x}", FAKE_BUMP.load(std::sync::atomic::Ordering::Relaxed));
        let build: extern "system" fn(*mut u8, *mut u8, *const BuildDesc, u32, *const u8) = std::mem::transmute(proc(
            &m,
            "?BuildRaytracingAccelerationStructure@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList4@@PEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@IPEBUD3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC@@@Z",
        ));
        build(this.as_mut_ptr(), list.as_mut_ptr(), &desc, 0, std::ptr::null());
        println!("== wywołania metod listy");
        for (slot, a) in FAKE_CALLS.lock().unwrap().iter() {
            println!("  #{slot:<4} {:x?}", &a[1..]);
        }
        println!("== zmienione pola obiektu listy (offset: przed → po)");
        for k in (0..0x10000).step_by(8) {
            let (b, a) = (u64::from_le_bytes(snapshot[k..k + 8].try_into().unwrap()), u64::from_le_bytes(list[k..k + 8].try_into().unwrap()));
            if a != b {
                println!("  +{k:#06x}: {b:#x} → {a:#x}");
            }
        }
        let end = (list.as_ptr().add(0x18) as *const usize).read();
        // what the pointers in the records are: the vtable each points to
        println!("== urządzenie {dev:?}");
        let mut seen = std::collections::BTreeSet::new();
        let rec64 = std::slice::from_raw_parts(records as *const u32, (end - records) / 4);
        for w in rec64.windows(2) {
            let p = (w[0] as usize) | (w[1] as usize) << 32;
            if (0x1_0000_0000..0x7FFF_0000_0000).contains(&p) && w[1] != 4 && seen.insert(p) {
                let vt = (p as *const usize).read();
                println!("  wskaźnik {p:#x}: vtable {vt:#x} = {}", xcl::modules::describe_address(vt));
            }
        }
        println!("== bufor rekordów: {} B", end - records);
        let rec = std::slice::from_raw_parts(records as *const u32, (end - records) / 4);
        for (i, ch) in rec.chunks(8).enumerate().take(64) {
            println!("  {:#06x}: {:08x?}", i * 32, ch);
        }
    }
}
