//! `xcl rpf <paczka> [--top N] [--find FRAGMENT] [--extract FRAGMENT --out FOLDER]`:
//! lists the records of an RPF9 package (name, sizes, signature of the unpacked data) and
//! extracts the matching ones. Same record scan as the shader pack (`gpu::pack`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::gpu::pack;

/// Printable signature of unpacked data: 4 ASCII bytes or hex.
fn magic(b: &[u8]) -> String {
    let m = &b[..b.len().min(4)];
    if m.len() == 4 && m.iter().all(|c| c.is_ascii_alphanumeric() || *c == b' ') {
        String::from_utf8_lossy(m).into_owned()
    } else {
        m.iter().map(|c| format!("{c:02x}")).collect()
    }
}

pub fn run(path: &Path, args: &[String]) -> i32 {
    let opt = |n: &str| args.iter().position(|a| a == n).and_then(|i| args.get(i + 1)).cloned();
    let top: usize = opt("--top").and_then(|v| v.parse().ok()).unwrap_or(30);
    let find = opt("--find");
    let extract = opt("--extract");
    let out = opt("--out").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("rpf_out"));
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("xcl rpf: {}: {e}", path.display());
            return 2;
        }
    };
    if !data.starts_with(pack::RPF9_MAGIC) {
        eprintln!("xcl rpf: {} nie jest paczką RPF9", path.display());
        return 2;
    }
    let recs = pack::scan(&data, 0x40);
    if recs.is_empty() {
        // no inline records: list the (nested) RPF9 headers and say whether the TOC is readable
        let heads: Vec<usize> = data.windows(4).enumerate().filter(|(i, w)| i % 0x800 == 0 && *w == pack::RPF9_MAGIC).map(|(i, _)| i).collect();
        let toc = |h: usize| data.get(h + 0x40..h + 0x60).map(|b| b.to_vec()).unwrap_or_default();
        let same = heads.len() > 1 && heads.iter().all(|&h| toc(h) == toc(heads[0]));
        println!("{}: {:.1} MB, brak rekordów z jawnymi nazwami", path.display(), data.len() as f64 / 1048576.0);
        println!("archiwa RPF9 (nagłówki co 2 KB): {}", heads.len());
        for &h in heads.iter().take(top) {
            let w = |o: usize| u32::from_le_bytes(data[h + o..h + o + 4].try_into().unwrap());
            println!("  {h:#010x}: pola {:#x} {:#x} {:#x}", w(4), w(8), w(12));
        }
        if same {
            println!("TOC zaszyfrowany: wszystkie archiwa zaczynają spis od tych samych bajtów (stały klucz z gry) – bez exe nie do odczytania");
        }
        return 0;
    }
    let (csum, usum): (usize, usize) = recs.iter().fold((0, 0), |a, r| (a.0 + r.csize, a.1 + r.usize));
    println!(
        "{}: {:.1} MB, {} rekordów, spakowane {:.1} MB → {:.1} MB",
        path.display(),
        data.len() as f64 / 1048576.0,
        recs.len(),
        csum as f64 / 1048576.0,
        usum as f64 / 1048576.0
    );
    // kinds by extension / name prefix and by signature
    let mut by_ext: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut by_magic: BTreeMap<String, usize> = BTreeMap::new();
    let mut shown = 0;
    let mut extracted = 0;
    for r in &recs {
        let ext = r.name.rsplit_once('.').map(|x| x.1.to_ascii_lowercase()).unwrap_or_else(|| "(bez rozszerzenia)".into());
        let e = by_ext.entry(ext).or_default();
        e.0 += 1;
        e.1 += r.usize;
        let hit = find.as_deref().is_none_or(|f| r.name.contains(f));
        let want = extract.as_deref().is_some_and(|f| r.name.contains(f));
        if !(hit && shown < top) && !want && by_magic.len() > 64 {
            continue;
        }
        let Some(blob) = pack::inflate(&data, r) else { continue };
        *by_magic.entry(magic(&blob)).or_default() += 1;
        if hit && shown < top {
            println!("  {:#010x} {:>9} → {:>9} B  [{}]  {}", r.offset, r.csize, r.usize, magic(&blob), r.name);
            shown += 1;
        }
        if want {
            let _ = std::fs::create_dir_all(&out);
            let file = out.join(r.name.replace(['/', '\\', ':'], "_"));
            if std::fs::write(&file, &blob).is_ok() {
                extracted += 1;
            }
        }
    }
    println!("rozszerzenia (rekordy, rozmiar po rozpakowaniu):");
    for (k, (n, s)) in &by_ext {
        println!("  {k:<20} {n:>7} {:>10.1} MB", *s as f64 / 1048576.0);
    }
    let mut m: Vec<_> = by_magic.into_iter().collect();
    m.sort_by(|a, b| b.1.cmp(&a.1));
    println!("sygnatury danych (z próbki): {}", m.iter().take(12).map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", "));
    if extract.is_some() {
        println!("wypakowano {extracted} do {}", out.display());
    }
    0
}
