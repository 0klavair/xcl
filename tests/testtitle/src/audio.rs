//! XAudio2 the way a console title uses it: engine with console-only flags, mastering
//! voice with a console device id, an XMA2 voice (no decoder on PC – XCL plays silence of
//! the same length) and a plain PCM voice. Buffer callbacks must come back with the
//! title's contexts and the stream must end on time.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[link(name = "xaudio2_9", kind = "raw-dylib")]
extern "system" {
    fn XAudio2Create(pp: *mut *mut c_void, flags: u32, processor: u32) -> i32;
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
struct WaveFormatEx {
    tag: u16,
    channels: u16,
    rate: u32,
    avg_bytes: u32,
    block_align: u16,
    bits: u16,
    cb_size: u16,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
struct Xma2Format {
    wfx: WaveFormatEx,
    num_streams: u16,
    channel_mask: u32,
    samples_encoded: u32,
    bytes_per_block: u32,
    play_begin: u32,
    play_length: u32,
    loop_begin: u32,
    loop_length: u32,
    loop_count: u8,
    encoder_version: u8,
    block_count: u16,
}

#[repr(C, packed)]
struct Buffer {
    flags: u32,
    audio_bytes: u32,
    audio_data: *const u8,
    play_begin: u32,
    play_length: u32,
    loop_begin: u32,
    loop_length: u32,
    loop_count: u32,
    context: *mut c_void,
}

#[repr(C, packed)]
#[derive(Default)]
struct VoiceDetails {
    creation_flags: u32,
    active_flags: u32,
    channels: u32,
    rate: u32,
}

const END_OF_STREAM: u32 = 0x40;
const XMA_CONTEXT: usize = 0x100;
const PCM_CONTEXT: usize = 0x2_0000;

static BUFFER_END: AtomicUsize = AtomicUsize::new(0);
static STREAM_END: AtomicUsize = AtomicUsize::new(0);
static VOICE_ERRORS: AtomicUsize = AtomicUsize::new(0);
static STARTED: OnceLock<Instant> = OnceLock::new();
/// ms from Start to the end of the XMA2 buffer
static XMA_END_MS: AtomicU64 = AtomicU64::new(u64::MAX);

// IXAudio2VoiceCallback (no IUnknown)
#[repr(C)]
struct CallbackVtbl {
    pass_start: unsafe extern "system" fn(*mut c_void, u32),
    pass_end: unsafe extern "system" fn(*mut c_void),
    stream_end: unsafe extern "system" fn(*mut c_void),
    buffer_start: unsafe extern "system" fn(*mut c_void, *mut c_void),
    buffer_end: unsafe extern "system" fn(*mut c_void, *mut c_void),
    loop_end: unsafe extern "system" fn(*mut c_void, *mut c_void),
    voice_error: unsafe extern "system" fn(*mut c_void, *mut c_void, i32),
}

unsafe extern "system" fn on_pass_start(_: *mut c_void, _: u32) {}
unsafe extern "system" fn on_nothing(_: *mut c_void) {}
unsafe extern "system" fn on_stream_end(_: *mut c_void) {
    STREAM_END.fetch_add(1, Ordering::SeqCst);
}
unsafe extern "system" fn on_ctx(_: *mut c_void, _: *mut c_void) {}
unsafe extern "system" fn on_buffer_end(_: *mut c_void, ctx: *mut c_void) {
    if ctx as usize == XMA_CONTEXT {
        if let Some(t0) = STARTED.get() {
            XMA_END_MS.store(t0.elapsed().as_millis() as u64, Ordering::SeqCst);
        }
    }
    BUFFER_END.fetch_add(ctx as usize, Ordering::SeqCst);
}
unsafe extern "system" fn on_error(_: *mut c_void, _: *mut c_void, _: i32) {
    VOICE_ERRORS.fetch_add(1, Ordering::SeqCst);
}

static VTBL: CallbackVtbl = CallbackVtbl {
    pass_start: on_pass_start,
    pass_end: on_nothing,
    stream_end: on_stream_end,
    buffer_start: on_ctx,
    buffer_end: on_buffer_end,
    loop_end: on_ctx,
    voice_error: on_error,
};

#[repr(C)]
struct Callback {
    vtbl: &'static CallbackVtbl,
}

static CALLBACK: Callback = Callback { vtbl: &VTBL };

unsafe fn method<F: Copy>(obj: *mut c_void, slot: usize) -> F {
    let vt = *(obj as *const *const usize);
    std::mem::transmute_copy::<usize, F>(&*vt.add(slot))
}

type CreateSourceVoice =
    unsafe extern "system" fn(*mut c_void, *mut *mut c_void, *const c_void, u32, f32, *const c_void, *const c_void, *const c_void) -> i32;
type CreateMasteringVoice = unsafe extern "system" fn(*mut c_void, *mut *mut c_void, u32, u32, u32, *const u16, *const c_void, u32) -> i32;
type GetVoiceDetails = unsafe extern "system" fn(*mut c_void, *mut VoiceDetails);
type Submit = unsafe extern "system" fn(*mut c_void, *const Buffer, *const c_void) -> i32;
type Start = unsafe extern "system" fn(*mut c_void, u32, u32) -> i32;
type Destroy = unsafe extern "system" fn(*mut c_void);
type Release = unsafe extern "system" fn(*mut c_void) -> u32;

pub fn xaudio2_console_voices() -> Result<(), String> {
    unsafe {
        let mut eng: *mut c_void = std::ptr::null_mut();
        // 0x1 / 0x40: flags the Windows engine does not accept; 0x3c: a console core mask
        let hr = XAudio2Create(&mut eng, 0x41, 0x3c);
        if hr != 0 || eng.is_null() {
            return Err(format!("XAudio2Create → {:#x}", hr as u32));
        }
        let release: Release = method(eng, 2);
        let device = super::wide("console-audio-endpoint-0");
        let mut master: *mut c_void = std::ptr::null_mut();
        let hr = method::<CreateMasteringVoice>(eng, 7)(eng, &mut master, 2, 48_000, 0x4, device.as_ptr(), std::ptr::null(), 6);
        if hr < 0 {
            println!("  (brak urządzenia audio: CreateMasteringVoice → {:#x}; pomijam głosy)", hr as u32);
            release(eng);
            return Ok(());
        }

        // XMA2 mono 48 kHz – Windows XAudio2 cannot play it
        let xma = Xma2Format {
            wfx: WaveFormatEx { tag: 0x166, channels: 1, rate: 48_000, avg_bytes: 6000, block_align: 1, bits: 16, cb_size: 34 },
            num_streams: 1,
            channel_mask: 4,
            samples_encoded: 9600,
            bytes_per_block: 2048,
            encoder_version: 4,
            block_count: 1,
            ..Default::default()
        };
        let create_source: CreateSourceVoice = method(eng, 5);
        let mut xv: *mut c_void = std::ptr::null_mut();
        let cb = &CALLBACK as *const Callback as *const c_void;
        let hr = create_source(eng, &mut xv, &xma as *const Xma2Format as *const c_void, 0x2, 2.0, cb, std::ptr::null(), std::ptr::null());
        if hr != 0 || xv.is_null() {
            return Err(format!("CreateSourceVoice(XMA2) → {:#x}", hr as u32));
        }
        let mut det = VoiceDetails::default();
        method::<GetVoiceDetails>(xv, 0)(xv, &mut det);
        let (ch, rate) = (det.channels, det.rate);
        if (ch, rate) != (1, 48_000) {
            return Err(format!("XMA2 voice: {ch} kan., {rate} Hz"));
        }
        let packet = vec![0x5au8; 2048];
        let xb = Buffer {
            flags: END_OF_STREAM,
            audio_bytes: 2048,
            audio_data: packet.as_ptr(),
            play_begin: 0,
            play_length: 4800, // 0.1 s
            loop_begin: 0,
            loop_length: 0,
            loop_count: 0,
            context: XMA_CONTEXT as *mut c_void,
        };
        let hr = method::<Submit>(xv, 21)(xv, &xb, std::ptr::null());
        if hr != 0 {
            return Err(format!("SubmitSourceBuffer(XMA2) → {:#x}", hr as u32));
        }

        // plain PCM16 voice must pass through untouched
        let pcm = WaveFormatEx { tag: 1, channels: 1, rate: 48_000, avg_bytes: 96_000, block_align: 2, bits: 16, cb_size: 0 };
        let mut pv: *mut c_void = std::ptr::null_mut();
        let hr = create_source(eng, &mut pv, &pcm as *const WaveFormatEx as *const c_void, 0, 2.0, cb, std::ptr::null(), std::ptr::null());
        if hr != 0 || pv.is_null() {
            return Err(format!("CreateSourceVoice(PCM) → {:#x}", hr as u32));
        }
        let samples = vec![0i16; 2400]; // 0.05 s
        let pb = Buffer {
            flags: END_OF_STREAM,
            audio_bytes: 4800,
            audio_data: samples.as_ptr() as *const u8,
            play_begin: 0,
            play_length: 0,
            loop_begin: 0,
            loop_length: 0,
            loop_count: 0,
            context: PCM_CONTEXT as *mut c_void,
        };
        if method::<Submit>(pv, 21)(pv, &pb, std::ptr::null()) != 0 {
            return Err("SubmitSourceBuffer(PCM)".into());
        }
        STARTED.get_or_init(Instant::now);
        for v in [xv, pv] {
            let hr = method::<Start>(v, 19)(v, 0, 0);
            if hr != 0 {
                return Err(format!("Start → {:#x}", hr as u32));
            }
        }
        let t0 = Instant::now();
        while (BUFFER_END.load(Ordering::SeqCst) != XMA_CONTEXT + PCM_CONTEXT || STREAM_END.load(Ordering::SeqCst) < 2)
            && t0.elapsed() < Duration::from_secs(5)
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let ends = BUFFER_END.load(Ordering::SeqCst);
        if ends != XMA_CONTEXT + PCM_CONTEXT || STREAM_END.load(Ordering::SeqCst) < 2 {
            return Err(format!("callbacki: OnBufferEnd suma kontekstów {ends:#x}, OnStreamEnd {}", STREAM_END.load(Ordering::SeqCst)));
        }
        if VOICE_ERRORS.load(Ordering::SeqCst) != 0 {
            return Err("OnVoiceError".into());
        }
        // the XMA2 buffer (PlayLength 4800 = 0.1 s) must take as long as the title expects
        let ms = XMA_END_MS.load(Ordering::SeqCst);
        if !(80..=2000).contains(&ms) {
            return Err(format!("XMA2 buffer ended after {ms} ms (expected ≈100)"));
        }
        println!("  (bufor XMA2 skończył się po {ms} ms)");
        for v in [xv, pv, master] {
            method::<Destroy>(v, 18)(v);
        }
        release(eng);
        drop((packet, samples));
        Ok(())
    }
}
