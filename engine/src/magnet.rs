//! Magnet URI parsing (BEP 9).

use anyhow::{bail, Context, Result};
use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct Magnet {
    pub info_hash: [u8; 20],
    pub name: Option<String>,
    pub trackers: Vec<String>,
    /// Peers given directly via `x.pe`.
    pub peers: Vec<SocketAddr>,
}

pub fn parse(uri: &str) -> Result<Magnet> {
    let query = uri
        .trim()
        .strip_prefix("magnet:?")
        .context("not a magnet link")?;

    let mut info_hash = None;
    let mut name = None;
    let mut trackers = Vec::new();
    let mut peers = Vec::new();

    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode(value);
        // Some clients number repeated keys: tr.1, tr.2, ...
        let key = key.split('.').next().unwrap_or(key);
        match key {
            "xt" => {
                if let Some(h) = value.strip_prefix("urn:btih:") {
                    info_hash = Some(decode_hash(h)?);
                }
            }
            "dn" => name = Some(value),
            "tr" => trackers.push(value),
            "x" if pair.starts_with("x.pe=") => {
                if let Ok(addr) = value.parse() {
                    peers.push(addr);
                }
            }
            _ => {}
        }
    }

    crate::metainfo::dedup(&mut trackers);
    Ok(Magnet {
        info_hash: info_hash.context("magnet link has no BitTorrent info hash")?,
        name,
        trackers,
        peers,
    })
}

fn decode_hash(s: &str) -> Result<[u8; 20]> {
    if !s.is_ascii() {
        bail!("invalid info hash");
    }
    let bytes = match s.len() {
        40 => (0..40)
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16))
            .collect::<Result<Vec<_>, _>>()
            .context("invalid hex info hash")?,
        32 => base32(s)?,
        _ => bail!("unsupported info hash length"),
    };
    Ok(bytes.try_into().unwrap())
}

fn base32(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(20);
    let (mut buf, mut bits) = (0u64, 0);
    for c in s.bytes() {
        let v = match c.to_ascii_uppercase() {
            c @ b'A'..=b'Z' => c - b'A',
            c @ b'2'..=b'7' => c - b'2' + 26,
            _ => bail!("invalid base32 info hash"),
        };
        buf = (buf << 5) | v as u64;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 3 <= b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_magnet() {
        let m = parse("magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a&dn=My+File&tr=udp%3A%2F%2Ftracker.example%3A80&tr=http%3A%2F%2Fa%2Fannounce&x.pe=127.0.0.1:6881").unwrap();
        assert_eq!(m.info_hash[0], 0xc1);
        assert_eq!(m.name.as_deref(), Some("My File"));
        assert_eq!(m.trackers, vec!["udp://tracker.example:80", "http://a/announce"]);
        assert_eq!(m.peers, vec!["127.0.0.1:6881".parse().unwrap()]);
    }

    #[test]
    fn parses_base32_magnet() {
        let hex = parse("magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a").unwrap();
        let b32 = parse("magnet:?xt=urn:btih:YEX6DQDLXISUVHOJ6UM3GNNKPQJWPKEK").unwrap();
        assert_eq!(hex.info_hash, b32.info_hash);
    }

    #[test]
    fn rejects_non_ascii_hash() {
        assert!(parse("magnet:?xt=urn:btih:a%C3%A9000000000000000000000000000000000000").is_err());
    }

    #[test]
    fn percent_edge_cases() {
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("a%41"), "aA");
        assert_eq!(percent_decode("%zz"), "%zz");
    }
}
