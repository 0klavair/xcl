//! xcl.exe – command line front-end and (in host mode) the process the game runs in.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

/// Our own TLS callback: ntdll calls it for every thread start/exit in the process.
/// In host mode it initialises the game's static TLS for the new thread and forwards
/// the notification to the game's TLS callbacks / DllMains.
unsafe extern "system" fn xcl_tls_callback(_h: *mut c_void, reason: u32, _r: *mut c_void) {
    if xcl::host::HOST_MODE.load(Ordering::Acquire) {
        xcl::loader::on_tls_event(reason);
    }
}

#[used]
#[link_section = ".CRT$XLY"]
static XCL_TLS_CALLBACK: unsafe extern "system" fn(*mut c_void, u32, *mut c_void) = xcl_tls_callback;

const HELP: &str = "\
XCL – warstwa zgodności Xbox Series X|S → Windows

Użycie:
  xcl run <folder gry | plik.exe> [-- argumenty gry]
      Uruchamia grę. Folder gry jest tylko czytany; zapisy, zapisy gry i logi trafiają do
      %LOCALAPPDATA%\\XCL\\<tytuł>\\ (lub %XCL_DATA_ROOT%\\<tytuł>\\).
  xcl inspect <plik.exe|plik.dll> [--json]
      Analiza bez uruchamiania: importy i co XCL dla nich ma (HLE / Windows / zaślepka).
  xcl disasm <plik> [--offset N] [--length N] [--all] [--hex] [--fssm PLIK]
      Dezasembler kodu GPU konsoli (RDNA2) – np. dla shaderów zapisanych w <dane>\\shaders\\.
      Kontenery konsolowe (XDAT/XBCH/FSSM) są rozpakowywane; --fssm zapisuje rozpakowany FSSM.
  xcl shaders <plik | folder | paczka.rpf> [--top N] [--json] [--recomp] [--compile N]
      Statystyki korpusu shaderów: formaty, etapy, histogram instrukcji RDNA2 (priorytety
      dla recompilera). Paczki RPF9 są skanowane w poszukiwaniu rekordów shaderów.
      --recomp: tłumaczy wszystkie programy RDNA2 → HLSL i pokazuje, czego brakuje;
      --compile N: dodatkowo kompiluje N pełnych tłumaczeń przez DXC.
  xcl --version
";

fn main() {
    // keep the TLS callback from being discarded by the linker
    unsafe { std::ptr::read_volatile(&XCL_TLS_CALLBACK) };

    if let Some(boot) = std::env::var_os("XCL_HOST") {
        std::env::remove_var("XCL_HOST");
        std::process::exit(xcl::host::host_main(&PathBuf::from(boot)));
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("run") if args.len() >= 2 => {
            let rest: Vec<String> = match args.iter().position(|a| a == "--") {
                Some(i) => args[i + 1..].to_vec(),
                None => args[2..].to_vec(),
            };
            xcl::launcher::run(&PathBuf::from(&args[1]), &rest)
        }
        Some("inspect") if args.len() >= 2 => {
            let json = args.iter().any(|a| a == "--json");
            xcl::inspect::run(&PathBuf::from(&args[1]), json)
        }
        Some("disasm") if args.len() >= 2 => xcl::disasm::run(&PathBuf::from(&args[1]), &args[2..]),
        Some("shaders") if args.len() >= 2 => xcl::shaders::run(&PathBuf::from(&args[1]), &args[2..]),
        Some("rpf") if args.len() >= 2 => xcl::rpf::run(&PathBuf::from(&args[1]), &args[2..]),
        Some("--version") | Some("-V") => {
            println!("xcl {}", env!("CARGO_PKG_VERSION"));
            0
        }
        _ => {
            print!("{HELP}");
            if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
                0
            } else {
                2
            }
        }
    };
    std::process::exit(code);
}
