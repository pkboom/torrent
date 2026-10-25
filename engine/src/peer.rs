//! Peer wire protocol (BEP 3) with the extension protocol (BEP 10),
//! metadata exchange (BEP 9) and peer exchange. Download only: we never unchoke
//! or serve requests.

use crate::bencode::{self, Value};
use crate::picker::{BlockReq, BlockResult};
use crate::torrent::Torrent;
use crate::tracker::parse_compact_v4;
use anyhow::{bail, Context, Result};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_MESSAGE: usize = 2 * 1024 * 1024;
const METADATA_PIECE: usize = 16 * 1024;
const MAX_METADATA: usize = 32 * 1024 * 1024;
/// Extension ids we advertise in our handshake.
const UT_METADATA: u8 = 1;
const UT_PEX: u8 = 2;
/// Most clients drop requests beyond ~250 queued.
const MAX_PIPELINE: usize = 250;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// BEP 11 caps `added` at 50 peers per message.
const MAX_PEX_ADDED: usize = 50;

enum Msg {
    Choke,
    Unchoke,
    Have(u32),
    Bitfield(Vec<u8>),
    Piece { index: u32, begin: u32, data: Vec<u8> },
    Extended { id: u8, payload: Vec<u8> },
    Other,
}

impl Msg {
    fn parse(buf: Vec<u8>) -> Result<Msg> {
        let id = buf[0];
        let body = &buf[1..];
        let u32_at = |i: usize| -> Result<u32> {
            Ok(u32::from_be_bytes(body.get(i..i + 4).context("short message")?.try_into().unwrap()))
        };
        Ok(match id {
            0 => Msg::Choke,
            1 => Msg::Unchoke,
            4 => Msg::Have(u32_at(0)?),
            5 => Msg::Bitfield(body.to_vec()),
            7 => Msg::Piece { index: u32_at(0)?, begin: u32_at(4)?, data: body[8..].to_vec() },
            20 if !body.is_empty() => Msg::Extended { id: body[0], payload: body[1..].to_vec() },
            _ => Msg::Other,
        })
    }
}

/// Aborts the wrapped task when dropped.
struct AbortOnDrop(tokio::task::JoinHandle<Result<()>>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Peer {
    t: Arc<Torrent>,
    addr: SocketAddr,
    handshaked: bool,
    peer_choking: bool,
    am_interested: bool,
    raw_bits: Vec<u8>,
    /// Whether the peer has told us anything about its pieces yet.
    got_bits: bool,
    /// Peer's pieces, once metadata is known and availability is registered with the picker.
    bits: Option<Vec<bool>>,
    /// Pieces the peer has, and whether it is counted as a connected seed.
    piece_count: usize,
    counted_seed: bool,
    /// Requests in flight and when they were sent.
    outstanding: HashMap<BlockReq, Instant>,
    ut_metadata: Option<u8>,
    metadata_size: Option<usize>,
    metadata: Vec<Option<Vec<u8>>>,
    connected_at: Instant,
    last_message: Instant,
    last_block: Instant,
    unchoked_ever: bool,
    window_bytes: u64,
    rate: f64,
}

/// Returns the address and whether the handshake succeeded.
pub(crate) async fn run(t: Arc<Torrent>, addr: SocketAddr) -> (SocketAddr, bool) {
    let now = Instant::now();
    let mut p = Peer {
        t,
        addr,
        handshaked: false,
        peer_choking: true,
        am_interested: false,
        raw_bits: Vec::new(),
        got_bits: false,
        bits: None,
        piece_count: 0,
        counted_seed: false,
        outstanding: HashMap::new(),
        ut_metadata: None,
        metadata_size: None,
        metadata: Vec::new(),
        connected_at: now,
        last_message: now,
        last_block: now,
        unchoked_ever: false,
        window_bytes: 0,
        rate: 0.0,
    };
    if let Err(e) = p.run().await {
        log::trace!("peer {addr}: {e:#}");
    }
    (addr, p.handshaked)
}

impl Drop for Peer {
    fn drop(&mut self) {
        let mut inner = self.t.lock();
        if self.handshaked {
            inner.handshaked = inner.handshaked.saturating_sub(1);
        }
        if self.counted_seed {
            inner.connected_seeds = inner.connected_seeds.saturating_sub(1);
        }
        if let Some(picker) = inner.picker.as_mut() {
            for r in self.outstanding.keys() {
                picker.release(r);
            }
            if let Some(bits) = &self.bits {
                picker.remove_peer(bits);
            }
        }
    }
}

impl Peer {
    async fn run(&mut self) -> Result<()> {
        let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(self.addr)).await??;
        stream.set_nodelay(true)?;
        let (mut rd, mut wr) = stream.into_split();

        let mut hs = Vec::with_capacity(68);
        hs.push(19);
        hs.extend_from_slice(b"BitTorrent protocol");
        hs.extend_from_slice(&[0, 0, 0, 0, 0, 0x10, 0, 0]);
        hs.extend_from_slice(&self.t.info_hash);
        hs.extend_from_slice(&self.t.peer_id);
        timeout(CONNECT_TIMEOUT, wr.write_all(&hs)).await??;
        let mut resp = [0u8; 68];
        timeout(CONNECT_TIMEOUT, rd.read_exact(&mut resp)).await??;
        if resp[0] != 19 || &resp[1..20] != b"BitTorrent protocol" || resp[28..48] != self.t.info_hash {
            bail!("bad handshake");
        }
        if resp[48..68] == self.t.peer_id {
            bail!("connected to ourselves");
        }
        let supports_ext = resp[25] & 0x10 != 0;
        if !supports_ext && self.t.lock().meta.is_none() {
            bail!("peer cannot send metadata");
        }
        self.handshaked = true;
        self.t.lock().handshaked += 1;

        let (tx, mut rx) = mpsc::channel(256);
        let _reader = AbortOnDrop(tokio::spawn(read_loop(BufReader::with_capacity(64 * 1024, rd), tx)));

        let mut out = Vec::new();
        if supports_ext {
            let mut m = Value::dict([
                ("m", Value::dict([("ut_metadata", Value::Int(UT_METADATA as i64)), ("ut_pex", Value::Int(UT_PEX as i64))])),
                ("v", Value::bytes(concat!("RustTorrent ", env!("CARGO_PKG_VERSION")))),
            ]);
            if let (Value::Dict(d), Some(meta)) = (&mut m, self.t.lock().meta.as_ref()) {
                d.insert(b"metadata_size".to_vec(), Value::Int(meta.info_bytes.len() as i64));
            }
            push_extended(&mut out, 0, &bencode::encode(&m));
        }

        let mut phase_rx = self.t.phase.subscribe();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut last_sent = Instant::now();
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(m) => self.handle(m).await?,
                    None => bail!("connection closed"),
                },
                _ = tick.tick() => self.on_tick()?,
                r = phase_rx.changed() => r?,
            }
            while let Ok(m) = rx.try_recv() {
                self.handle(m).await?;
            }
            if self.t.lock().picker.as_ref().is_some_and(|p| p.is_complete()) {
                return Ok(());
            }
            self.update(&mut out)?;
            if out.is_empty() && last_sent.elapsed() > Duration::from_secs(90) {
                out.extend_from_slice(&[0, 0, 0, 0]);
            }
            if !out.is_empty() {
                timeout(Duration::from_secs(30), wr.write_all(&out)).await??;
                out.clear();
                last_sent = Instant::now();
            }
        }
    }

    fn on_tick(&mut self) -> Result<()> {
        let now = Instant::now();
        // Per-peer rate drives how deep we pipeline requests.
        self.rate = self.rate * 0.5 + self.window_bytes as f64 * 0.5;
        self.window_bytes = 0;

        if now.duration_since(self.last_message) > Duration::from_secs(120) {
            bail!("idle");
        }
        if !self.outstanding.is_empty() && now.duration_since(self.last_block) > Duration::from_secs(30) {
            bail!("snubbed");
        }
        let age = now.duration_since(self.connected_at);
        let mut inner = self.t.lock();
        if inner.banned.contains(&self.addr.ip()) {
            bail!("banned");
        }
        // Hand silently dropped requests back so other peers can fetch them.
        let expired: Vec<BlockReq> = self
            .outstanding
            .iter()
            .filter(|(_, sent)| now.duration_since(**sent) > REQUEST_TIMEOUT)
            .map(|(r, _)| *r)
            .collect();
        if let Some(picker) = inner.picker.as_mut() {
            for r in &expired {
                self.outstanding.remove(r);
                picker.release(r);
            }
        }
        if inner.meta.is_none() {
            if age > Duration::from_secs(20) && self.ut_metadata.is_none() {
                bail!("no metadata support");
            }
        } else if let (Some(picker), Some(bits)) = (inner.picker.as_ref(), self.bits.as_ref()) {
            // Free the slot for someone more useful.
            if age > Duration::from_secs(30) && !picker.wants_from(bits) {
                bail!("peer has nothing we need");
            }
            if !self.unchoked_ever && age > Duration::from_secs(90) {
                bail!("never unchoked");
            }
        }
        Ok(())
    }

    async fn handle(&mut self, msg: Msg) -> Result<()> {
        self.last_message = Instant::now();
        match msg {
            Msg::Choke => {
                self.peer_choking = true;
                let reqs: Vec<BlockReq> = self.outstanding.drain().map(|(r, _)| r).collect();
                if let Some(picker) = self.t.lock().picker.as_mut() {
                    reqs.iter().for_each(|r| picker.release(r));
                }
            }
            Msg::Unchoke => {
                self.peer_choking = false;
                self.unchoked_ever = true;
                self.last_block = Instant::now();
            }
            Msg::Have(i) => {
                self.got_bits = true;
                let (byte, bit) = (i as usize / 8, 0x80u8 >> (i % 8));
                if self.raw_bits.len() <= byte {
                    if byte > MAX_MESSAGE {
                        bail!("have index out of range");
                    }
                    self.raw_bits.resize(byte + 1, 0);
                }
                self.raw_bits[byte] |= bit;
                if let Some(bits) = self.bits.as_mut() {
                    if let Some(b) = bits.get_mut(i as usize) {
                        if !*b {
                            *b = true;
                            self.piece_count += 1;
                            if let Some(p) = self.t.lock().picker.as_mut() {
                                p.peer_has(i);
                            }
                            self.update_seed_status();
                        }
                    }
                }
            }
            Msg::Bitfield(b) => {
                self.got_bits = true;
                if self.raw_bits.len() < b.len() {
                    self.raw_bits.resize(b.len(), 0);
                }
                for (dst, src) in self.raw_bits.iter_mut().zip(&b) {
                    *dst |= src;
                }
                // Arrived after we gave up waiting: re-register with the full picture.
                if let Some(old) = self.bits.take() {
                    if let Some(p) = self.t.lock().picker.as_mut() {
                        p.remove_peer(&old);
                    }
                    self.piece_count = 0;
                }
            }
            Msg::Piece { index, begin, data } => self.on_piece(index, begin, data).await?,
            Msg::Extended { id: 0, payload } => {
                let Ok((hs, _)) = bencode::decode_prefix(&payload) else { return Ok(()) };
                if let Some(m) = hs.get("m") {
                    self.ut_metadata = m.get("ut_metadata").and_then(Value::as_int).filter(|&v| v > 0 && v < 256).map(|v| v as u8);
                }
                self.metadata_size = hs
                    .get("metadata_size")
                    .and_then(Value::as_int)
                    .filter(|&n| n > 0 && n as usize <= MAX_METADATA)
                    .map(|n| n as usize);
            }
            Msg::Extended { id: UT_METADATA, payload } => self.on_metadata(&payload)?,
            Msg::Extended { id: UT_PEX, payload } => {
                if let Ok((v, _)) = bencode::decode_prefix(&payload) {
                    if let Some(added) = v.get("added").and_then(Value::as_bytes) {
                        self.t.add_peers(parse_compact_v4(added).into_iter().take(MAX_PEX_ADDED));
                    }
                }
            }
            Msg::Extended { .. } | Msg::Other => {}
        }
        Ok(())
    }

    async fn on_piece(&mut self, index: u32, begin: u32, data: Vec<u8>) -> Result<()> {
        let len = data.len() as u64;
        self.t.bytes_received.fetch_add(len, Ordering::Relaxed);
        self.window_bytes += len;
        self.last_block = Instant::now();
        // Only accept blocks we asked this peer for (late ones after a choke are dropped).
        let Some(req) = self.outstanding.keys().find(|r| r.piece == index && r.begin == begin).copied() else {
            return Ok(());
        };
        self.outstanding.remove(&req);

        let (buf, contributors, expected, storage) = {
            let mut inner = self.t.lock();
            let Some(picker) = inner.picker.as_mut() else { return Ok(()) };
            if data.len() != req.len as usize {
                picker.release(&req);
                bail!("block has wrong length");
            }
            let BlockResult::PieceDone(buf, contributors) = picker.on_block(index, begin, &data, self.addr) else {
                return Ok(());
            };
            let expected = inner.meta.as_ref().unwrap().pieces[index as usize];
            (buf, contributors, expected, inner.storage.clone().unwrap())
        };
        let result = tokio::task::spawn_blocking(move || {
            if <[u8; 20]>::from(Sha1::digest(&buf)) != expected {
                return Ok(false);
            }
            storage.write_piece(index, &buf).map(|_| true)
        })
        .await?;

        let ok = matches!(result, Ok(true));
        if let Some(p) = self.t.lock().picker.as_mut() {
            p.piece_verified(index, ok);
        }
        match result {
            Ok(true) => Ok(()),
            Ok(false) => {
                self.t.blame(&contributors);
                if self.t.lock().banned.contains(&self.addr.ip()) {
                    bail!("peer sent corrupt data");
                }
                Ok(())
            }
            Err(e) => {
                self.t.lock().error = Some(format!("Disk write failed: {e}"));
                bail!("disk write failed: {e}")
            }
        }
    }

    fn on_metadata(&mut self, payload: &[u8]) -> Result<()> {
        let (msg, used) = bencode::decode_prefix(payload)?;
        let piece = msg.get("piece").and_then(Value::as_int).unwrap_or(-1);
        match msg.get("msg_type").and_then(Value::as_int) {
            Some(1) => {
                let i = usize::try_from(piece).ok().filter(|&i| i < self.metadata.len()).context("unexpected metadata piece")?;
                let size = self.metadata_size.unwrap_or(0);
                let data = &payload[used..];
                if data.len() != METADATA_PIECE.min(size - i * METADATA_PIECE) {
                    bail!("metadata piece has wrong length");
                }
                self.metadata[i] = Some(data.to_vec());
                if self.metadata.iter().all(Option::is_some) {
                    let info: Vec<u8> = self.metadata.drain(..).flatten().flatten().collect();
                    if self.t.lock().meta.is_none() {
                        self.t.set_metadata_bytes(&info)?;
                    }
                }
            }
            Some(2) if self.t.lock().meta.is_none() => bail!("peer rejected metadata request"),
            _ => {}
        }
        Ok(())
    }

    fn update_seed_status(&mut self) {
        let Some(bits) = &self.bits else { return };
        if !self.counted_seed && self.piece_count == bits.len() {
            self.counted_seed = true;
            self.t.lock().connected_seeds += 1;
        }
    }

    fn update(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let mut inner = self.t.lock();
        if inner.meta.is_none() {
            if let (Some(id), Some(size)) = (self.ut_metadata, self.metadata_size) {
                if self.metadata.is_empty() {
                    let n = size.div_ceil(METADATA_PIECE);
                    self.metadata = vec![None; n];
                    for i in 0..n {
                        let req = Value::dict([("msg_type", Value::Int(0)), ("piece", Value::Int(i as i64))]);
                        push_extended(out, id, &bencode::encode(&req));
                    }
                }
            }
            return Ok(());
        }
        let num_pieces = inner.meta.as_ref().unwrap().num_pieces() as usize;
        let Some(picker) = inner.picker.as_mut() else { return Ok(()) };

        if self.bits.is_none() {
            // Give the peer a moment to send its bitfield; seeds without one send Haves or nothing.
            if !self.got_bits && self.connected_at.elapsed() < Duration::from_secs(3) {
                return Ok(());
            }
            if self.raw_bits.len() > num_pieces.div_ceil(8) {
                bail!("bitfield too long");
            }
            let bits: Vec<bool> = (0..num_pieces)
                .map(|i| self.raw_bits.get(i / 8).is_some_and(|b| b & (0x80 >> (i % 8)) != 0))
                .collect();
            picker.add_peer(&bits);
            self.piece_count = bits.iter().filter(|b| **b).count();
            let seed = self.piece_count == num_pieces;
            if seed != self.counted_seed {
                self.counted_seed = seed;
                if seed { inner.connected_seeds += 1 } else { inner.connected_seeds -= 1 }
            }
            self.bits = Some(bits);
        }
        let picker = inner.picker.as_mut().unwrap();
        let bits = self.bits.as_ref().unwrap();

        if !self.am_interested && picker.wants_from(bits) {
            self.am_interested = true;
            out.extend_from_slice(&[0, 0, 0, 1, 2]);
        }
        if self.am_interested && !self.peer_choking {
            // Keep ~2 seconds of data in flight, within sane bounds.
            let depth = ((self.rate * 2.0) as usize / crate::picker::BLOCK_SIZE as usize).clamp(32, MAX_PIPELINE);
            if self.outstanding.len() < depth / 2 || self.outstanding.len() < 16 {
                let mine = &self.outstanding;
                let reqs = picker.pick(bits, depth.saturating_sub(mine.len()), |i, b| mine.keys().any(|r| r.piece == i && r.begin == b));
                let now = Instant::now();
                for r in reqs {
                    out.extend_from_slice(&13u32.to_be_bytes());
                    out.push(6);
                    out.extend_from_slice(&r.piece.to_be_bytes());
                    out.extend_from_slice(&r.begin.to_be_bytes());
                    out.extend_from_slice(&r.len.to_be_bytes());
                    self.outstanding.insert(r, now);
                }
            }
        }
        Ok(())
    }
}

fn push_extended(out: &mut Vec<u8>, ext_id: u8, payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32 + 2).to_be_bytes());
    out.push(20);
    out.push(ext_id);
    out.extend_from_slice(payload);
}

async fn read_loop(mut rd: BufReader<OwnedReadHalf>, tx: mpsc::Sender<Msg>) -> Result<()> {
    loop {
        let len = rd.read_u32().await? as usize;
        if len == 0 {
            // Keep-alive: still counts as activity.
            if tx.send(Msg::Other).await.is_err() {
                return Ok(());
            }
            continue;
        }
        if len > MAX_MESSAGE {
            bail!("message too large");
        }
        let mut buf = vec![0u8; len];
        rd.read_exact(&mut buf).await?;
        if tx.send(Msg::parse(buf)?).await.is_err() {
            return Ok(());
        }
    }
}
