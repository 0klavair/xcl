//! The XGameRuntime thunk protocol – how GDK titles and GDK libraries (XCurl, libHttpClient)
//! reach the runtime. They link a static thunk library that loads `xgameruntime.dll`, calls
//! `InitializeApiImpl(Ex/Ex2)` and gets each API family as a COM-style object from
//! `QueryApiImpl(clsid, iid)`; an API call is a call through a slot of its vtable (0..2 are
//! IUnknown). The by-name exports in `mod.rs` are for titles that import them directly.
//!
//! XCL answers itself for what must stay offline and XCL-managed – XUser (one local user, its
//! devices), XGameSave (saves in the XCL data folder), persistent local storage, XPackage (a
//! fully installed package), XGame (title id from the game's MicrosoftGame.Config), XSystem and
//! its analytics (a Series X / S), runtime features, display deferrals, game events (offline:
//! dropped), the XGameUi dialogs that do not take a user –, refuses XStore, app broadcast and
//! app capture (the thunk then returns its "API not available" error), and passes everything
//! else to the PC's own XGameRuntime (`System32\xgameruntime.dll`: the same classes under the
//! same GUIDs) – XTaskQueue / XAsync, XThread, XNetworking, XGameInvite… Once the PC runtime is
//! in use, the queues and async blocks are its own, so XCL's async APIs run on its XAsync
//! (`xasync::use_pc`). Nothing that takes an XCL user handle goes to the PC runtime: it does not
//! validate foreign handles.
//!
//! Layout: `gdk_table` (extracted from the public PC GDK's thunk library) gives every
//! function's (class, interface, slot); XCL's objects are built from it by
//! function name. A class's interface ids that do not share slots share one vtable (later GDK
//! versions append functions under new interface ids); an interface id the table does not know
//! (another GDK version) gets the class's main vtable. The slot maps agree with what the thunks
//! in the game's DLLs use (argument and result widths) and with the names in the PC runtime's
//! slots. Functions XCL does not implement are logged and listed in the report by name
//! (`xgameruntime.unimplemented`).

#![allow(non_snake_case)]

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use super::gamesave::*;
use super::*;
use crate::util::{E_INVALIDARG, HRESULT, S_OK};

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Guid(pub u32, pub u16, pub u16, pub [u8; 8]);

impl Guid {
    pub const fn from_u128(v: u128) -> Guid {
        Guid((v >> 96) as u32, (v >> 80) as u16, (v >> 64) as u16, (v as u64).to_be_bytes())
    }
}

impl std::fmt::Display for Guid {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let d = &self.3;
        write!(f, "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{}", self.0, self.1, self.2, d[0], d[1], d[2..].iter().map(|b| format!("{b:02x}")).collect::<String>())
    }
}

const fn g(v: u128) -> Guid {
    Guid::from_u128(v)
}

pub const XUSER: Guid = g(0x01acd177_91f9_4763_a38e_ccbb55ce32e0);
/// interface of XUserGetGamertag (with a gamertag component): its own vtable
#[cfg_attr(not(test), allow(dead_code))]
const XUSER_GAMERTAG_IID: Guid = g(0xcef4fac0_7676_4a94_a119_4c43f9eb5b74);
const XUSER_DEVICE: Guid = g(0x7d824997_10dc_45ab_86b7_2737767c0bf1);
pub const XGAMESAVE: Guid = g(0x704c3f58_e629_4cc2_b197_30511b996fe2);
/// the interface the game's libHttpClient asks for (GDK table: 0b9d556e)
#[cfg_attr(not(test), allow(dead_code))]
const XPACKAGE_OLD_IID: Guid = g(0xe2a4734b_2f4a_456d_aa8f_d065e04fb209);
pub const XPACKAGE: Guid = g(0xaf406016_e850_4aa8_a88d_2f3dcb9dac7e);
pub const XPLS: Guid = g(0xf4faf4d4_2d04_4fce_b3e0_474a713a3e84);
pub const XGAME: Guid = g(0x973a344e_24bf_4d0f_8457_56c534892b29);
pub const XSYSTEM: Guid = g(0xe349bd1a_fc20_4e40_b99c_4178cc6b409f);
const XANALYTICS: Guid = g(0xb884675d_b738_4a9c_815d_9a9a1e0c6c9b);
const XFEATURE: Guid = g(0x8836fe87_edb9_4fe3_8dad_05f0d2cd5b40);
const XDISPLAY: Guid = g(0x1b339674_328d_4283_a200_3171f18d3639);
const XGAMEEVENT: Guid = g(0xbbfbdcc7_bfe7_409b_a5ca_edf054960b4d);
pub const XGAMEUI: Guid = g(0xdfcd4649_4ff8_4043_ba07_35d607df98b0);
const XSTORE: Guid = g(0x0dd112ac_7c24_448c_b92b_3960fb5bd30c);
const XAPPBROADCAST: Guid = g(0x186d5592_a72d_45fb_9560_11aed0e6647a);
const XAPPCAPTURE: Guid = g(0xa4f1aee2_4bf1_4485_b008_a7c26d52ac27);
/// XAsync + XTaskQueue + XThread (iid = clsid)
pub const XASYNC: Guid = g(0x073b7dcb_1fcf_4030_94be_e3c9eb623428);

/// classes that reach Xbox Live / the Store or take XCL's user handles: not available
const REFUSED: [(Guid, &str); 3] = [(XSTORE, "XStore"), (XAPPBROADCAST, "XAppBroadcast"), (XAPPCAPTURE, "XAppCapture")];

/// rows of the GDK table: (function, class, interface, slot)
fn table() -> impl Iterator<Item = (&'static str, Guid, Guid, usize)> {
    super::gdk_table::API.iter().map(|&(n, c, i, s)| (n, g(c), g(i), s as usize))
}

/// a class's name for the log: the prefix of its first function in the GDK table
fn family(c: &Guid) -> String {
    let Some((n, ..)) = table().find(|r| r.1 == *c) else { return "?".into() };
    let end = n.char_indices().skip(1).filter(|(_, ch)| ch.is_ascii_uppercase()).nth(1).map_or(n.len(), |(k, _)| k);
    n[..end].to_string()
}

const REGDB_E_CLASSNOTREG: HRESULT = 0x8004_0154_u32 as i32;
const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;
const E_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;

// ---------------------------------------------------------------------------------------
// the PC runtime
// ---------------------------------------------------------------------------------------

type QueryFn = unsafe extern "system" fn(*const Guid, *const Guid, *mut *mut c_void) -> HRESULT;
type InitEx2 = unsafe extern "system" fn(u64, u64, u64, *mut c_void) -> HRESULT;
type InitEx = unsafe extern "system" fn(u64, u64, u64) -> HRESULT;
type Init = unsafe extern "system" fn(u64, u64) -> HRESULT;

struct Pc {
    query: QueryFn,
    init_ex2: Option<InitEx2>,
    init_ex: Option<InitEx>,
    init: Option<Init>,
    uninit: Option<unsafe extern "system" fn()>,
}

static PC: OnceLock<Option<Pc>> = OnceLock::new();

fn pc() -> Option<&'static Pc> {
    PC.get_or_init(|| unsafe {
        use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
        let h = LoadLibraryExW(crate::util::wide("xgameruntime.dll").as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
        if h.is_null() {
            crate::xlog_warn!("XGameRuntime: no System32\\xgameruntime.dll (Gaming Services) – XTaskQueue/XAsync and the other APIs XCL does not implement are unavailable");
            return None;
        }
        let f = |n: &str| GetProcAddress(h, format!("{n}\0").as_ptr()).map(|p| p as usize);
        let query = f("QueryApiImpl")?;
        crate::xlog_info!("XGameRuntime: APIs XCL does not implement come from the system xgameruntime.dll");
        Some(Pc {
            query: std::mem::transmute::<usize, QueryFn>(query),
            init_ex2: f("InitializeApiImplEx2").map(|p| std::mem::transmute::<usize, InitEx2>(p)),
            init_ex: f("InitializeApiImplEx").map(|p| std::mem::transmute::<usize, InitEx>(p)),
            init: f("InitializeApiImpl").map(|p| std::mem::transmute::<usize, Init>(p)),
            uninit: f("UninitializeApiImpl").map(|p| std::mem::transmute::<usize, unsafe extern "system" fn()>(p)),
        })
    })
    .as_ref()
}

unsafe fn pc_object(c: &Guid, i: &Guid) -> Option<usize> {
    let mut o: *mut c_void = std::ptr::null_mut();
    ((pc()?.query)(c, i, &mut o) >= 0 && !o.is_null()).then_some(o as usize)
}

/// After the PC runtime is initialised: XCL's async APIs on its XAsync.
unsafe fn start_pc_async() {
    if let Some(o) = pc_object(&XASYNC, &XASYNC) {
        super::xasync::use_pc(o);
    }
}

// ---------------------------------------------------------------------------------------
// entry points
// ---------------------------------------------------------------------------------------

pub unsafe extern "system" fn InitializeApiImplEx2(gdk: u64, os: u64, flags: u64, options: *mut c_void) -> HRESULT {
    crate::xlog_info!("XGameRuntime: InitializeApiImplEx2({gdk:#x}, {os:#x}, flags {flags:#x}) – title goes through the GDK thunks");
    pc_init(gdk, os, flags, options);
    S_OK
}

/// Initialise the PC runtime with the title's versions; options this PC runtime does not know
/// (newer GDK) fall back to the older entry points.
unsafe fn pc_init(gdk: u64, os: u64, flags: u64, options: *mut c_void) {
    let Some(p) = pc() else { return };
    let mut hr = p.init_ex2.map(|f| f(gdk, os, flags, options)).unwrap_or(-1);
    if hr < 0 {
        hr = p.init_ex.map(|f| f(gdk, os, flags)).unwrap_or(-1);
    }
    if hr < 0 {
        hr = p.init.map(|f| f(gdk >> 32, gdk & 0xFFFF_FFFF)).unwrap_or(-1);
    }
    if hr < 0 {
        crate::xlog_warn!("XGameRuntime PC: initialization → {:#x}", hr as u32);
    }
    start_pc_async();
}

pub unsafe extern "system" fn InitializeApiImplEx(gdk: u64, os: u64, flags: u64) -> HRESULT {
    crate::xlog_info!("XGameRuntime: InitializeApiImplEx({gdk:#x}, {os:#x}, flags {flags:#x})");
    pc_init(gdk, os, flags, std::ptr::null_mut());
    S_OK
}

pub unsafe extern "system" fn InitializeApiImpl(gdk: u64, os: u64) -> HRESULT {
    crate::xlog_info!("XGameRuntime: InitializeApiImpl({gdk:#x}, {os:#x})");
    pc_init(gdk << 32 | (os & 0xFFFF_FFFF), 0, 0, std::ptr::null_mut());
    S_OK
}

pub unsafe extern "system" fn UninitializeApiImpl() {
    if let Some(f) = pc().and_then(|p| p.uninit) {
        f();
    }
}

pub unsafe extern "system" fn XErrorReport(hr: HRESULT, message: *const u8) {
    crate::xlog_warn!("XErrorReport {:#x}: {}", hr as u32, crate::util::from_cstr_ptr(message).unwrap_or_default());
}

pub unsafe extern "system" fn QueryApiImpl(clsid: *const Guid, iid: *const Guid, out: *mut *mut c_void) -> HRESULT {
    if clsid.is_null() || iid.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    *out = std::ptr::null_mut();
    let (c, i) = (*clsid, *iid);
    if let Some((_, name)) = REFUSED.iter().find(|r| r.0 == c) {
        note(c, i, format!("{name}: unavailable (offline)"));
        return REGDB_E_CLASSNOTREG;
    }
    if let Some(o) = own(&c, &i) {
        let known = o.iface.iids.contains(&i) || i == c;
        note(c, i, format!("{}: XCL{}", o.iface.name, if known { "" } else { " (interface version not in the GDK table: the main vtable)" }));
        if !o.iface.forward.is_empty() && o.pc.load(Ordering::Acquire) == 0 {
            o.pc.store(pc_object(&c, &i).unwrap_or(0), Ordering::Release);
        }
        *out = o as *const Obj as *mut c_void;
        return S_OK;
    }
    match pc_object(&c, &i) {
        Some(p) => {
            note(c, i, format!("{}: PC", family(&c)));
            *out = p as *mut c_void;
            S_OK
        }
        None => {
            note(c, i, format!("{}: missing", family(&c)));
            REGDB_E_CLASSNOTREG
        }
    }
}

// ---------------------------------------------------------------------------------------
// XCL's API objects
// ---------------------------------------------------------------------------------------

/// One vtable of an XCL class.
struct Iface {
    name: &'static str,
    clsid: Guid,
    /// interface ids this vtable answers
    iids: Vec<Guid>,
    /// the class's main vtable: also for interface ids the GDK table does not know
    main: bool,
    /// slot → function (GDK table)
    names: Vec<Option<&'static str>>,
    /// (slot, function): the API function itself, without `this` (a shim drops it)
    slots: Vec<(usize, usize)>,
    /// slots passed to the PC runtime's object of the same interface
    forward: Vec<usize>,
}

/// A class XCL answers for: its functions by GDK name, and the ones passed to the PC runtime.
struct Class {
    name: &'static str,
    clsid: Guid,
    impls: Vec<(&'static str, usize)>,
    forward: &'static [&'static str],
}

/// Functions that return bool (an unimplemented one answers false).
fn returns_bool(name: &str) -> bool {
    let rest = name.trim_start_matches('X');
    let verb = rest.char_indices().skip(1).find(|(_, ch)| ch.is_ascii_uppercase()).map_or("", |(k, _)| &rest[k..]);
    verb.starts_with("Is") || verb.contains("Unregister") || name == "XPackageUpdateInstallationMonitor" || name == "XPackageUninstallPackage"
}

#[repr(C)]
struct Obj {
    vtbl: *const usize,
    iface: Iface,
    /// the PC runtime's object, for `forward`
    pc: AtomicUsize,
    /// slot → API function the shim calls
    targets: [usize; SLOTS],
}

unsafe impl Sync for Obj {}
unsafe impl Send for Obj {}

const SLOTS: usize = 80;

unsafe extern "system" fn query_interface(this: *const Obj, iid: *const Guid, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() || iid.is_null() {
        return E_INVALIDARG;
    }
    let f = &(*this).iface;
    if *iid == f.clsid || f.iids.contains(&*iid) || (f.main && own(&f.clsid, &*iid).is_some_and(|o| std::ptr::eq(o, this))) {
        *out = this as *mut c_void;
        return S_OK;
    }
    *out = std::ptr::null_mut();
    E_NOINTERFACE
}

unsafe extern "system" fn add_ref(_this: *const Obj) -> u32 {
    1
}

static UNIMPLEMENTED: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());

unsafe fn unknown_slot(this: *const Obj, slot: usize) -> usize {
    let f = &(*this).iface;
    let key = match f.names.get(slot).copied().flatten() {
        Some(n) => n.to_string(),
        None => format!("{}#{slot}", f.name),
    };
    let first = {
        let mut g = UNIMPLEMENTED.lock().unwrap();
        let n = g.entry(key.clone()).or_insert(0);
        *n += 1;
        *n == 1
    };
    if first {
        crate::xlog_warn!("XGameRuntime: {key} – not supported (E_NOTIMPL)");
    }
    if f.names.get(slot).copied().flatten().is_some_and(returns_bool) {
        0
    } else {
        E_NOTIMPL as u32 as usize
    }
}

type Fwd = unsafe extern "system" fn(*const Obj, usize, usize, usize, usize, usize, usize, usize, usize, usize) -> usize;

/// Call slot `slot` of the PC object with the caller's arguments (up to 9; extra ones are
/// ignored by the callee).
unsafe fn forward(this: *const Obj, slot: usize, a: [usize; 9]) -> usize {
    let o = (*this).pc.load(Ordering::Acquire);
    if o == 0 {
        return unknown_slot(this, slot);
    }
    let f: Fwd = std::mem::transmute(*(*(o as *const *const usize)).add(slot));
    f(o as *const Obj, a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8])
}

/// Slot `slot` of an XCL object: its API function with the caller's arguments after `this`.
unsafe fn call_target(this: *const Obj, slot: usize, a: [usize; 9]) -> usize {
    let f: unsafe extern "system" fn(usize, usize, usize, usize, usize, usize, usize, usize, usize) -> usize = std::mem::transmute((*this).targets[slot]);
    f(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8])
}

macro_rules! per_slot {
    ($($n:literal)*) => {
        const SHIMS: [Fwd; SLOTS] = [$({
            unsafe extern "system" fn f(this: *const Obj, a: usize, b: usize, c: usize, d: usize, e: usize, g: usize, h: usize, i: usize, j: usize) -> usize {
                call_target(this, $n, [a, b, c, d, e, g, h, i, j])
            }
            f
        }),*];
        const STUBS: [unsafe extern "system" fn(*const Obj) -> usize; SLOTS] = [$({
            unsafe extern "system" fn f(this: *const Obj) -> usize {
                unknown_slot(this, $n)
            }
            f
        }),*];
        const FORWARDS: [Fwd; SLOTS] = [$({
            unsafe extern "system" fn f(this: *const Obj, a: usize, b: usize, c: usize, d: usize, e: usize, g: usize, h: usize, i: usize, j: usize) -> usize {
                forward(this, $n, [a, b, c, d, e, g, h, i, j])
            }
            f
        }),*];
    };
}
per_slot!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39
    40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63 64 65 66 67 68 69 70 71 72 73 74 75 76 77 78 79);

fn object(iface: Iface) -> &'static Obj {
    let mut v: Vec<usize> = (0..SLOTS).map(|k| STUBS[k] as usize).collect();
    v[0] = query_interface as usize;
    v[1] = add_ref as usize;
    v[2] = add_ref as usize;
    for &k in &iface.forward {
        v[k] = FORWARDS[k] as usize;
    }
    let mut targets = [0usize; SLOTS];
    for &(k, f) in &iface.slots {
        v[k] = SHIMS[k] as usize;
        targets[k] = f;
    }
    let vtbl = Box::leak(v.into_boxed_slice()).as_ptr();
    Box::leak(Box::new(Obj { vtbl, iface, pc: AtomicUsize::new(0), targets }))
}

/// The vtables of a class from the GDK table: interface ids whose slots do not collide share
/// one (the one holding the class id itself, or the first, is the main one).
fn layout(c: Class) -> Vec<Iface> {
    let mut by_iid: BTreeMap<Guid, Vec<(usize, &'static str)>> = BTreeMap::new();
    for (n, cl, i, slot) in table() {
        if cl == c.clsid && slot < SLOTS {
            by_iid.entry(i).or_default().push((slot, n));
        }
    }
    let mut order: Vec<Guid> = by_iid.keys().copied().collect();
    order.sort_by_key(|i| (*i != c.clsid, by_iid[i].iter().map(|r| r.0).min()));
    let mut groups: Vec<(Vec<Guid>, Vec<Option<&'static str>>)> = Vec::new();
    for i in order {
        let rows = &by_iid[&i];
        let free = groups.iter().position(|(_, names)| rows.iter().all(|&(s, _)| names[s].is_none()));
        let k = free.unwrap_or_else(|| {
            groups.push((Vec::new(), vec![None; SLOTS]));
            groups.len() - 1
        });
        groups[k].0.push(i);
        for &(s, n) in rows {
            groups[k].1[s] = Some(n);
        }
    }
    groups
        .into_iter()
        .enumerate()
        .map(|(k, (iids, names))| {
            let slot_of = |f: &str| names.iter().position(|n| *n == Some(f));
            Iface {
                name: c.name,
                clsid: c.clsid,
                main: k == 0,
                slots: c.impls.iter().filter_map(|&(f, p)| Some((slot_of(f)?, p))).collect(),
                forward: c.forward.iter().filter_map(|f| slot_of(f)).collect(),
                iids,
                names,
            }
        })
        .collect()
}

fn classes() -> Vec<Class> {
    vec![
            Class { name: "XUser", clsid: XUSER, impls: xuser(), forward: &[] },
            Class { name: "XUser (devices)", clsid: XUSER_DEVICE, impls: xuser_devices(), forward: &[] },
            Class { name: "XGameSave", clsid: XGAMESAVE, impls: xgamesave(), forward: &[] },
            Class { name: "XPersistentLocalStorage", clsid: XPLS, impls: xpls(), forward: &[] },
            Class { name: "XPackage", clsid: XPACKAGE, impls: xpackage(), forward: &[] },
            Class { name: "XGame", clsid: XGAME, impls: named![XGameGetXboxTitleId, XLaunchNewGame, XLaunchRestartOnCrash], forward: &[] },
            Class { name: "XSystem", clsid: XSYSTEM, impls: xsystem(), forward: &[] },
            Class { name: "XSystemAnalytics", clsid: XANALYTICS, impls: named![XSystemGetAnalyticsInfo], forward: &[] },
            Class { name: "XGameRuntimeFeature", clsid: XFEATURE, impls: named![XGameRuntimeIsFeatureAvailable], forward: &[] },
            Class {
                name: "XDisplay",
                clsid: XDISPLAY,
                impls: vec![("XLaunchUri", launch_uri as usize), ("XDisplayAcquireTimeoutDeferral", display_deferral as usize), ("XDisplayCloseTimeoutDeferralHandle", nothing as usize)],
                forward: &[],
            },
            // telemetry: offline, accepted and dropped
            Class { name: "XGameEvent", clsid: XGAMEEVENT, impls: vec![("XGameEventWrite", ok as usize)], forward: &[] },
            // dialogs without a user handle go to the PC runtime (message, error, text entry)
            Class {
                name: "XGameUi",
                clsid: XGAMEUI,
                impls: Vec::new(),
                forward: &[
                    "XGameUiShowMessageDialogAsync",
                    "XGameUiShowMessageDialogResult",
                    "XGameUiShowErrorDialogAsync",
                    "XGameUiShowErrorDialogResult",
                    "XGameUiShowTextEntryAsync",
                    "XGameUiShowTextEntryResultSize",
                    "XGameUiShowTextEntryResult",
                ],
            },
    ]
}

fn objects() -> &'static [&'static Obj] {
    static O: OnceLock<Vec<&'static Obj>> = OnceLock::new();
    O.get_or_init(|| classes().into_iter().flat_map(layout).map(object).collect())
}

/// XCL's object for (clsid, iid): the vtable holding that interface, else the class's main one.
fn own(c: &Guid, i: &Guid) -> Option<&'static Obj> {
    let all = objects();
    all.iter().copied().find(|o| o.iface.clsid == *c && o.iface.iids.contains(i)).or_else(|| all.iter().copied().find(|o| o.iface.clsid == *c && o.iface.main))
}

/// `(name, function)` pairs for functions named like their GDK function
macro_rules! named {
    ($($f:ident),* $(,)?) => {
        vec![$( (stringify!($f), $f as usize) ),*]
    };
}
use named;

fn xuser() -> Vec<(&'static str, usize)> {
    let mut v = named![
        XUserDuplicateHandle,
        XUserCloseHandle,
        XUserCompare,
        XUserGetMaxUsers,
        XUserAddAsync,
        XUserAddResult,
        XUserGetLocalId,
        XUserGetId,
        XUserGetIsGuest,
        XUserGetState,
        XUserGetGamertag,
        XUserGetAgeGroup,
        XUserCheckPrivilege,
        XUserRegisterForChangeEvent,
        XUserUnregisterForChangeEvent,
    ];
    v.extend([
        ("XUserFindUserByLocalId", find_user as usize),
        ("XUserFindUserById", find_user as usize),
        ("XUserResolvePrivilegeWithUiAsync", resolve_privilege_async as usize),
        ("XUserResolvePrivilegeWithUiResult", empty_result as usize),
        ("XUserResolveIssueWithUiAsync", resolve_issue_async as usize),
        ("XUserResolveIssueWithUiResult", empty_result as usize),
        ("XUserResolveIssueWithUiUtf16Async", resolve_issue_async as usize),
        ("XUserResolveIssueWithUiUtf16Result", empty_result as usize),
        ("XUserGetSignOutDeferral", sign_out_deferral as usize),
        ("XUserCloseSignOutDeferralHandle", nothing as usize),
        ("XUserAddByIdWithUiAsync", add_by_id_async as usize),
        ("XUserAddByIdWithUiResult", XUserAddResult as usize),
        ("XUserIsStoreUser", no as usize),
        ("XUserPlatformRemoteConnectSetEventHandlers", ok as usize),
        ("XUserPlatformRemoteConnectCancelPrompt", ok as usize),
        ("XUserPlatformSpopPromptSetEventHandlers", ok as usize),
        ("XUserPlatformSpopPromptComplete", ok as usize),
        ("XUserIsSignOutPresent", no as usize),
        ("XUserSignOutAsync", sign_out_async as usize),
        ("XUserSignOutResult", empty_result as usize),
    ]);
    v
}

/// device association: the local user plays on every controller, no device is a user's
fn xuser_devices() -> Vec<(&'static str, usize)> {
    vec![
        ("XUserRegisterForDeviceAssociationChanged", register_token4 as usize),
        ("XUserUnregisterForDeviceAssociationChanged", yes as usize),
        ("XUserRegisterForDefaultAudioEndpointUtf16Changed", register_token4 as usize),
        ("XUserUnregisterForDefaultAudioEndpointUtf16Changed", yes as usize),
        ("XUserFindForDevice", user_not_found as usize),
    ]
}

fn xgamesave() -> Vec<(&'static str, usize)> {
    named![
        XGameSaveInitializeProvider,
        XGameSaveInitializeProviderAsync,
        XGameSaveInitializeProviderResult,
        XGameSaveCloseProvider,
        XGameSaveGetRemainingQuota,
        XGameSaveGetRemainingQuotaAsync,
        XGameSaveGetRemainingQuotaResult,
        XGameSaveDeleteContainer,
        XGameSaveDeleteContainerAsync,
        XGameSaveDeleteContainerResult,
        XGameSaveGetContainerInfo,
        XGameSaveEnumerateContainerInfo,
        XGameSaveEnumerateContainerInfoByName,
        XGameSaveCreateContainer,
        XGameSaveCloseContainer,
        XGameSaveEnumerateBlobInfo,
        XGameSaveEnumerateBlobInfoByName,
        XGameSaveReadBlobData,
        XGameSaveReadBlobDataAsync,
        XGameSaveReadBlobDataResult,
        XGameSaveCreateUpdate,
        XGameSaveCloseUpdate,
        XGameSaveSubmitBlobWrite,
        XGameSaveSubmitBlobDelete,
        XGameSaveSubmitUpdate,
        XGameSaveSubmitUpdateAsync,
        XGameSaveSubmitUpdateResult,
        XGameSaveFilesGetFolderWithUiAsync,
        XGameSaveFilesGetFolderWithUiResult,
        XGameSaveFilesGetRemainingQuota,
    ]
}

fn xpls() -> Vec<(&'static str, usize)> {
    let mut v = named![XPersistentLocalStorageGetPathSize, XPersistentLocalStorageGetPath, XPersistentLocalStorageGetSpaceInfo];
    // no prompt to show: the space asked for is granted
    v.extend([("XPersistentLocalStoragePromptUserForSpaceAsync", prompt_space_async as usize), ("XPersistentLocalStoragePromptUserForSpaceResult", empty_result as usize)]);
    v
}

fn xpackage() -> Vec<(&'static str, usize)> {
    let mut v = named![
        XPackageGetCurrentProcessPackageIdentifier,
        XPackageIsPackagedProcess,
        XPackageCreateInstallationMonitor,
        XPackageGetInstallationProgress,
        XPackageRegisterInstallationProgressChanged,
        XPackageUnregisterInstallationProgressChanged,
        XPackageGetUserLocale,
        XPackageFindChunkAvailability,
        XPackageEstimateDownloadSize,
        XPackageGetMountPathSize,
        XPackageGetMountPath,
        XPackageCloseMountHandle,
        XPackageGetWriteStats,
        XPackageGetPackageKind,
    ];
    v.extend([
        ("XPackageCloseInstallationMonitorHandle", nothing as usize),
        ("XPackageUpdateInstallationMonitor", no as usize),
        ("XPackageEnumerateChunkAvailability", ok as usize),
        ("XPackageChangeChunkInstallOrder", ok as usize),
        ("XPackageInstallChunks", install_chunks as usize),
        ("XPackageInstallChunksAsync", install_chunks_async as usize),
        ("XPackageInstallChunksResult", install_chunks_result as usize),
        ("XPackageUninstallChunks", ok as usize),
        ("XPackageRegisterPackageInstalled", register_token4 as usize),
        ("XPackageUnregisterPackageInstalled", yes as usize),
        ("XPackageEnumerateFeatures", ok as usize),
        ("XPackageUninstallPackage", no as usize),
        ("XPackageMountWithUiAsync", mount_with_ui_async as usize),
        ("XPackageMountWithUiResult", mount_with_ui_result as usize),
        ("XPackageEnumeratePackages", enumerate_packages as usize),
    ]);
    v
}

fn xsystem() -> Vec<(&'static str, usize)> {
    let mut v = named![XSystemGetConsoleId, XSystemGetXboxLiveSandboxId, XSystemGetAppSpecificDeviceId];
    v.extend([("XSystemHandleTrack", ok as usize), ("XSystemIsHandleValid", yes as usize), ("XSystemAllowFullDownloadBandwidth", nothing as usize)]);
    v
}

// ---------------------------------------------------------------------------------------
// functions only the thunk path has
// ---------------------------------------------------------------------------------------

unsafe extern "system" fn nothing(_a: usize) {}

unsafe extern "system" fn no(_a: usize) -> bool {
    false
}

unsafe extern "system" fn yes() -> bool {
    true
}

unsafe extern "system" fn ok() -> HRESULT {
    S_OK
}

const E_GAMEUSER_DEFERRAL_NOT_AVAILABLE: HRESULT = 0x8924_5103_u32 as i32;
const E_GAMEUSER_USER_NOT_FOUND: HRESULT = 0x8924_5104_u32 as i32;

static NEXT_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Register*(queue, context, callback, token): a token, nothing will be called
unsafe extern "system" fn register_token4(_q: usize, _ctx: usize, _cb: usize, token: *mut u64) -> HRESULT {
    if token.is_null() {
        return E_INVALIDARG;
    }
    *token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    S_OK
}

static EMPTY_ID: u8 = 0;

/// an async call that completes at once with no result
unsafe fn completed(b: *mut XAsyncBlock) -> HRESULT {
    begin_with(b, &EMPTY_ID as *const u8 as usize, || Ok(Vec::new()))
}

/// the Result of such a call: its status
unsafe extern "system" fn empty_result(b: *mut XAsyncBlock) -> HRESULT {
    XAsyncGetStatus(b, false)
}

unsafe extern "system" fn resolve_privilege_async(_user: usize, _options: u32, _privilege: u32, b: *mut XAsyncBlock) -> HRESULT {
    completed(b)
}

unsafe extern "system" fn resolve_issue_async(_user: usize, _url: usize, b: *mut XAsyncBlock) -> HRESULT {
    completed(b)
}

unsafe extern "system" fn sign_out_async(_user: usize, b: *mut XAsyncBlock) -> HRESULT {
    completed(b)
}

unsafe extern "system" fn prompt_space_async(_bytes: u64, b: *mut XAsyncBlock) -> HRESULT {
    completed(b)
}

unsafe extern "system" fn sign_out_deferral(out: *mut usize) -> HRESULT {
    if !out.is_null() {
        *out = 0;
    }
    E_GAMEUSER_DEFERRAL_NOT_AVAILABLE
}

/// XUserAddByIdWithUiAsync: the local user by either of its ids (XUserAddByIdWithUiResult =
/// XUserAddResult)
unsafe extern "system" fn add_by_id_async(id: u64, b: *mut XAsyncBlock) -> HRESULT {
    let mut h = std::ptr::null_mut();
    if find_user(id, &mut h) < 0 {
        return E_GAMEUSER_USER_NOT_FOUND;
    }
    XUserCloseHandle(h);
    XUserAddAsync(0, b)
}

unsafe extern "system" fn user_not_found(_device: usize, out: *mut usize) -> HRESULT {
    if !out.is_null() {
        *out = 0;
    }
    E_GAMEUSER_USER_NOT_FOUND
}

/// Opening links on the title's behalf would act on the PC: refused.
unsafe extern "system" fn launch_uri(_user: usize, uri: *const u8) -> HRESULT {
    crate::xlog_warn!("XLaunchUri({}) – refused", crate::util::from_cstr_ptr(uri).unwrap_or_default());
    E_NOTIMPL
}

/// nothing to defer on a PC
unsafe extern "system" fn display_deferral(out: *mut usize) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = 1;
    S_OK
}

/// XPackageInstallChunks(id, count, selectors, interval, suppress, queue, monitor): installed
unsafe extern "system" fn install_chunks(id: *const u8, count: u32, sel: *const c_void, interval: u32, _suppress: bool, queue: *mut c_void, out: *mut *const u64) -> HRESULT {
    XPackageCreateInstallationMonitor(id, count, sel, interval, queue, out)
}

static INSTALL_ID: u8 = 0;
static MOUNT_ID: u8 = 0;

unsafe extern "system" fn install_chunks_async(_id: usize, _count: u32, _sel: usize, _interval: u32, _suppress: bool, b: *mut XAsyncBlock) -> HRESULT {
    begin_with(b, &INSTALL_ID as *const u8 as usize, || Ok((&MONITOR as *const u64 as usize).to_le_bytes().to_vec()))
}

unsafe extern "system" fn install_chunks_result(b: *mut XAsyncBlock, out: *mut usize) -> HRESULT {
    XAsyncGetResult(b, &INSTALL_ID as *const u8 as *const c_void, 8, out as *mut c_void, std::ptr::null_mut())
}

unsafe extern "system" fn mount_with_ui_async(id: *const u8, b: *mut XAsyncBlock) -> HRESULT {
    let mut m: *mut c_void = std::ptr::null_mut();
    let hr = XPackageMount(id, &mut m);
    if hr < 0 {
        return hr;
    }
    let m = m as usize;
    begin_with(b, &MOUNT_ID as *const u8 as usize, move || Ok(m.to_le_bytes().to_vec()))
}

unsafe extern "system" fn mount_with_ui_result(b: *mut XAsyncBlock, out: *mut usize) -> HRESULT {
    XAsyncGetResult(b, &MOUNT_ID as *const u8 as *const c_void, 8, out as *mut c_void, std::ptr::null_mut())
}

/// XPackageDetails (XPackage.h)
#[repr(C)]
struct PackageDetails {
    package_identifier: *const u8,
    version: XVersion,
    kind: u32,
    display_name: *const u8,
    description: *const u8,
    publisher: *const u8,
    store_id: *const u8,
    installing: bool,
    index: u32,
    count: u32,
    age_restricted: bool,
    title_id: *const u8,
}

/// XPackageEnumeratePackages(kind, scope, context, callback): the game itself (no DLC).
unsafe extern "system" fn enumerate_packages(kind: u32, _scope: u32, ctx: *mut c_void, cb: Option<unsafe extern "system" fn(*mut c_void, *const PackageDetails) -> bool>) -> HRESULT {
    let Some(cb) = cb else { return E_INVALIDARG };
    if kind != 0 {
        return S_OK;
    }
    let c = |s: String| std::ffi::CString::new(s).unwrap_or_default();
    let id = c(package_identifier());
    let title = c(crate::state::boot().map(|b| b.title.clone()).unwrap_or_default());
    let title_id = c(crate::state::config().title_id.trim_start_matches("0x").to_ascii_uppercase());
    let empty = c(String::new());
    let d = PackageDetails {
        package_identifier: id.as_ptr() as *const u8,
        version: XVersion { major: 1, minor: 0, build: 0, revision: 0 },
        kind: 0,
        display_name: title.as_ptr() as *const u8,
        description: empty.as_ptr() as *const u8,
        publisher: empty.as_ptr() as *const u8,
        store_id: empty.as_ptr() as *const u8,
        installing: false,
        index: 0,
        count: 1,
        age_restricted: false,
        title_id: title_id.as_ptr() as *const u8,
    };
    cb(ctx, &d);
    S_OK
}

unsafe extern "system" fn XPackageGetUserLocale(size: usize, buf: *mut u8) -> HRESULT {
    let mut w = [0u16; 85];
    let n = windows_sys::Win32::Globalization::GetUserDefaultLocaleName(w.as_mut_ptr(), w.len() as i32);
    let loc = if n > 1 { String::from_utf16_lossy(&w[..n as usize - 1]) } else { "en-US".into() };
    crate::util::write_cstr_out(&loc, buf, size, std::ptr::null_mut())
}

/// every chunk is installed (XPackageChunkAvailability::Ready = 0)
unsafe extern "system" fn XPackageFindChunkAvailability(_id: usize, _count: u32, _sel: usize, out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = 0;
    S_OK
}

unsafe extern "system" fn XPackageEstimateDownloadSize(_id: usize, _count: u32, _sel: usize, size: *mut u64, confirm: *mut bool) -> HRESULT {
    if !size.is_null() {
        *size = 0;
    }
    if !confirm.is_null() {
        *confirm = false;
    }
    S_OK
}

/// XPackageWriteStats { interval, budget, elapsed, bytesWritten }: nothing measured
unsafe extern "system" fn XPackageGetWriteStats(out: *mut [u64; 4]) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = [0; 4];
    S_OK
}

/// XPackageKind::Game
unsafe extern "system" fn XPackageGetPackageKind(_id: usize, out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = 0;
    S_OK
}

/// A stable id per console and title.
unsafe extern "system" fn XSystemGetAppSpecificDeviceId(size: usize, buf: *mut u8, used: *mut usize) -> HRESULT {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (crate::state::config().console_id.as_str(), crate::state::config().title_id.as_str()).hash(&mut h);
    crate::util::write_cstr_out(&format!("{:016X}", h.finish()), buf, size, used)
}

/// XUserFindUserByLocalId / XUserFindUserById (slots 10 / 12): the local user by either id.
unsafe extern "system" fn find_user(id: u64, out: *mut *mut c_void) -> HRESULT {
    let hr = XUserFindUserByLocalId(id, out);
    if hr >= 0 {
        return hr;
    }
    XUserFindUserById(id, out)
}

/// The local user has every privilege (no Xbox Live to deny one).
unsafe extern "system" fn XUserCheckPrivilege(_user: *mut c_void, _options: u32, _privilege: u32, has: *mut bool, reasons: *mut u32) -> HRESULT {
    if has.is_null() {
        return E_INVALIDARG;
    }
    *has = true;
    if !reasons.is_null() {
        *reasons = 0;
    }
    S_OK
}

unsafe extern "system" fn XGameSaveFilesGetRemainingQuota(_user: *mut c_void, _config: *const u8, quota: *mut i64) -> HRESULT {
    if quota.is_null() {
        return E_INVALIDARG;
    }
    *quota = 256 << 20;
    S_OK
}

/// The installation monitor of a fully installed package (XCL runs from an unpacked folder).
static MONITOR: u64 = 0x4D4F_4E49;

unsafe extern "system" fn XPackageCreateInstallationMonitor(_id: *const u8, _count: u32, _sel: *const c_void, _interval: u32, _queue: *mut c_void, out: *mut *const u64) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = &MONITOR;
    S_OK
}

/// XPackageInstallationProgress { totalBytes, installedBytes, launchBytes, launchable,
/// completed }: everything installed.
unsafe extern "system" fn XPackageGetInstallationProgress(_monitor: *const u64, p: *mut u8) {
    if p.is_null() {
        return;
    }
    for k in 0..3 {
        std::ptr::write_unaligned(p.add(8 * k) as *mut u64, 1);
    }
    *p.add(24) = 1;
    *p.add(25) = 1;
}

unsafe extern "system" fn XPackageRegisterInstallationProgressChanged(_monitor: *const u64, _ctx: *mut c_void, _cb: usize, token: *mut u64) -> HRESULT {
    if !token.is_null() {
        *token = 1;
    }
    S_OK
}

unsafe extern "system" fn XPackageUnregisterInstallationProgressChanged(_monitor: *const u64, _token: u64, _wait: bool) -> bool {
    true
}

/// `<TitleId>` of the game's MicrosoftGame.Config (xcl.toml `title_id` overrides).
pub unsafe extern "system" fn XGameGetXboxTitleId(out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let t = &crate::state::config().title_id;
    *out = u32::from_str_radix(t.trim_start_matches("0x"), 16).unwrap_or(0);
    if *out == 0 {
        crate::xlog_warn!("XGameGetXboxTitleId: no TitleId in MicrosoftGame.Config / xcl.toml → 0");
    }
    S_OK
}

unsafe extern "system" fn XLaunchNewGame(exe: *const u8, args: *const u8, _user: *mut c_void) {
    crate::xlog_warn!(
        "XLaunchNewGame({}, {}) – ignored (XCL runs a single title)",
        crate::util::from_cstr_ptr(exe).unwrap_or_default(),
        crate::util::from_cstr_ptr(args).unwrap_or_default()
    );
}

unsafe extern "system" fn XLaunchRestartOnCrash(_args: *const u8, _reserved: u32) -> HRESULT {
    S_OK
}

// ---------------------------------------------------------------------------------------
// report
// ---------------------------------------------------------------------------------------

static SEEN: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

fn note(c: Guid, i: Guid, what: String) {
    let key = format!("{c}/{i}");
    let mut g = SEEN.lock().unwrap();
    if !g.contains_key(&key) {
        crate::xlog_info!("XGameRuntime: QueryApiImpl {key} → {what}");
        g.insert(key, what);
    }
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApiStats {
    /// "clsid/iid" → who answered (XCL, PC, unavailable, missing)
    pub apis: BTreeMap<String, String>,
    /// XCL objects' slots the title called that XCL does not implement: "Iface#slot" → calls
    pub unimplemented: BTreeMap<String, u64>,
}

pub fn stats() -> ApiStats {
    ApiStats { apis: SEEN.lock().unwrap().clone(), unimplemented: UNIMPLEMENTED.lock().unwrap().clone() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    unsafe fn query(c: Guid, i: Guid) -> (HRESULT, *mut c_void) {
        let mut o = std::ptr::null_mut();
        (QueryApiImpl(&c, &i, &mut o), o)
    }

    unsafe fn slot<T>(o: *mut c_void, k: usize) -> T {
        std::mem::transmute_copy(&*(*(o as *const *const usize)).add(k))
    }

    static DONE: AtomicU32 = AtomicU32::new(0);
    unsafe extern "system" fn done(_b: *mut XAsyncBlock) {
        DONE.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn guids_format_like_the_registry() {
        assert_eq!(XUSER.to_string(), "01acd177-91f9-4763-a38e-ccbb55ce32e0");
        let b: [u8; 16] = unsafe { std::mem::transmute(XUSER) };
        assert_eq!(&b[..4], &[0x77, 0xd1, 0xac, 0x01]);
    }

    fn slot_in_table(name: &str) -> (Guid, Guid, usize) {
        table().find(|r| r.0 == name).map(|r| (r.1, r.2, r.3)).unwrap_or_else(|| panic!("{name} not in the GDK table"))
    }

    /// XCL's objects follow the GDK table: every implemented function is in its class; one
    /// vtable for the interface versions that share slots, its own for the gamertag one; an
    /// interface id the table does not know gets the main vtable. The table agrees with the
    /// slots the thunks in the game's DLLs use (argument / result widths, see HANDOFF).
    #[test]
    fn vtables_follow_the_gdk_table() {
        for c in classes() {
            for (f, _) in c.impls.iter().map(|x| (x.0, x.1)).chain(c.forward.iter().map(|f| (*f, 0))) {
                assert_eq!(slot_in_table(f).0, c.clsid, "{f} is not a {} function", c.name);
            }
        }
        // slots the game's own thunks call (libHttpClient.GDK / XCurl), matched by signature
        for (f, slot) in [
            ("XUserAddAsync", 7),
            ("XUserGetState", 14),
            ("XUserCheckPrivilege", 20),
            ("XUserUnregisterForChangeEvent", 34),
            ("XUserIsStoreUser", 42),
            ("XGameSaveCreateContainer", 16),
            ("XGameSaveCloseContainer", 17),
            ("XGameSaveReadBlobData", 20),
            ("XPackageInstallChunksAsync", 16),
            ("XPackageEnumeratePackages", 39),
            ("XPersistentLocalStorageGetPath", 4),
            ("XSystemGetAppSpecificDeviceId", 5),
            ("XGameGetXboxTitleId", 3),
            ("XAsyncBegin", 7),
            ("XTaskQueueCreate", 12),
        ] {
            assert_eq!(slot_in_table(f).2, slot, "{f}");
        }
        let main = own(&XUSER, &XUSER).unwrap();
        assert!(std::ptr::eq(own(&XUSER, &g(0x26f3c674_a2fe_44fa_b6c4_a323bc94ff53)).unwrap(), main), "a later XUser interface: the same vtable");
        assert!(std::ptr::eq(own(&XUSER, &g(0x5131d685_4394_4ee6_8c18_bfb5d4aef1ff)).unwrap(), main));
        let gt = own(&XUSER, &XUSER_GAMERTAG_IID).unwrap();
        assert!(!std::ptr::eq(gt, main), "XUserGetGamertag has its own vtable");
        assert_eq!(gt.targets[3], XUserGetGamertag as usize);
        assert_eq!(main.targets[7], XUserAddAsync as usize);
        let pkg = own(&XPACKAGE, &XPACKAGE_OLD_IID).expect("an XPackage interface id the table does not know");
        assert!(pkg.iface.main);
        assert_eq!(pkg.targets[slot_in_table("XPackageEnumeratePackages").2], enumerate_packages as usize);
        let pls = own(&XPLS, &g(0x41a4e10c_5a7e_41d9_8c37_37bde62a07d6)).expect("persistent local storage is XCL's");
        assert_eq!(pls.targets[4], XPersistentLocalStorageGetPath as usize);
        assert!(own(&XSTORE, &XSTORE).is_none());
        assert!(returns_bool("XUserIsStoreUser") && returns_bool("XPackageUnregisterPackageInstalled") && !returns_bool("XUserGetState"));
    }

    /// The thunk protocol end to end on the PC runtime: XUser and XGameSave are XCL's objects,
    /// their async calls complete on the PC runtime's task queues; XStore is refused; XAsync
    /// comes from the PC runtime.
    #[test]
    fn thunk_protocol_with_xcl_objects_on_pc_queues() {
    let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if pc().is_none() {
            println!("no system xgameruntime.dll – skipping");
            return;
        }
        unsafe {
            assert_eq!(InitializeApiImplEx2(0x6336_0000_2712, 0x65f4_0000_1edd, 2, std::ptr::null_mut()), S_OK);
            assert_eq!(query(XSTORE, XSTORE).0, REGDB_E_CLASSNOTREG);
            let (hr, xa) = query(XASYNC, XASYNC);
            assert!(hr >= 0 && !xa.is_null(), "XAsync from PC: {hr:#x}");
            let get_status: unsafe extern "system" fn(*mut c_void, *mut XAsyncBlock, bool) -> HRESULT = slot(xa, 3);

            // XUserAddAsync / Result through the vtable, completed on the PC process queue
            let (hr, u) = query(XUSER, g(0x26f3c674_a2fe_44fa_b6c4_a323bc94ff53));
            assert_eq!(hr, S_OK);
            let add: unsafe extern "system" fn(*mut c_void, u32, *mut XAsyncBlock) -> HRESULT = slot(u, 7);
            let result: unsafe extern "system" fn(*mut c_void, *mut XAsyncBlock, *mut *mut c_void) -> HRESULT = slot(u, 8);
            let mut b: XAsyncBlock = std::mem::zeroed();
            b.callback = done as usize;
            assert_eq!(add(u, 1, &mut b), S_OK);
            assert_eq!(get_status(xa, &mut b, true), S_OK);
            let mut user = std::ptr::null_mut();
            assert_eq!(result(u, &mut b, &mut user), S_OK);
            assert!(!user.is_null());
            let (_, gt) = query(XUSER, XUSER_GAMERTAG_IID);
            let gamertag: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, usize, *mut u8, *mut usize) -> HRESULT = slot(gt, 3);
            let mut buf = [0u8; 64];
            let mut used = 0usize;
            assert_eq!(gamertag(gt, user, 0, 64, buf.as_mut_ptr(), &mut used), S_OK);
            assert!(used > 1);
            // an unimplemented slot answers E_NOTIMPL and is reported
            let token: unsafe extern "system" fn(*mut c_void) -> HRESULT = slot(u, 23);
            assert_eq!(token(u), E_NOTIMPL);
            assert!(stats().unimplemented.contains_key("XUserGetTokenAndSignatureAsync"), "{:?}", stats().unimplemented);

            // XGameSave: provider (async), container, write a blob, read it back (async)
            let (_, s) = query(XGAMESAVE, g(0x1bfff3af_f14a_40a3_8e35_9ada906593f9));
            let init: unsafe extern "system" fn(*mut c_void, *mut c_void, *const u8, bool, *mut XAsyncBlock) -> HRESULT = slot(s, 4);
            let init_r: unsafe extern "system" fn(*mut c_void, *mut XAsyncBlock, *mut *mut Provider) -> HRESULT = slot(s, 5);
            let mut b: XAsyncBlock = std::mem::zeroed();
            assert_eq!(init(s, user, c"xcl-thunk-test".as_ptr() as *const u8, false, &mut b), S_OK);
            assert_eq!(get_status(xa, &mut b, true), S_OK);
            let mut p = std::ptr::null_mut();
            assert_eq!(init_r(s, &mut b, &mut p), S_OK);
            let create: unsafe extern "system" fn(*mut c_void, *mut Provider, *const u8, *mut *mut Container) -> HRESULT = slot(s, 16);
            let mut c = std::ptr::null_mut();
            assert_eq!(create(s, p, c"slot".as_ptr() as *const u8, &mut c), S_OK);
            let mut upd = std::ptr::null_mut();
            assert_eq!(XGameSaveCreateUpdate(c, c"t".as_ptr() as *const u8, &mut upd), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(upd, c"b".as_ptr() as *const u8, b"thunk".as_ptr(), 5), S_OK);
            assert_eq!(XGameSaveSubmitUpdate(upd), S_OK);
            XGameSaveCloseUpdate(upd);
            let read: unsafe extern "system" fn(*mut c_void, *mut Container, *const *const u8, u32, *mut XAsyncBlock) -> HRESULT = slot(s, 21);
            let read_r: unsafe extern "system" fn(*mut c_void, *mut XAsyncBlock, usize, *mut Blob, *mut u32) -> HRESULT = slot(s, 22);
            let names = [c"b".as_ptr() as *const u8];
            let mut b: XAsyncBlock = std::mem::zeroed();
            assert_eq!(read(s, c, names.as_ptr(), 1, &mut b), S_OK);
            assert_eq!(get_status(xa, &mut b, true), S_OK);
            let mut size = 0usize;
            assert_eq!(XAsyncGetResultSize(&mut b, &mut size), S_OK);
            let mut out = vec![0u64; size / 8 + 1];
            let mut n = 0u32;
            assert_eq!(read_r(s, &mut b, size, out.as_mut_ptr() as *mut Blob, &mut n), S_OK);
            assert_eq!(n, 1);
            XGameSaveCloseContainer(c);
            XGameSaveCloseProvider(p);

            // persistent local storage is XCL's (the PC runtime fails it for an unpackaged process)
            let (hr, pls) = query(XPLS, g(0x41a4e10c_5a7e_41d9_8c37_37bde62a07d6));
            assert_eq!(hr, S_OK);
            let size: unsafe extern "system" fn(*mut c_void, *mut usize) -> HRESULT = slot(pls, 3);
            let mut n = 0usize;
            assert_eq!(size(pls, &mut n), S_OK);
            assert!(n > 1);
            // completion callbacks run on the PC queue's threads, after the status
            for _ in 0..200 {
                if DONE.load(Ordering::SeqCst) >= 1 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            super::super::xasync::use_pc(0);
            assert!(DONE.load(Ordering::SeqCst) >= 1, "XAsync completion callback");
        }
    }
}
