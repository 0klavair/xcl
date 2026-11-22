//! The console's audio processor (`acphal.dll`, `xapu.dll`): memory the audio hardware
//! addresses, the processor's command / message channel, SHAPE contexts and the decoder queue.
//!
//! Nothing is decoded or mixed here – the processor answers as hardware with no work to do:
//! commands are acknowledged, audio frames tick (one every 128 samples at 48 kHz), SHAPE
//! context arrays exist, and the decoder returns silence of the length asked for. A title's
//! audio engine then starts and runs (XDefiant: `ApuCreateHeap` failing made it drop its whole
//! audio setup), sounds decoded in software go wherever the engine sends them, and nothing
//! waits forever on the processor. Audio through the console's mixer (SHAPE) stays silent.
//!
//! Interfaces as titles call them (observed on retail titles; another project's notes on
//! the same titles agree): `ApuAlloc(&va, &apu, size, alignment, flags)`,
//! `ApuMapVirtualAddress(&apu, va, size)`, `ApuMapApuAddress(apu) -> va`,
//! `AcpHalCreate(&hal)` – an object whose vtable is Connect(commands, messages), Disconnect,
//! SubmitCommand(type, id, frame, data, size), PopMessage(&message) -> bool,
//! GetNumMessages, Release; messages {u32 type, u32, u64 value, u64 id}: 2 audio frame,
//! 0x10 command done (id), 0x80 disconnected. `chad.dll` (XDsp) is left to the stub database:
//! E_NOTIMPL, the processor's DSP is not there.

#![allow(non_snake_case)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::util::{HRESULT, S_OK};

const E_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;
const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;
const E_UNEXPECTED: HRESULT = 0x8000_FFFF_u32 as i32;
/// XApuDequeueResult: no result waiting
const XAPU_NO_RESULTS: HRESULT = 0x8AC8_1001_u32 as i32;

/// one audio frame: 128 samples at 48 kHz
const FRAME: Duration = Duration::from_micros(2667);

// ---------------------------------------------------------------------------------------
// memory the audio processor addresses (32-bit APU addresses)
// ---------------------------------------------------------------------------------------

struct Region {
    va: usize,
    size: u32,
    apu: u32,
    /// allocated here (ApuAlloc), released by ApuFree
    owned: bool,
}

struct Apu {
    regions: Vec<Region>,
    next: u32,
}

static APU: Mutex<Apu> = Mutex::new(Apu { regions: Vec::new(), next: 0x1000_0000 });

impl Apu {
    /// The APU address of `va` – in a known region, or a new region of `size` bytes there.
    fn address(&mut self, va: usize, size: u32, owned: bool) -> u32 {
        if let Some(r) = self.regions.iter().find(|r| va >= r.va && va - r.va < r.size.max(1) as usize) {
            return r.apu + (va - r.va) as u32;
        }
        let apu = self.next;
        self.next = self.next.wrapping_add((size.max(1) + 0xFFF) & !0xFFF);
        self.regions.push(Region { va, size, apu, owned });
        apu
    }
}

/// `size` bytes aligned to `align` (zeroed; the allocation's base is `va` rounded down to
/// 64 KiB – VirtualAlloc's granularity – so MEM_RELEASE of it frees all)
unsafe fn alloc_aligned(size: usize, align: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE};
    let align = align.max(1).next_power_of_two();
    if align <= 0x1_0000 {
        let p = VirtualAlloc(std::ptr::null(), size.max(1), MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) as usize;
        return (p != 0).then_some(p);
    }
    // reserve enough to find an aligned start, then reserve exactly there
    for _ in 0..8 {
        let p = VirtualAlloc(std::ptr::null(), size + align, MEM_RESERVE, PAGE_READWRITE) as usize;
        if p == 0 {
            return None;
        }
        VirtualFree(p as *mut c_void, 0, MEM_RELEASE);
        let a = (p + align - 1) & !(align - 1);
        let q = VirtualAlloc(a as *const c_void, size.max(1), MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) as usize;
        if q == a {
            return Some(q);
        }
        if q != 0 {
            VirtualFree(q as *mut c_void, 0, MEM_RELEASE);
        }
    }
    None
}

/// ApuCreateHeap(size, flags): the heap ApuAlloc takes from – the process's memory here.
pub unsafe extern "system" fn ApuCreateHeap(size: u32, flags: u32) -> HRESULT {
    crate::xlog_info!("acphal: ApuCreateHeap({size:#x}, {flags:#x}) – the audio processor's memory is the process's");
    S_OK
}

/// ApuAlloc(&va, &apu, size, alignment, flags)
pub unsafe extern "system" fn ApuAlloc(va: *mut *mut c_void, apu: *mut u32, size: u32, alignment: u32, _flags: u32) -> HRESULT {
    if va.is_null() {
        return E_INVALIDARG;
    }
    let Some(p) = alloc_aligned(size as usize, alignment as usize) else {
        *va = std::ptr::null_mut();
        return E_OUTOFMEMORY;
    };
    *va = p as *mut c_void;
    let a = APU.lock().unwrap().address(p, size, true);
    if !apu.is_null() {
        *apu = a;
    }
    S_OK
}

/// ApuFree(va)
pub unsafe extern "system" fn ApuFree(va: *mut c_void) -> HRESULT {
    use windows_sys::Win32::System::Memory::{VirtualFree, MEM_RELEASE};
    if va.is_null() {
        return E_INVALIDARG;
    }
    let owned = {
        let mut g = APU.lock().unwrap();
        let i = g.regions.iter().position(|r| r.va == va as usize);
        i.map(|i| g.regions.swap_remove(i).owned).unwrap_or(false)
    };
    if owned {
        VirtualFree(va, 0, MEM_RELEASE);
    }
    S_OK
}

/// ApuMapVirtualAddress(&apu, va, size): an APU address for the title's own memory
pub unsafe extern "system" fn ApuMapVirtualAddress(apu: *mut u32, va: *mut c_void, size: u32) -> HRESULT {
    if apu.is_null() {
        return E_INVALIDARG;
    }
    *apu = APU.lock().unwrap().address(va as usize, size, false);
    S_OK
}

/// ApuMapApuAddress(apu) -> the memory an APU address stands for (null: none)
pub unsafe extern "system" fn ApuMapApuAddress(apu: u32) -> *mut c_void {
    let g = APU.lock().unwrap();
    g.regions
        .iter()
        .find(|r| apu >= r.apu && apu - r.apu < r.size.max(1))
        .map_or(std::ptr::null_mut(), |r| (r.va + (apu - r.apu) as usize) as *mut c_void)
}

/// ApuIsVirtualAddressValid: all of the process's memory is the processor's here
pub unsafe extern "system" fn ApuIsVirtualAddressValid(_va: *const c_void, _size: u32) -> i32 {
    1
}

/// ApuHeapGetState(&state, flags): free bytes, bytes in use, bytes lost, the largest free
/// block, blocks – there is always room
pub unsafe extern "system" fn ApuHeapGetState(state: *mut u32, _flags: u32) -> HRESULT {
    if state.is_null() {
        return E_INVALIDARG;
    }
    let g = APU.lock().unwrap();
    let used: u32 = g.regions.iter().filter(|r| r.owned).map(|r| r.size).fold(0, u32::wrapping_add);
    let s = std::slice::from_raw_parts_mut(state, 5);
    s.copy_from_slice(&[256 << 20, used, 0, 64 << 20, g.regions.len() as u32]);
    S_OK
}

// ---------------------------------------------------------------------------------------
// SHAPE contexts
// ---------------------------------------------------------------------------------------

/// AcpHalAllocateShapeContexts' argument: counts of the five kinds, then an array of each
#[repr(C)]
pub struct ShapeContexts {
    counts: [u32; 5],
    reserved: u32,
    contexts: [*mut c_void; 5],
}

/// bytes of a context entry (the largest seen)
const CONTEXT_BYTES: u32 = 64;
static SHAPE: Mutex<[usize; 5]> = Mutex::new([0; 5]);

/// AcpHalAllocateShapeContexts(&contexts): an array of each kind in APU memory (none run)
pub unsafe extern "system" fn AcpHalAllocateShapeContexts(c: *mut ShapeContexts) -> HRESULT {
    if c.is_null() {
        return E_INVALIDARG;
    }
    let c = &mut *c;
    let mut arrays = SHAPE.lock().unwrap();
    for i in 0..5 {
        c.contexts[i] = std::ptr::null_mut();
        if c.counts[i] == 0 {
            continue;
        }
        let mut va = std::ptr::null_mut();
        let hr = ApuAlloc(&mut va, std::ptr::null_mut(), c.counts[i].saturating_mul(CONTEXT_BYTES), 256, 0);
        if hr < 0 {
            return hr;
        }
        arrays[i] = va as usize;
        c.contexts[i] = va;
    }
    crate::xlog_info!("acphal: SHAPE contexts {:?} (they do not run: no audio from the console's mixer)", c.counts);
    S_OK
}

/// AcpHalReleaseShapeContexts()
pub unsafe extern "system" fn AcpHalReleaseShapeContexts() -> HRESULT {
    let arrays = std::mem::take(&mut *SHAPE.lock().unwrap());
    for a in arrays.into_iter().filter(|&a| a != 0) {
        ApuFree(a as *mut c_void);
    }
    S_OK
}

// ---------------------------------------------------------------------------------------
// the processor's command / message channel (AcpHalCreate)
// ---------------------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Message {
    pub kind: u32,
    pub reserved: u32,
    pub value: u64,
    pub id: u64,
}

pub const MSG_AUDIO_FRAME: u32 = 2;
pub const MSG_COMMAND_DONE: u32 = 0x10;
pub const MSG_DISCONNECTED: u32 = 0x80;

/// What the channel holds: queued messages and when the next audio frame is due.
#[derive(Default)]
pub struct Channel {
    messages: VecDeque<Message>,
    connected: bool,
    next_frame: Option<Instant>,
}

impl Channel {
    pub fn connect(&mut self, now: Instant) {
        self.connected = true;
        self.next_frame = Some(now + FRAME);
    }

    pub fn disconnect(&mut self) {
        self.connected = false;
        self.messages.push_back(Message { kind: MSG_DISCONNECTED, ..Default::default() });
    }

    /// A command: done at once (a message for it unless its id is 0 – nobody waits).
    pub fn submit(&mut self, id: u64) -> bool {
        if !self.connected {
            return false;
        }
        if id != 0 {
            self.messages.push_back(Message { kind: MSG_COMMAND_DONE, id, ..Default::default() });
        }
        true
    }

    fn frame_due(&self, now: Instant) -> bool {
        self.connected && self.next_frame.is_some_and(|t| now >= t)
    }

    /// The next message: queued ones first, else an audio frame when one is due (a frame
    /// late by more than 8 is not made up for).
    pub fn pop(&mut self, now: Instant) -> Option<Message> {
        if let Some(m) = self.messages.pop_front() {
            return Some(m);
        }
        if self.frame_due(now) {
            let t = self.next_frame.unwrap_or(now) + FRAME;
            self.next_frame = Some(t.max(now - FRAME * 8));
            return Some(Message { kind: MSG_AUDIO_FRAME, ..Default::default() });
        }
        None
    }

    pub fn count(&self, now: Instant) -> u32 {
        self.messages.len() as u32 + self.frame_due(now) as u32
    }
}

#[repr(C)]
struct Hal {
    vtable: *const usize,
    channel: Mutex<Channel>,
}

unsafe extern "system" fn hal_connect(h: *mut Hal, commands: u32, messages: u32) -> HRESULT {
    (*h).channel.lock().unwrap().connect(Instant::now());
    crate::xlog_info!("acphal: audio processor connected ({commands} commands, {messages} messages) – commands are acknowledged, nothing runs");
    S_OK
}

unsafe extern "system" fn hal_disconnect(h: *mut Hal) -> HRESULT {
    (*h).channel.lock().unwrap().disconnect();
    S_OK
}

unsafe extern "system" fn hal_submit(h: *mut Hal, kind: u32, id: u64, _frame: u32, _data: *const c_void, _size: u32) -> HRESULT {
    static SEEN: Mutex<Vec<u32>> = Mutex::new(Vec::new());
    {
        let mut s = SEEN.lock().unwrap();
        if !s.contains(&kind) && s.len() < 64 {
            s.push(kind);
            crate::xlog_info!("acphal: audio processor command {kind:#x} first seen");
        }
    }
    if (*h).channel.lock().unwrap().submit(id) {
        S_OK
    } else {
        E_UNEXPECTED
    }
}

unsafe extern "system" fn hal_pop(h: *mut Hal, m: *mut Message) -> bool {
    if m.is_null() {
        return false;
    }
    match (*h).channel.lock().unwrap().pop(Instant::now()) {
        Some(x) => {
            *m = x;
            true
        }
        None => false,
    }
}

unsafe extern "system" fn hal_count(h: *mut Hal) -> u32 {
    (*h).channel.lock().unwrap().count(Instant::now())
}

unsafe extern "system" fn hal_release(h: *mut Hal) {
    drop(Box::from_raw(h));
}

/// the channel object's vtable (made once; function addresses are not constants)
fn hal_vtable() -> *const usize {
    static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        let v: Box<[usize; 6]> = Box::new([hal_connect as usize, hal_disconnect as usize, hal_submit as usize, hal_pop as usize, hal_count as usize, hal_release as usize]);
        Box::leak(v).as_ptr() as usize
    }) as *const usize
}

/// AcpHalCreate(&hal)
pub unsafe extern "system" fn AcpHalCreate(out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let h = Box::new(Hal { vtable: hal_vtable(), channel: Mutex::new(Channel::default()) });
    *out = Box::into_raw(h) as *mut c_void;
    S_OK
}

// ---------------------------------------------------------------------------------------
// xapu.dll: the decoder's command queue
// ---------------------------------------------------------------------------------------

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct XApuCommand {
    pub kind: u32,
    pub stream: u32,
    pub sequence: u32,
    pub options: u32,
    pub context: *mut c_void,
}

#[repr(C, packed)]
struct XApuActivate {
    id: XApuCommand,
    channels: u32,
}

#[repr(C, packed)]
struct XApuProcess {
    id: XApuCommand,
    ramp: f32,
    pitch: f32,
    first_frame: u32,
    frames: u32,
    input_bytes: u32,
    input: *mut c_void,
    output_bytes: u32,
    output: *mut c_void,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct XApuResult {
    pub id: XApuCommand,
    pub state: u32,
    pub state_data: u64,
    pub time: u64,
    pub error: HRESULT,
    pub frames: u32,
    pub output_bytes: u32,
    pub output: *mut c_void,
    pub pitch: f32,
    pub input_bytes: u32,
    pub input: *mut c_void,
}

#[repr(C, packed)]
pub struct XApuConnectInput {
    process: u32,
    streams: u32,
    commands_per_stream: u32,
    data_bytes: u32,
    frames_per_output: u32,
    options: u32,
}

#[repr(C, packed)]
pub struct XApuConnectOutput {
    data: *mut c_void,
    ready: *mut c_void,
}

const XAPU_ACTIVATE: u32 = 1;
const XAPU_PROCESS: u32 = 3;

struct XApu {
    results: Mutex<VecDeque<XApuResult>>,
    channels: Mutex<Vec<u32>>,
    data: usize,
    ready: usize,
    frames_per_output: u32,
}

/// XApuConnect(&input, &output, &handle)
pub unsafe extern "system" fn XApuConnect(input: *const XApuConnectInput, output: *mut XApuConnectOutput, handle: *mut *mut c_void) -> HRESULT {
    use windows_sys::Win32::System::Threading::CreateEventW;
    if input.is_null() || output.is_null() || handle.is_null() {
        return E_INVALIDARG;
    }
    let i = std::ptr::read_unaligned(input);
    let data = if i.data_bytes != 0 { alloc_aligned(i.data_bytes as usize, 0x1_0000).unwrap_or(0) } else { 0 };
    let ready = if i.options & 8 != 0 { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) as usize } else { 0 };
    let x = Box::new(XApu { results: Mutex::new(VecDeque::new()), channels: Mutex::new(vec![2; i.streams.max(1) as usize]), data, ready, frames_per_output: i.frames_per_output });
    std::ptr::write_unaligned(output, XApuConnectOutput { data: data as *mut c_void, ready: ready as *mut c_void });
    *handle = Box::into_raw(x) as *mut c_void;
    let (process, streams) = (i.process, i.streams);
    crate::xlog_info!("xapu: decoder connected (process {process}, {streams} streams) – it answers with silence");
    S_OK
}

/// XApuDisconnect(handle)
pub unsafe extern "system" fn XApuDisconnect(handle: *mut c_void) -> HRESULT {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Memory::{VirtualFree, MEM_RELEASE};
    if handle.is_null() {
        return E_INVALIDARG;
    }
    let x = Box::from_raw(handle as *mut XApu);
    if x.data != 0 {
        VirtualFree(x.data as *mut c_void, 0, MEM_RELEASE);
    }
    if x.ready != 0 {
        CloseHandle(x.ready as _);
    }
    S_OK
}

/// XApuEnqueueCommand(handle, &command, &results waiting): done at once – a Process command
/// with silence of the length asked for
pub unsafe extern "system" fn XApuEnqueueCommand(handle: *mut c_void, command: *const XApuCommand, count: *mut u32) -> HRESULT {
    use windows_sys::Win32::System::Threading::SetEvent;
    if handle.is_null() || command.is_null() {
        return E_INVALIDARG;
    }
    let x = &*(handle as *const XApu);
    let c = std::ptr::read_unaligned(command);
    let mut r = XApuResult { id: c, state: 0, state_data: 0, time: 0, error: S_OK, frames: 0, output_bytes: 0, output: std::ptr::null_mut(), pitch: 1.0, input_bytes: 0, input: std::ptr::null_mut() };
    {
        let mut ch = x.channels.lock().unwrap();
        let s = if (c.stream as usize) < ch.len() { c.stream as usize } else { 0 };
        if c.kind == XAPU_ACTIVATE {
            let n = std::ptr::read_unaligned(command as *const XApuActivate).channels;
            ch[s] = if n == 1 { 1 } else { 2 };
        } else if c.kind == XAPU_PROCESS {
            let p = std::ptr::read_unaligned(command as *const XApuProcess);
            let frame_bytes = ch[s] * 4;
            let asked = if p.frames != 0 { p.frames } else { x.frames_per_output };
            let frames = (if asked != 0 { asked } else { 960 }).min(p.output_bytes / frame_bytes);
            if !p.output.is_null() && frames != 0 {
                std::ptr::write_bytes(p.output as *mut u8, 0, (frames * frame_bytes) as usize);
            }
            // output ready, more input wanted
            r.state = 1 | 2;
            r.frames = frames;
            r.output_bytes = frames * frame_bytes;
            r.output = p.output;
            r.pitch = p.pitch;
            r.input_bytes = p.input_bytes;
            r.input = p.input;
        }
    }
    let n = {
        let mut q = x.results.lock().unwrap();
        q.push_back(r);
        q.len() as u32
    };
    if !count.is_null() {
        *count = n;
    }
    if x.ready != 0 && c.options & 1 != 0 {
        SetEvent(x.ready as _);
    }
    S_OK
}

/// XApuDequeueResult(handle, &result)
pub unsafe extern "system" fn XApuDequeueResult(handle: *mut c_void, result: *mut XApuResult) -> HRESULT {
    if handle.is_null() || result.is_null() {
        return E_INVALIDARG;
    }
    match (*(handle as *const XApu)).results.lock().unwrap().pop_front() {
        Some(r) => {
            std::ptr::write_unaligned(result, r);
            S_OK
        }
        None => XAPU_NO_RESULTS,
    }
}

/// XApuGetResultCount(handle, &count) -> count
pub unsafe extern "system" fn XApuGetResultCount(handle: *mut c_void, total: *mut u32) -> u32 {
    if handle.is_null() {
        return 0;
    }
    let n = (*(handle as *const XApu)).results.lock().unwrap().len() as u32;
    if !total.is_null() {
        *total = n;
    }
    n
}

/// XApuGetUtilization(…, &utilization): streams available and the most there can be
pub unsafe extern "system" fn XApuGetUtilization(_a: u32, _b: u32, u: *mut u32) -> HRESULT {
    if u.is_null() {
        return E_INVALIDARG;
    }
    *u = 256;
    *u.add(1) = 256;
    S_OK
}

pub fn exports() -> Vec<(&'static str, usize)> {
    crate::hle_exports![
        ApuCreateHeap,
        ApuAlloc,
        ApuFree,
        ApuMapVirtualAddress,
        ApuMapApuAddress,
        ApuIsVirtualAddressValid,
        ApuHeapGetState,
        AcpHalCreate,
        AcpHalAllocateShapeContexts,
        AcpHalReleaseShapeContexts,
        XApuConnect,
        XApuDisconnect,
        XApuEnqueueCommand,
        XApuDequeueResult,
        XApuGetResultCount,
        XApuGetUtilization,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apu_memory_maps_both_ways() {
        unsafe {
            let (mut va, mut apu) = (std::ptr::null_mut(), 0u32);
            assert_eq!(ApuAlloc(&mut va, &mut apu, 0x3000, 0x2_0000, 0), S_OK);
            assert_eq!(va as usize % 0x2_0000, 0, "alignment above 64 KiB");
            assert_eq!(ApuMapApuAddress(apu + 0x10) as usize, va as usize + 0x10);
            let mut again = 0u32;
            assert_eq!(ApuMapVirtualAddress(&mut again, (va as usize + 0x20) as *mut c_void, 4), S_OK);
            assert_eq!(again, apu + 0x20);
            assert_eq!(ApuFree(va), S_OK);
            assert!(ApuMapApuAddress(apu).is_null());
        }
    }

    #[test]
    fn channel_acknowledges_and_ticks() {
        let t0 = Instant::now();
        let mut c = Channel::default();
        assert!(!c.submit(7), "not connected yet");
        c.connect(t0);
        assert!(c.submit(7));
        assert!(c.submit(0), "id 0: no message");
        assert_eq!(c.count(t0), 1);
        assert_eq!(c.pop(t0), Some(Message { kind: MSG_COMMAND_DONE, id: 7, ..Default::default() }));
        assert_eq!(c.pop(t0), None, "no frame due yet");
        let later = t0 + FRAME * 3;
        assert_eq!(c.pop(later).map(|m| m.kind), Some(MSG_AUDIO_FRAME));
        c.disconnect();
        assert_eq!(c.pop(later).map(|m| m.kind), Some(MSG_DISCONNECTED));
    }

    #[test]
    fn decoder_returns_silence() {
        unsafe {
            let input = XApuConnectInput { process: 1, streams: 2, commands_per_stream: 4, data_bytes: 0, frames_per_output: 256, options: 0 };
            let mut out = XApuConnectOutput { data: std::ptr::null_mut(), ready: std::ptr::null_mut() };
            let mut h = std::ptr::null_mut();
            assert_eq!(XApuConnect(&input, &mut out, &mut h), S_OK);
            let mut buf = vec![0xAAu8; 4096];
            let p = XApuProcess {
                id: XApuCommand { kind: XAPU_PROCESS, stream: 1, sequence: 1, options: 0, context: std::ptr::null_mut() },
                ramp: 0.0,
                pitch: 1.0,
                first_frame: 0,
                frames: 0,
                input_bytes: 0,
                input: std::ptr::null_mut(),
                output_bytes: buf.len() as u32,
                output: buf.as_mut_ptr() as *mut c_void,
            };
            let mut n = 0;
            assert_eq!(XApuEnqueueCommand(h, &p as *const _ as *const XApuCommand, &mut n), S_OK);
            assert_eq!(n, 1);
            assert_eq!(XApuGetResultCount(h, std::ptr::null_mut()), 1);
            let mut r: XApuResult = std::mem::zeroed();
            assert_eq!(XApuDequeueResult(h, &mut r), S_OK);
            let frames = r.frames;
            assert_eq!(frames, 256, "frames_per_output, stereo floats");
            assert!(buf[..256 * 8].iter().all(|&b| b == 0) && buf[256 * 8] == 0xAA);
            assert_eq!(XApuDequeueResult(h, &mut r), XAPU_NO_RESULTS);
            assert_eq!(XApuDisconnect(h), S_OK);
        }
    }
}
