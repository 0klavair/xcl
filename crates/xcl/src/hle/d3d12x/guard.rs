//! Slot guards for extended vtables.
//!
//! Real D3D12 objects keep internal virtual methods right after their public
//! interface (probe on Windows 11: device 125 entries vs 104 public, queue 42 vs 19).
//! The runtime calls those internally, so they must stay intact – but a console title
//! calling the same slot index means an Xbox-only method (e.g. PresentX).
//!
//! A guard thunk looks at its return address: called from the game's image → log it
//! as an Xbox method and return 0; called from anywhere else (the runtime itself) →
//! jump to the original function with all argument registers untouched.
//!
//! ```text
//! push rcx/rdx/r8/r9 ; sub rsp,0x68 ; save xmm0-3
//! rcx = [rsp+0x88] (return address), rdx = StubInfo*, call classify
//! restore xmm0-3 ; add rsp,0x68 ; pop r9/r8/rdx/rcx
//! test rax,rax ; jnz stub ; mov rax, original ; jmp rax
//! stub: xor eax,eax ; ret
//! ```

use crate::stubs::{self, StubInfo};

unsafe extern "system" fn classify(caller: usize, info: *const StubInfo) -> usize {
    if crate::modules::image_base_for_addr(caller).is_some() {
        stubs::record_call(&*info, caller);
        1
    } else {
        0
    }
}

pub fn make_guard(original: usize, info: &'static StubInfo) -> usize {
    let mut c: Vec<u8> = Vec::with_capacity(128);
    c.extend_from_slice(&[0x51, 0x52, 0x41, 0x50, 0x41, 0x51]); // push rcx, rdx, r8, r9
    c.extend_from_slice(&[0x48, 0x83, 0xEC, 0x68]); // sub rsp, 0x68
    for (reg, off) in [(0x44u8, 0x20u8), (0x4C, 0x30), (0x54, 0x40), (0x5C, 0x50)] {
        c.extend_from_slice(&[0xF3, 0x0F, 0x7F, reg, 0x24, off]); // movdqu [rsp+off], xmmN
    }
    c.extend_from_slice(&[0x48, 0x8B, 0x8C, 0x24, 0x88, 0x00, 0x00, 0x00]); // mov rcx, [rsp+0x88]
    c.extend_from_slice(&[0x48, 0xBA]); // mov rdx, imm64
    c.extend_from_slice(&(info as *const StubInfo as u64).to_le_bytes());
    c.extend_from_slice(&[0x48, 0xB8]); // mov rax, imm64
    c.extend_from_slice(&(classify as usize as u64).to_le_bytes());
    c.extend_from_slice(&[0xFF, 0xD0]); // call rax
    for (reg, off) in [(0x44u8, 0x20u8), (0x4C, 0x30), (0x54, 0x40), (0x5C, 0x50)] {
        c.extend_from_slice(&[0xF3, 0x0F, 0x6F, reg, 0x24, off]); // movdqu xmmN, [rsp+off]
    }
    c.extend_from_slice(&[0x48, 0x83, 0xC4, 0x68]); // add rsp, 0x68
    c.extend_from_slice(&[0x41, 0x59, 0x41, 0x58, 0x5A, 0x59]); // pop r9, r8, rdx, rcx
    c.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
    c.extend_from_slice(&[0x75, 0x0C]); // jnz +12
    c.extend_from_slice(&[0x48, 0xB8]); // mov rax, imm64
    c.extend_from_slice(&(original as u64).to_le_bytes());
    c.extend_from_slice(&[0xFF, 0xE0]); // jmp rax
    c.extend_from_slice(&[0x31, 0xC0, 0xC3]); // xor eax, eax ; ret
    let p = stubs::alloc_code(c.len());
    unsafe {
        std::ptr::copy_nonoverlapping(c.as_ptr(), p, c.len());
        windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache(
            windows_sys::Win32::System::Threading::GetCurrentProcess(),
            p as *const _,
            c.len(),
        );
    }
    p as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "system" fn original(a: u64, b: u64, c: u64, d: u64, e: u64, f: f64) -> u64 {
        a + 10 * b + 100 * c + 1000 * d + 10000 * e + (f * 100000.0) as u64
    }

    #[test]
    fn guard_forwards_calls_that_do_not_come_from_a_game_image() {
        let info = stubs::new_info("unit.dll", "IGuard::#7", 0);
        let g = make_guard(original as usize, info);
        let f: extern "system" fn(u64, u64, u64, u64, u64, f64) -> u64 = unsafe { std::mem::transmute(g) };
        // all integer args, a stack arg and a float arg survive the round trip
        assert_eq!(f(1, 2, 3, 4, 5, 6.0), 654321);
        assert_eq!(info.calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
}
