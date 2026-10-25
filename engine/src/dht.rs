//! Minimal mainline DHT client (BEP 5): an iterative `get_peers` lookup.
//! Download-only, so we never announce ourselves or answer queries.

use crate::bencode::{self, Value};
use crate::tracker::parse_compact_v4;
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::net::{lookup_host, UdpSocket};
use tokio::sync::mpsc;

const BOOTSTRAP: &[&str] = &[
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "router.utorrent.com:6881",
    "dht.libtorrent.org:25401",
];
const ALPHA: usize = 8;
const LOOKUP_TIME: Duration = Duration::from_secs(30);

/// Runs one lookup for `info_hash`, sending peers to `out` as they are found.
pub async fn get_peers(info_hash: [u8; 20], out: mpsc::Sender<Vec<SocketAddr>>) {
    if let Err(e) = lookup(info_hash, out).await {
        log::debug!("dht lookup failed: {e}");
    }
}

async fn lookup(target: [u8; 20], out: mpsc::Sender<Vec<SocketAddr>>) -> anyhow::Result<()> {
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    let my_id: [u8; 20] = rand::random();

    // node addr -> distance to target (unknown for bootstrap nodes)
    let mut candidates: HashMap<SocketAddr, [u8; 20]> = HashMap::new();
    for host in BOOTSTRAP {
        if let Ok(Ok(addrs)) = tokio::time::timeout(Duration::from_secs(5), lookup_host(*host)).await {
            for a in addrs.filter(SocketAddr::is_ipv4) {
                candidates.insert(a, [0; 20]);
            }
        }
    }
    let mut queried: HashSet<SocketAddr> = HashSet::new();
    let mut seen_peers: HashSet<SocketAddr> = HashSet::new();
    let mut buf = vec![0u8; 2048];
    let deadline = Instant::now() + LOOKUP_TIME;
    let mut tid: u16 = 0;

    while Instant::now() < deadline && !out.is_closed() {
        let mut next: Vec<(&SocketAddr, &[u8; 20])> = candidates
            .iter()
            .filter(|(a, _)| !queried.contains(*a))
            .collect();
        next.sort_by_key(|(_, d)| **d);
        let batch: Vec<SocketAddr> = next.into_iter().take(ALPHA).map(|(a, _)| *a).collect();
        if batch.is_empty() && queried.len() > 0 {
            break;
        }
        for addr in &batch {
            tid = tid.wrapping_add(1);
            let q = bencode::encode(&Value::dict([
                ("t", Value::bytes(tid.to_be_bytes().to_vec())),
                ("y", Value::bytes("q")),
                ("q", Value::bytes("get_peers")),
                (
                    "a",
                    Value::dict([
                        ("id", Value::bytes(my_id.to_vec())),
                        ("info_hash", Value::bytes(target.to_vec())),
                    ]),
                ),
            ]));
            let _ = sock.send_to(&q, addr).await;
            queried.insert(*addr);
        }

        // Collect replies for a short window, then send the next wave.
        let wave_end = Instant::now() + Duration::from_millis(800);
        while let Ok(Ok((n, _from))) = tokio::time::timeout_at(wave_end.into(), sock.recv_from(&mut buf)).await {
            let Ok(msg) = bencode::decode(&buf[..n]) else { continue };
            let Some(r) = msg.get("r") else { continue };
            if let Some(values) = r.get("values").and_then(Value::as_list) {
                let fresh: Vec<SocketAddr> = values
                    .iter()
                    .filter_map(Value::as_bytes)
                    .flat_map(parse_compact_v4)
                    .filter(|p| seen_peers.insert(*p))
                    .collect();
                if !fresh.is_empty() && out.send(fresh).await.is_err() {
                    return Ok(());
                }
            }
            if let Some(nodes) = r.get("nodes").and_then(Value::as_bytes) {
                for n in nodes.chunks_exact(26) {
                    let id: [u8; 20] = n[..20].try_into().unwrap();
                    let ip = Ipv4Addr::new(n[20], n[21], n[22], n[23]);
                    let port = u16::from_be_bytes([n[24], n[25]]);
                    if port == 0 || ip.is_unspecified() {
                        continue;
                    }
                    candidates.entry(SocketAddr::new(ip.into(), port)).or_insert_with(|| xor(&id, &target));
                }
            }
        }
        if candidates.len() > 5000 {
            break;
        }
    }
    Ok(())
}

fn xor(a: &[u8; 20], b: &[u8; 20]) -> [u8; 20] {
    std::array::from_fn(|i| a[i] ^ b[i])
}
