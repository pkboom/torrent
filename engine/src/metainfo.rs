//! `.torrent` metainfo parsing.

use crate::bencode::{self, Value};
use anyhow::{bail, Context, Result};
use sha1::{Digest, Sha1};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Path relative to the download directory (includes the torrent's folder for multi-file torrents).
    pub path: PathBuf,
    pub length: u64,
    /// Byte offset of this file within the concatenated torrent data.
    pub offset: u64,
    /// BEP 47 padding file: occupies piece space but is never written to disk.
    pub pad: bool,
}

#[derive(Debug, Clone)]
pub struct Metainfo {
    pub info_hash: [u8; 20],
    pub name: String,
    pub piece_length: u64,
    pub pieces: Vec<[u8; 20]>,
    pub files: Vec<FileEntry>,
    pub total_length: u64,
    pub trackers: Vec<String>,
    pub info_bytes: Vec<u8>,
}

impl Metainfo {
    pub fn from_torrent_bytes(data: &[u8]) -> Result<Self> {
        let root = bencode::decode(data).context("invalid torrent file")?;
        let (start, end) = bencode::dict_value_span(data, b"info")?
            .context("torrent file has no info dictionary")?;

        let mut trackers = Vec::new();
        if let Some(tiers) = root.get("announce-list").and_then(Value::as_list) {
            for tier in tiers {
                for url in tier.as_list().unwrap_or_default() {
                    if let Some(u) = url.as_str() {
                        trackers.push(u.to_string());
                    }
                }
            }
        }
        if let Some(u) = root.get("announce").and_then(Value::as_str) {
            trackers.push(u.to_string());
        }
        dedup(&mut trackers);

        Self::from_info_bytes(&data[start..end], trackers)
    }

    pub fn from_info_bytes(info_bytes: &[u8], trackers: Vec<String>) -> Result<Self> {
        let info = bencode::decode(info_bytes).context("invalid info dictionary")?;
        let info_hash: [u8; 20] = Sha1::digest(info_bytes).into();

        let name = info
            .get("name.utf-8")
            .or_else(|| info.get("name"))
            .and_then(Value::as_bytes)
            .map(|b| sanitize_component(&String::from_utf8_lossy(b)))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| hex(&info_hash));

        let piece_length = info
            .get("piece length")
            .and_then(Value::as_int)
            .filter(|&n| n > 0 && n <= MAX_PIECE_LENGTH)
            .context("missing or unsupported piece length")? as u64;

        let raw_pieces = info
            .get("pieces")
            .and_then(Value::as_bytes)
            .context("missing pieces")?;
        if raw_pieces.len() % 20 != 0 {
            bail!("pieces field is not a multiple of 20 bytes");
        }
        let pieces: Vec<[u8; 20]> = raw_pieces
            .chunks_exact(20)
            .map(|c| c.try_into().unwrap())
            .collect();

        let mut files = Vec::new();
        let mut offset = 0u64;
        if let Some(list) = info.get("files").and_then(Value::as_list) {
            for f in list {
                let length = f
                    .get("length")
                    .and_then(Value::as_int)
                    .filter(|&n| n >= 0)
                    .context("file without length")? as u64;
                let parts = f
                    .get("path.utf-8")
                    .or_else(|| f.get("path"))
                    .and_then(Value::as_list)
                    .context("file without path")?;
                let mut path = PathBuf::from(&name);
                for p in parts {
                    let c = sanitize_component(&String::from_utf8_lossy(
                        p.as_bytes().context("bad path component")?,
                    ));
                    if !c.is_empty() {
                        path.push(c);
                    }
                }
                let pad = f
                    .get("attr")
                    .and_then(Value::as_str)
                    .is_some_and(|a| a.contains('p'));
                files.push(FileEntry { path, length, offset, pad });
                offset = offset.checked_add(length).filter(|&o| o <= MAX_TOTAL).context("torrent is too large")?;
            }
            if files.is_empty() {
                bail!("torrent has an empty file list");
            }
        } else {
            let length = info
                .get("length")
                .and_then(Value::as_int)
                .filter(|&n| n >= 0 && n as u64 <= MAX_TOTAL)
                .context("missing or invalid length")? as u64;
            files.push(FileEntry { path: PathBuf::from(&name), length, offset: 0, pad: false });
            offset = length;
        }
        let total_length = offset;

        let expected = total_length.div_ceil(piece_length);
        if expected != pieces.len() as u64 {
            bail!("piece count {} does not match total size (expected {expected})", pieces.len());
        }
        if pieces.len() > u32::MAX as usize {
            bail!("too many pieces");
        }

        Ok(Metainfo {
            info_hash,
            name,
            piece_length,
            pieces,
            files,
            total_length,
            trackers,
            info_bytes: info_bytes.to_vec(),
        })
    }

    pub fn num_pieces(&self) -> u32 {
        self.pieces.len() as u32
    }

    pub fn piece_size(&self, index: u32) -> u64 {
        let start = index as u64 * self.piece_length;
        (self.total_length - start).min(self.piece_length)
    }

    /// Serializes back into a `.torrent` file (used to persist magnet downloads).
    pub fn to_torrent_bytes(&self) -> Vec<u8> {
        let mut out = b"d".to_vec();
        if let Some(first) = self.trackers.first() {
            out.extend_from_slice(&bencode::encode(&Value::bytes("announce")));
            out.extend_from_slice(&bencode::encode(&Value::bytes(first.as_bytes())));
            let tiers = Value::List(
                self.trackers
                    .iter()
                    .map(|t| Value::List(vec![Value::bytes(t.as_bytes())]))
                    .collect(),
            );
            out.extend_from_slice(&bencode::encode(&Value::bytes("announce-list")));
            out.extend_from_slice(&bencode::encode(&tiers));
        }
        out.extend_from_slice(b"4:info");
        out.extend_from_slice(&self.info_bytes);
        out.push(b'e');
        out
    }

    /// The top-level path this torrent creates inside the download directory.
    pub fn root_path(&self, download_dir: &Path) -> PathBuf {
        download_dir.join(&self.name)
    }
}

const MAX_PIECE_LENGTH: i64 = 128 * 1024 * 1024;
const MAX_TOTAL: u64 = 1 << 50;

/// Strips anything that could escape the download directory.
fn sanitize_component(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c == '/' || c == '\\' || c == '\0' || c.is_control() { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    match Path::new(trimmed).components().next() {
        Some(Component::Normal(_)) if trimmed != "." && trimmed != ".." => trimmed.to_string(),
        _ => String::new(),
    }
}

pub(crate) fn dedup(v: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|x| seen.insert(x.clone()));
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(info: Value) -> Vec<u8> {
        bencode::encode(&Value::dict([
            ("announce", Value::bytes("http://t/announce")),
            ("info", info),
        ]))
    }

    #[test]
    fn single_file() {
        let data = make(Value::dict([
            ("name", Value::bytes("a.txt")),
            ("length", Value::Int(5)),
            ("piece length", Value::Int(4)),
            ("pieces", Value::bytes(vec![0u8; 40])),
        ]));
        let m = Metainfo::from_torrent_bytes(&data).unwrap();
        assert_eq!(m.total_length, 5);
        assert_eq!(m.piece_size(1), 1);
        assert_eq!(m.trackers, vec!["http://t/announce"]);
        let again = Metainfo::from_torrent_bytes(&m.to_torrent_bytes()).unwrap();
        assert_eq!(again.info_hash, m.info_hash);
    }

    #[test]
    fn path_traversal_is_neutralized() {
        let data = make(Value::dict([
            ("name", Value::bytes("..")),
            ("piece length", Value::Int(4)),
            ("pieces", Value::bytes(vec![0u8; 20])),
            (
                "files",
                Value::List(vec![Value::dict([
                    ("length", Value::Int(3)),
                    (
                        "path",
                        Value::List(vec![Value::bytes(".."), Value::bytes("/etc/passwd")]),
                    ),
                ])]),
            ),
        ]));
        let m = Metainfo::from_torrent_bytes(&data).unwrap();
        let p = &m.files[0].path;
        assert!(p.components().all(|c| matches!(c, Component::Normal(_))), "{p:?}");
        assert!(p.ends_with("_etc_passwd"));
    }
}
