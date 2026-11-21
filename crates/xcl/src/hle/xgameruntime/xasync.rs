//! XAsync / XAsyncProvider (contract: libHttpClient XAsync.h + XAsyncProvider.h, MIT).
//!
//! `XAsyncBlock` = { queue, context, callback, internal[32] }. We keep a pointer to our
//! state in `internal[0..8]`.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex};

use super::taskqueue::{self, Item, PORT_COMPLETION, PORT_WORK};
use crate::util::{E_ABORT, E_INVALIDARG, E_NOT_SUFFICIENT_BUFFER, E_PENDING, HRESULT, S_OK};

pub const OP_BEGIN: u32 = 0;
pub const OP_DO_WORK: u32 = 1;
pub const OP_GET_RESULT: u32 = 2;
pub const OP_CANCEL: u32 = 3;
pub const OP_CLEANUP: u32 = 4;

#[repr(C)]
pub struct XAsyncBlock {
    pub queue: *mut c_void,
    pub context: *mut c_void,
    pub callback: usize,
    pub internal: [u8; 32],
}

#[repr(C)]
pub struct XAsyncProviderData {
    pub async_: *mut XAsyncBlock,
    pub buffer_size: usize,
    pub buffer: *mut c_void,
    pub context: *mut c_void,
}

pub type XAsyncProvider = unsafe extern "system" fn(u32, *const XAsyncProviderData) -> HRESULT;
pub type XAsyncWork = unsafe extern "system" fn(*mut XAsyncBlock) -> HRESULT;
type Completion = unsafe extern "system" fn(*mut XAsyncBlock);

struct Inner {
    status: HRESULT,
    result_size: usize,
    done: bool,
    cleaned: bool,
}

pub struct State {
    provider: usize,
    context: usize,
    identity: usize,
    /// For XAsyncRun.
    work: usize,
    inner: Mutex<Inner>,
    cv: Condvar,
}

unsafe fn state_of(b: *mut XAsyncBlock) -> Option<Arc<State>> {
    if b.is_null() {
        return None;
    }
    let p = *((*b).internal.as_ptr() as *const usize);
    if p == 0 {
        return None;
    }
    let arc = Arc::from_raw(p as *const State);
    let c = arc.clone();
    std::mem::forget(arc);
    Some(c)
}

unsafe fn set_state(b: *mut XAsyncBlock, s: Option<Arc<State>>) {
    let slot = (*b).internal.as_mut_ptr() as *mut usize;
    let old = *slot;
    *slot = s.map(|a| Arc::into_raw(a) as usize).unwrap_or(0);
    if old != 0 {
        drop(Arc::from_raw(old as *const State));
    }
}

unsafe fn call_provider(s: &State, op: u32, b: *mut XAsyncBlock, buffer: *mut c_void, size: usize) -> HRESULT {
    if s.provider == 0 {
        return S_OK;
    }
    let data = XAsyncProviderData { async_: b, buffer_size: size, buffer, context: s.context as *mut c_void };
    let f: XAsyncProvider = std::mem::transmute(s.provider);
    f(op, &data)
}

unsafe extern "system" fn do_work_cb(ctx: *mut c_void, canceled: bool) {
    let b = ctx as *mut XAsyncBlock;
    let Some(s) = state_of(b) else { return };
    if canceled {
        XAsyncComplete(b, E_ABORT, 0);
        return;
    }
    let hr = if s.work != 0 {
        let f: XAsyncWork = std::mem::transmute(s.work);
        f(b)
    } else {
        call_provider(&s, OP_DO_WORK, b, std::ptr::null_mut(), 0)
    };
    if hr != E_PENDING {
        // providers that finish synchronously in DoWork may not call XAsyncComplete
        let done = s.inner.lock().unwrap().done;
        if !done {
            XAsyncComplete(b, hr, 0);
        }
    }
}

unsafe extern "system" fn completion_cb(ctx: *mut c_void, _canceled: bool) {
    let b = ctx as *mut XAsyncBlock;
    if (*b).callback != 0 {
        let f: Completion = std::mem::transmute((*b).callback);
        f(b);
    }
}

pub unsafe extern "system" fn XAsyncBegin(
    b: *mut XAsyncBlock,
    context: *mut c_void,
    identity: *const c_void,
    _identity_name: *const u8,
    provider: usize,
) -> HRESULT {
    if b.is_null() || provider == 0 {
        return E_INVALIDARG;
    }
    let s = Arc::new(State {
        provider,
        context: context as usize,
        identity: identity as usize,
        work: 0,
        inner: Mutex::new(Inner { status: E_PENDING, result_size: 0, done: false, cleaned: false }),
        cv: Condvar::new(),
    });
    set_state(b, Some(s.clone()));
    let hr = call_provider(&s, OP_BEGIN, b, std::ptr::null_mut(), 0);
    if hr < 0 {
        XAsyncComplete(b, hr, 0);
    }
    hr
}

pub unsafe extern "system" fn XAsyncSchedule(b: *mut XAsyncBlock, delay_ms: u32) -> HRESULT {
    if b.is_null() {
        return E_INVALIDARG;
    }
    let Some(q) = taskqueue::queue_from((*b).queue) else { return E_INVALIDARG };
    taskqueue::submit_delayed(&q, PORT_WORK, delay_ms, Item { cb: do_work_cb as usize, ctx: b as usize });
    S_OK
}

pub unsafe extern "system" fn XAsyncComplete(b: *mut XAsyncBlock, result: HRESULT, required: usize) {
    let Some(s) = state_of(b) else { return };
    let cleanup_now;
    {
        let mut g = s.inner.lock().unwrap();
        if g.done {
            return;
        }
        g.status = if result == E_PENDING { crate::util::E_FAIL } else { result };
        g.result_size = if result >= 0 { required } else { 0 };
        g.done = true;
        cleanup_now = g.result_size == 0;
        if cleanup_now {
            g.cleaned = true;
        }
    }
    if cleanup_now {
        call_provider(&s, OP_CLEANUP, b, std::ptr::null_mut(), 0);
    }
    s.cv.notify_all();
    if (*b).callback != 0 {
        if let Some(q) = taskqueue::queue_from((*b).queue) {
            taskqueue::submit_now(&q, PORT_COMPLETION, Item { cb: completion_cb as usize, ctx: b as usize });
        }
    }
}

pub unsafe extern "system" fn XAsyncGetStatus(b: *mut XAsyncBlock, wait: bool) -> HRESULT {
    let Some(s) = state_of(b) else { return E_INVALIDARG };
    let mut g = s.inner.lock().unwrap();
    if wait {
        while !g.done {
            g = s.cv.wait(g).unwrap();
        }
    }
    if g.done {
        g.status
    } else {
        E_PENDING
    }
}

pub unsafe extern "system" fn XAsyncGetResultSize(b: *mut XAsyncBlock, size: *mut usize) -> HRESULT {
    let Some(s) = state_of(b) else { return E_INVALIDARG };
    let g = s.inner.lock().unwrap();
    if !g.done {
        return E_PENDING;
    }
    if !size.is_null() {
        *size = g.result_size;
    }
    g.status
}

pub unsafe extern "system" fn XAsyncGetResult(
    b: *mut XAsyncBlock,
    identity: *const c_void,
    size: usize,
    buffer: *mut c_void,
    used: *mut usize,
) -> HRESULT {
    let Some(s) = state_of(b) else { return E_INVALIDARG };
    let (status, need, cleaned) = {
        let g = s.inner.lock().unwrap();
        if !g.done {
            return E_PENDING;
        }
        (g.status, g.result_size, g.cleaned)
    };
    if !identity.is_null() && s.identity != 0 && identity as usize != s.identity {
        return E_INVALIDARG;
    }
    if status < 0 {
        return status;
    }
    if need > 0 {
        if cleaned {
            return E_INVALIDARG; // result already consumed
        }
        if size < need || buffer.is_null() {
            return E_NOT_SUFFICIENT_BUFFER;
        }
        let hr = call_provider(&s, OP_GET_RESULT, b, buffer, size);
        if !used.is_null() {
            *used = need;
        }
        s.inner.lock().unwrap().cleaned = true;
        call_provider(&s, OP_CLEANUP, b, std::ptr::null_mut(), 0);
        return hr;
    }
    if !used.is_null() {
        *used = 0;
    }
    S_OK
}

pub unsafe extern "system" fn XAsyncCancel(b: *mut XAsyncBlock) {
    let Some(s) = state_of(b) else { return };
    if s.inner.lock().unwrap().done {
        return;
    }
    let _ = call_provider(&s, OP_CANCEL, b, std::ptr::null_mut(), 0);
    XAsyncComplete(b, E_ABORT, 0);
}

pub unsafe extern "system" fn XAsyncRun(b: *mut XAsyncBlock, work: usize) -> HRESULT {
    if b.is_null() || work == 0 {
        return E_INVALIDARG;
    }
    let s = Arc::new(State {
        provider: 0,
        context: 0,
        identity: 0,
        work,
        inner: Mutex::new(Inner { status: E_PENDING, result_size: 0, done: false, cleaned: false }),
        cv: Condvar::new(),
    });
    set_state(b, Some(s));
    XAsyncSchedule(b, 0)
}

// ---------------------------------------------------------------------------------------
// Helper for our own async APIs: run `work` on the queue, complete with a byte payload.
// ---------------------------------------------------------------------------------------

struct Payload {
    result: Mutex<Option<Result<Vec<u8>, HRESULT>>>,
    work: Mutex<Option<Box<dyn FnOnce() -> Result<Vec<u8>, HRESULT> + Send>>>,
}

unsafe extern "system" fn payload_provider(op: u32, data: *const XAsyncProviderData) -> HRESULT {
    let d = &*data;
    let p = &*(d.context as *const Payload);
    match op {
        OP_BEGIN => XAsyncSchedule(d.async_, 0),
        OP_DO_WORK => {
            let w = p.work.lock().unwrap().take();
            let r = w.map(|f| f()).unwrap_or(Err(crate::util::E_FAIL));
            let (hr, size) = match &r {
                Ok(v) => (S_OK, v.len()),
                Err(e) => (*e, 0),
            };
            *p.result.lock().unwrap() = Some(r);
            XAsyncComplete(d.async_, hr, size);
            S_OK
        }
        OP_GET_RESULT => {
            if let Some(Ok(v)) = p.result.lock().unwrap().as_ref() {
                std::ptr::copy_nonoverlapping(v.as_ptr(), d.buffer as *mut u8, v.len().min(d.buffer_size));
            }
            S_OK
        }
        OP_CLEANUP => {
            drop(Box::from_raw(d.context as *mut Payload));
            S_OK
        }
        _ => S_OK,
    }
}

/// Start an async op whose result is `work()`'s bytes; `identity` ties GetResult calls.
pub unsafe fn begin_with<F>(b: *mut XAsyncBlock, identity: usize, work: F) -> HRESULT
where
    F: FnOnce() -> Result<Vec<u8>, HRESULT> + Send + 'static,
{
    let p = Box::into_raw(Box::new(Payload { result: Mutex::new(None), work: Mutex::new(Some(Box::new(work))) }));
    // On failure XAsyncBegin completes the block, which runs OP_CLEANUP and frees `p`.
    XAsyncBegin(b, p as *mut c_void, identity as *const c_void, std::ptr::null(), payload_provider as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static DONE: AtomicU32 = AtomicU32::new(0);
    unsafe extern "system" fn on_done(_b: *mut XAsyncBlock) {
        DONE.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn begin_with_payload_roundtrip() {
        unsafe {
            let mut b: XAsyncBlock = std::mem::zeroed();
            b.callback = on_done as usize;
            let id = 0x1234usize;
            assert_eq!(begin_with(&mut b, id, || Ok(b"hello".to_vec())), S_OK);
            assert_eq!(XAsyncGetStatus(&mut b, true), S_OK);
            let mut size = 0usize;
            assert_eq!(XAsyncGetResultSize(&mut b, &mut size), S_OK);
            assert_eq!(size, 5);
            let mut buf = [0u8; 8];
            let mut used = 0usize;
            assert_eq!(XAsyncGetResult(&mut b, id as _, 2, buf.as_mut_ptr() as _, &mut used), E_NOT_SUFFICIENT_BUFFER);
            assert_eq!(XAsyncGetResult(&mut b, id as _, 8, buf.as_mut_ptr() as _, &mut used), S_OK);
            assert_eq!(&buf[..5], b"hello");
            assert_eq!(used, 5);
            // completion callback is delivered on the (thread pool) completion port
            for _ in 0..200 {
                if DONE.load(Ordering::SeqCst) > 0 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(DONE.load(Ordering::SeqCst) > 0);
        }
    }

    #[test]
    fn errors_propagate() {
        unsafe {
            let mut b: XAsyncBlock = std::mem::zeroed();
            begin_with(&mut b, 1, || Err(crate::util::E_FAIL));
            assert_eq!(XAsyncGetStatus(&mut b, true), crate::util::E_FAIL);
        }
    }

    unsafe extern "system" fn run_work(_b: *mut XAsyncBlock) -> HRESULT {
        7
    }

    #[test]
    fn xasync_run() {
        unsafe {
            let mut b: XAsyncBlock = std::mem::zeroed();
            assert_eq!(XAsyncRun(&mut b, run_work as usize), S_OK);
            assert_eq!(XAsyncGetStatus(&mut b, true), 7);
        }
    }
}
