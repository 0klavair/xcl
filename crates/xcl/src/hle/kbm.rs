//! Keyboard and mouse as the gamepad (`keyboard_mouse` in `xcl.toml`, on by default).
//!
//! Console titles read only gamepads. While the XCL window has the focus the keyboard and
//! the mouse play as the one gamepad XCL reports (on top of a real controller):
//!
//! | PC | gamepad |
//! |---|---|
//! | W A S D | left stick |
//! | mouse movement | right stick (the cursor is held in the window; Alt+Tab frees it) |
//! | left / right mouse button | right / left trigger |
//! | wheel up / down | right / left bumper (one press a notch) |
//! | middle mouse button | right stick click |
//! | Left Shift / Left Ctrl | left / right stick click |
//! | E / Q | Y / X |
//! | Space, Enter / Escape, Backspace | A / B |
//! | arrows / Tab / P | D-pad / View / Menu |
//!
//! The mouse is read with Raw Input on the XCL window (movement in mouse counts, not the
//! cursor); its speed becomes a stick deflection – the stick turns the camera at a rate,
//! so a mouse moving at a given speed turns it at a matching one (`mouse_sensitivity`).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use super::gameinput::{GamepadState, LEFT_SHOULDER, LEFT_STICK, RIGHT_SHOULDER, RIGHT_STICK, X, Y};

// ---------------------------------------------------------------------------------------
// the mapping (pure, tested)
// ---------------------------------------------------------------------------------------

/// The left stick from the movement keys: full deflection, diagonals on the unit circle.
pub fn stick_from_keys(up: bool, left: bool, down: bool, right: bool) -> (f32, f32) {
    let x = (right as i32 - left as i32) as f32;
    let y = (up as i32 - down as i32) as f32;
    let len = (x * x + y * y).sqrt();
    if len == 0.0 {
        (0.0, 0.0)
    } else {
        (x / len, y / len)
    }
}

/// mouse counts a second that give full deflection at sensitivity 1
const FULL_SPEED: f32 = 4000.0;
/// slower than this (counts a second) the stick rests (sensor noise, a hand on the mouse)
const REST_SPEED: f32 = 20.0;
/// deflection the slowest movement starts at: past the inner dead zone games give sticks
const START: f32 = 0.15;
/// time constant of the speed's smoothing (s): the mouse reports in bursts, a stick is read
/// once a frame
const SMOOTHING: f32 = 0.03;

/// The right stick for a mouse velocity (counts a second, screen axes: y grows downwards).
pub fn stick_from_mouse(vx: f32, vy: f32, sensitivity: f32) -> (f32, f32) {
    let speed = (vx * vx + vy * vy).sqrt();
    if speed < REST_SPEED || sensitivity <= 0.0 {
        return (0.0, 0.0);
    }
    let m = (speed * sensitivity / FULL_SPEED).min(1.0);
    let out = START + (1.0 - START) * m;
    (vx / speed * out, -vy / speed * out)
}

/// The mouse's velocity, smoothed: counts since the last update over `dt` seconds.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
}

impl Velocity {
    pub fn update(&mut self, dx: i32, dy: i32, dt: f32) {
        let dt = dt.clamp(0.001, 0.1);
        let a = 1.0 - (-dt / SMOOTHING).exp();
        self.x += a * (dx as f32 / dt - self.x);
        self.y += a * (dy as f32 / dt - self.y);
    }
}

/// press time of a wheel notch and the gap after it (µs): long enough for a title that reads
/// the gamepad once a frame to see each press
const NOTCH_HOLD_US: u64 = 50_000;
const NOTCH_GAP_US: u64 = 50_000;

/// Wheel notches as bumper presses, one after the other.
#[derive(Debug, Default)]
pub struct Wheel {
    /// notches still to press (+ up, - down)
    pending: i32,
    /// the press under way: (button, µs since it started)
    current: Option<(u32, u64)>,
}

impl Wheel {
    pub fn notches(&mut self, n: i32) {
        self.pending = (self.pending + n).clamp(-8, 8);
    }

    /// The bumper held now, `dt` seconds after the last call.
    pub fn step(&mut self, dt: f32) -> u32 {
        if let Some((b, t)) = self.current.as_mut() {
            *t += (dt.max(0.0) * 1e6).round() as u64;
            if *t < NOTCH_HOLD_US {
                return *b;
            }
            if *t < NOTCH_HOLD_US + NOTCH_GAP_US {
                return 0;
            }
            self.current = None;
        }
        if self.pending == 0 {
            return 0;
        }
        let b = if self.pending > 0 { RIGHT_SHOULDER } else { LEFT_SHOULDER };
        self.pending -= self.pending.signum();
        self.current = Some((b, 0));
        b
    }
}

// ---------------------------------------------------------------------------------------
// input from the PC
// ---------------------------------------------------------------------------------------

#[link(name = "user32")]
extern "system" {
    fn GetAsyncKeyState(key: i32) -> i16;
    fn GetForegroundWindow() -> *mut c_void;
    fn RegisterRawInputDevices(devices: *const RawInputDevice, n: u32, size: u32) -> i32;
    fn GetRawInputData(input: isize, command: u32, data: *mut c_void, size: *mut u32, header: u32) -> u32;
    fn ClipCursor(rect: *const Rect) -> i32;
    fn GetClientRect(hwnd: *mut c_void, rect: *mut Rect) -> i32;
    fn ClientToScreen(hwnd: *mut c_void, point: *mut Point) -> i32;
    fn SetCursor(cursor: *mut c_void) -> *mut c_void;
}

#[repr(C)]
struct RawInputDevice {
    usage_page: u16,
    usage: u16,
    flags: u32,
    target: *mut c_void,
}

#[repr(C)]
#[derive(Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
#[derive(Default)]
struct Point {
    x: i32,
    y: i32,
}

/// RAWINPUTHEADER (24 bytes) + RAWMOUSE
#[repr(C)]
#[derive(Default)]
struct RawMouseInput {
    kind: u32,
    size: u32,
    device: usize,
    wparam: usize,
    flags: u16,
    _pad: u16,
    button_flags: u16,
    button_data: u16,
    raw_buttons: u32,
    last_x: i32,
    last_y: i32,
    extra: u32,
}

const WM_INPUT: u32 = 0x00FF;
const WM_ACTIVATE: u32 = 0x0006;
const WM_SETCURSOR: u32 = 0x0020;
const WM_SIZE: u32 = 0x0005;
const WM_MOVE: u32 = 0x0003;
const WM_LBUTTONDOWN: u32 = 0x0201;
const RID_INPUT: u32 = 0x1000_0003;
const RIM_TYPEMOUSE: u32 = 0;
const MOUSE_MOVE_ABSOLUTE: u16 = 1;
const RI_MOUSE_WHEEL: u16 = 0x0400;
const HTCLIENT: isize = 1;

/// the XCL window (raw input goes there; the cursor is held in it)
static WINDOW: AtomicIsize = AtomicIsize::new(0);
/// the cursor is held in the window (it has the focus)
static CAPTURED: AtomicBool = AtomicBool::new(false);
/// mouse counts not yet taken by `apply`
static DX: AtomicI32 = AtomicI32::new(0);
static DY: AtomicI32 = AtomicI32::new(0);
/// wheel movement not yet taken (WHEEL_DELTA = 120 a notch)
static WHEEL: AtomicI32 = AtomicI32::new(0);

struct Mouse {
    last: Instant,
    velocity: Velocity,
    wheel: Wheel,
    wheel_rest: i32,
}

static MOUSE: Mutex<Option<Mouse>> = Mutex::new(None);

pub fn enabled() -> bool {
    crate::state::config().keyboard_mouse
}

/// The XCL window is up: the mouse reports to it.
pub fn attach(hwnd: *mut c_void) {
    if !enabled() {
        return;
    }
    WINDOW.store(hwnd as isize, Ordering::Release);
    // generic desktop mouse
    let dev = RawInputDevice { usage_page: 1, usage: 2, flags: 0, target: hwnd };
    let ok = unsafe { RegisterRawInputDevices(&dev, 1, std::mem::size_of::<RawInputDevice>() as u32) } != 0;
    crate::xlog_info!(
        "keyboard and mouse play as the gamepad (WASD left stick, mouse right stick, mouse buttons triggers, wheel bumpers){}",
        if ok { "" } else { " – raw mouse input unavailable, the mouse is not read" }
    );
}

unsafe fn hold_cursor(hwnd: *mut c_void, on: bool) {
    if !on {
        if CAPTURED.swap(false, Ordering::AcqRel) {
            ClipCursor(std::ptr::null());
        }
        return;
    }
    // a point at the client area's centre: the cursor stays there, hidden
    let mut r = Rect::default();
    GetClientRect(hwnd, &mut r);
    let mut c = Point { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 };
    ClientToScreen(hwnd, &mut c);
    let at = Rect { left: c.x, top: c.y, right: c.x + 1, bottom: c.y + 1 };
    ClipCursor(&at);
    CAPTURED.store(true, Ordering::Release);
}

/// The process is ending: the cursor is let go (a held cursor outlives its window).
pub fn release() {
    unsafe { hold_cursor(WINDOW.load(Ordering::Acquire) as *mut c_void, false) };
}

/// The XCL window's messages: true when handled (the window procedure returns 0).
pub unsafe fn on_message(hwnd: *mut c_void, msg: u32, wp: usize, lp: isize) -> bool {
    if WINDOW.load(Ordering::Acquire) != hwnd as isize {
        return false;
    }
    match msg {
        WM_INPUT => {
            let mut raw = RawMouseInput::default();
            let mut size = std::mem::size_of::<RawMouseInput>() as u32;
            let n = GetRawInputData(lp, RID_INPUT, &mut raw as *mut _ as *mut c_void, &mut size, 24);
            if n != u32::MAX && n as usize >= std::mem::size_of::<RawMouseInput>() && raw.kind == RIM_TYPEMOUSE && CAPTURED.load(Ordering::Acquire) {
                if raw.flags & MOUSE_MOVE_ABSOLUTE == 0 {
                    DX.fetch_add(raw.last_x, Ordering::Relaxed);
                    DY.fetch_add(raw.last_y, Ordering::Relaxed);
                }
                if raw.button_flags & RI_MOUSE_WHEEL != 0 {
                    WHEEL.fetch_add(raw.button_data as i16 as i32, Ordering::Relaxed);
                }
            }
            false // DefWindowProc cleans up
        }
        WM_ACTIVATE => {
            hold_cursor(hwnd, wp & 0xFFFF != 0);
            false
        }
        WM_LBUTTONDOWN | WM_SIZE | WM_MOVE => {
            if GetForegroundWindow() == hwnd {
                hold_cursor(hwnd, true);
            }
            false
        }
        WM_SETCURSOR if CAPTURED.load(Ordering::Acquire) && (lp & 0xFFFF) == HTCLIENT => {
            SetCursor(std::ptr::null_mut());
            true
        }
        _ => false,
    }
}

fn key(vk: i32) -> bool {
    unsafe { GetAsyncKeyState(vk) as u16 & 0x8000 != 0 }
}

/// The keyboard and the mouse on top of `s` (the real controller), while the XCL window
/// has the focus.
pub fn apply(s: &mut GamepadState) {
    if !enabled() {
        return;
    }
    let hwnd = WINDOW.load(Ordering::Acquire);
    let focused = hwnd != 0 && unsafe { GetForegroundWindow() } as isize == hwnd;
    if !focused {
        // Alt+Tab, another window: the cursor is free, nothing is pressed
        unsafe { hold_cursor(hwnd as *mut c_void, false) };
        DX.store(0, Ordering::Relaxed);
        DY.store(0, Ordering::Relaxed);
        WHEEL.store(0, Ordering::Relaxed);
        return;
    }
    // movement keys: the left stick (a real stick moved further wins; so does it for the mouse)
    let (lx, ly) = stick_from_keys(key(0x57), key(0x41), key(0x53), key(0x44));
    if (lx != 0.0 || ly != 0.0) && lx * lx + ly * ly >= s.left_x * s.left_x + s.left_y * s.left_y {
        s.left_x = lx;
        s.left_y = ly;
    }
    // (Space / Enter, Escape / Backspace, arrows, Tab, P: `gameinput::keyboard`)
    const KEYS: [(i32, u32); 5] = [
        (0xA0, LEFT_STICK),  // Left Shift
        (0xA2, RIGHT_STICK), // Left Ctrl
        (0x04, RIGHT_STICK), // middle mouse button
        (0x45, Y),           // E
        (0x51, X),           // Q
    ];
    s.buttons |= KEYS.iter().filter(|(k, _)| key(*k)).fold(0, |b, (_, g)| b | g);
    if key(0x01) {
        s.right_trigger = 1.0;
    }
    if key(0x02) {
        s.left_trigger = 1.0;
    }
    let mut g = MOUSE.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(|| Mouse { last: Instant::now(), velocity: Velocity::default(), wheel: Wheel::default(), wheel_rest: 0 });
    let now = Instant::now();
    let dt = now.duration_since(m.last).as_secs_f32();
    m.last = now;
    m.velocity.update(DX.swap(0, Ordering::Relaxed), DY.swap(0, Ordering::Relaxed), dt);
    let (rx, ry) = stick_from_mouse(m.velocity.x, m.velocity.y, crate::state::config().mouse_sensitivity);
    if (rx != 0.0 || ry != 0.0) && rx * rx + ry * ry >= s.right_x * s.right_x + s.right_y * s.right_y {
        s.right_x = rx;
        s.right_y = ry;
    }
    m.wheel_rest += WHEEL.swap(0, Ordering::Relaxed);
    let notches = m.wheel_rest / 120;
    m.wheel_rest -= notches * 120;
    m.wheel.notches(notches);
    s.buttons |= m.wheel.step(dt.min(0.1));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn movement_keys_make_a_unit_stick() {
        assert_eq!(stick_from_keys(false, false, false, false), (0.0, 0.0));
        assert_eq!(stick_from_keys(true, false, false, false), (0.0, 1.0));
        assert_eq!(stick_from_keys(false, true, false, false), (-1.0, 0.0));
        let (x, y) = stick_from_keys(true, false, false, true);
        assert!((x - y).abs() < 1e-6 && (x * x + y * y - 1.0).abs() < 1e-6);
        // opposite keys cancel
        assert_eq!(stick_from_keys(true, true, true, true), (0.0, 0.0));
    }

    #[test]
    fn mouse_speed_becomes_a_deflection() {
        // resting
        assert_eq!(stick_from_mouse(5.0, 0.0, 1.0), (0.0, 0.0));
        // slow movement starts past the dead zone, to the right
        let (x, y) = stick_from_mouse(100.0, 0.0, 1.0);
        assert!(x > START && x < 0.2 && y == 0.0);
        // fast movement: full deflection; the mouse moving down tilts the stick down
        let (x, y) = stick_from_mouse(0.0, 10_000.0, 1.0);
        assert!(x == 0.0 && (y + 1.0).abs() < 1e-6);
        // sensitivity scales the speed
        let (a, _) = stick_from_mouse(1000.0, 0.0, 1.0);
        let (b, _) = stick_from_mouse(1000.0, 0.0, 2.0);
        assert!(b > a);
    }

    #[test]
    fn velocity_follows_the_counts_and_settles() {
        let mut v = Velocity::default();
        for _ in 0..50 {
            v.update(8, -4, 0.004); // 2000 counts/s right, 1000 up
        }
        assert!((v.x - 2000.0).abs() < 20.0 && (v.y + 1000.0).abs() < 10.0);
        for _ in 0..50 {
            v.update(0, 0, 0.004);
        }
        assert!(v.x.abs() < REST_SPEED && v.y.abs() < REST_SPEED);
    }

    #[test]
    fn wheel_notches_press_bumpers_one_by_one() {
        let mut w = Wheel::default();
        w.notches(2);
        w.notches(-1); // net one notch up
        let mut presses = Vec::new();
        let mut last = 0;
        for _ in 0..100 {
            let b = w.step(0.01);
            if b != 0 && b != last {
                presses.push(b);
            }
            last = b;
        }
        assert_eq!(presses, vec![RIGHT_SHOULDER]);
        w.notches(-2);
        let held: Vec<u32> = (0..30).map(|_| w.step(0.01)).collect();
        // held for NOTCH_HOLD, released for NOTCH_GAP, then the second press
        assert_eq!(held.iter().filter(|&&b| b == LEFT_SHOULDER).count(), 10);
        assert!(held[..5].iter().all(|&b| b == LEFT_SHOULDER) && held[5..10].iter().all(|&b| b == 0));
    }
}
