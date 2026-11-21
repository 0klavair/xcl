//! Exception-handling support for manually mapped images.
//!
//! * `.pdata` is registered with `RtlAddFunctionTable` so the unwinder finds our frames.
//! * `RtlPcToFileHeader` is hooked: x64 C++ `throw` (`_CxxThrowException`) and the
//!   catch matching code use it to find the image base of the thrower's `ThrowInfo`;
//!   the real function only knows modules on the Windows loader list.
//! * An unhandled-exception filter logs crashes with module+RVA and writes the report.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows_sys::Win32::System::Diagnostics::Debug::{
    RtlAddFunctionTable, SetUnhandledExceptionFilter, EXCEPTION_POINTERS,
    IMAGE_RUNTIME_FUNCTION_ENTRY,
};

use crate::hooks;
use crate::modules;

pub fn register_function_table(base: usize, rva: u32, size: u32) -> bool {
    let count = size / std::mem::size_of::<IMAGE_RUNTIME_FUNCTION_ENTRY>() as u32;
    let ok = unsafe {
        RtlAddFunctionTable(
            (base + rva as usize) as *const IMAGE_RUNTIME_FUNCTION_ENTRY,
            count,
            base as u64,
        )
    };
    if !ok {
        crate::xlog_warn!("RtlAddFunctionTable({base:#x}) nie powiodło się");
    }
    ok
}

static ORIG_PC_TO_FILE_HEADER: AtomicUsize = AtomicUsize::new(0);
type PcToFileHeader = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> *mut c_void;

unsafe extern "system" fn hk_rtl_pc_to_file_header(
    pc: *mut c_void,
    base_out: *mut *mut c_void,
) -> *mut c_void {
    if let Some(b) = modules::image_base_for_addr(pc as usize) {
        if !base_out.is_null() {
            *base_out = b as *mut c_void;
        }
        return b as *mut c_void;
    }
    let orig: PcToFileHeader = std::mem::transmute(ORIG_PC_TO_FILE_HEADER.load(Ordering::Acquire));
    orig(pc, base_out)
}

pub fn install_pc_hook() -> Result<(), String> {
    unsafe {
        hooks::hook(
            "ntdll.dll",
            "RtlPcToFileHeader",
            hk_rtl_pc_to_file_header as usize,
            &ORIG_PC_TO_FILE_HEADER,
        )
    }
}

unsafe extern "system" fn unhandled_filter(info: *const EXCEPTION_POINTERS) -> i32 {
    if !info.is_null() && !(*info).ExceptionRecord.is_null() {
        let rec = &*(*info).ExceptionRecord;
        let addr = rec.ExceptionAddress as usize;
        let mut extra = String::new();
        if rec.ExceptionCode as u32 == 0xC000_0005 && rec.NumberParameters >= 2 {
            let op = match rec.ExceptionInformation[0] {
                0 => "odczyt",
                1 => "zapis",
                8 => "wykonanie",
                _ => "?",
            };
            extra = format!(" ({op} adresu {:#x})", rec.ExceptionInformation[1]);
        }
        let ctx = (*info).ContextRecord;
        let regs = if ctx.is_null() {
            String::new()
        } else {
            let c = &*ctx;
            format!(
                " rax={:#x} rcx={:#x} rdx={:#x} r8={:#x} r9={:#x} rsp={:#x}",
                c.Rax, c.Rcx, c.Rdx, c.R8, c.R9, c.Rsp
            )
        };
        let msg = format!(
            "nieobsłużony wyjątek {:#010x} w {}{extra}{regs}",
            rec.ExceptionCode as u32,
            modules::describe_address(addr)
        );
        crate::xlog_error!("{msg}");
        crate::report::write_final(Some(msg));
        crate::log::flush();
    }
    0 // EXCEPTION_CONTINUE_SEARCH
}

pub fn install_crash_handler() {
    unsafe {
        SetUnhandledExceptionFilter(Some(unhandled_filter));
    }
}
