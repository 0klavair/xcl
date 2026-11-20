//! XTaskQueue – GDK's async dispatch primitive (contract: libHttpClient XTaskQueue.h, MIT).
//!
//! Dispatch modes: Manual (caller pumps `XTaskQueueDispatch`), ThreadPool,
//! SerializedThreadPool (pool, one at a time) and Immediate (run on the submitting
//! thread). Callbacks submitted after `XTaskQueueTerminate` run with `canceled = true`.

#![allow(non_snake_case)]

use std::collections::{BinaryHeap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::util::{E_INVALIDARG, HRESULT, S_OK};

pub const MODE_MANUAL: u32 = 0;
pub const MODE_THREAD_POOL: u32 = 1;
pub const MODE_SERIALIZED_THREAD_POOL: u32 = 2;
pub const MODE_IMMEDIATE: u32 = 3;
pub const PORT_WORK: u32 = 0;
pub const PORT_COMPLETION: u32 = 1;
pub const E_ABORT_QUEUE_TERMINATED: HRESULT = 0x8004_0001u32 as i32; // E_ABORT-like

pub type XTaskQueueCallback = unsafe extern "system" fn(*mut c_void, bool);
pub type XTaskQueueTerminatedCallback = unsafe extern "system" fn(*mut c_void);

#[derive(Clone, Copy)]
pub struct Item {
    pub cb: usize,
    pub ctx: usize,
}

impl Item {
    pub unsafe fn run(self, canceled: bool) {
        let f: XTaskQueueCallback = std::mem::transmute(self.cb);
        f(self.ctx as *mut c_void, canceled);
    }
}

pub struct Port {
    mode: u32,
    items: Mutex<VecDeque<Item>>,
    cv: Condvar,
    running: AtomicBool,
}

impl Port {
    fn new(mode: u32) -> Port {
        Port { mode, items: Mutex::new(VecDeque::new()), cv: Condvar::new(), running: AtomicBool::new(false) }
    }
}

pub struct Queue {
    ports: [Port; 2],
    terminated: AtomicBool,
    inflight: AtomicUsize,
    idle: (Mutex<()>, Condvar),
}

/// What an `XTaskQueueHandle` points to.
pub struct QueueHandle {
    magic: u64,
    pub queue: Arc<Queue>,
}

const MAGIC: u64 = 0x5843_4C51_5545_5545; // "XCLQUEUE"

// ---------------------------------------------------------------------------------------
// shared worker pool + timer
// ---------------------------------------------------------------------------------------

type Job = Box<dyn FnOnce() + Send>;

fn pool() -> &'static Mutex<mpsc::Sender<Job>> {
    static POOL: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8);
        for i in 0..n {
            let rx = rx.clone();
            std::thread::Builder::new()
                .name(format!("xcl-taskqueue-{i}"))
                .spawn(move || loop {
                    let job = { rx.lock().unwrap().recv() };
                    match job {
                        Ok(j) => j(),
                        Err(_) => break,
                    }
                })
                .expect("spawn task queue worker");
        }
        Mutex::new(tx)
    })
}

fn spawn(job: Job) {
    let _ = pool().lock().unwrap().send(job);
}

struct Timed {
    at: Instant,
    seq: u64,
    queue: Arc<Queue>,
    port: u32,
    item: Item,
}
impl PartialEq for Timed {
    fn eq(&self, o: &Self) -> bool {
        self.at == o.at && self.seq == o.seq
    }
}
impl Eq for Timed {}
impl PartialOrd for Timed {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Timed {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        // min-heap on (at, seq)
        o.at.cmp(&self.at).then(o.seq.cmp(&self.seq))
    }
}

fn timer() -> &'static (Mutex<BinaryHeap<Timed>>, Condvar) {
    static T: OnceLock<(Mutex<BinaryHeap<Timed>>, Condvar)> = OnceLock::new();
    static STARTED: AtomicBool = AtomicBool::new(false);
    let t = T.get_or_init(|| (Mutex::new(BinaryHeap::new()), Condvar::new()));
    if !STARTED.swap(true, Ordering::AcqRel) {
        std::thread::Builder::new()
            .name("xcl-taskqueue-timer".into())
            .spawn(|| {
                let (m, cv) = timer();
                let mut g = m.lock().unwrap();
                loop {
                    let now = Instant::now();
                    match g.peek() {
                        None => g = cv.wait(g).unwrap(),
                        Some(t) if t.at <= now => {
                            let t = g.pop().unwrap();
                            drop(g);
                            submit_now(&t.queue, t.port, t.item);
                            g = m.lock().unwrap();
                        }
                        Some(t) => {
                            let d = t.at - now;
                            g = cv.wait_timeout(g, d).unwrap().0;
                        }
                    }
                }
            })
            .expect("spawn timer");
    }
    t
}

static SEQ: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------------------
// core operations
// ---------------------------------------------------------------------------------------

impl Queue {
    pub fn new(work_mode: u32, completion_mode: u32) -> Arc<Queue> {
        Arc::new(Queue {
            ports: [Port::new(work_mode), Port::new(completion_mode)],
            terminated: AtomicBool::new(false),
            inflight: AtomicUsize::new(0),
            idle: (Mutex::new(()), Condvar::new()),
        })
    }

    fn done_one(&self) {
        if self.inflight.fetch_sub(1, Ordering::AcqRel) == 1 {
            let _g = self.idle.0.lock().unwrap();
            self.idle.1.notify_all();
        }
    }

    unsafe fn execute(self: &Arc<Queue>, item: Item) {
        let canceled = self.terminated.load(Ordering::Acquire);
        item.run(canceled);
        self.done_one();
    }
}

fn drain_serialized(q: Arc<Queue>, port: u32) {
    let p = &q.ports[port as usize];
    loop {
        let next = p.items.lock().unwrap().pop_front();
        match next {
            Some(it) => unsafe { q.execute(it) },
            None => {
                p.running.store(false, Ordering::Release);
                // re-check: an item may have arrived after pop but before the store
                let more = !p.items.lock().unwrap().is_empty();
                if more && !p.running.swap(true, Ordering::AcqRel) {
                    continue;
                }
                break;
            }
        }
    }
}

pub fn submit_now(q: &Arc<Queue>, port: u32, item: Item) {
    let p = &q.ports[port as usize];
    q.inflight.fetch_add(1, Ordering::AcqRel);
    match p.mode {
        MODE_IMMEDIATE => unsafe { q.execute(item) },
        MODE_THREAD_POOL => {
            let q2 = q.clone();
            spawn(Box::new(move || unsafe { q2.execute(item) }));
        }
        MODE_SERIALIZED_THREAD_POOL => {
            p.items.lock().unwrap().push_back(item);
            if !p.running.swap(true, Ordering::AcqRel) {
                let q2 = q.clone();
                spawn(Box::new(move || drain_serialized(q2, port)));
            }
        }
        _ => {
            p.items.lock().unwrap().push_back(item);
            p.cv.notify_one();
        }
    }
}

pub fn submit_delayed(q: &Arc<Queue>, port: u32, delay_ms: u32, item: Item) {
    if delay_ms == 0 {
        return submit_now(q, port, item);
    }
    let (m, cv) = timer();
    m.lock().unwrap().push(Timed {
        at: Instant::now() + Duration::from_millis(delay_ms as u64),
        seq: SEQ.fetch_add(1, Ordering::Relaxed),
        queue: q.clone(),
        port,
        item,
    });
    cv.notify_all();
}

/// Dispatch one item from a Manual port. Returns true when something ran.
pub fn dispatch(q: &Arc<Queue>, port: u32, timeout_ms: u32) -> bool {
    let p = &q.ports[port as usize];
    let mut g = p.items.lock().unwrap();
    if g.is_empty() && timeout_ms != 0 {
        let dur = if timeout_ms == u32::MAX { Duration::from_secs(u64::MAX / 4) } else { Duration::from_millis(timeout_ms as u64) };
        let deadline = Instant::now() + dur.min(Duration::from_secs(365 * 24 * 3600));
        while g.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            g = p.cv.wait_timeout(g, deadline - now).unwrap().0;
        }
    }
    let Some(item) = g.pop_front() else { return false };
    drop(g);
    unsafe { q.execute(item) };
    true
}

// ---------------------------------------------------------------------------------------
// handles + process queue
// ---------------------------------------------------------------------------------------

pub fn new_handle(q: Arc<Queue>) -> *mut QueueHandle {
    Box::into_raw(Box::new(QueueHandle { magic: MAGIC, queue: q }))
}

pub unsafe fn queue_from(h: *mut c_void) -> Option<Arc<Queue>> {
    if h.is_null() {
        return Some(process_queue());
    }
    let qh = &*(h as *const QueueHandle);
    (qh.magic == MAGIC).then(|| qh.queue.clone())
}

static PROCESS_QUEUE: Mutex<Option<Arc<Queue>>> = Mutex::new(None);

pub fn process_queue() -> Arc<Queue> {
    PROCESS_QUEUE
        .lock()
        .unwrap()
        .get_or_insert_with(|| Queue::new(MODE_THREAD_POOL, MODE_THREAD_POOL))
        .clone()
}

// ---------------------------------------------------------------------------------------
// exported API
// ---------------------------------------------------------------------------------------

pub unsafe extern "system" fn XTaskQueueCreate(work: u32, completion: u32, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() || work > 3 || completion > 3 {
        return E_INVALIDARG;
    }
    *out = new_handle(Queue::new(work, completion)) as *mut c_void;
    S_OK
}

pub unsafe extern "system" fn XTaskQueueDuplicateHandle(h: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    match queue_from(h) {
        Some(q) => {
            *out = new_handle(q) as *mut c_void;
            S_OK
        }
        None => E_INVALIDARG,
    }
}

pub unsafe extern "system" fn XTaskQueueCloseHandle(h: *mut c_void) {
    if h.is_null() {
        return;
    }
    let qh = h as *mut QueueHandle;
    if (*qh).magic == MAGIC {
        (*qh).magic = 0;
        drop(Box::from_raw(qh));
    }
}

pub unsafe extern "system" fn XTaskQueueSubmitCallback(h: *mut c_void, port: u32, ctx: *mut c_void, cb: usize) -> HRESULT {
    if cb == 0 || port > 1 {
        return E_INVALIDARG;
    }
    let Some(q) = queue_from(h) else { return E_INVALIDARG };
    submit_now(&q, port, Item { cb, ctx: ctx as usize });
    S_OK
}

pub unsafe extern "system" fn XTaskQueueSubmitDelayedCallback(h: *mut c_void, port: u32, delay: u32, ctx: *mut c_void, cb: usize) -> HRESULT {
    if cb == 0 || port > 1 {
        return E_INVALIDARG;
    }
    let Some(q) = queue_from(h) else { return E_INVALIDARG };
    submit_delayed(&q, port, delay, Item { cb, ctx: ctx as usize });
    S_OK
}

pub unsafe extern "system" fn XTaskQueueDispatch(h: *mut c_void, port: u32, timeout: u32) -> bool {
    if port > 1 {
        return false;
    }
    match queue_from(h) {
        Some(q) => dispatch(&q, port, timeout),
        None => false,
    }
}

pub unsafe extern "system" fn XTaskQueueIsEmpty(h: *mut c_void, port: u32) -> bool {
    match queue_from(h) {
        Some(q) if port <= 1 => q.ports[port as usize].items.lock().unwrap().is_empty() && q.inflight.load(Ordering::Acquire) == 0,
        _ => true,
    }
}

pub unsafe extern "system" fn XTaskQueueTerminate(h: *mut c_void, wait: bool, ctx: *mut c_void, cb: usize) -> HRESULT {
    let Some(q) = queue_from(h) else { return E_INVALIDARG };
    q.terminated.store(true, Ordering::Release);
    // cancel everything still queued on manual/serialized ports
    for port in 0..2 {
        let pending: Vec<Item> = q.ports[port].items.lock().unwrap().drain(..).collect();
        for it in pending {
            q.execute(it);
        }
    }
    let ctx = ctx as usize; // raw pointers are not Send
    let finish = move || {
        let (m, cv) = &q.idle;
        let mut g = m.lock().unwrap();
        while q.inflight.load(Ordering::Acquire) != 0 {
            g = cv.wait_timeout(g, Duration::from_millis(50)).unwrap().0;
        }
        drop(g);
        if cb != 0 {
            let f: XTaskQueueTerminatedCallback = std::mem::transmute(cb);
            f(ctx as *mut c_void);
        }
    };
    if wait {
        finish();
    } else {
        spawn(Box::new(finish));
    }
    S_OK
}

pub unsafe extern "system" fn XTaskQueueGetCurrentProcessTaskQueue(out: *mut *mut c_void) -> bool {
    if out.is_null() {
        return false;
    }
    *out = new_handle(process_queue()) as *mut c_void;
    true
}

pub unsafe extern "system" fn XTaskQueueSetCurrentProcessTaskQueue(h: *mut c_void) {
    let q = if h.is_null() { None } else { queue_from(h) };
    *PROCESS_QUEUE.lock().unwrap() = q;
}

#[repr(C)]
pub struct RegistrationToken {
    pub token: u64,
}

struct Waiter {
    queue: Arc<Queue>,
    port: u32,
    item: Item,
    wait: usize,
}

unsafe extern "system" fn waiter_fired(ctx: *mut c_void, _timed_out: bool) {
    let w = &*(ctx as *const Waiter);
    submit_now(&w.queue, w.port, w.item);
}

pub unsafe extern "system" fn XTaskQueueRegisterWaiter(
    h: *mut c_void,
    port: u32,
    handle: *mut c_void,
    ctx: *mut c_void,
    cb: usize,
    token: *mut RegistrationToken,
) -> HRESULT {
    use windows_sys::Win32::System::Threading::{RegisterWaitForSingleObject, INFINITE, WT_EXECUTEDEFAULT};
    let Some(q) = queue_from(h) else { return E_INVALIDARG };
    if token.is_null() || cb == 0 {
        return E_INVALIDARG;
    }
    let w = Box::into_raw(Box::new(Waiter { queue: q, port, item: Item { cb, ctx: ctx as usize }, wait: 0 }));
    let mut wh = std::ptr::null_mut();
    if RegisterWaitForSingleObject(&mut wh, handle, Some(waiter_fired), w as *const c_void, INFINITE, WT_EXECUTEDEFAULT) == 0 {
        drop(Box::from_raw(w));
        return crate::util::E_FAIL;
    }
    (*w).wait = wh as usize;
    (*token).token = w as u64;
    S_OK
}

pub unsafe extern "system" fn XTaskQueueUnregisterWaiter(_h: *mut c_void, token: RegistrationToken) {
    use windows_sys::Win32::System::Threading::UnregisterWaitEx;
    let w = token.token as *mut Waiter;
    if w.is_null() {
        return;
    }
    UnregisterWaitEx((*w).wait as *mut c_void, windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE);
    drop(Box::from_raw(w));
}

pub unsafe extern "system" fn XTaskQueueRegisterMonitor(_h: *mut c_void, _ctx: *mut c_void, _cb: usize, token: *mut RegistrationToken) -> HRESULT {
    // Monitors are notified when work is queued; games use them to wake manual pumps.
    // Not needed for thread-pool queues – accept and never fire.
    if !token.is_null() {
        (*token).token = 1;
    }
    S_OK
}

pub unsafe extern "system" fn XTaskQueueUnregisterMonitor(_h: *mut c_void, _token: RegistrationToken) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    static HITS: AtomicU32 = AtomicU32::new(0);
    static CANCELED: AtomicU32 = AtomicU32::new(0);

    unsafe extern "system" fn bump(ctx: *mut c_void, canceled: bool) {
        HITS.fetch_add(ctx as usize as u32, Ordering::SeqCst);
        if canceled {
            CANCELED.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn manual_threadpool_immediate_delayed_and_terminate() {
        unsafe {
            let mut h = std::ptr::null_mut();
            assert_eq!(XTaskQueueCreate(MODE_MANUAL, MODE_IMMEDIATE, &mut h), S_OK);
            let before = HITS.load(Ordering::SeqCst);
            assert_eq!(XTaskQueueSubmitCallback(h, PORT_WORK, 1 as _, bump as usize), S_OK);
            assert_eq!(HITS.load(Ordering::SeqCst), before, "manual port must wait for dispatch");
            assert!(XTaskQueueDispatch(h, PORT_WORK, 0));
            assert_eq!(HITS.load(Ordering::SeqCst), before + 1);
            assert!(!XTaskQueueDispatch(h, PORT_WORK, 0));
            // immediate completion port runs inline
            XTaskQueueSubmitCallback(h, PORT_COMPLETION, 10 as _, bump as usize);
            assert_eq!(HITS.load(Ordering::SeqCst), before + 11);
            // delayed on manual port: dispatch with timeout waits for it
            XTaskQueueSubmitDelayedCallback(h, PORT_WORK, 30, 100 as _, bump as usize);
            assert!(XTaskQueueDispatch(h, PORT_WORK, 2000));
            assert_eq!(HITS.load(Ordering::SeqCst), before + 111);
            // terminate cancels pending manual work
            XTaskQueueSubmitCallback(h, PORT_WORK, 1000 as _, bump as usize);
            let c0 = CANCELED.load(Ordering::SeqCst);
            assert_eq!(XTaskQueueTerminate(h, true, std::ptr::null_mut(), 0), S_OK);
            assert_eq!(CANCELED.load(Ordering::SeqCst), c0 + 1);
            XTaskQueueCloseHandle(h);

            // thread pool + serialized
            for mode in [MODE_THREAD_POOL, MODE_SERIALIZED_THREAD_POOL] {
                let mut t = std::ptr::null_mut();
                XTaskQueueCreate(mode, mode, &mut t);
                let b = HITS.load(Ordering::SeqCst);
                for _ in 0..50 {
                    XTaskQueueSubmitCallback(t, PORT_WORK, 1 as _, bump as usize);
                }
                XTaskQueueTerminate(t, true, std::ptr::null_mut(), 0);
                assert_eq!(HITS.load(Ordering::SeqCst), b + 50);
                XTaskQueueCloseHandle(t);
            }
        }
    }
}
