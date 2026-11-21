//! Per-title configuration (`%LOCALAPPDATA%\XCL\<title>\xcl.toml`) and the boot record
//! the launcher hands to the host process.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// "anaconda" (Series X) or "lockhart" (Series S).
    pub device_type: String,
    /// Raw value returned by kernelx `GetConsoleType`. Series values are not public;
    /// this is a guess and can be overridden.
    pub console_type_raw: u32,
    pub gamertag: String,
    pub user_id: u64,
    pub console_id: String,
    pub sandbox_id: String,
    /// Title memory budget reported by TitleMemoryStatus (MiB).
    pub title_memory_mb: u64,
    pub log_level: String,
    pub log_to_stderr: bool,
    /// Executable inside the game folder (auto-detected when empty).
    pub exe: String,
    pub package_identifier: String,
    /// Console path prefix → target. `{game}` and `{data}` are expanded.
    pub mounts: BTreeMap<String, String>,
    /// Import DLL pattern → route: an HLE module ("kernelx.dll"), "@system" or "@stub".
    pub aliases: BTreeMap<String, String>,
    /// "dll!function" (or "*!function") → return value for auto-stubs.
    pub stub_returns: BTreeMap<String, String>,
    /// "d3d12" (real GPU, default) or "null" (no rendering).
    pub graphics: String,
    /// "auto" (high-performance GPU, then WARP) or "warp".
    pub d3d12_adapter: String,
    /// Enable the D3D12 debug layer (needs Graphics Tools).
    pub d3d12_debug: bool,
    /// "auto" (show the title's frames in an XCL window) or "off".
    pub present: String,
    pub window_width: u32,
    pub window_height: u32,
}

impl Default for Config {
    fn default() -> Self {
        let mut mounts = BTreeMap::new();
        mounts.insert("GAME:".into(), "{game}".into());
        mounts.insert("G:".into(), "{game}".into());
        mounts.insert("T:".into(), "{data}\\temp".into());
        Config {
            device_type: "anaconda".into(),
            console_type_raw: 6,
            gamertag: "Player1".into(),
            user_id: 0x0009_0000_0000_0001,
            console_id: "XCL00000000000000000000000000000000000001".into(),
            sandbox_id: "RETAIL".into(),
            title_memory_mb: 13_824,
            log_level: "info".into(),
            log_to_stderr: true,
            exe: String::new(),
            package_identifier: String::new(),
            mounts,
            aliases: BTreeMap::new(),
            stub_returns: BTreeMap::new(),
            graphics: "d3d12".into(),
            d3d12_adapter: "auto".into(),
            d3d12_debug: false,
            present: "auto".into(),
            window_width: 1280,
            window_height: 720,
        }
    }
}

const HEADER: &str = "# XCL – konfiguracja tytułu. Plik tworzony automatycznie; można go edytować.\n\
# device_type: \"anaconda\" (Series X) lub \"lockhart\" (Series S)\n\
# mounts: prefiks ścieżki konsolowej -> folder ({game} = folder gry, tylko odczyt; {data} = dane XCL)\n\
# aliases: wzorzec DLL -> \"kernelx.dll\" / \"xgameruntime.dll\" / \"d3d12_xs.dll\" / \"@system\" / \"@stub\"\n\
# stub_returns: \"dll!Funkcja\" -> wartość zwracana przez zaślepkę (np. \"E_NOTIMPL\", \"0\", \"0x1\")\n\n";

impl Config {
    pub fn load_or_create(path: &Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).map_err(|e| format!("{}: {e}", path.display())),
            Err(_) => {
                let c = Config::default();
                let body = toml::to_string_pretty(&c).map_err(|e| e.to_string())?;
                if let Some(d) = path.parent() {
                    let _ = std::fs::create_dir_all(d);
                }
                let _ = std::fs::write(path, format!("{HEADER}{body}"));
                Ok(c)
            }
        }
    }

    pub fn is_series_s(&self) -> bool {
        self.device_type.eq_ignore_ascii_case("lockhart")
            || self.device_type.eq_ignore_ascii_case("series s")
    }
}

/// Everything the host process needs; written by the launcher as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Boot {
    pub title: String,
    pub exe_path: PathBuf,
    pub game_root: PathBuf,
    pub data_dir: PathBuf,
    pub config: Config,
}

impl Boot {
    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.data_dir.join("cache")
    }
    pub fn report_path(&self) -> PathBuf {
        self.logs_dir().join("report.json")
    }
    pub fn expand(&self, s: &str) -> String {
        s.replace("{game}", &self.game_root.to_string_lossy())
            .replace("{data}", &self.data_dir.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_roundtrips_through_toml() {
        let c = Config::default();
        let s = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.mounts.get("G:").map(String::as_str), Some("{game}"));
        assert_eq!(back.title_memory_mb, 13_824);
    }

    #[test]
    fn partial_file_uses_defaults() {
        let c: Config = toml::from_str("device_type = \"lockhart\"\n").unwrap();
        assert!(c.is_series_s());
        assert_eq!(c.gamertag, "Player1");
    }
}
