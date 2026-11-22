//! The few `MicrosoftGame.Config` settings XCL uses (game manifest in the game folder):
//! the persistent local storage quota and the title memory mode (the title sees the PC's
//! processors, so SMT does not matter). Plain text search –
//! the file is small XML and only these elements matter.

use std::path::Path;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// `<PersistentLocalStorage><SizeMB>`
    pub pls_mb: Option<u64>,
    /// `<XboxAnacondaTitleMemory>` / `<XboxLockhartTitleMemory>`: "Standard" or "Advanced"
    pub anaconda_memory: Option<String>,
    pub lockhart_memory: Option<String>,
    /// `<TitleId>` (hex)
    pub title_id: Option<String>,
}

fn element<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let at = xml.find(&open)? + open.len();
    let end = xml[at..].find(&format!("</{name}>"))?;
    Some(xml[at..at + end].trim())
}

pub fn parse(xml: &str) -> Manifest {
    let pls = element(xml, "PersistentLocalStorage").and_then(|section| element(section, "SizeMB"));
    Manifest {
        pls_mb: pls.and_then(|v| v.parse().ok()),
        anaconda_memory: element(xml, "XboxAnacondaTitleMemory").map(str::to_string),
        lockhart_memory: element(xml, "XboxLockhartTitleMemory").map(str::to_string),
        title_id: element(xml, "TitleId").map(str::to_string),
    }
}

/// `MicrosoftGame.Config` of a game folder (None: no manifest).
pub fn load(game_root: &Path) -> Option<Manifest> {
    let p = std::fs::read_dir(game_root).ok()?.flatten().map(|e| e.path()).find(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("MicrosoftGame.Config")))?;
    let raw = std::fs::read(p).ok()?;
    // UTF-8 or UTF-16 LE (with BOM)
    let text = if raw.starts_with(&[0xFF, 0xFE]) {
        String::from_utf16_lossy(&raw[2..].chunks(2).map(|c| u16::from_le_bytes([c[0], *c.get(1).unwrap_or(&0)])).collect::<Vec<_>>())
    } else {
        String::from_utf8_lossy(&raw).into_owned()
    };
    Some(parse(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_settings_xcl_uses() {
        let xml = r#"<Game configVersion="1">
  <PersistentLocalStorage>
    <SizeMB>2048</SizeMB>
  </PersistentLocalStorage>
  <VirtualMachine>
    <SMTEnabled>true</SMTEnabled>
    <XboxLockhartTitleMemory>Advanced</XboxLockhartTitleMemory>
    <XboxAnacondaTitleMemory>Advanced</XboxAnacondaTitleMemory>
  </VirtualMachine>
</Game>"#;
        let m = parse(xml);
        assert_eq!(m.pls_mb, Some(2048));
        assert_eq!((m.anaconda_memory.as_deref(), m.lockhart_memory.as_deref()), (Some("Advanced"), Some("Advanced")));
        assert_eq!(parse("<Game/>"), Manifest::default());
    }

    #[test]
    fn does_not_take_storage_size_from_another_section() {
        let xml = r#"<Game>
  <PersistentLocalStorage></PersistentLocalStorage>
  <Other><SizeMB>512</SizeMB></Other>
</Game>"#;
        assert_eq!(parse(xml).pls_mb, None);
    }
}
