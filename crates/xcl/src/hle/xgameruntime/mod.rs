//! XGameRuntime – offline implementation of the GDK system APIs a title needs to boot.
//! Signatures: public GDK reference (docs.xbox.com).

#![allow(non_snake_case)]

pub mod api;
pub mod gamesave;
pub mod taskqueue;
pub mod xasync;

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::util::{write_cstr_out, E_INVALIDARG, E_NOT_FOUND, E_NOT_SUFFICIENT_BUFFER, E_NOT_SUPPORTED, HRESULT, S_OK};
use gamesave::*;
use taskqueue::*;
use api::{InitializeApiImpl, InitializeApiImplEx, InitializeApiImplEx2, QueryApiImpl, UninitializeApiImpl, XErrorReport};
use xasync::*;

pub fn exports() -> Vec<(&'static str, usize)> {
    crate::hle_exports![
        // the GDK thunk protocol (api.rs) – what titles actually call
        InitializeApiImpl,
        InitializeApiImplEx,
        InitializeApiImplEx2,
        UninitializeApiImpl,
        QueryApiImpl,
        XErrorReport,
        // runtime
        XGameRuntimeInitialize,
        XGameRuntimeInitializeWithOptions,
        XGameRuntimeUninitialize,
        XGameRuntimeIsFeatureAvailable,
        // task queue
        XTaskQueueCreate,
        XTaskQueueDuplicateHandle,
        XTaskQueueCloseHandle,
        XTaskQueueSubmitCallback,
        XTaskQueueSubmitDelayedCallback,
        XTaskQueueDispatch,
        XTaskQueueIsEmpty,
        XTaskQueueTerminate,
        XTaskQueueGetCurrentProcessTaskQueue,
        XTaskQueueSetCurrentProcessTaskQueue,
        XTaskQueueRegisterWaiter,
        XTaskQueueUnregisterWaiter,
        XTaskQueueRegisterMonitor,
        XTaskQueueUnregisterMonitor,
        // async
        XAsyncBegin,
        XAsyncSchedule,
        XAsyncComplete,
        XAsyncGetStatus,
        XAsyncGetResultSize,
        XAsyncGetResult,
        XAsyncCancel,
        XAsyncRun,
        // threads
        XThreadSetTimeSensitive,
        XThreadIsTimeSensitive,
        // system
        XSystemGetDeviceType,
        XSystemGetConsoleId,
        XSystemGetAnalyticsInfo,
        XSystemGetXboxLiveSandboxId,
        // package
        XPackageIsPackagedProcess,
        XPackageGetCurrentProcessPackageIdentifier,
        XPackageMount,
        XPackageGetMountPathSize,
        XPackageGetMountPath,
        XPackageCloseMountHandle,
        XPackageEnumeratePackages,
        // storage
        XPersistentLocalStorageGetPathSize,
        XPersistentLocalStorageGetPath,
        XPersistentLocalStorageGetSpaceInfo,
        XGameSaveFilesGetFolderWithUiAsync,
        XGameSaveFilesGetFolderWithUiResult,
        // game save containers
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
        XGameSaveCreateContainer,
        XGameSaveCloseContainer,
        XGameSaveGetContainerInfo,
        XGameSaveEnumerateContainerInfo,
        XGameSaveEnumerateContainerInfoByName,
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
        // users
        XUserAddAsync,
        XUserAddResult,
        XUserDuplicateHandle,
        XUserCloseHandle,
        XUserCompare,
        XUserGetMaxUsers,
        XUserGetId,
        XUserGetLocalId,
        XUserFindUserByLocalId,
        XUserFindUserById,
        XUserGetIsGuest,
        XUserGetState,
        XUserGetGamertag,
        XUserGetAgeGroup,
        XUserRegisterForChangeEvent,
        XUserUnregisterForChangeEvent,
        // store (offline)
        XStoreCreateContext,
        XStoreCloseContextHandle,
    ]
}

// ---------------------------------------------------------------------------------------
// runtime
// ---------------------------------------------------------------------------------------

static INIT_COUNT: AtomicU32 = AtomicU32::new(0);

pub unsafe extern "system" fn XGameRuntimeInitialize() -> HRESULT {
    if INIT_COUNT.fetch_add(1, Ordering::AcqRel) == 0 {
        crate::xlog_info!("XGameRuntimeInitialize (XCL offline runtime)");
    }
    S_OK
}

pub unsafe extern "system" fn XGameRuntimeInitializeWithOptions(_options: *const c_void) -> HRESULT {
    XGameRuntimeInitialize()
}

pub unsafe extern "system" fn XGameRuntimeUninitialize() {
    INIT_COUNT.fetch_sub(1, Ordering::AcqRel);
}

pub unsafe extern "system" fn XGameRuntimeIsFeatureAvailable(_feature: u32) -> bool {
    true
}

// ---------------------------------------------------------------------------------------
// XThread
// ---------------------------------------------------------------------------------------

thread_local! {
    static TIME_SENSITIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub unsafe extern "system" fn XThreadSetTimeSensitive(v: bool) -> HRESULT {
    TIME_SENSITIVE.with(|c| c.set(v));
    S_OK
}

pub unsafe extern "system" fn XThreadIsTimeSensitive() -> bool {
    TIME_SENSITIVE.with(|c| c.get())
}

// ---------------------------------------------------------------------------------------
// XSystem
// ---------------------------------------------------------------------------------------

pub const DEVICE_XBOX_SCARLETT_LOCKHART: u32 = 0x06;
pub const DEVICE_XBOX_SCARLETT_ANACONDA: u32 = 0x07;

pub unsafe extern "system" fn XSystemGetDeviceType() -> u32 {
    if crate::state::config().is_series_s() {
        DEVICE_XBOX_SCARLETT_LOCKHART
    } else {
        DEVICE_XBOX_SCARLETT_ANACONDA
    }
}

pub unsafe extern "system" fn XSystemGetConsoleId(size: usize, buf: *mut u8, used: *mut usize) -> HRESULT {
    write_cstr_out(&crate::state::config().console_id, buf, size, used)
}

pub unsafe extern "system" fn XSystemGetXboxLiveSandboxId(size: usize, buf: *mut u8, used: *mut usize) -> HRESULT {
    write_cstr_out(&crate::state::config().sandbox_id, buf, size, used)
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct XVersion {
    pub major: u16,
    pub minor: u16,
    pub build: u16,
    pub revision: u16,
}

#[repr(C)]
pub struct XSystemAnalyticsInfo {
    pub os_version: XVersion,
    pub hosting_os_version: XVersion,
    pub family: [u8; 64],
    pub form: [u8; 64],
}

fn fixed64(s: &str) -> [u8; 64] {
    let mut a = [0u8; 64];
    let b = s.as_bytes();
    a[..b.len().min(63)].copy_from_slice(&b[..b.len().min(63)]);
    a
}

pub unsafe extern "system" fn XSystemGetAnalyticsInfo() -> XSystemAnalyticsInfo {
    let (major, minor, build) = super::kernelx::host_os_version();
    let v = XVersion { major: major as u16, minor: minor as u16, build: build as u16, revision: 0 };
    let form = if crate::state::config().is_series_s() { "Xbox Series S" } else { "Xbox Series X" };
    XSystemAnalyticsInfo { os_version: v, hosting_os_version: v, family: fixed64("Gaming"), form: fixed64(form) }
}

// ---------------------------------------------------------------------------------------
// XPackage
// ---------------------------------------------------------------------------------------

pub fn package_identifier() -> String {
    let c = crate::state::config();
    if !c.package_identifier.is_empty() {
        return c.package_identifier.clone();
    }
    let title = crate::state::boot().map(|b| b.title.clone()).unwrap_or_else(|| "XCL.Title".into());
    format!("{title}_1.0.0.0_x64__xcl")
}

fn game_root() -> String {
    crate::state::boot()
        .map(|b| b.game_root.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".into())
}

pub unsafe extern "system" fn XPackageIsPackagedProcess() -> bool {
    true
}

pub unsafe extern "system" fn XPackageGetCurrentProcessPackageIdentifier(size: usize, buf: *mut u8) -> HRESULT {
    write_cstr_out(&package_identifier(), buf, size, std::ptr::null_mut())
}

struct Mount {
    path: String,
}

pub unsafe extern "system" fn XPackageMount(id: *const u8, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let id = crate::util::from_cstr_ptr(id).unwrap_or_default();
    if !id.is_empty() && !id.eq_ignore_ascii_case(&package_identifier()) {
        crate::xlog_warn!("XPackageMount({id}) – unknown package (DLC?)");
        return E_NOT_FOUND;
    }
    *out = Box::into_raw(Box::new(Mount { path: game_root() })) as *mut c_void;
    S_OK
}

pub unsafe extern "system" fn XPackageGetMountPathSize(m: *mut c_void, size: *mut usize) -> HRESULT {
    if m.is_null() || size.is_null() {
        return E_INVALIDARG;
    }
    let mount = &*(m as *const Mount);
    *size = mount.path.len() + 1;
    S_OK
}

pub unsafe extern "system" fn XPackageGetMountPath(m: *mut c_void, size: usize, buf: *mut u8) -> HRESULT {
    if m.is_null() {
        return E_INVALIDARG;
    }
    write_cstr_out(&(*(m as *const Mount)).path, buf, size, std::ptr::null_mut())
}

pub unsafe extern "system" fn XPackageCloseMountHandle(m: *mut c_void) {
    if !m.is_null() {
        drop(Box::from_raw(m as *mut Mount));
    }
}

pub unsafe extern "system" fn XPackageEnumeratePackages(_kind: u32, _scope: u32, _ctx: *mut c_void, _cb: usize) -> HRESULT {
    // No DLC/related packages offline.
    S_OK
}

// ---------------------------------------------------------------------------------------
// Persistent local storage + save files
// ---------------------------------------------------------------------------------------

fn data_subdir(name: &str) -> PathBuf {
    let p = crate::state::data_dir().join(name);
    let _ = std::fs::create_dir_all(&p);
    p
}

fn pls_path() -> String {
    data_subdir("pls").to_string_lossy().into_owned()
}

pub unsafe extern "system" fn XPersistentLocalStorageGetPathSize(size: *mut usize) -> HRESULT {
    if size.is_null() {
        return E_INVALIDARG;
    }
    *size = pls_path().len() + 1;
    S_OK
}

pub unsafe extern "system" fn XPersistentLocalStorageGetPath(size: usize, buf: *mut u8, used: *mut usize) -> HRESULT {
    write_cstr_out(&pls_path(), buf, size, used)
}

#[repr(C)]
pub struct XPersistentLocalStorageSpaceInfo {
    pub available_free_bytes: u64,
    pub total_free_bytes: u64,
    pub used_bytes: u64,
    pub total_bytes: u64,
}

pub unsafe extern "system" fn XPersistentLocalStorageGetSpaceInfo(info: *mut XPersistentLocalStorageSpaceInfo) -> HRESULT {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    if info.is_null() {
        return E_INVALIDARG;
    }
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    GetDiskFreeSpaceExW(crate::util::wide(&pls_path()).as_ptr(), &mut avail, &mut total, &mut free);
    // the title's quota (MicrosoftGame.Config <PersistentLocalStorage><SizeMB>, else 4 GiB)
    let budget = match crate::state::config().pls_mb {
        0 => 4096,
        mb => mb,
    } << 20;
    *info = XPersistentLocalStorageSpaceInfo {
        available_free_bytes: avail.min(budget),
        total_free_bytes: free.min(budget),
        used_bytes: 0,
        total_bytes: budget,
    };
    S_OK
}

static SAVE_FILES_IDENTITY: u8 = 0;

pub unsafe extern "system" fn XGameSaveFilesGetFolderWithUiAsync(_user: *mut c_void, scid: *const u8, b: *mut XAsyncBlock) -> HRESULT {
    let scid = crate::util::from_cstr_ptr(scid).unwrap_or_else(|| "default".into());
    let safe: String = scid.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    begin_with(b, &SAVE_FILES_IDENTITY as *const u8 as usize, move || {
        let p = data_subdir("saves").join(if safe.is_empty() { "default".into() } else { safe });
        std::fs::create_dir_all(&p).map_err(|_| crate::util::E_FAIL)?;
        let mut v = p.to_string_lossy().into_owned().into_bytes();
        v.push(0);
        Ok(v)
    })
}

pub unsafe extern "system" fn XGameSaveFilesGetFolderWithUiResult(b: *mut XAsyncBlock, size: usize, buf: *mut u8) -> HRESULT {
    XAsyncGetResult(b, &SAVE_FILES_IDENTITY as *const u8 as *const c_void, size, buf as *mut c_void, std::ptr::null_mut())
}

// ---------------------------------------------------------------------------------------
// XUser – one signed-in offline user
// ---------------------------------------------------------------------------------------

#[repr(C)]
pub struct User {
    magic: u32,
    refs: AtomicU32,
}

static USER: User = User { magic: 0x5553_4552, refs: AtomicU32::new(1) };
const LOCAL_ID: u64 = 1;
static USER_ADD_IDENTITY: u8 = 0;

fn user_ptr() -> *mut c_void {
    &USER as *const User as *mut c_void
}

unsafe fn valid_user(h: *mut c_void) -> bool {
    h == user_ptr()
}

pub unsafe extern "system" fn XUserAddAsync(options: u32, b: *mut XAsyncBlock) -> HRESULT {
    crate::xlog_info!("XUserAddAsync(options={options:#x}) → local offline user '{}'", crate::state::config().gamertag);
    begin_with(b, &USER_ADD_IDENTITY as *const u8 as usize, || {
        USER.refs.fetch_add(1, Ordering::AcqRel);
        Ok((user_ptr() as usize).to_le_bytes().to_vec())
    })
}

pub unsafe extern "system" fn XUserAddResult(b: *mut XAsyncBlock, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    XAsyncGetResult(b, &USER_ADD_IDENTITY as *const u8 as *const c_void, 8, out as *mut c_void, std::ptr::null_mut())
}

pub unsafe extern "system" fn XUserDuplicateHandle(h: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    if !valid_user(h) || out.is_null() {
        return E_INVALIDARG;
    }
    USER.refs.fetch_add(1, Ordering::AcqRel);
    *out = h;
    S_OK
}

pub unsafe extern "system" fn XUserCloseHandle(h: *mut c_void) {
    if valid_user(h) {
        let _ = USER.refs.fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| Some(v.saturating_sub(1).max(1)));
    }
}

pub unsafe extern "system" fn XUserCompare(a: *mut c_void, b: *mut c_void) -> i32 {
    (a as isize - b as isize).signum() as i32
}

pub unsafe extern "system" fn XUserGetMaxUsers(out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = 1;
    S_OK
}

pub unsafe extern "system" fn XUserGetId(h: *mut c_void, out: *mut u64) -> HRESULT {
    if !valid_user(h) || out.is_null() {
        return E_INVALIDARG;
    }
    *out = crate::state::config().user_id;
    S_OK
}

pub unsafe extern "system" fn XUserGetLocalId(h: *mut c_void, out: *mut u64) -> HRESULT {
    if !valid_user(h) || out.is_null() {
        return E_INVALIDARG;
    }
    *out = LOCAL_ID;
    S_OK
}

pub unsafe extern "system" fn XUserFindUserByLocalId(id: u64, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    if id != LOCAL_ID {
        return E_NOT_FOUND;
    }
    USER.refs.fetch_add(1, Ordering::AcqRel);
    *out = user_ptr();
    S_OK
}

pub unsafe extern "system" fn XUserFindUserById(id: u64, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    if id != crate::state::config().user_id {
        return E_NOT_FOUND;
    }
    USER.refs.fetch_add(1, Ordering::AcqRel);
    *out = user_ptr();
    S_OK
}

pub unsafe extern "system" fn XUserGetIsGuest(h: *mut c_void, out: *mut bool) -> HRESULT {
    if !valid_user(h) || out.is_null() {
        return E_INVALIDARG;
    }
    *out = false;
    S_OK
}

pub unsafe extern "system" fn XUserGetState(h: *mut c_void, out: *mut u32) -> HRESULT {
    if !valid_user(h) || out.is_null() {
        return E_INVALIDARG;
    }
    *out = 0; // SignedIn
    S_OK
}

pub const GAMERTAG_SIZES: [usize; 4] = [16, 97, 15, 101]; // Classic, Modern, ModernSuffix, UniqueModern

pub unsafe extern "system" fn XUserGetGamertag(h: *mut c_void, component: u32, size: usize, buf: *mut u8, used: *mut usize) -> HRESULT {
    if !valid_user(h) || component > 3 {
        return E_INVALIDARG;
    }
    let tag = crate::state::config().gamertag.clone();
    let value = match component {
        2 => String::new(), // ModernSuffix: none
        3 => format!("{tag}#0001"),
        _ => tag,
    };
    if size < value.len() + 1 {
        if !used.is_null() {
            *used = value.len() + 1;
        }
        return E_NOT_SUFFICIENT_BUFFER;
    }
    write_cstr_out(&value, buf, size, used)
}

pub unsafe extern "system" fn XUserGetAgeGroup(h: *mut c_void, out: *mut u32) -> HRESULT {
    if !valid_user(h) || out.is_null() {
        return E_INVALIDARG;
    }
    *out = 3; // Adult
    S_OK
}

pub unsafe extern "system" fn XUserRegisterForChangeEvent(_q: *mut c_void, _ctx: *mut c_void, _cb: usize, token: *mut RegistrationToken) -> HRESULT {
    if token.is_null() {
        return E_INVALIDARG;
    }
    (*token).token = 1; // offline: the user never changes
    S_OK
}

pub unsafe extern "system" fn XUserUnregisterForChangeEvent(_token: RegistrationToken, _wait: bool) -> bool {
    true
}

// ---------------------------------------------------------------------------------------
// XStore – deliberately unavailable (no fake licences)
// ---------------------------------------------------------------------------------------

pub unsafe extern "system" fn XStoreCreateContext(_user: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    crate::xlog_warn!("XStoreCreateContext → unavailable (offline mode, XCL does not fake licenses)");
    if !out.is_null() {
        *out = std::ptr::null_mut();
    }
    E_NOT_SUPPORTED
}

pub unsafe extern "system" fn XStoreCloseContextHandle(_h: *mut c_void) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_add_and_queries() {
    let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let mut b: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XUserAddAsync(1, &mut b), S_OK);
            assert_eq!(XAsyncGetStatus(&mut b, true), S_OK);
            let mut u = std::ptr::null_mut();
            assert_eq!(XUserAddResult(&mut b, &mut u), S_OK);
            assert!(!u.is_null());
            let mut tag = [0u8; 16];
            let mut used = 0usize;
            assert_eq!(XUserGetGamertag(u, 0, 16, tag.as_mut_ptr(), &mut used), S_OK);
            assert_eq!(&tag[..used - 1], b"Player1");
            assert_eq!(XUserGetGamertag(u, 0, 3, tag.as_mut_ptr(), &mut used), E_NOT_SUFFICIENT_BUFFER);
            let mut id = 0u64;
            assert_eq!(XUserGetId(u, &mut id), S_OK);
            assert_ne!(id, 0);
            XUserCloseHandle(u);
            assert_eq!(XUserGetId(std::ptr::null_mut(), &mut id), E_INVALIDARG);
        }
    }

    #[test]
    fn system_info() {
        unsafe {
            assert_eq!(XSystemGetDeviceType(), DEVICE_XBOX_SCARLETT_ANACONDA);
            let info = XSystemGetAnalyticsInfo();
            assert!(info.os_version.major >= 10);
            assert_eq!(&info.form[..13], b"Xbox Series X");
            assert_eq!(std::mem::size_of::<XSystemAnalyticsInfo>(), 144);
        }
    }

    #[test]
    fn save_folder_async() {
    let _g = crate::hle::xgameruntime::xasync::ASYNC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            let mut b: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XGameSaveFilesGetFolderWithUiAsync(std::ptr::null_mut(), b"scid-1\0".as_ptr(), &mut b), S_OK);
            assert_eq!(XAsyncGetStatus(&mut b, true), S_OK);
            let mut buf = [0u8; 512];
            assert_eq!(XGameSaveFilesGetFolderWithUiResult(&mut b, 512, buf.as_mut_ptr()), S_OK);
            let s = std::ffi::CStr::from_ptr(buf.as_ptr() as *const _).to_string_lossy().into_owned();
            assert!(s.ends_with("saves\\scid-1"), "{s}");
            assert!(std::path::Path::new(&s).is_dir());
        }
    }
}
