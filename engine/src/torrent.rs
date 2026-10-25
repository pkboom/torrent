//! Per-torrent shared state and the session loop that drives discovery, connections and completion.

use crate::metainfo::{hex, Metainfo};
use crate::picker::Picker;
use crate::storage::Storage;
use crate::tracker::{self, AnnounceRequest, Event};
use crate::{dht, peer};
use anyhow::{bail, Result};
use serde::Serialize;
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

/// Port we report to trackers. We never accept incoming connections.
const ANNOUNCE_PORT: u16 = 6881;
const MAX_KNOWN_PEERS: usize = 2000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    FetchingMetadata,
    Checking,
    Downloading,
    Completed,
    Paused,
    Error,
}

pub(crate) struct PeerRecord {
    next_attempt: Instant,
    failures: u32,
}

pub(crate) struct Inner {
    pub name: String,
    pub trackers: Vec<String>,
    pub meta: Option<Arc<Metainfo>>,
    pub storage: Option<Arc<Storage>>,
    pub picker: Option<Picker>,
    pub peers: HashMap<SocketAddr, PeerRecord>,
    pub connected: HashSet<SocketAddr>,
    pub handshaked: usize,
    /// Connected peers that have the complete torrent.
    pub connected_seeds: usize,
    /// Latest (seeders, leechers) reported by each tracker.
    pub swarm: HashMap<String, (Option<u32>, Option<u32>)>,
    pub rate: f64,
    pub error: Option<String>,
    /// Hash-failure strikes per IP; three strikes and the address is banned.
    pub strikes: HashMap<IpAddr, u32>,
    pub banned: HashSet<IpAddr>,
}

pub(crate) struct Torrent {
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub save_dir: PathBuf,
    pub state_dir: Option<PathBuf>,
    pub enable_dht: bool,
    pub max_peers: usize,
    pub inner: Mutex<Inner>,
    pub phase: watch::Sender<Phase>,
    pub bytes_received: AtomicU64,
    pub checked_pieces: AtomicU32,
    /// Set once the torrent is removed so late background work leaves no files behind.
    pub removed: AtomicBool,
}

impl Torrent {
    pub fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    pub fn phase(&self) -> Phase {
        *self.phase.borrow()
    }

    pub fn set_phase(&self, p: Phase) {
        self.phase.send_replace(p);
    }

    pub fn id(&self) -> String {
        hex(&self.info_hash)
    }

    pub fn torrent_file_path(&self) -> Option<PathBuf> {
        Some(self.state_dir.as_ref()?.join(format!("{}.torrent", self.id())))
    }

    fn resume_path(&self) -> Option<PathBuf> {
        Some(self.state_dir.as_ref()?.join(format!("{}.resume", self.id())))
    }

    pub fn add_peers(&self, list: impl IntoIterator<Item = SocketAddr>) {
        let mut inner = self.lock();
        let now = Instant::now();
        for addr in list {
            if addr.port() == 0 || addr.ip().is_unspecified() || inner.banned.contains(&addr.ip()) || inner.peers.contains_key(&addr) {
                continue;
            }
            if inner.peers.len() >= MAX_KNOWN_PEERS {
                // Make room by evicting the least reliable idle address, if any has failed.
                let worst = inner
                    .peers
                    .iter()
                    .filter(|(a, r)| r.failures > 0 && !inner.connected.contains(*a))
                    .max_by_key(|(_, r)| r.failures)
                    .map(|(a, _)| *a);
                match worst {
                    Some(w) => inner.peers.remove(&w),
                    None => break,
                };
            }
            inner.peers.insert(addr, PeerRecord { next_attempt: now, failures: 0 });
        }
    }

    /// Records a failed hash check against every peer that contributed to the piece.
    pub fn blame(&self, contributors: &HashSet<SocketAddr>) {
        let mut inner = self.lock();
        // A piece from a single peer is conclusive; shared pieces only build suspicion.
        let weight = if contributors.len() == 1 { 3 } else { 1 };
        for addr in contributors {
            let ip = addr.ip();
            let strikes = inner.strikes.entry(ip).or_default();
            *strikes += weight;
            if *strikes >= 3 {
                inner.banned.insert(ip);
                inner.peers.retain(|a, _| a.ip() != ip);
            }
        }
    }

    pub fn install_metadata(&self, meta: Metainfo) {
        let mut inner = self.lock();
        if inner.meta.is_some() {
            return;
        }
        if let Some(path) = self.torrent_file_path() {
            if let Err(e) = std::fs::write(&path, meta.to_torrent_bytes()) {
                log::warn!("could not save torrent file: {e}");
            }
        }
        inner.name = meta.name.clone();
        inner.storage = Some(Arc::new(Storage::new(&self.save_dir, &meta)));
        inner.meta = Some(Arc::new(meta));
    }

    /// Called by a peer once it has downloaded the full info dictionary.
    pub fn set_metadata_bytes(&self, info: &[u8]) -> Result<()> {
        if <[u8; 20]>::from(Sha1::digest(info)) != self.info_hash {
            bail!("metadata hash mismatch");
        }
        let trackers = self.lock().trackers.clone();
        self.install_metadata(Metainfo::from_info_bytes(info, trackers)?);
        Ok(())
    }

    pub fn save_resume(&self) {
        let Some(path) = self.resume_path() else { return };
        if self.removed.load(Ordering::Relaxed) {
            return;
        }
        let (bits, storage) = {
            let inner = self.lock();
            match (inner.picker.as_ref(), inner.storage.clone()) {
                (Some(p), Some(s)) => (p.bitfield(), s),
                _ => return,
            }
        };
        // Pieces must be on disk before resume data claims them.
        if let Err(e) = storage.sync() {
            log::warn!("could not flush data: {e}");
            return;
        }
        if let Err(e) = std::fs::write(path, bits) {
            log::warn!("could not save resume data: {e}");
        }
    }

    pub fn delete_state_files(&self) {
        for p in [self.torrent_file_path(), self.resume_path()].into_iter().flatten() {
            let _ = std::fs::remove_file(p);
        }
    }

    /// Resets per-run state after the session task has been stopped.
    pub fn on_stopped(&self) {
        let mut inner = self.lock();
        inner.connected.clear();
        inner.handshaked = 0;
        inner.connected_seeds = 0;
        inner.rate = 0.0;
        if let Some(s) = &inner.storage {
            s.close();
        }
    }

    fn peers_to_connect(&self, n: usize) -> Vec<SocketAddr> {
        let mut inner = self.lock();
        let now = Instant::now();
        let mut ready: Vec<(u32, SocketAddr)> = inner
            .peers
            .iter()
            .filter(|(a, r)| r.next_attempt <= now && !inner.connected.contains(*a) && !inner.banned.contains(&a.ip()))
            .map(|(a, r)| (r.failures, *a))
            .collect();
        ready.sort_unstable_by_key(|(f, _)| *f);
        let picked: Vec<SocketAddr> = ready.into_iter().take(n).map(|(_, a)| a).collect();
        inner.connected.extend(picked.iter().copied());
        picked
    }

    fn peer_finished(&self, addr: SocketAddr, handshaked: bool) {
        let mut inner = self.lock();
        inner.connected.remove(&addr);
        if let Some(r) = inner.peers.get_mut(&addr) {
            r.failures = if handshaked { r.failures.min(1) } else { r.failures + 1 };
            if r.failures > 6 {
                inner.peers.remove(&addr);
            } else {
                r.next_attempt = Instant::now() + Duration::from_secs(30 * (1 << r.failures.min(5)) as u64);
            }
        }
    }

    fn announce_request(&self, event: Event) -> AnnounceRequest {
        let inner = self.lock();
        let (downloaded, left) = match (&inner.meta, &inner.picker) {
            (Some(m), Some(p)) => (p.have_bytes(), m.total_length - p.have_bytes()),
            (Some(m), None) => (0, m.total_length),
            // Size unknown until metadata arrives; claim to need data so trackers return seeds.
            _ => (0, 1 << 30),
        };
        AnnounceRequest {
            info_hash: self.info_hash,
            peer_id: self.peer_id,
            port: ANNOUNCE_PORT,
            downloaded,
            left,
            event,
        }
    }

    /// Builds the picker, trusting saved resume data when the files back it up,
    /// otherwise hashing whatever is already on disk.
    async fn check(self: &Arc<Self>) -> Result<()> {
        self.set_phase(Phase::Checking);
        self.checked_pieces.store(0, Ordering::Relaxed);
        let (meta, storage) = {
            let inner = self.lock();
            (inner.meta.clone().unwrap(), inner.storage.clone().unwrap())
        };
        let resume = self.resume_path().and_then(|p| std::fs::read(p).ok());
        let me = self.clone();
        let have = tokio::task::spawn_blocking(move || -> Result<Vec<bool>> {
            let n = meta.num_pieces() as usize;
            let mut have = vec![false; n];
            if let Some(bits) = resume.filter(|b| b.len() == n.div_ceil(8)) {
                let mut sizes = HashMap::new();
                for (i, h) in have.iter_mut().enumerate() {
                    let marked = bits[i / 8] & (0x80 >> (i % 8)) != 0;
                    *h = marked && storage.piece_present(i as u32, meta.piece_size(i as u32) as usize, &mut sizes);
                    me.checked_pieces.fetch_add(1, Ordering::Relaxed);
                }
                return Ok(have);
            }
            for (i, h) in have.iter_mut().enumerate() {
                let len = meta.piece_size(i as u32) as usize;
                if let Some(buf) = storage.read_piece(i as u32, len)? {
                    *h = <[u8; 20]>::from(Sha1::digest(&buf)) == meta.pieces[i];
                }
                me.checked_pieces.fetch_add(1, Ordering::Relaxed);
            }
            Ok(have)
        })
        .await??;
        let meta = self.lock().meta.clone().unwrap();
        self.lock().picker = Some(Picker::new(meta.piece_length, meta.total_length, &have));
        self.save_resume();
        Ok(())
    }
}

enum Aux {
    Tracker(String, Result<(Duration, Option<u32>, Option<u32>)>),
    Dht,
}

/// Drives one torrent until it completes, fails, or the task is aborted (pause/remove).
pub(crate) async fn run(t: Arc<Torrent>) {
    if let Err(e) = run_inner(&t).await {
        log::warn!("torrent {} failed: {e:#}", t.id());
        t.lock().error = Some(format!("{e:#}"));
        t.set_phase(Phase::Error);
    }
    t.on_stopped();
}

async fn run_inner(t: &Arc<Torrent>) -> Result<()> {
    t.lock().error = None;
    if let Some(p) = t.lock().picker.as_mut() {
        p.reset_transient();
    }
    let mut peers: JoinSet<(SocketAddr, bool)> = JoinSet::new();
    let mut aux: JoinSet<Aux> = JoinSet::new();
    let (peer_tx, mut peer_rx) = mpsc::channel::<Vec<SocketAddr>>(64);

    let has_meta = t.lock().meta.is_some();
    if !has_meta {
        t.set_phase(Phase::FetchingMetadata);
    } else if t.lock().picker.is_none() {
        t.check().await?;
    }
    if t.lock().meta.is_some() {
        t.set_phase(Phase::Downloading);
    }

    let mut tracker_next: HashMap<String, Instant> = HashMap::new();
    let mut tracker_fails: HashMap<String, u32> = HashMap::new();
    let mut announced: HashSet<String> = HashSet::new();
    let mut dht_next = Instant::now();
    let mut dht_running = false;
    let mut last_bytes = t.bytes_received.load(Ordering::Relaxed);
    let mut last_rate_at = Instant::now();
    let mut last_resume_save = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(500));

    loop {
        tokio::select! {
            _ = tick.tick() => {}
            Some(list) = peer_rx.recv() => t.add_peers(list),
            Some(res) = peers.join_next(), if !peers.is_empty() => {
                if let Ok((addr, ok)) = res {
                    t.peer_finished(addr, ok);
                }
            }
            Some(res) = aux.join_next(), if !aux.is_empty() => match res {
                Ok(Aux::Tracker(url, r)) => {
                    let now = Instant::now();
                    match r {
                        Ok((interval, seeders, leechers)) => {
                            t.lock().swarm.insert(url.clone(), (seeders, leechers));
                            tracker_fails.remove(&url);
                            tracker_next.insert(url, now + interval);
                        }
                        Err(e) => {
                            log::debug!("tracker {url}: {e:#}");
                            let f = tracker_fails.entry(url.clone()).or_default();
                            *f += 1;
                            tracker_next.insert(url, now + Duration::from_secs(60 * (1 << (*f).min(5)) as u64));
                        }
                    }
                }
                Ok(Aux::Dht) => dht_running = false,
                Err(_) => {}
            },
        }

        if let Some(e) = t.lock().error.clone() {
            bail!(e);
        }

        if t.phase() == Phase::FetchingMetadata && t.lock().meta.is_some() {
            t.check().await?;
            t.set_phase(Phase::Downloading);
        }

        if t.lock().picker.as_ref().is_some_and(Picker::is_complete) {
            let storage = t.lock().storage.clone();
            if let Some(s) = storage {
                s.finalize()?;
            }
            t.save_resume();
            let req = t.announce_request(Event::Completed);
            for url in announced.iter().cloned() {
                tokio::spawn(async move {
                    let _ = tracker::announce(&url, &req).await;
                });
            }
            t.set_phase(Phase::Completed);
            return Ok(());
        }

        let now = Instant::now();
        let elapsed = now.duration_since(last_rate_at).as_secs_f64();
        if elapsed >= 1.0 {
            let bytes = t.bytes_received.load(Ordering::Relaxed);
            let sample = (bytes - last_bytes) as f64 / elapsed;
            let mut inner = t.lock();
            inner.rate = if inner.rate == 0.0 { sample } else { inner.rate * 0.6 + sample * 0.4 };
            drop(inner);
            last_bytes = bytes;
            last_rate_at = now;
        }

        let trackers = t.lock().trackers.clone();
        for url in trackers {
            if tracker_next.get(&url).is_some_and(|&n| n > now) {
                continue;
            }
            tracker_next.insert(url.clone(), now + Duration::from_secs(3600));
            let event = if announced.insert(url.clone()) { Event::Started } else { Event::None };
            let req = t.announce_request(event);
            let tx = peer_tx.clone();
            aux.spawn(async move {
                let r = tracker::announce(&url, &req).await.and_then(|resp| {
                    let _ = tx.try_send(resp.peers);
                    Ok((resp.interval.unwrap_or(Duration::from_secs(1800)), resp.seeders, resp.leechers))
                });
                Aux::Tracker(url, r)
            });
        }

        let connected = t.lock().handshaked;
        if t.enable_dht && !dht_running && now >= dht_next {
            dht_running = true;
            dht_next = now + if connected < 10 { Duration::from_secs(60) } else { Duration::from_secs(600) };
            let (hash, tx) = (t.info_hash, peer_tx.clone());
            aux.spawn(async move {
                dht::get_peers(hash, tx).await;
                Aux::Dht
            });
        }

        let slots = t.max_peers.saturating_sub(peers.len());
        for addr in t.peers_to_connect(slots) {
            peers.spawn(peer::run(t.clone(), addr));
        }

        if now.duration_since(last_resume_save) > Duration::from_secs(30) {
            last_resume_save = now;
            let t2 = t.clone();
            tokio::task::spawn_blocking(move || t2.save_resume());
        }
    }
}
