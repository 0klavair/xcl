//! `xcrdapi.dll` – the console's DirectStorage: `DStorageGetFactory` → IDStorageFactoryX.
//!
//! The interfaces are the PC DirectStorage ones in the console's vtable order and structure
//! layouts (as observed on Fortnite, which reads all its data through them):
//! * factory: QueryInterface, AddRef, Release, CreateQueue(desc, riid, out), OpenFile(path,
//!   riid, out), CreateStatusArray(capacity, name, riid, out), SetCpuAffinity(mask),
//!   SetDebugFlags(flags), SetStagingBufferSize(bytes), then 7 more,
//! * queue desc (0x18): source type u32 @0 (0 file, 1 memory), capacity u16 @8, priority i8
//!   @10 (-1 … 2; memory queues 2), name @0x10,
//! * request (0x48): options u64 @0 (compression = bits 0–7: 0 none, 1 zlib; bit 22 memory
//!   source), destination @8, destination size u32 @0x18, source @0x20 (an IDStorageFileX or
//!   memory), file offset @0x28, source size u32 @0x30, intermediate size u32 @0x34,
//!   cancellation tag @0x38, name @0x40,
//! * queue: QueryInterface, AddRef, Release, EnqueueRequest, EnqueueStatus(array, index),
//!   EnqueueSignal(fence, value), Submit, CancelRequestsWithTag(mask, value), Close,
//!   GetErrorEvent, RetrieveErrorRecord (0x290 bytes), Query (0x1C: the desc, then empty
//!   slots u16 @0x18 and requests until the automatic submit u16 @0x1A), then 6 more,
//! * file: QueryInterface, AddRef, Release, Close, GetHandle, 2 unknown, GetId, then 4 more;
//!   status array: QueryInterface, AddRef, Release, IsComplete(index), GetHResult(index),
//!   then 5 more.
//!
//! Each queue has a worker thread: submitted requests run in order (file reads through the
//! VFS like the title's own, memory copies, zlib), a status entry gets the first failure of
//! the requests since the previous one, a signal sets the fence from the CPU. Methods not
//! listed are logged and answer E_NOTIMPL; BCPack and other console formats are refused.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::os::windows::fs::FileExt;
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE};
use windows_sys::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT};
use windows_sys::Win32::System::Threading::{CreateEventW, GetCurrentProcess, SetEvent};

use crate::util::{HRESULT, S_OK};

const DLL: &str = "xcrdapi.dll";

const E_NOTIMPL: HRESULT = 0x8000_4001u32 as i32;
const E_NOINTERFACE: HRESULT = 0x8000_4002u32 as i32;
const E_POINTER: HRESULT = 0x8000_4003u32 as i32;
const E_PENDING: HRESULT = 0x8000_000Au32 as i32;
const E_INVALIDARG: HRESULT = 0x8007_0057u32 as i32;
const E_ABORT: HRESULT = 0x8000_4004u32 as i32;
const E_QUEUE_CAPACITY: HRESULT = 0x8924_0003u32 as i32;
const E_END_OF_FILE: HRESULT = 0x8924_0007u32 as i32;
const E_ACCESS_VIOLATION: HRESULT = 0x8924_0009u32 as i32;
const E_DESTINATION_SIZE: HRESULT = 0x8924_000Fu32 as i32;
const E_QUEUE_CLOSED: HRESULT = 0x8924_0010u32 as i32;
const E_QUEUE_PRIORITY: HRESULT = 0x8924_0013u32 as i32;
const E_FILE_HANDLE: HRESULT = 0x8924_0017u32 as i32;
const E_SOURCE_TYPE: HRESULT = 0x8924_001Bu32 as i32;
const E_DECOMPRESSION: HRESULT = 0x8924_0030u32 as i32;
const E_ZLIB_DATA: HRESULT = 0x8924_0032u32 as i32;
const E_QUEUE_TYPE: HRESULT = 0x8924_00F5u32 as i32;

const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
const IID_FACTORY: GUID = GUID::from_u128(0xfb3b54ac_bf82_463a_b777_73195837864d);
const IID_FILE: GUID = GUID::from_u128(0x43704f9f_27c6_4a9f_8815_d16527894df4);
const IID_QUEUE: GUID = GUID::from_u128(0xa3f6649a_8c2c_4b3a_8b6e_10186a04b9bc);
const IID_STATUS_ARRAY: GUID = GUID::from_u128(0xcd66d752_2786_44f7_9070_e2a6fdad3f1b);

const SOURCE_MEMORY_BIT: u64 = 1 << 22;
const REQUEST_BYTES: usize = 0x48;
const ERROR_RECORD_BYTES: usize = 0x290;
const NO_COMMAND: u32 = 0xFFFF_FFFF;
const COMMAND_REQUEST: u32 = 0;
/// ID3D12Fence::Signal
const FENCE_SIGNAL: usize = 10;

pub fn exports() -> Vec<(&'static str, usize)> {
    crate::hle_exports![DStorageGetFactory]
}

fn same(a: &GUID, b: &GUID) -> bool {
    (a.data1, a.data2, a.data3, a.data4) == (b.data1, b.data2, b.data3, b.data4)
}

fn warn_once(what: &str, why: std::fmt::Arguments) {
    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let mut s = SEEN.lock().unwrap();
    if s.len() < 64 && !s.iter().any(|x| x == what) {
        s.push(what.to_string());
        crate::xlog_warn!("DirectStorage: {what}: {why}");
    }
}

/// A vtable: the listed methods, then logged E_NOTIMPL stubs up to `total` entries.
fn vtable(iface: &str, methods: &[usize], total: usize) -> &'static [usize] {
    let mut v = methods.to_vec();
    for i in methods.len()..total {
        let (stub, _) = crate::stubs::make_stub_info(DLL, &format!("{iface}::#{i}"), E_NOTIMPL as u32 as u64);
        v.push(stub);
    }
    Box::leak(v.into_boxed_slice())
}

/// COM object header: vtable, reference count, the shared part.
#[repr(C)]
struct Object<T> {
    vtbl: *const usize,
    refs: AtomicU32,
    inner: Arc<T>,
}

unsafe fn new_object<T>(vtbl: &'static [usize], inner: Arc<T>) -> *mut c_void {
    Box::into_raw(Box::new(Object { vtbl: vtbl.as_ptr(), refs: AtomicU32::new(1), inner })) as *mut c_void
}

unsafe fn inner<'a, T>(p: *mut c_void) -> &'a Arc<T> {
    &(*(p as *const Object<T>)).inner
}

unsafe fn add_ref<T>(p: *mut c_void) -> u32 {
    (*(p as *const Object<T>)).refs.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe fn release<T>(p: *mut c_void) -> u32 {
    let n = (*(p as *const Object<T>)).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if n == 0 {
        drop(Box::from_raw(p as *mut Object<T>));
    }
    n
}

unsafe fn no_ref(_p: *mut c_void) -> u32 {
    1
}

unsafe fn query(p: *mut c_void, own: &GUID, riid: *const GUID, out: *mut *mut c_void, add: unsafe fn(*mut c_void) -> u32) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    if !riid.is_null() && (same(&*riid, own) || same(&*riid, &IID_IUNKNOWN)) {
        add(p);
        *out = p;
        return S_OK;
    }
    *out = std::ptr::null_mut();
    E_NOINTERFACE
}

// ---------------------------------------------------------------------------------------
// factory
// ---------------------------------------------------------------------------------------

/// `HRESULT DStorageGetFactory(REFIID riid, void** factory)`
pub unsafe extern "system" fn DStorageGetFactory(riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    static FACTORY: OnceLock<usize> = OnceLock::new();
    let f = *FACTORY.get_or_init(|| {
        let v = vtable(
            "IDStorageFactoryX",
            &[
                factory_qi as usize,
                static_ref as usize,
                static_ref as usize,
                factory_create_queue as usize,
                factory_open_file as usize,
                factory_create_status_array as usize,
                factory_set_cpu_affinity as usize,
                factory_set_debug_flags as usize,
                factory_set_staging_buffer_size as usize,
            ],
            16,
        );
        Box::into_raw(Box::new(v.as_ptr())) as usize
    });
    factory_qi(f as *mut c_void, riid, out)
}

unsafe extern "system" fn static_ref(_p: *mut c_void) -> u32 {
    1
}

unsafe extern "system" fn factory_qi(p: *mut c_void, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    query(p, &IID_FACTORY, riid, out, no_ref)
}

unsafe extern "system" fn factory_set_cpu_affinity(_p: *mut c_void, _mask: u64) -> HRESULT {
    S_OK
}

unsafe extern "system" fn factory_set_debug_flags(_p: *mut c_void, _flags: u32) {}

unsafe extern "system" fn factory_set_staging_buffer_size(_p: *mut c_void, _bytes: u32) -> HRESULT {
    S_OK
}

// ---------------------------------------------------------------------------------------
// files
// ---------------------------------------------------------------------------------------

struct FileInner {
    id: u32,
    path: Vec<u16>,
    file: Mutex<Option<Arc<std::fs::File>>>,
}

impl FileInner {
    fn handle(&self) -> Option<Arc<std::fs::File>> {
        self.file.lock().unwrap().clone()
    }
}

fn file_vtbl() -> &'static [usize] {
    static V: OnceLock<&'static [usize]> = OnceLock::new();
    V.get_or_init(|| {
        vtable(
            "IDStorageFileX",
            &[file_qi as usize, file_add_ref as usize, file_release as usize, file_close as usize, file_get_handle as usize, file_unknown as usize, file_unknown as usize, file_get_id as usize],
            12,
        )
    })
}

unsafe extern "system" fn factory_open_file(_p: *mut c_void, path: *const u16, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    static NEXT_ID: AtomicU32 = AtomicU32::new(1);
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    let Some(name) = crate::util::from_wide_ptr(path) else { return E_INVALIDARG };
    // through the VFS hooks, as the title's own CreateFileW
    let file = match std::fs::File::open(&name) {
        Ok(f) => f,
        Err(e) => {
            crate::xlog_warn!("DirectStorage: OpenFile '{name}': {e}");
            return e.raw_os_error().map_or(E_INVALIDARG, |c| (0x8007_0000u32 | (c as u32 & 0xFFFF)) as i32);
        }
    };
    let inner = Arc::new(FileInner { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), path: name.encode_utf16().collect(), file: Mutex::new(Some(Arc::new(file))) });
    let o = new_object(file_vtbl(), inner);
    let hr = file_qi(o, riid, out);
    file_release(o);
    hr
}

unsafe extern "system" fn file_qi(p: *mut c_void, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    query(p, &IID_FILE, riid, out, add_ref::<FileInner>)
}

unsafe extern "system" fn file_add_ref(p: *mut c_void) -> u32 {
    add_ref::<FileInner>(p)
}

unsafe extern "system" fn file_release(p: *mut c_void) -> u32 {
    release::<FileInner>(p)
}

unsafe extern "system" fn file_close(p: *mut c_void) {
    inner::<FileInner>(p).file.lock().unwrap().take();
}

/// a handle of the title's own (it closes it)
unsafe extern "system" fn file_get_handle(p: *mut c_void) -> HANDLE {
    let Some(f) = inner::<FileInner>(p).handle() else { return std::ptr::null_mut() };
    let mut h: HANDLE = std::ptr::null_mut();
    let me = GetCurrentProcess();
    if DuplicateHandle(me, f.as_raw_handle() as HANDLE, me, &mut h, 0, 0, DUPLICATE_SAME_ACCESS) == 0 {
        return std::ptr::null_mut();
    }
    h
}

unsafe extern "system" fn file_unknown(_p: *mut c_void) -> HRESULT {
    warn_once("IDStorageFileX #5 / #6", format_args!("not modelled"));
    E_NOTIMPL
}

unsafe extern "system" fn file_get_id(p: *mut c_void) -> u32 {
    let f = inner::<FileInner>(p);
    if f.handle().is_some() {
        f.id
    } else {
        u32::MAX
    }
}

// ---------------------------------------------------------------------------------------
// status arrays
// ---------------------------------------------------------------------------------------

struct StatusInner {
    slots: Box<[AtomicI32]>,
}

fn status_vtbl() -> &'static [usize] {
    static V: OnceLock<&'static [usize]> = OnceLock::new();
    V.get_or_init(|| {
        vtable(
            "IDStorageStatusArrayX",
            &[status_qi as usize, status_add_ref as usize, status_release as usize, status_is_complete as usize, status_get_hresult as usize],
            10,
        )
    })
}

unsafe extern "system" fn factory_create_status_array(_p: *mut c_void, capacity: u32, _name: *const u8, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    if capacity == 0 || capacity > 1 << 20 {
        return E_INVALIDARG;
    }
    let inner = Arc::new(StatusInner { slots: (0..capacity).map(|_| AtomicI32::new(E_PENDING)).collect() });
    let o = new_object(status_vtbl(), inner);
    let hr = status_qi(o, riid, out);
    status_release(o);
    hr
}

unsafe extern "system" fn status_qi(p: *mut c_void, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    query(p, &IID_STATUS_ARRAY, riid, out, add_ref::<StatusInner>)
}

unsafe extern "system" fn status_add_ref(p: *mut c_void) -> u32 {
    add_ref::<StatusInner>(p)
}

unsafe extern "system" fn status_release(p: *mut c_void) -> u32 {
    release::<StatusInner>(p)
}

unsafe extern "system" fn status_is_complete(p: *mut c_void, index: u32) -> bool {
    inner::<StatusInner>(p).slots.get(index as usize).is_some_and(|s| s.load(Ordering::Acquire) != E_PENDING)
}

unsafe extern "system" fn status_get_hresult(p: *mut c_void, index: u32) -> HRESULT {
    inner::<StatusInner>(p).slots.get(index as usize).map_or(E_INVALIDARG, |s| s.load(Ordering::Acquire))
}

// ---------------------------------------------------------------------------------------
// requests
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Request {
    options: u64,
    destination: u64,
    destination_size: u32,
    source: u64,
    offset: u64,
    source_size: u32,
    intermediate_size: u32,
    tag: u64,
}

impl Request {
    fn parse(b: &[u8; REQUEST_BYTES]) -> Request {
        let q = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let d = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let memory = q(0) & SOURCE_MEMORY_BIT != 0;
        Request {
            options: q(0),
            destination: q(8),
            destination_size: d(0x18),
            source: q(0x20),
            offset: if memory { 0 } else { q(0x28) },
            source_size: d(0x30),
            intermediate_size: d(0x34),
            tag: q(0x38),
        }
    }

    fn memory_source(&self) -> bool {
        self.options & SOURCE_MEMORY_BIT != 0
    }

    fn compression(&self) -> u8 {
        self.options as u8
    }

    /// What a queue of `source_memory` refuses at enqueue.
    fn check(&self, source_memory: bool) -> Result<(), HRESULT> {
        if self.memory_source() != source_memory {
            return Err(E_SOURCE_TYPE);
        }
        if self.destination == 0 || (self.memory_source() && self.source == 0) {
            return Err(E_ACCESS_VIOLATION);
        }
        let sizes_ok = match self.compression() {
            0 => self.destination_size == self.source_size,
            1 => self.destination_size > self.source_size,
            _ => return Err(E_DECOMPRESSION),
        };
        if !sizes_ok || (self.intermediate_size != 0 && self.intermediate_size != self.destination_size) {
            return Err(E_DESTINATION_SIZE);
        }
        Ok(())
    }
}

/// Whether `n` bytes at `p` are committed, writable memory.
fn writable(p: usize, n: usize) -> bool {
    let Some(end) = p.checked_add(n) else { return false };
    let mut at = p;
    while at < end {
        let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { VirtualQuery(at as *const c_void, &mut m, std::mem::size_of_val(&m)) } == 0 {
            return false;
        }
        let rw = 0x04 | 0x08 | 0x40 | 0x80; // PAGE_READWRITE / WRITECOPY / EXECUTE_READWRITE / EXECUTE_WRITECOPY
        if m.State != MEM_COMMIT || m.Protect & rw == 0 || m.Protect & 0x100 != 0 {
            return false;
        }
        at = m.BaseAddress as usize + m.RegionSize;
    }
    true
}

fn read_exact_at(f: &std::fs::File, offset: u64, buf: &mut [u8]) -> Result<(), HRESULT> {
    let mut done = 0;
    while done < buf.len() {
        match f.seek_read(&mut buf[done..], offset + done as u64) {
            Ok(0) => return Err(E_END_OF_FILE),
            Ok(n) => done += n,
            Err(e) if e.raw_os_error() == Some(38) => return Err(E_END_OF_FILE), // ERROR_HANDLE_EOF
            Err(e) => return Err(e.raw_os_error().map_or(E_ABORT, |c| (0x8007_0000u32 | (c as u32 & 0xFFFF)) as i32)),
        }
    }
    Ok(())
}

/// Run a request; the destination gets the bytes or nothing.
unsafe fn execute(r: &Request, file: Option<&FileInner>) -> Result<(), HRESULT> {
    let n = r.destination_size as usize;
    if !writable(r.destination as usize, n) {
        return Err(E_ACCESS_VIOLATION);
    }
    let src: Vec<u8> = if r.memory_source() {
        if !crate::gpu::shader::readable(r.source as usize, r.source_size as usize) {
            return Err(E_ACCESS_VIOLATION);
        }
        std::slice::from_raw_parts(r.source as *const u8, r.source_size as usize).to_vec()
    } else {
        let f = file.and_then(|f| f.handle()).ok_or(E_FILE_HANDLE)?;
        let mut b = vec![0u8; r.source_size as usize];
        read_exact_at(&f, r.offset, &mut b)?;
        b
    };
    let out = std::slice::from_raw_parts_mut(r.destination as *mut u8, n);
    match r.compression() {
        0 => out.copy_from_slice(&src[..n]),
        1 => {
            let d = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&src, n).map_err(|_| E_ZLIB_DATA)?;
            if d.len() != n {
                return Err(E_ZLIB_DATA);
            }
            out.copy_from_slice(&d);
        }
        _ => return Err(E_DECOMPRESSION),
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// queues
// ---------------------------------------------------------------------------------------

enum Entry {
    Request(Request, Option<Arc<FileInner>>),
    /// refused at enqueue: counts as a failed request
    Refused(HRESULT),
    Status(Arc<StatusInner>, u32),
    /// a fence (one reference held) and the value it gets
    Signal(usize, u64),
}

struct FirstError {
    hr: HRESULT,
    request: Request,
    file: Vec<u16>,
}

struct QueueInner {
    desc: [u8; 0x18],
    source_memory: bool,
    capacity: usize,
    pending: Mutex<Vec<Entry>>,
    worker: Mutex<Option<mpsc::Sender<Vec<Entry>>>>,
    closed: AtomicBool,
    error_event: usize,
    errors: Mutex<(u32, Option<FirstError>)>,
}

impl QueueInner {
    fn fail(&self, hr: HRESULT, request: Request, file: Option<&FileInner>) {
        let mut e = self.errors.lock().unwrap();
        e.0 += 1;
        if e.1.is_none() {
            e.1 = Some(FirstError { hr, request, file: file.map(|f| f.path.clone()).unwrap_or_default() });
        }
        unsafe { SetEvent(self.error_event as HANDLE) };
    }

    fn submit(self: &Arc<Self>) {
        let batch = std::mem::take(&mut *self.pending.lock().unwrap());
        if batch.is_empty() {
            return;
        }
        let mut w = self.worker.lock().unwrap();
        if w.is_none() {
            let (tx, rx) = mpsc::channel::<Vec<Entry>>();
            let q = Arc::clone(self);
            let started = std::thread::Builder::new().name("xcl-dstorage".into()).spawn(move || q.run(rx));
            if started.is_err() {
                return;
            }
            *w = Some(tx);
        }
        let _ = w.as_ref().unwrap().send(batch);
    }

    fn run(&self, rx: mpsc::Receiver<Vec<Entry>>) {
        // the first failure of the requests since the last status entry
        let mut since_status = S_OK;
        for batch in rx {
            for e in batch {
                match e {
                    Entry::Request(r, f) => {
                        if let Err(hr) = unsafe { execute(&r, f.as_deref()) } {
                            self.fail(hr, r, f.as_deref());
                            if since_status >= 0 {
                                since_status = hr;
                            }
                        }
                    }
                    Entry::Refused(hr) => {
                        if since_status >= 0 {
                            since_status = hr;
                        }
                    }
                    Entry::Status(s, i) => {
                        if let Some(slot) = s.slots.get(i as usize) {
                            slot.store(since_status, Ordering::Release);
                        }
                        since_status = S_OK;
                    }
                    Entry::Signal(fence, value) => unsafe {
                        let vt = *(fence as *const *const usize);
                        let signal: unsafe extern "system" fn(usize, u64) -> HRESULT = std::mem::transmute(*vt.add(FENCE_SIGNAL));
                        signal(fence, value);
                        let release: unsafe extern "system" fn(usize) -> u32 = std::mem::transmute(*vt.add(2));
                        release(fence);
                    },
                }
            }
        }
    }

    fn enqueue(self: &Arc<Self>, e: Entry) {
        let n = {
            let mut p = self.pending.lock().unwrap();
            p.push(e);
            p.len()
        };
        // as on PC: a half-full queue submits by itself
        if n >= (self.capacity / 2).max(1) {
            self.submit();
        }
    }
}

impl Drop for QueueInner {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.error_event as HANDLE) };
    }
}

fn queue_vtbl() -> &'static [usize] {
    static V: OnceLock<&'static [usize]> = OnceLock::new();
    V.get_or_init(|| {
        vtable(
            "IDStorageQueueX",
            &[
                queue_qi as usize,
                queue_add_ref as usize,
                queue_release as usize,
                queue_enqueue_request as usize,
                queue_enqueue_status as usize,
                queue_enqueue_signal as usize,
                queue_submit as usize,
                queue_cancel as usize,
                queue_close as usize,
                queue_get_error_event as usize,
                queue_retrieve_error_record as usize,
                queue_query as usize,
            ],
            18,
        )
    })
}

unsafe extern "system" fn factory_create_queue(_p: *mut c_void, desc: *const u8, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    if desc.is_null() {
        return E_INVALIDARG;
    }
    let raw: [u8; 0x18] = (desc as *const [u8; 0x18]).read_unaligned();
    let source = u32::from_le_bytes(raw[0..4].try_into().unwrap());
    let capacity = u16::from_le_bytes(raw[8..10].try_into().unwrap()) as usize;
    let priority = raw[10] as i8;
    if source > 1 {
        return E_QUEUE_TYPE;
    }
    if !(-1..=2).contains(&priority) || (source == 1 && priority != 2) {
        return E_QUEUE_PRIORITY;
    }
    if capacity < 2 {
        return E_QUEUE_CAPACITY;
    }
    let ev = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
    if ev.is_null() {
        return E_ABORT;
    }
    let inner = Arc::new(QueueInner {
        desc: raw,
        source_memory: source == 1,
        capacity,
        pending: Mutex::new(Vec::new()),
        worker: Mutex::new(None),
        closed: AtomicBool::new(false),
        error_event: ev as usize,
        errors: Mutex::new((0, None)),
    });
    let o = new_object(queue_vtbl(), inner);
    let hr = queue_qi(o, riid, out);
    queue_release(o);
    hr
}

unsafe fn queue<'a>(p: *mut c_void) -> &'a Arc<QueueInner> {
    inner::<QueueInner>(p)
}

unsafe extern "system" fn queue_qi(p: *mut c_void, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    query(p, &IID_QUEUE, riid, out, add_ref::<QueueInner>)
}

unsafe extern "system" fn queue_add_ref(p: *mut c_void) -> u32 {
    add_ref::<QueueInner>(p)
}

unsafe extern "system" fn queue_release(p: *mut c_void) -> u32 {
    let last = (*(p as *const Object<QueueInner>)).refs.load(Ordering::Acquire) == 1;
    if last {
        // the worker ends with its channel
        queue(p).worker.lock().unwrap().take();
    }
    release::<QueueInner>(p)
}

unsafe extern "system" fn queue_enqueue_request(p: *mut c_void, req: *const u8) {
    let q = queue(p);
    if q.closed.load(Ordering::Acquire) || req.is_null() {
        return;
    }
    let r = Request::parse(&*(req as *const [u8; REQUEST_BYTES]));
    if let Err(hr) = r.check(q.source_memory) {
        warn_once("EnqueueRequest", format_args!("refused ({hr:#x}): {r:x?}"));
        q.fail(hr, r, None);
        return q.enqueue(Entry::Refused(hr));
    }
    let file = if r.memory_source() {
        None
    } else {
        // only a file this factory opened
        let f = r.source as *mut c_void;
        if !crate::gpu::shader::readable(f as usize, 8) || *(f as *const *const usize) != file_vtbl().as_ptr() {
            q.fail(E_FILE_HANDLE, r, None);
            return q.enqueue(Entry::Refused(E_FILE_HANDLE));
        }
        Some(Arc::clone(inner::<FileInner>(f)))
    };
    q.enqueue(Entry::Request(r, file));
}

unsafe extern "system" fn queue_enqueue_status(p: *mut c_void, array: *mut c_void, index: u32) {
    let q = queue(p);
    if q.closed.load(Ordering::Acquire) || array.is_null() {
        return;
    }
    let s = Arc::clone(inner::<StatusInner>(array));
    if let Some(slot) = s.slots.get(index as usize) {
        slot.store(E_PENDING, Ordering::Release);
    }
    q.enqueue(Entry::Status(s, index));
}

unsafe extern "system" fn queue_enqueue_signal(p: *mut c_void, fence: *mut c_void, value: u64) {
    let q = queue(p);
    if q.closed.load(Ordering::Acquire) || fence.is_null() {
        return;
    }
    let vt = *(fence as *const *const usize);
    let add: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vt.add(1));
    add(fence);
    q.enqueue(Entry::Signal(fence as usize, value));
}

unsafe extern "system" fn queue_submit(p: *mut c_void) {
    let q = queue(p);
    if !q.closed.load(Ordering::Acquire) {
        q.submit();
    }
}

/// requests not submitted yet whose tag matches (`tag & mask == value`) are dropped
unsafe extern "system" fn queue_cancel(p: *mut c_void, mask: u64, value: u64) {
    queue(p).pending.lock().unwrap().retain(|e| !matches!(e, Entry::Request(r, _) if r.tag & mask == value));
}

unsafe extern "system" fn queue_close(p: *mut c_void) {
    let q = queue(p);
    if q.closed.swap(true, Ordering::AcqRel) {
        return;
    }
    for e in std::mem::take(&mut *q.pending.lock().unwrap()) {
        match e {
            Entry::Status(s, i) => {
                if let Some(slot) = s.slots.get(i as usize) {
                    slot.store(E_QUEUE_CLOSED, Ordering::Release);
                }
            }
            Entry::Signal(fence, _) => {
                let vt = *(fence as *const *const usize);
                let release: unsafe extern "system" fn(usize) -> u32 = std::mem::transmute(*vt.add(2));
                release(fence);
            }
            _ => {}
        }
    }
    q.worker.lock().unwrap().take();
}

unsafe extern "system" fn queue_get_error_event(p: *mut c_void) -> HANDLE {
    queue(p).error_event as HANDLE
}

/// DSTORAGE_ERROR_RECORD (0x290): failure count u32 @0, then the first failure: HRESULT @8,
/// command type @0xC (none: 0xFFFFFFFF), the request's options @0x10, destination @0x18 and
/// size @0x20, its file name @0x24 (0x104 UTF-16) or memory source @0x238
unsafe extern "system" fn queue_retrieve_error_record(p: *mut c_void, out: *mut u8) {
    if out.is_null() {
        return;
    }
    let mut rec = [0u8; ERROR_RECORD_BYTES];
    let e = queue(p).errors.lock().unwrap();
    match &e.1 {
        Some(f) => {
            rec[0..4].copy_from_slice(&e.0.to_le_bytes());
            rec[8..12].copy_from_slice(&f.hr.to_le_bytes());
            rec[0xC..0x10].copy_from_slice(&COMMAND_REQUEST.to_le_bytes());
            rec[0x10..0x18].copy_from_slice(&f.request.options.to_le_bytes());
            rec[0x18..0x20].copy_from_slice(&f.request.destination.to_le_bytes());
            rec[0x20..0x24].copy_from_slice(&f.request.destination_size.to_le_bytes());
            if f.request.memory_source() {
                rec[0x238..0x240].copy_from_slice(&f.request.source.to_le_bytes());
            } else {
                for (i, c) in f.file.iter().take(0x103).enumerate() {
                    rec[0x24 + 2 * i..0x26 + 2 * i].copy_from_slice(&c.to_le_bytes());
                }
            }
        }
        None => rec[0xC..0x10].copy_from_slice(&NO_COMMAND.to_le_bytes()),
    }
    std::ptr::copy_nonoverlapping(rec.as_ptr(), out, ERROR_RECORD_BYTES);
}

/// DSTORAGE_QUEUE_INFO (0x1C): the queue's desc, empty slots, requests until it submits by itself
unsafe extern "system" fn queue_query(p: *mut c_void, out: *mut u8) {
    if out.is_null() {
        return;
    }
    let q = queue(p);
    let pending = q.pending.lock().unwrap().len();
    let empty = q.capacity.saturating_sub(pending) as u16;
    let until = (q.capacity / 2).max(1).saturating_sub(pending).max(1) as u16;
    let mut info = [0u8; 0x1C];
    info[..0x18].copy_from_slice(&q.desc);
    info[0x18..0x1A].copy_from_slice(&empty.to_le_bytes());
    info[0x1A..0x1C].copy_from_slice(&until.to_le_bytes());
    std::ptr::copy_nonoverlapping(info.as_ptr(), out, info.len());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(options: u64, dst: u64, dst_size: u32, src: u64, offset: u64, src_size: u32) -> [u8; REQUEST_BYTES] {
        let mut b = [0u8; REQUEST_BYTES];
        b[0..8].copy_from_slice(&options.to_le_bytes());
        b[8..16].copy_from_slice(&dst.to_le_bytes());
        b[0x18..0x1C].copy_from_slice(&dst_size.to_le_bytes());
        b[0x20..0x28].copy_from_slice(&src.to_le_bytes());
        b[0x28..0x30].copy_from_slice(&offset.to_le_bytes());
        b[0x30..0x34].copy_from_slice(&src_size.to_le_bytes());
        b
    }

    #[test]
    fn requests_are_checked_like_the_console() {
        let ok = Request::parse(&request(0, 0x1000, 64, 0x2000, 0, 64));
        assert_eq!(ok.check(false), Ok(()));
        assert_eq!(ok.check(true), Err(E_SOURCE_TYPE));
        let mem = Request::parse(&request(SOURCE_MEMORY_BIT | 1, 0x1000, 64, 0x2000, 0x99, 20));
        assert_eq!((mem.offset, mem.check(true)), (0, Ok(())), "a memory source has no offset");
        assert_eq!(Request::parse(&request(1, 0x1000, 20, 0x2000, 0, 20)).check(false), Err(E_DESTINATION_SIZE));
        assert_eq!(Request::parse(&request(3, 0x1000, 64, 0x2000, 0, 20)).check(false), Err(E_DECOMPRESSION), "BCPack");
        assert_eq!(Request::parse(&request(0, 0, 64, 0x2000, 0, 64)).check(false), Err(E_ACCESS_VIOLATION));
    }

    #[test]
    fn a_memory_queue_inflates_zlib_and_reports_through_status_and_errors() {
        unsafe {
            let mut f = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY, &mut f), S_OK);
            let vt = *(f as *const *const usize);
            let call = |i: usize| *vt.add(i);
            let mut desc = [0u8; 0x18];
            desc[0..4].copy_from_slice(&1u32.to_le_bytes());
            desc[8..10].copy_from_slice(&8u16.to_le_bytes());
            desc[10] = 2;
            let create_queue: unsafe extern "system" fn(*mut c_void, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(call(3));
            let mut q = std::ptr::null_mut();
            assert_eq!(create_queue(f, desc.as_ptr(), &IID_QUEUE, &mut q), S_OK);
            desc[10] = 0;
            let mut bad = std::ptr::null_mut();
            assert_eq!(create_queue(f, desc.as_ptr(), &IID_QUEUE, &mut bad), E_QUEUE_PRIORITY, "a memory queue is realtime");
            let create_status: unsafe extern "system" fn(*mut c_void, u32, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(call(5));
            let mut s = std::ptr::null_mut();
            assert_eq!(create_status(f, 2, std::ptr::null(), &IID_STATUS_ARRAY, &mut s), S_OK);

            let plain = b"XCL DirectStorage zlib payload, XCL DirectStorage zlib payload".to_vec();
            let packed = miniz_oxide::deflate::compress_to_vec_zlib(&plain, 6);
            let mut dst = vec![0u8; plain.len()];
            let qv = *(q as *const *const usize);
            let enqueue: unsafe extern "system" fn(*mut c_void, *const u8) = std::mem::transmute(*qv.add(3));
            let status: unsafe extern "system" fn(*mut c_void, *mut c_void, u32) = std::mem::transmute(*qv.add(4));
            let submit: unsafe extern "system" fn(*mut c_void) = std::mem::transmute(*qv.add(6));
            enqueue(q, request(SOURCE_MEMORY_BIT | 1, dst.as_mut_ptr() as u64, dst.len() as u32, packed.as_ptr() as u64, 0, packed.len() as u32).as_ptr());
            status(q, s, 0);
            // a refused request: the next status reports it, the error record names it
            enqueue(q, request(SOURCE_MEMORY_BIT, dst.as_mut_ptr() as u64, 8, packed.as_ptr() as u64, 0, 4).as_ptr());
            status(q, s, 1);
            submit(q);
            let sv = *(s as *const *const usize);
            let complete: unsafe extern "system" fn(*mut c_void, u32) -> bool = std::mem::transmute(*sv.add(3));
            let hresult: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT = std::mem::transmute(*sv.add(4));
            let t = std::time::Instant::now();
            while !complete(s, 1) && t.elapsed().as_secs() < 5 {
                std::thread::yield_now();
            }
            assert_eq!((hresult(s, 0), hresult(s, 1)), (S_OK, E_DESTINATION_SIZE));
            assert_eq!(dst, plain);
            let record: unsafe extern "system" fn(*mut c_void, *mut u8) = std::mem::transmute(*qv.add(10));
            let mut rec = [0u8; ERROR_RECORD_BYTES];
            record(q, rec.as_mut_ptr());
            assert_eq!((u32::from_le_bytes(rec[0..4].try_into().unwrap()), i32::from_le_bytes(rec[8..12].try_into().unwrap())), (1, E_DESTINATION_SIZE));
            let qinfo: unsafe extern "system" fn(*mut c_void, *mut u8) = std::mem::transmute(*qv.add(11));
            let mut info = [0u8; 0x1C];
            qinfo(q, info.as_mut_ptr());
            assert_eq!((info[10], u16::from_le_bytes([info[0x18], info[0x19]])), (2, 8));
            queue_close(q);
            queue_release(q);
            status_release(s);
        }
    }
}
