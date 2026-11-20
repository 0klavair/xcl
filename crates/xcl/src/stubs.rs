//! Automatic stubs for functions XCL does not implement yet.
//!
//! Each stub is a tiny x64 thunk:
//! ```text
//! sub  rsp, 0x28
//! mov  rcx, <StubInfo*>
//! mov  rdx, [rsp+0x28]        ; return address = caller
//! mov  rax, <stub_report>
//! call rax
//! add  rsp, 0x28
//! ret                         ; rax = configured return value
//! ```
//! Argument registers are clobbered but the stub never looks at them; RCX/RDX are
//! volatile in the Win64 ABI so the caller does not care either.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::Deserialize;

use crate::util::parse_ret;

pub struct StubInfo {
    pub dll: String,
    pub name: String,
    pub ret: u64,
    pub calls: AtomicU64,
    pub first_caller: AtomicUsize,
    /// Optional `fn(&StubInfo, caller)` invoked on every call (e.g. present heuristics).
    pub hook: AtomicUsize,
}

pub type StubHook = fn(&StubInfo, usize);

static ALL: Mutex<Vec<&'static StubInfo>> = Mutex::new(Vec::new());
static ARENA: Mutex<(usize, usize)> = Mutex::new((0, 0)); // (next, end)
static BY_KEY: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();

const THUNK_SIZE: usize = 48;
const ARENA_CHUNK: usize = 64 * 1024;

/// Count a call, log the first one, run the hook.
pub fn record_call(info: &StubInfo, caller: usize) {
    let n = info.calls.fetch_add(1, Ordering::Relaxed);
    if n == 0 {
        info.first_caller.store(caller, Ordering::Relaxed);
        crate::xlog_warn!(
            "[STUB] {}!{} wywołane po raz pierwszy z {} → zwracam {:#x}",
            info.dll,
            info.name,
            crate::modules::describe_address(caller),
            info.ret
        );
    }
    let h = info.hook.load(Ordering::Acquire);
    if h != 0 {
        let f: StubHook = unsafe { std::mem::transmute(h) };
        f(info, caller);
    }
}

unsafe extern "system" fn stub_report(info: *const StubInfo, caller: usize) -> u64 {
    let info = &*info;
    record_call(info, caller);
    info.ret
}

/// A StubInfo that is not backed by a stub thunk (used by D3D12.X slot guards).
pub fn new_info(dll: &str, name: &str, ret: u64) -> &'static StubInfo {
    let info: &'static StubInfo = Box::leak(Box::new(StubInfo {
        dll: dll.to_string(),
        name: name.to_string(),
        ret,
        calls: AtomicU64::new(0),
        first_caller: AtomicUsize::new(0),
        hook: AtomicUsize::new(0),
    }));
    ALL.lock().unwrap().push(info);
    info
}

/// Allocate `size` bytes of executable memory for generated thunks.
pub fn alloc_code(size: usize) -> *mut u8 {
    use windows_sys::Win32::System::Memory::{VirtualAlloc, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE};
    let size = (size + 15) & !15;
    let mut a = ARENA.lock().unwrap();
    if a.0 == 0 || a.0 + size > a.1 {
        let p = unsafe {
            VirtualAlloc(std::ptr::null(), ARENA_CHUNK, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE)
        } as usize;
        assert!(p != 0, "VirtualAlloc for stub arena failed");
        *a = (p, p + ARENA_CHUNK);
    }
    let p = a.0;
    a.0 += size;
    p as *mut u8
}

fn alloc_thunk() -> *mut u8 {
    alloc_code(THUNK_SIZE)
}

pub fn info_of_stub(addr: usize) -> Option<&'static StubInfo> {
    let map = BY_KEY.get()?;
    let g = map.lock().ok()?;
    let key = g.iter().find(|(_, &a)| a == addr).map(|(k, _)| k.clone())?;
    drop(g);
    ALL.lock().ok()?.iter().copied().find(|s| format!("{}!{}", s.dll.to_ascii_lowercase(), s.name) == key)
}

/// Create (or reuse) a stub for `dll!name`. Returns the callable address.
pub fn make_stub(dll: &str, name: &str, ret: u64) -> usize {
    make_stub_info(dll, name, ret).0
}

/// Like `make_stub`, also returning the StubInfo (to attach a hook).
pub fn make_stub_info(dll: &str, name: &str, ret: u64) -> (usize, &'static StubInfo) {
    let key = format!("{}!{}", dll.to_ascii_lowercase(), name);
    let map = BY_KEY.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(&addr) = map.lock().unwrap().get(&key) {
        let info = ALL.lock().unwrap().iter().copied().find(|s| s.dll.eq_ignore_ascii_case(dll) && s.name == name);
        if let Some(i) = info {
            return (addr, i);
        }
    }
    let info: &'static StubInfo = Box::leak(Box::new(StubInfo {
        dll: dll.to_string(),
        name: name.to_string(),
        ret,
        calls: AtomicU64::new(0),
        first_caller: AtomicUsize::new(0),
        hook: AtomicUsize::new(0),
    }));
    let p = alloc_thunk();
    let mut code: Vec<u8> = Vec::with_capacity(THUNK_SIZE);
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]); // sub rsp, 0x28
    code.extend_from_slice(&[0x48, 0xB9]); // mov rcx, imm64
    code.extend_from_slice(&(info as *const StubInfo as u64).to_le_bytes());
    code.extend_from_slice(&[0x48, 0x8B, 0x54, 0x24, 0x28]); // mov rdx, [rsp+0x28]
    code.extend_from_slice(&[0x48, 0xB8]); // mov rax, imm64
    code.extend_from_slice(&(stub_report as usize as u64).to_le_bytes());
    code.extend_from_slice(&[0xFF, 0xD0]); // call rax
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28]); // add rsp, 0x28
    code.push(0xC3); // ret
    debug_assert!(code.len() <= THUNK_SIZE);
    unsafe {
        std::ptr::copy_nonoverlapping(code.as_ptr(), p, code.len());
        windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache(
            windows_sys::Win32::System::Threading::GetCurrentProcess(),
            p as *const _,
            code.len(),
        );
    }
    ALL.lock().unwrap().push(info);
    map.lock().unwrap().insert(key, p as usize);
    (p as usize, info)
}

pub fn all_stubs() -> Vec<&'static StubInfo> {
    ALL.lock().map(|v| v.clone()).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------
// Stub database: default return values + ordinal names
// ---------------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct RawDb {
    #[serde(default)]
    module_defaults: HashMap<String, String>,
    #[serde(default)]
    functions: HashMap<String, String>,
    #[serde(default)]
    ordinals: HashMap<String, HashMap<String, String>>,
}

pub struct StubDb {
    module_defaults: HashMap<String, u64>,
    functions: HashMap<String, u64>,
    ordinals: HashMap<String, HashMap<u16, String>>,
}

static DB: OnceLock<StubDb> = OnceLock::new();

const BUILTIN_DB: &str = include_str!("../data/stubdb.toml");

impl StubDb {
    fn parse(text: &str, overrides: &std::collections::BTreeMap<String, String>) -> StubDb {
        let raw: RawDb = toml::from_str(text).unwrap_or_default();
        let mut functions: HashMap<String, u64> = raw
            .functions
            .iter()
            .filter_map(|(k, v)| Some((k.to_ascii_lowercase(), parse_ret(v)?)))
            .collect();
        for (k, v) in overrides {
            if let Some(r) = parse_ret(v) {
                functions.insert(k.to_ascii_lowercase(), r);
            }
        }
        StubDb {
            module_defaults: raw
                .module_defaults
                .iter()
                .filter_map(|(k, v)| Some((k.to_ascii_lowercase(), parse_ret(v)?)))
                .collect(),
            functions,
            ordinals: raw
                .ordinals
                .into_iter()
                .map(|(dll, m)| {
                    (
                        dll.to_ascii_lowercase(),
                        m.into_iter().filter_map(|(o, n)| Some((o.parse().ok()?, n))).collect(),
                    )
                })
                .collect(),
        }
    }

    pub fn return_for(&self, dll: &str, name: &str) -> u64 {
        let dll = dll.to_ascii_lowercase();
        let n = name.to_ascii_lowercase();
        if let Some(&r) = self.functions.get(&format!("{dll}!{n}")) {
            return r;
        }
        if let Some(&r) = self.functions.get(&format!("*!{n}")) {
            return r;
        }
        self.module_defaults.get(&dll).copied().unwrap_or(0)
    }

    pub fn ordinal_name(&self, dll: &str, ordinal: u16) -> Option<&str> {
        self.ordinals.get(&dll.to_ascii_lowercase())?.get(&ordinal).map(String::as_str)
    }
}

pub fn init_db(overrides: &std::collections::BTreeMap<String, String>) {
    let _ = DB.set(StubDb::parse(BUILTIN_DB, overrides));
}

pub fn db() -> &'static StubDb {
    DB.get_or_init(|| StubDb::parse(BUILTIN_DB, &Default::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_returns_configured_value_and_counts_calls() {
        let addr = make_stub("unit.dll", "Answer", 42);
        let f: extern "system" fn(usize, usize, usize, usize, usize) -> u64 = unsafe { std::mem::transmute(addr) };
        assert_eq!(f(1, 2, 3, 4, 5), 42);
        assert_eq!(f(0, 0, 0, 0, 0), 42);
        let info = all_stubs().into_iter().find(|s| s.name == "Answer").unwrap();
        assert_eq!(info.calls.load(Ordering::Relaxed), 2);
        assert_ne!(info.first_caller.load(Ordering::Relaxed), 0);
        // same key → same thunk
        assert_eq!(make_stub("UNIT.dll", "Answer", 1), addr);
    }

    #[test]
    fn builtin_db_parses_with_overrides() {
        let mut ov = std::collections::BTreeMap::new();
        ov.insert("kernelx.dll!Foo".to_string(), "7".to_string());
        let db = StubDb::parse(BUILTIN_DB, &ov);
        assert_eq!(db.return_for("kernelx.dll", "Foo"), 7);
        assert_eq!(db.return_for("xgameruntime.dll", "XSomethingUnknown"), 0x8000_4001);
        assert_eq!(db.return_for("kernelx.dll", "SomethingUnknown"), 0);
        assert_eq!(db.ordinal_name("d3d12_x.dll", 7), Some("D3D12CreateDevice"));
    }
}
