//! End-to-end: run the fake console title through `xcl run` and verify that
//! (1) every check inside the title passes, (2) the game folder is byte-for-byte
//! unchanged, (3) writes landed in the overlay, (4) the report lists the stub it hit.
//!
//! Needs the test title and DLL built first:  cargo build --workspace

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h ^ bytes.len() as u64
}

fn snapshot(dir: &Path) -> BTreeMap<String, u64> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, u64>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
            if p.is_dir() {
                out.insert(format!("{rel}\\"), 0);
                walk(base, &p, out);
            } else {
                out.insert(rel, fnv(&std::fs::read(&p).unwrap()));
            }
        }
    }
    let mut m = BTreeMap::new();
    walk(dir, dir, &mut m);
    m
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap().flatten() {
        let p = e.path();
        let t = dst.join(e.file_name());
        if p.is_dir() {
            copy_dir(&p, &t);
        } else {
            std::fs::copy(&p, &t).unwrap();
        }
    }
}

#[test]
fn test_title_runs_and_game_folder_is_untouched() {
    let xcl = PathBuf::from(env!("CARGO_BIN_EXE_xcl"));
    let bin_dir = xcl.parent().unwrap();
    let title = bin_dir.join("xcl_testtitle.exe");
    let dll = bin_dir.join("testgame_mw.dll");
    assert!(
        title.is_file() && dll.is_file(),
        "brak {} / {} – zbuduj najpierw: cargo build --workspace",
        title.display(),
        dll.display()
    );
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("..").join("..").join("target").join("e2e");
    let _ = std::fs::remove_dir_all(&root);
    let game = root.join("game");
    let data = root.join("data");
    copy_dir(
        &manifest
            .join("..")
            .join("..")
            .join("tests")
            .join("data")
            .join("testgame"),
        &game,
    );
    std::fs::copy(&title, game.join("xcl_testtitle.exe")).unwrap();
    std::fs::copy(&dll, game.join("testgame_mw.dll")).unwrap();
    let before = snapshot(&game);

    let out = Command::new(&xcl)
        .arg("run")
        .arg(&game)
        .arg("--")
        .arg("--hello")
        .env("XCL_DATA_ROOT", &data)
        .output()
        .expect("spawn xcl");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    println!("--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}");

    assert!(
        stdout.contains("konsola: importuje kernelx.dll") && !stdout.contains("UWAGA"),
        "launcher powinien rozpoznać tytuł konsolowy"
    );
    let fails: Vec<&str> = stdout.lines().filter(|l| l.starts_with("[FAIL]")).collect();
    assert!(
        fails.is_empty(),
        "nieudane sprawdzenia w tytule testowym:\n{}",
        fails.join("\n")
    );
    assert!(
        stdout.contains("xcl_testtitle: koniec, błędów: 0"),
        "tytuł testowy nie doszedł do końca"
    );
    assert!(out.status.success(), "kod wyjścia {:?}", out.status.code());

    // (2) the game folder is untouched
    assert_eq!(snapshot(&game), before, "FOLDER GRY ZOSTAŁ ZMIENIONY");

    // (3) overlay contents
    let tdata = data.join("XCL.TestTitle");
    let ov = tdata.join("overlay").join("data");
    let hello = std::fs::read_to_string(ov.join("hello.txt")).expect("overlay hello.txt");
    assert!(
        hello.starts_with("hello from the game folder") && hello.contains("+modified by title")
    );
    assert_eq!(std::fs::read(ov.join("renamed.bin")).unwrap(), b"save-data");
    assert!(!ov.join("new_save.bin").exists());
    let wo = std::fs::read_to_string(tdata.join("overlay").join(".xcl_whiteouts")).unwrap();
    assert!(
        wo.lines().any(|l| l == "data\\delete_me.txt"),
        "whiteouts: {wo}"
    );
    assert!(tdata.join("temp").join("scratch.tmp").is_file());
    assert!(tdata
        .join("saves")
        .join("test-scid")
        .join("slot0.sav")
        .is_file());

    // (4) report
    let report: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(tdata.join("logs").join("report.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(report["exit_code"], 0);
    let called = report["called_stubs"].as_array().unwrap();
    assert!(
        called
            .iter()
            .any(|s| s["function"] == "XclDefinitelyMissingFunction"),
        "report.called_stubs: {called:?}"
    );
    assert!(report["crash"].is_null(), "crash: {}", report["crash"]);

    // (5) shader capture: the PC DXBC and the fake console container were classified,
    //     dumped, and the RDNA2 code in the unknown part was disassembled
    let sh = &report["shaders"];
    assert!(
        sh["by_kind"]["dxbc"].as_u64().unwrap_or(0) >= 3,
        "shaders: {sh}"
    );
    assert_eq!(sh["by_kind"]["container-unknown"], 1, "shaders: {sh}");
    assert_eq!(sh["unknown_parts"]["XBOX"], 1, "shaders: {sh}");
    let dumps: Vec<String> = std::fs::read_dir(tdata.join("shaders"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let blobs = dumps
        .iter()
        .filter(|n| n.ends_with(".bin") && !n.contains(".fssm"))
        .count() as u64;
    assert_eq!(blobs, sh["unique"].as_u64().unwrap(), "{dumps:?}");
    // retail-format console shader: unpacked down to the FSSM, console-only v_mac_f32 decoded
    assert_eq!(sh["by_kind"]["xbox-native"], 1, "shaders: {sh}");
    assert_eq!(sh["xbox_hw_stages"]["PS"], 1, "shaders: {sh}");
    assert_eq!(sh["xbox_errors"], 0, "shaders: {sh}");
    assert_eq!(sh["rdna2_undecoded_dwords"], 0, "shaders: {sh}");
    let mnem: Vec<&str> = sh["rdna2_mnemonics"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m[0].as_str())
        .collect();
    assert!(
        mnem.contains(&"v_mac_f32") && mnem.contains(&"s_endpgm"),
        "rdna2_mnemonics: {mnem:?}"
    );
    assert_eq!(
        dumps.iter().filter(|n| n.ends_with(".fssm0.bin")).count(),
        1,
        "{dumps:?}"
    );
    // recompiled at capture: PS mode from the container, HLSL next to the dumps
    assert_eq!(
        (
            sh["recomp_programs"].as_u64(),
            sh["recomp_complete"].as_u64()
        ),
        (Some(1), Some(1)),
        "shaders: {sh}"
    );
    assert_eq!(sh["recomp_modes"]["PS"], 1, "shaders: {sh}");
    // console unified memory opened as a GPU heap on the real device
    let d3 = &report["d3d12"];
    assert!(d3["uma_mb"].as_u64().unwrap_or(0) >= 1, "d3d12: {d3}");
    assert!(
        d3["uma_gpu_va"]
            .as_str()
            .is_some_and(|v| v.starts_with("0x")),
        "d3d12: {d3}"
    );
    assert_eq!(
        dumps
            .iter()
            .filter(|n| n.ends_with(".fssm0.p0.hlsl"))
            .count(),
        1,
        "{dumps:?}"
    );

    // (5b) XAudio2: genuine Windows engine; XMA2 voice replaced, console flags dropped
    let a = &report["audio"];
    assert_eq!(a["engines"], 1, "audio: {a}");
    assert_eq!(a["engine_errors"], 0, "audio: {a}");
    assert!(
        a["dropped_flags"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "XAudio2Create:0x41"),
        "audio: {a}"
    );
    if a["mastering_voices"] == 1 {
        assert_eq!(a["xma_voices"], 1, "audio: {a}");
        assert_eq!(a["silent_voices"], 1, "audio: {a}");
        assert_eq!(a["source_voices"], 2, "audio: {a}");
        assert_eq!(a["buffers_silenced"], 1, "audio: {a}");
        assert_eq!(a["source_voice_errors"], 0, "audio: {a}");
    } else {
        eprintln!("e2e: brak urządzenia audio – głosy XAudio2 niesprawdzone ({a})");
    }

    // (6) real D3D12 backend: genuine GPU objects, console shader replaced by a stand-in
    let d = &report["d3d12"];
    assert_eq!(d["backend"], "real", "d3d12: {d}");
    assert!(
        d["fallback_pipelines"].as_u64().unwrap_or(0) >= 1,
        "d3d12: {d}"
    );
    let classes: Vec<&str> = d["classes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["interface"].as_str())
        .collect();
    assert!(
        classes.iter().any(|c| c.starts_with("ID3D12Device")),
        "classes: {classes:?}"
    );
    assert!(
        classes
            .iter()
            .any(|c| c.starts_with("ID3D12GraphicsCommandList")),
        "classes: {classes:?}"
    );
    let listings: Vec<String> = dumps
        .iter()
        .filter(|n| n.ends_with(".rdna2.txt"))
        .map(|n| std::fs::read_to_string(tdata.join("shaders").join(n)).unwrap())
        .collect();
    assert!(
        listings.iter().any(|t| t.contains("s_mov_b32 s0, s1")
            && t.contains("v_add_f32 v5, v1, v2")
            && t.contains("s_endpgm")),
        "{listings:?}"
    );
    assert!(
        listings
            .iter()
            .any(|t| t.contains("FSSM #0 [PS]") && t.contains("v_mac_f32 v6, s40, v21")),
        "{listings:?}"
    );
}
