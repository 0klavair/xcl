//! Extracting shader blobs from packed archives without a table of contents.
//!
//! RPF9 packs ("9FPR" magic) encrypt their TOC, but the shader packs inside store every
//! shader as a plain record (found empirically, 23 869 records in a 411 MB pack):
//!
//! ```text
//! u8 len | name[len-1] (identifier) | 0x00 | u8 flag | u32 csize | u32 unk | u32 csize | u32 usize | deflate-raw[csize]
//! ```
//!
//! The record payload inflates to a DXBC container. Names longer than 255 bytes do not
//! fit the length byte, so a few names come out truncated.

pub const RPF9_MAGIC: &[u8; 4] = b"9FPR";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub name: String,
    /// offset of the record (its length byte)
    pub offset: usize,
    pub flag: u8,
    /// offset and size of the raw deflate stream
    pub data: usize,
    pub csize: usize,
    pub usize: usize,
}

fn ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Find shader records in `d`, starting at `from`.
pub fn scan(d: &[u8], from: usize) -> Vec<Record> {
    let mut out = Vec::new();
    let mut i = from;
    while i + 32 < d.len() {
        let len = d[i] as usize;
        let p = i + len;
        if len >= 4
            && p + 18 <= d.len()
            && d[p] == 0
            && (d[i + 1].is_ascii_alphabetic() || d[i + 1] == b'_')
            && d[i + 2..p].iter().all(|&c| ident(c))
        {
            let (c1, c2, u) = (
                rd32(d, p + 2) as usize,
                rd32(d, p + 10) as usize,
                rd32(d, p + 14) as usize,
            );
            if c1 == c2
                && c1 > 0
                && c1 < 16 << 20
                && u > 0
                && u < 64 << 20
                && p + 18 + c1 <= d.len()
            {
                out.push(Record {
                    name: String::from_utf8_lossy(&d[i + 1..p]).into_owned(),
                    offset: i,
                    flag: d[p + 1],
                    data: p + 18,
                    csize: c1,
                    usize: u,
                });
                i = p + 18 + c1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Inflate a record. `None` when the stream is not valid deflate.
pub fn inflate(d: &[u8], r: &Record) -> Option<Vec<u8>> {
    miniz_oxide::inflate::decompress_to_vec_with_limit(&d[r.data..r.data + r.csize], r.usize).ok()
}

/// Stage prefix of a shader name (`PS_Foo` → `PS`).
pub fn name_stage(name: &str) -> Option<&'static str> {
    let up = name.get(..2)?.to_ascii_uppercase();
    ["PS", "VS", "MS", "GS", "CS", "HS", "DS", "AS"]
        .into_iter()
        .find(|s| *s == up)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, payload: &[u8]) -> Vec<u8> {
        let z = miniz_oxide::deflate::compress_to_vec(payload, 6);
        let mut v = vec![(name.len() + 1) as u8];
        v.extend_from_slice(name.as_bytes());
        v.push(0);
        v.push(50);
        for x in [
            z.len() as u32,
            0xddce7,
            z.len() as u32,
            payload.len() as u32,
        ] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v.extend_from_slice(&z);
        v
    }

    #[test]
    fn finds_records_between_noise() {
        let mut pack = b"9FPR".to_vec();
        pack.resize(0x40, 0xAA);
        let a = crate::gpu::xbox::tests::sample(
            0,
            &[crate::gpu::xbox::tests::fssm(
                &crate::gpu::xbox::tests::CODE,
                &[(5, 1)],
            )],
        );
        pack.extend(record("PS_Test_1234abcd_Wrapped", &a));
        pack.extend_from_slice(&[0x13, 0x37, 0, 1, 2, 3]);
        pack.extend(record("VS_Other", b"DXBC-not-really"));
        pack.extend_from_slice(&[0u8; 40]);
        let recs = scan(&pack, 0x40);
        assert_eq!(
            recs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["PS_Test_1234abcd_Wrapped", "VS_Other"]
        );
        assert_eq!(inflate(&pack, &recs[0]).unwrap(), a);
        assert_eq!(recs[0].flag, 50);
        assert_eq!(name_stage(&recs[0].name), Some("PS"));
        assert_eq!(name_stage("ptfx_x"), None);
    }
}
