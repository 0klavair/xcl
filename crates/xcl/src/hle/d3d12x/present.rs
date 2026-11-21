//! Presentation for the real backend.
//!
//! Console titles do not use DXGI swap chains: they render into their own back buffers
//! and call `PresentX` – command queue slot 30, `PresentX(planeCount, planes, parameters)`
//! with plane 0 `{ u64 frame token; u32 resource count @8; ID3D12Resource** @0x10 }` (as
//! observed on Fortnite). XCL presents that resource in its own window at once and tells the
//! frame clock (`frame`) the token flipped.
//!
//! Until a title's first `PresentX`, a fallback guesses the frame: XCL remembers which
//! resource every RTV descriptor points to and the first render target of each command list,
//! and after ExecuteCommandLists presents the latest back-buffer-like render target – on a
//! call of another console-only queue method, or rate-limited after submissions.
//!
//! The back buffer is copied on the title's own queue (implicit promotion from
//! COMMON/PRESENT state, which consoles require before PresentX).

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, AtomicU64, Ordering};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::stubs::StubInfo;

static RTVS: Mutex<Option<HashMap<usize, usize>>> = Mutex::new(None);
static LISTS: Mutex<Option<HashMap<usize, usize>>> = Mutex::new(None);
static CANDIDATE: Mutex<Option<(usize, usize)>> = Mutex::new(None); // (queue, resource) – resource AddRef'd
static PRESENTS: AtomicU64 = AtomicU64::new(0);
static BOUNDARIES: AtomicU64 = AtomicU64::new(0);
static LAST_BOUNDARY_MS: AtomicU64 = AtomicU64::new(0);
static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
/// the title called PresentX: the fallback guessing is off
static PRESENTX: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// command queue #30 PresentX(planeCount, planes, parameters): plane 0's first resource is
/// the frame; S_OK always (a console present does not fail for the title).
pub unsafe extern "system" fn present_x(queue: *mut c_void, plane_count: u32, planes: *const u8, _params: *const u8) -> i32 {
    if plane_count == 0 || planes.is_null() || !crate::gpu::shader::readable(planes as usize, 0x18) {
        return 0;
    }
    super::frame::mark_flipped((planes as *const u64).read_unaligned());
    BOUNDARIES.fetch_add(1, Ordering::Relaxed);
    if !PRESENTX.swap(true, Ordering::Relaxed) {
        crate::xlog_info!("D3D12.X: PresentX – the title presents its own frames ({plane_count} plane(s))");
    }
    let count = (planes.add(8) as *const u32).read_unaligned();
    let list = (planes.add(0x10) as *const usize).read_unaligned();
    if count == 0 || !crate::gpu::shader::readable(list, 8) || !enabled() {
        return 0;
    }
    let res = (list as *const usize).read_unaligned();
    if res == 0 {
        return 0;
    }
    {
        let mut c = CANDIDATE.lock().unwrap();
        if c.map(|(_, r)| r) != Some(res) {
            add_ref(res);
            if let Some((_, old)) = c.replace((queue as usize, res)) {
                release(old);
            }
        } else if let Some(cur) = c.as_mut() {
            cur.0 = queue as usize;
        }
    }
    present_now(Duration::ZERO);
    0
}

thread_local! {
    static IN_PRESENT: Cell<bool> = const { Cell::new(false) };
}

fn now_ms() -> u64 {
    static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_millis() as u64
}

pub fn counters() -> (u64, u64) {
    (PRESENTS.load(Ordering::Relaxed), BOUNDARIES.load(Ordering::Relaxed))
}

fn enabled() -> bool {
    !crate::state::config().present.eq_ignore_ascii_case("off")
}

unsafe fn add_ref(p: usize) {
    let f: unsafe extern "system" fn(usize) -> u32 = std::mem::transmute(*(*(p as *const *const usize)).add(1));
    f(p);
}

unsafe fn release(p: usize) {
    let f: unsafe extern "system" fn(usize) -> u32 = std::mem::transmute(*(*(p as *const *const usize)).add(2));
    f(p);
}

pub unsafe fn note_rtv(handle: usize, res: *mut c_void) {
    if !enabled() {
        return;
    }
    let mut g = RTVS.lock().unwrap();
    let m = g.get_or_insert_with(HashMap::new);
    if !res.is_null() {
        add_ref(res as usize);
    }
    let old = if res.is_null() { m.remove(&handle) } else { m.insert(handle, res as usize) };
    if let Some(o) = old {
        release(o);
    }
}

pub fn note_rt0(list: usize, handle: usize) {
    if !enabled() {
        return;
    }
    LISTS.lock().unwrap().get_or_insert_with(HashMap::new).insert(list, handle);
}

/// Format the swap chain uses for a back buffer of `f` (None = not presentable).
pub fn swap_format(f: DXGI_FORMAT) -> Option<DXGI_FORMAT> {
    Some(match f {
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_R8G8B8A8_UNORM_SRGB | DXGI_FORMAT_R8G8B8A8_TYPELESS => DXGI_FORMAT_R8G8B8A8_UNORM,
        DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB | DXGI_FORMAT_B8G8R8A8_TYPELESS => DXGI_FORMAT_B8G8R8A8_UNORM,
        DXGI_FORMAT_R10G10B10A2_UNORM | DXGI_FORMAT_R10G10B10A2_TYPELESS => DXGI_FORMAT_R10G10B10A2_UNORM,
        DXGI_FORMAT_R16G16B16A16_FLOAT | DXGI_FORMAT_R16G16B16A16_TYPELESS => DXGI_FORMAT_R16G16B16A16_FLOAT,
        _ => return None,
    })
}

unsafe fn backbuffer_like(res: usize) -> Option<D3D12_RESOURCE_DESC> {
    let raw = res as *mut c_void;
    let r = ID3D12Resource::from_raw_borrowed(&raw)?;
    let d = r.GetDesc();
    let ok = d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D
        && d.Width >= 640
        && d.Height >= 360
        && d.DepthOrArraySize == 1
        && d.SampleDesc.Count == 1
        && swap_format(d.Format).is_some();
    ok.then_some(d)
}

pub unsafe fn note_execute(queue: *mut c_void, lists: &[*mut c_void]) {
    if !enabled() || IN_PRESENT.with(|c| c.get()) || PRESENTX.load(Ordering::Relaxed) {
        return;
    }
    let handle = {
        let g = LISTS.lock().unwrap();
        let Some(m) = g.as_ref() else { return };
        lists.iter().rev().find_map(|l| m.get(&(*l as usize)).copied())
    };
    let Some(h) = handle else { return };
    let res = RTVS.lock().unwrap().as_ref().and_then(|m| m.get(&h).copied());
    let Some(res) = res else { return };
    if backbuffer_like(res).is_none() {
        return;
    }
    {
        let mut c = CANDIDATE.lock().unwrap();
        if c.map(|(_, r)| r) != Some(res) {
            add_ref(res);
            if let Some((_, old)) = c.replace((queue as usize, res)) {
                release(old);
            }
        }
    }
    // no console present call seen recently → present on submissions (rate limited)
    if now_ms().saturating_sub(LAST_BOUNDARY_MS.load(Ordering::Relaxed)) > 500 {
        present_now(Duration::from_millis(15));
    }
}

/// StubHook for console-only queue methods (PresentX, frame events, …).
pub fn on_queue_console_method(_info: &StubInfo, _caller: usize) {
    if !enabled() || PRESENTX.load(Ordering::Relaxed) {
        return;
    }
    BOUNDARIES.fetch_add(1, Ordering::Relaxed);
    LAST_BOUNDARY_MS.store(now_ms(), Ordering::Relaxed);
    unsafe { present_now(Duration::from_millis(4)) };
}

// ---------------------------------------------------------------------------------------
// window
// ---------------------------------------------------------------------------------------

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CLOSE => {
            crate::xlog_info!("XCL window closed – ending the game process");
            crate::report::write_final(None);
            crate::log::flush();
            windows::Win32::System::Threading::ExitProcess(0);
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn window_thread() {
    unsafe {
        let Ok(inst) = GetModuleHandleW(PCWSTR::null()) else { return };
        let class = windows::core::w!("XCLGameWindow");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: inst.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let cfg = crate::state::config();
        let title = crate::state::boot().map(|b| b.title.clone()).unwrap_or_else(|| "XCL".into());
        let mut rc = RECT { left: 0, top: 0, right: cfg.window_width as i32, bottom: cfg.window_height as i32 };
        let _ = AdjustWindowRect(&mut rc, WS_OVERLAPPEDWINDOW, false);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            &HSTRING::from(format!("XCL – {title}")),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rc.right - rc.left,
            rc.bottom - rc.top,
            None,
            None,
            Some(inst.into()),
            None,
        );
        let Ok(hwnd) = hwnd else {
            crate::xlog_error!("could not create the XCL window");
            return;
        };
        HWND_RAW.store(hwnd.0 as isize, Ordering::Release);
        crate::xlog_info!("XCL window created ({}x{})", cfg.window_width, cfg.window_height);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn ensure_window() -> Option<HWND> {
    static START: Once = Once::new();
    START.call_once(|| {
        let _ = std::thread::Builder::new().name("xcl-window".into()).spawn(window_thread);
    });
    let t0 = Instant::now();
    while HWND_RAW.load(Ordering::Acquire) == 0 {
        if t0.elapsed() > Duration::from_secs(3) {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Some(HWND(HWND_RAW.load(Ordering::Acquire) as *mut c_void))
}

// ---------------------------------------------------------------------------------------
// presenter
// ---------------------------------------------------------------------------------------

struct Presenter {
    queue: usize,
    sc: Option<IDXGISwapChain3>,
    w: u32,
    h: u32,
    fmt: DXGI_FORMAT,
    alloc: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    value: u64,
    event: HANDLE,
    last: Instant,
}

unsafe impl Send for Presenter {}

static PRESENTER: Mutex<Option<Presenter>> = Mutex::new(None);

unsafe fn transition(res: &ID3D12Resource, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: std::mem::transmute_copy(res),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

unsafe fn new_presenter(queue: usize) -> Option<Presenter> {
    let dev_raw = super::real::device_raw();
    let dev = ID3D12Device::from_raw_borrowed(&dev_raw)?;
    let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).ok()?;
    let list: ID3D12GraphicsCommandList = dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).ok()?;
    list.Close().ok()?;
    let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
    let event = CreateEventW(None, false, false, None).ok()?;
    Some(Presenter { queue, sc: None, w: 0, h: 0, fmt: DXGI_FORMAT_UNKNOWN, alloc, list, fence, value: 0, event, last: Instant::now() - Duration::from_secs(1) })
}

unsafe fn ensure_swapchain(p: &mut Presenter, hwnd: HWND, w: u32, h: u32, fmt: DXGI_FORMAT) -> bool {
    if let Some(sc) = &p.sc {
        if p.w == w && p.h == h && p.fmt == fmt {
            return true;
        }
        if sc.ResizeBuffers(2, w, h, fmt, DXGI_SWAP_CHAIN_FLAG(0)).is_ok() {
            p.w = w;
            p.h = h;
            p.fmt = fmt;
            crate::xlog_info!("present: swap chain {w}x{h} {:?}", fmt);
            return true;
        }
        return false;
    }
    let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory2>(DXGI_CREATE_FACTORY_FLAGS(0)) else { return false };
    let qraw = p.queue as *mut c_void;
    let Some(queue) = ID3D12CommandQueue::from_raw_borrowed(&qraw) else { return false };
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: w,
        Height: h,
        Format: fmt,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        ..Default::default()
    };
    match factory.CreateSwapChainForHwnd(queue, hwnd, &desc, None, None) {
        Ok(sc1) => {
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
            p.sc = sc1.cast::<IDXGISwapChain3>().ok();
            p.w = w;
            p.h = h;
            p.fmt = fmt;
            crate::xlog_info!("present: swap chain {w}x{h} {:?} – the game image will appear in the XCL window", fmt);
            p.sc.is_some()
        }
        Err(e) => {
            crate::xlog_error!("present: CreateSwapChainForHwnd: {e}");
            false
        }
    }
}

unsafe fn present_now(min_interval: Duration) {
    if IN_PRESENT.with(|c| c.replace(true)) {
        return;
    }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            IN_PRESENT.with(|c| c.set(false));
        }
    }
    let _reset = Reset;
    let Some((queue, res)) = *CANDIDATE.lock().unwrap() else { return };
    let Ok(mut guard) = PRESENTER.try_lock() else { return };
    if guard.is_none() {
        *guard = new_presenter(queue);
    }
    let Some(p) = guard.as_mut() else { return };
    if p.last.elapsed() < min_interval {
        return;
    }
    let Some(desc) = backbuffer_like(res) else { return };
    let Some(fmt) = swap_format(desc.Format) else { return };
    let Some(hwnd) = ensure_window() else { return };
    if p.queue != queue {
        return; // swap chain is bound to the first queue we presented from
    }
    // wait for the previous copy so the allocator can be reused
    if p.fence.GetCompletedValue() < p.value {
        let _ = p.fence.SetEventOnCompletion(p.value, p.event);
        WaitForSingleObject(p.event, 100);
    }
    if !ensure_swapchain(p, hwnd, desc.Width as u32, desc.Height, fmt) {
        return;
    }
    let Some(sc) = p.sc.clone() else { return };
    let rraw = res as *mut c_void;
    let Some(src) = ID3D12Resource::from_raw_borrowed(&rraw) else { return };
    let Ok(bb) = sc.GetBuffer::<ID3D12Resource>(sc.GetCurrentBackBufferIndex()) else { return };
    if p.alloc.Reset().is_err() || p.list.Reset(&p.alloc, None).is_err() {
        return;
    }
    let b1 = [transition(&bb, D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATE_COPY_DEST)];
    p.list.ResourceBarrier(&b1);
    p.list.CopyResource(&bb, src);
    let b2 = [transition(&bb, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_PRESENT)];
    p.list.ResourceBarrier(&b2);
    if p.list.Close().is_err() {
        return;
    }
    let qraw = queue as *mut c_void;
    let Some(q) = ID3D12CommandQueue::from_raw_borrowed(&qraw) else { return };
    let Ok(cl) = p.list.cast::<ID3D12CommandList>() else { return };
    q.ExecuteCommandLists(&[Some(cl)]);
    p.value += 1;
    let _ = q.Signal(&p.fence, p.value);
    let _ = sc.Present(0, DXGI_PRESENT(0));
    p.last = Instant::now();
    let n = PRESENTS.fetch_add(1, Ordering::Relaxed);
    if n == 0 {
        crate::xlog_info!("present: first game frame shown ({}x{})", desc.Width, desc.Height);
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}
