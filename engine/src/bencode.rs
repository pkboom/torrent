//! Bencode encoding and decoding (BEP 3).

use anyhow::{bail, Result};
use std::collections::BTreeMap;

const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(BTreeMap<Vec<u8>, Value>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_dict()?.get(key.as_bytes())
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(self.as_bytes()?).ok()
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, Value>> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    pub fn dict<const N: usize>(entries: [(&str, Value); N]) -> Value {
        Value::Dict(
            entries
                .into_iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), v))
                .collect(),
        )
    }

    pub fn bytes(b: impl Into<Vec<u8>>) -> Value {
        Value::Bytes(b.into())
    }
}

/// Decodes a complete bencoded value, rejecting trailing bytes.
pub fn decode(data: &[u8]) -> Result<Value> {
    let (v, used) = decode_prefix(data)?;
    if used != data.len() {
        bail!("trailing data after bencoded value");
    }
    Ok(v)
}

/// Decodes one value from the start of `data`, returning it and the number of bytes consumed.
pub fn decode_prefix(data: &[u8]) -> Result<(Value, usize)> {
    let mut pos = 0;
    let v = parse(data, &mut pos, 0)?;
    Ok((v, pos))
}

/// Returns the byte range of `key`'s value inside a top-level dictionary.
/// Used to hash the raw `info` dictionary exactly as it appears on disk.
pub fn dict_value_span(data: &[u8], key: &[u8]) -> Result<Option<(usize, usize)>> {
    if data.first() != Some(&b'd') {
        bail!("not a dictionary");
    }
    let mut pos = 1;
    let mut found = None;
    while data.get(pos) != Some(&b'e') {
        let k = parse_bytes(data, &mut pos)?;
        let start = pos;
        parse(data, &mut pos, 1)?;
        if k == key {
            found = Some((start, pos));
        }
    }
    Ok(found)
}

fn parse(data: &[u8], pos: &mut usize, depth: usize) -> Result<Value> {
    if depth > MAX_DEPTH {
        bail!("bencode nested too deeply");
    }
    match data.get(*pos) {
        Some(b'i') => {
            *pos += 1;
            let end = find(data, *pos, b'e')?;
            let s = std::str::from_utf8(&data[*pos..end])?;
            let n: i64 = s.parse()?;
            *pos = end + 1;
            Ok(Value::Int(n))
        }
        Some(b'l') => {
            *pos += 1;
            let mut list = Vec::new();
            loop {
                match data.get(*pos) {
                    Some(b'e') => {
                        *pos += 1;
                        return Ok(Value::List(list));
                    }
                    Some(_) => list.push(parse(data, pos, depth + 1)?),
                    None => bail!("unterminated list"),
                }
            }
        }
        Some(b'd') => {
            *pos += 1;
            let mut dict = BTreeMap::new();
            loop {
                match data.get(*pos) {
                    Some(b'e') => {
                        *pos += 1;
                        return Ok(Value::Dict(dict));
                    }
                    Some(_) => {
                        let k = parse_bytes(data, pos)?;
                        let v = parse(data, pos, depth + 1)?;
                        dict.insert(k, v);
                    }
                    None => bail!("unterminated dictionary"),
                }
            }
        }
        Some(b'0'..=b'9') => Ok(Value::Bytes(parse_bytes(data, pos)?)),
        Some(c) => bail!("unexpected byte {:?} in bencode", *c as char),
        None => bail!("unexpected end of bencode"),
    }
}

fn parse_bytes(data: &[u8], pos: &mut usize) -> Result<Vec<u8>> {
    let colon = find(data, *pos, b':')?;
    let len: usize = std::str::from_utf8(&data[*pos..colon])?.parse()?;
    let start = colon + 1;
    let end = start
        .checked_add(len)
        .filter(|&e| e <= data.len())
        .ok_or_else(|| anyhow::anyhow!("byte string runs past end of data"))?;
    *pos = end;
    Ok(data[start..end].to_vec())
}

fn find(data: &[u8], from: usize, byte: u8) -> Result<usize> {
    data.get(from..)
        .and_then(|rest| rest.iter().position(|&b| b == byte))
        .map(|i| from + i)
        .ok_or_else(|| anyhow::anyhow!("malformed bencode"))
}

pub fn encode(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(v, &mut out);
    out
}

fn encode_into(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Int(i) => out.extend_from_slice(format!("i{i}e").as_bytes()),
        Value::Bytes(b) => {
            out.extend_from_slice(format!("{}:", b.len()).as_bytes());
            out.extend_from_slice(b);
        }
        Value::List(l) => {
            out.push(b'l');
            for item in l {
                encode_into(item, out);
            }
            out.push(b'e');
        }
        Value::Dict(d) => {
            out.push(b'd');
            for (k, v) in d {
                out.extend_from_slice(format!("{}:", k.len()).as_bytes());
                out.extend_from_slice(k);
                encode_into(v, out);
            }
            out.push(b'e');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let raw = b"d3:bar4:spam3:fooi42e4:listli-1e0:ee";
        let v = decode(raw).unwrap();
        assert_eq!(v.get("foo").unwrap().as_int(), Some(42));
        assert_eq!(v.get("bar").unwrap().as_str(), Some("spam"));
        assert_eq!(encode(&v), raw);
    }

    #[test]
    fn span() {
        let raw = b"d8:announce3:url4:infod4:name1:xee";
        let (s, e) = dict_value_span(raw, b"info").unwrap().unwrap();
        assert_eq!(&raw[s..e], b"d4:name1:xe");
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(b"5:abc").is_err());
        assert!(decode(b"i12").is_err());
        assert!(decode(b"d3:fooe").is_err());
        assert!(decode(b"99999999999999999999:x").is_err());
    }
}
