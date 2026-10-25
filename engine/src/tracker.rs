//! HTTP (BEP 3) and UDP (BEP 15) tracker announces.

use crate::bencode::{self, Value};
use anyhow::{bail, Context, Result};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use tokio::net::{lookup_host, UdpSocket};
use tokio::time::timeout;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    None,
    Started,
    Completed,
    Stopped,
}

#[derive(Clone, Copy)]
pub struct AnnounceRequest {
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub port: u16,
    pub downloaded: u64,
    pub left: u64,
    pub event: Event,
}

#[derive(Debug, Default)]
pub struct AnnounceResponse {
    pub interval: Option<Duration>,
    pub peers: Vec<SocketAddr>,
    /// Swarm size as reported by the tracker.
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
}

pub async fn announce(url: &str, req: &AnnounceRequest) -> Result<AnnounceResponse> {
    if url.starts_with("http://") || url.starts_with("https://") {
        announce_http(url, req).await
    } else if let Some(rest) = url.strip_prefix("udp://") {
        announce_udp(rest, req).await
    } else {
        bail!("unsupported tracker scheme: {url}")
    }
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("RustTorrent/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("http client")
    })
}

async fn announce_http(url: &str, req: &AnnounceRequest) -> Result<AnnounceResponse> {
    let sep = if url.contains('?') { '&' } else { '?' };
    let event = match req.event {
        Event::Started => "&event=started",
        Event::Completed => "&event=completed",
        Event::Stopped => "&event=stopped",
        Event::None => "",
    };
    let full = format!(
        "{url}{sep}info_hash={}&peer_id={}&port={}&uploaded=0&downloaded={}&left={}&compact=1&numwant=200{event}",
        url_encode(&req.info_hash),
        url_encode(&req.peer_id),
        req.port,
        req.downloaded,
        req.left,
    );
    let mut resp = http_client().get(full).send().await?.error_for_status()?;
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        body.extend_from_slice(&chunk);
        if body.len() > 1 << 20 {
            bail!("tracker response too large");
        }
    }
    let resp = bencode::decode(&body).context("tracker sent invalid response")?;
    if let Some(reason) = resp.get("failure reason").and_then(Value::as_bytes) {
        bail!("tracker error: {}", String::from_utf8_lossy(reason));
    }

    let mut peers = Vec::new();
    match resp.get("peers") {
        Some(Value::Bytes(b)) => peers.extend(parse_compact_v4(b)),
        Some(Value::List(list)) => {
            for p in list {
                let ip = p.get("ip").and_then(Value::as_str).and_then(|s| s.parse().ok());
                let port = p.get("port").and_then(Value::as_int);
                if let (Some(ip), Some(port)) = (ip, port) {
                    peers.push(SocketAddr::new(ip, port as u16));
                }
            }
        }
        _ => {}
    }
    if let Some(b) = resp.get("peers6").and_then(Value::as_bytes) {
        peers.extend(parse_compact_v6(b));
    }
    let count = |k: &str| resp.get(k).and_then(Value::as_int).and_then(|n| u32::try_from(n).ok());
    Ok(AnnounceResponse {
        seeders: count("complete"),
        leechers: count("incomplete"),
        interval: resp
            .get("interval")
            .and_then(Value::as_int)
            .map(|s| Duration::from_secs(s.clamp(60, 3600) as u64)),
        peers,
    })
}

async fn announce_udp(host_and_path: &str, req: &AnnounceRequest) -> Result<AnnounceResponse> {
    let host = host_and_path.split('/').next().unwrap_or(host_and_path);
    let addr = lookup_host(host)
        .await?
        .find(SocketAddr::is_ipv4)
        .context("could not resolve tracker")?;
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    sock.connect(addr).await?;

    let mut buf = [0u8; 4096];
    let mut attempt = 0;
    loop {
        attempt += 1;
        match udp_exchange(&sock, req, &mut buf).await {
            Ok(r) => return Ok(r),
            Err(e) if attempt >= 2 => return Err(e),
            Err(_) => continue,
        }
    }
}

async fn udp_exchange(sock: &UdpSocket, req: &AnnounceRequest, buf: &mut [u8]) -> Result<AnnounceResponse> {
    const WAIT: Duration = Duration::from_secs(5);

    let tid: u32 = rand::random();
    let mut connect = Vec::with_capacity(16);
    connect.extend_from_slice(&0x41727101980u64.to_be_bytes());
    connect.extend_from_slice(&0u32.to_be_bytes());
    connect.extend_from_slice(&tid.to_be_bytes());
    sock.send(&connect).await?;
    let n = timeout(WAIT, sock.recv(buf)).await.context("udp tracker timed out")??;
    if n < 16 || u32_at(buf, 0) != 0 || u32_at(buf, 4) != tid {
        bail!("bad udp connect response");
    }
    let conn_id = &buf[8..16].to_vec();

    let tid: u32 = rand::random();
    let event = match req.event {
        Event::None => 0u32,
        Event::Completed => 1,
        Event::Started => 2,
        Event::Stopped => 3,
    };
    let mut ann = Vec::with_capacity(98);
    ann.extend_from_slice(conn_id);
    ann.extend_from_slice(&1u32.to_be_bytes());
    ann.extend_from_slice(&tid.to_be_bytes());
    ann.extend_from_slice(&req.info_hash);
    ann.extend_from_slice(&req.peer_id);
    ann.extend_from_slice(&req.downloaded.to_be_bytes());
    ann.extend_from_slice(&req.left.to_be_bytes());
    ann.extend_from_slice(&0u64.to_be_bytes()); // uploaded
    ann.extend_from_slice(&event.to_be_bytes());
    ann.extend_from_slice(&0u32.to_be_bytes()); // ip
    ann.extend_from_slice(&rand::random::<u32>().to_be_bytes()); // key
    ann.extend_from_slice(&(-1i32).to_be_bytes()); // num_want
    ann.extend_from_slice(&req.port.to_be_bytes());
    sock.send(&ann).await?;
    let n = timeout(WAIT, sock.recv(buf)).await.context("udp tracker timed out")??;
    if n < 20 || u32_at(buf, 4) != tid {
        bail!("bad udp announce response");
    }
    if u32_at(buf, 0) == 3 {
        bail!("tracker error: {}", String::from_utf8_lossy(&buf[8..n]));
    }
    if u32_at(buf, 0) != 1 {
        bail!("unexpected udp tracker action");
    }
    Ok(AnnounceResponse {
        seeders: Some(u32_at(buf, 16)),
        leechers: Some(u32_at(buf, 12)),
        interval: Some(Duration::from_secs(u32_at(buf, 8).clamp(60, 3600) as u64)),
        peers: parse_compact_v4(&buf[20..n]),
    })
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes(b[i..i + 4].try_into().unwrap())
}

pub fn parse_compact_v4(b: &[u8]) -> Vec<SocketAddr> {
    b.chunks_exact(6)
        .map(|c| {
            let ip = Ipv4Addr::new(c[0], c[1], c[2], c[3]);
            SocketAddr::new(ip.into(), u16::from_be_bytes([c[4], c[5]]))
        })
        .filter(|a| a.port() != 0)
        .collect()
}

pub fn parse_compact_v6(b: &[u8]) -> Vec<SocketAddr> {
    b.chunks_exact(18)
        .map(|c| {
            let ip: [u8; 16] = c[..16].try_into().unwrap();
            SocketAddr::new(Ipv6Addr::from(ip).into(), u16::from_be_bytes([c[16], c[17]]))
        })
        .filter(|a| a.port() != 0)
        .collect()
}

fn url_encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
