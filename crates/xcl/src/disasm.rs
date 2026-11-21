//! `xcl disasm <file> [--offset N] [--length N] [--all] [--hex] [--fssm FILE]` – RDNA2
//! disassembler for captured shader blobs (or any binary with console GPU code).
//! Console containers (XDAT/XBCH/FSSM) are unpacked and their layout is printed.

use std::path::Path;

use crate::gpu::{rdna2, shader, xbox};

fn parse_num(s: &str) -> Option<usize> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => usize::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

fn print_lines(code: &[u8], base: usize, stop: bool) {
    for l in rdna2::disassemble(code, base as u64, stop) {
        let hex: Vec<String> = l.bytes.iter().map(|b| format!("{b:02x}")).collect();
        let mark = if l.flags & rdna2::F_BRANCH != 0 {
            "  ; →"
        } else {
            ""
        };
        println!("  {:08x}: {:<52} {}{mark}", l.pc, l.text, hex.join(" "));
    }
}

fn hexdump(data: &[u8], base: usize) {
    for (i, row) in data.chunks(16).enumerate() {
        let hex: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
        let txt: String = row
            .iter()
            .map(|&b| {
                if (0x20..0x7f).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        println!("    {:06x}: {:<47}  {txt}", base + i * 16, hex.join(" "));
    }
}

fn xdat_label(id: u32) -> &'static str {
    match id {
        xbox::XDAT_HASH => "hash",
        xbox::XDAT_XBCH => "XBCH → FSSM (kod natywny)",
        xbox::XDAT_NESTED_HEADER => "zagnieżdżony nagłówek DXIL (NOBC)",
        _ => "?",
    }
}

fn print_xbox(sh: &xbox::XboxShader, full_hex: bool, fssm_out: Option<&str>) {
    println!(
        "kontener konsolowy: etap {} (DXIL), SM {}.{}, XDAT @ +{:#x}",
        sh.stage, sh.shader_model.0, sh.shader_model.1, sh.xdat_offset
    );
    for e in &sh.entries {
        println!(
            "  XDAT {:#04x}  +{:#x}  {} B  {}",
            e.id,
            e.offset,
            e.size,
            xdat_label(e.id)
        );
    }
    if let Some(h) = sh.hash {
        println!(
            "  hash: {}",
            h.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
    }
    for n in &sh.notes {
        println!("  UWAGA: {n}");
    }
    for (i, f) in sh.fssm.iter().enumerate() {
        println!(
            "FSSM #{i}: XBCH v{} @ +{:#x}, zlib {} B → {} B, FSSM v{:#x}",
            f.xbch_version,
            f.xbch_offset,
            f.compressed,
            f.data.len(),
            f.version
        );
        if let Some(e) = &f.error {
            println!("  UWAGA: {e}");
        }
        for s in &f.stages {
            println!(
                "  etap sprzętowy: {} – deskryptor @ +{:#x}",
                s.label(),
                s.offset
            );
        }
        match f.code {
            Some((o, l)) => println!(
                "  kod: +{o:#x}, {l} B ({}), programów: {}",
                if f.code_from_header {
                    "wskaźnik z nagłówka +0xC0"
                } else {
                    "znaleziony skanowaniem"
                },
                f.programs.len()
            ),
            None => println!("  kod: nie znaleziono"),
        }
        let limit = if full_hex { usize::MAX } else { 0x140 };
        println!(
            "  nagłówek – niezerowe qwordy (bez wypełniacza 0xDEADBEEF){}:",
            if full_hex {
                ""
            } else {
                ", do +0x140; --hex: całość"
            }
        );
        for (o, q) in xbox::header_qwords(f, limit) {
            println!("    +{o:#05x}: {q:016x}");
        }
        if full_hex {
            if let Some((o, l)) = f.code {
                println!("  dane za kodem (+{:#x}):", o + l);
                hexdump(&f.data[o + l..], o + l);
            }
        }
        if let Some(out) = fssm_out {
            let name = if i == 0 {
                out.to_string()
            } else {
                format!("{out}.{i}")
            };
            match std::fs::write(&name, &f.data) {
                Ok(()) => println!("  zapisano FSSM do {name}"),
                Err(e) => eprintln!("xcl disasm: {name}: {e}"),
            }
        }
    }
}

pub fn run(path: &Path, args: &[String]) -> i32 {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("xcl disasm: {}: {e}", path.display());
            return 2;
        }
    };
    let opt = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| parse_num(v))
    };
    let all = args.iter().any(|a| a == "--all");
    println!(
        "{} – {} B, dekoder: {}",
        path.display(),
        data.len(),
        crate::gpu::RDNA2_ARCH
    );

    if let Some(off) = opt("--offset") {
        if off >= data.len() {
            eprintln!("xcl disasm: offset {off:#x} poza plikiem");
            return 2;
        }
        let len = opt("--length")
            .unwrap_or(data.len() - off)
            .min(data.len() - off);
        print_lines(&data[off..off + len], off, !all);
        return 0;
    }

    let (kind, parts) = shader::classify(&data);
    println!("format: {}", kind.label());
    if !parts.is_empty() {
        println!("części kontenera:");
        for p in &parts {
            println!("  {}  +{:#x}  {} B", p.fourcc, p.offset, p.size);
        }
    }
    if kind == shader::Kind::XboxNative {
        if let Some(sh) = xbox::parse(&data) {
            let out = args
                .iter()
                .position(|a| a == "--fssm")
                .and_then(|i| args.get(i + 1))
                .map(String::as_str);
            print_xbox(&sh, args.iter().any(|a| a == "--hex"), out);
        }
    }
    let listing = shader::rdna2_listing(&data);
    if listing.is_empty() {
        println!("nie znaleziono programów RDNA2 (zakończonych s_endpgm); użyj --offset N, aby wymusić dekodowanie");
        if matches!(kind, shader::Kind::Dxil | shader::Kind::Dxbc) {
            println!("(to shader w formacie PC – nie wymaga rekompilacji)");
        }
    } else {
        print!("{listing}");
    }
    0
}
