//! In-process HLSL → DXIL compilation with DXC.
//!
//! `dxcompiler.dll` is not part of Windows, so it is loaded at run time from
//! `%XCL_DXC_DIR%`, the folder of xcl.exe, or the newest Windows SDK
//! (`Windows Kits\10\bin\<ver>\x64`). `dxil.dll` from the same folder is loaded first so
//! the compiler can sign the DXIL (unsigned DXIL is rejected by D3D12).

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows::core::{Interface, GUID, HRESULT, PCWSTR};
use windows::Win32::Graphics::Direct3D::Dxc::{DxcBuffer, IDxcBlob, IDxcCompiler3, IDxcResult, CLSID_DxcCompiler, DXC_CP_UTF8};

type CreateInstance = unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

fn candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = std::env::var_os("XCL_DXC_DIR") {
        v.push(PathBuf::from(d));
    }
    if let Some(d) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        v.push(d);
    }
    for root in [r"C:\Program Files (x86)\Windows Kits\10\bin", r"C:\Program Files\Windows Kits\10\bin"] {
        if let Ok(rd) = std::fs::read_dir(root) {
            let mut vers: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
            vers.sort();
            for ver in vers.into_iter().rev() {
                v.push(ver.join("x64"));
            }
        }
    }
    v
}

static LIB: OnceLock<Result<(PathBuf, usize), String>> = OnceLock::new();

/// Folder with dxcompiler.dll (+ dxil.dll) that will be used.
pub fn location() -> Result<&'static Path, String> {
    load().map(|(p, _)| p.as_path())
}

fn load() -> Result<&'static (PathBuf, usize), String> {
    LIB.get_or_init(|| {
        use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
        for dir in candidates() {
            let dxc = dir.join("dxcompiler.dll");
            if !dxc.is_file() {
                continue;
            }
            unsafe {
                let dxil = dir.join("dxil.dll");
                if dxil.is_file() {
                    LoadLibraryW(crate::util::wide(&dxil.to_string_lossy()).as_ptr());
                }
                let h = LoadLibraryW(crate::util::wide(&dxc.to_string_lossy()).as_ptr());
                if h.is_null() {
                    continue;
                }
                if let Some(f) = GetProcAddress(h, b"DxcCreateInstance\0".as_ptr()) {
                    return Ok((dir, f as usize));
                }
            }
        }
        Err("nie znaleziono dxcompiler.dll (ustaw XCL_DXC_DIR albo zainstaluj Windows SDK)".into())
    })
    .as_ref()
    .map_err(|e| e.clone())
}

fn create_fn() -> Result<CreateInstance, String> {
    let &(_, f) = load()?;
    Ok(unsafe { std::mem::transmute::<usize, CreateInstance>(f) })
}

thread_local! {
    static COMPILER: RefCell<Option<IDxcCompiler3>> = const { RefCell::new(None) };
}

fn compiler() -> Result<IDxcCompiler3, String> {
    COMPILER.with(|c| {
        if let Some(x) = c.borrow().as_ref() {
            return Ok(x.clone());
        }
        let create = create_fn()?;
        let mut p = std::ptr::null_mut();
        let hr = unsafe { create(&CLSID_DxcCompiler, &IDxcCompiler3::IID, &mut p) };
        if hr.is_err() || p.is_null() {
            return Err(format!("DxcCreateInstance: {hr:?}"));
        }
        let x = unsafe { IDxcCompiler3::from_raw(p) };
        *c.borrow_mut() = Some(x.clone());
        Ok(x)
    })
}

/// Compile HLSL to signed DXIL. `profile` e.g. "cs_6_6"; `extra` are dxc arguments.
pub fn compile(src: &str, entry: &str, profile: &str, extra: &[&str]) -> Result<Vec<u8>, String> {
    let c = compiler()?;
    let mut args: Vec<Vec<u16>> = ["-E", entry, "-T", profile, "-HV", "2021"].iter().map(|a| crate::util::wide(a)).collect();
    // DXC's optimiser is superlinear on big translated programs (1.2 MB HLSL: 140 s at
    // -O1..-O3, 5 s at -O0); the driver optimises the DXIL again anyway
    if src.len() > 256 * 1024 && !extra.iter().any(|a| a.starts_with("-O")) {
        args.push(crate::util::wide("-O0"));
    }
    args.extend(extra.iter().map(|a| crate::util::wide(a)));
    let ptrs: Vec<PCWSTR> = args.iter().map(|a| PCWSTR(a.as_ptr())).collect();
    let buf = DxcBuffer { Ptr: src.as_ptr() as *const c_void, Size: src.len(), Encoding: DXC_CP_UTF8.0 };
    unsafe {
        let r: IDxcResult = c.Compile(&buf, Some(&ptrs), None).map_err(|e| format!("Compile: {e}"))?;
        let status = r.GetStatus().map_err(|e| format!("{e}"))?;
        if status.is_err() {
            let msg = r
                .GetErrorBuffer()
                .ok()
                .map(|b| {
                    let s = std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize());
                    String::from_utf8_lossy(s).trim_end_matches('\0').to_string()
                })
                .unwrap_or_default();
            return Err(format!("dxc: {msg}"));
        }
        let blob: IDxcBlob = r.GetResult().map_err(|e| format!("{e}"))?;
        Ok(std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()).to_vec())
    }
}

/// FNV-1a 64 of the inputs that determine the DXIL.
fn cache_key(src: &str, entry: &str, profile: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for part in [env!("CARGO_PKG_VERSION"), profile, entry, src] {
        for b in part.bytes().chain([0u8]) {
            h ^= b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
    }
    h
}

/// Folder of the DXIL cache: `XCL_DXIL_CACHE`, else `%LOCALAPPDATA%\xcl\dxil`.
pub fn cache_dir() -> PathBuf {
    if let Ok(d) = std::env::var("XCL_DXIL_CACHE") {
        return PathBuf::from(d);
    }
    let base = std::env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    base.join("xcl").join("dxil")
}

/// `compile` through a disk cache in `dir` (big translated programs take seconds in DXC;
/// the same console shader comes back every run). Returns (DXIL, from cache).
pub fn compile_cached_in(dir: &Path, src: &str, entry: &str, profile: &str) -> Result<(Vec<u8>, bool), String> {
    let file = dir.join(format!("{:016x}.dxil", cache_key(src, entry, profile)));
    if let Ok(b) = std::fs::read(&file) {
        if !b.is_empty() {
            return Ok((b, true));
        }
    }
    let b = compile(src, entry, profile, &[])?;
    if std::fs::create_dir_all(dir).is_ok() {
        // write to a temporary name first: another thread / process may be reading
        let tmp = file.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, &b).is_ok() && std::fs::rename(&tmp, &file).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    Ok((b, false))
}

pub fn compile_cached(src: &str, entry: &str, profile: &str) -> Result<(Vec<u8>, bool), String> {
    compile_cached_in(&cache_dir(), src, entry, profile)
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn dxil_cache_round_trip() {
        if location().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("xcl-dxil-test-{}", std::process::id()));
        let src = "[numthreads(1, 1, 1)] void main() {}";
        let (a, hit) = compile_cached_in(&dir, src, "main", "cs_6_6").unwrap();
        assert!(!hit);
        let (b, hit) = compile_cached_in(&dir, src, "main", "cs_6_6").unwrap();
        assert!(hit && a == b);
        assert_ne!(cache_key(src, "main", "cs_6_6"), cache_key(src, "main", "cs_6_7"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
