//! Public API: manages a set of torrents and persists them across restarts.

use crate::magnet;
use crate::metainfo::{hex, Metainfo};
use crate::torrent::{self, Inner, Phase, Torrent};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio::task::JoinHandle;

#[derive(Clone, Debug)]
pub struct Config {
    pub download_dir: PathBuf,
    /// Where the session (torrent list, resume data) is persisted. `None` keeps everything in memory.
    pub state_dir: Option<PathBuf>,
    pub enable_dht: bool,
    pub max_peers_per_torrent: usize,
    /// Deleted downloads go to the Trash; when false they are removed permanently.
    pub use_trash: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            download_dir: std::env::current_dir().unwrap_or_default(),
            state_dir: None,
            enable_dht: true,
            max_peers_per_torrent: 80,
            use_trash: true,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct TorrentStatus {
    pub id: String,
    pub name: String,
    pub phase: Phase,
    pub progress: f64,
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
    pub download_rate: u64,
    pub peers: usize,
    /// Connected peers that have the whole torrent.
    pub seeds: usize,
    /// Seeders and leechers in the whole swarm, per the trackers (largest report wins).
    pub swarm_seeds: Option<u32>,
    pub swarm_leechers: Option<u32>,
    pub known_peers: usize,
    pub eta_secs: Option<u64>,
    pub save_path: String,
    pub error: Option<String>,
    /// The torrent contains a video file (see [`Client::video_path`]).
    pub has_video: bool,
    /// Number of real (non-padding) files.
    pub file_count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileStatus {
    /// Position in the torrent's file list; pass back to [`Client::file_path`].
    pub index: usize,
    /// Path inside the torrent (without the torrent's own folder).
    pub path: String,
    pub length: u64,
    pub downloaded_bytes: u64,
    pub is_video: bool,
}

#[derive(Serialize, Deserialize)]
struct SavedTorrent {
    info_hash: String,
    save_dir: PathBuf,
    paused: bool,
    magnet: Option<String>,
}

struct Entry {
    t: Arc<Torrent>,
    task: Option<JoinHandle<()>>,
    paused: bool,
    magnet: Option<String>,
}

#[derive(Clone)]
pub struct Client {
    config: Arc<Mutex<Config>>,
    torrents: Arc<Mutex<Vec<Entry>>>,
    peer_id: [u8; 20],
}

impl Client {
    pub fn new(config: Config) -> Self {
        if let Some(dir) = &config.state_dir {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut peer_id = *b"-RT0100-000000000000";
        for b in &mut peer_id[8..] {
            *b = b"0123456789abcdefghijklmnopqrstuvwxyz"[rand::random::<usize>() % 36];
        }
        Client {
            config: Arc::new(Mutex::new(config)),
            torrents: Arc::new(Mutex::new(Vec::new())),
            peer_id,
        }
    }

    pub fn download_dir(&self) -> PathBuf {
        self.config.lock().unwrap().download_dir.clone()
    }

    /// Only affects torrents added afterwards.
    pub fn set_download_dir(&self, dir: PathBuf) {
        self.config.lock().unwrap().download_dir = dir;
    }

    /// Re-adds torrents saved by a previous session. Must be called within a Tokio runtime.
    pub fn restore(&self) -> Result<()> {
        let Some(state) = self.config.lock().unwrap().state_dir.clone() else { return Ok(()) };
        let Ok(raw) = std::fs::read(state.join("session.json")) else { return Ok(()) };
        let saved: Vec<SavedTorrent> = serde_json::from_slice(&raw).context("corrupt session.json")?;
        for s in saved {
            let file = state.join(format!("{}.torrent", s.info_hash));
            let res = match std::fs::read(&file) {
                Ok(bytes) => Metainfo::from_torrent_bytes(&bytes).map(|m| self.insert(Source::Meta(m), s.magnet.clone(), s.save_dir.clone(), s.paused)),
                Err(_) => match &s.magnet {
                    Some(uri) => magnet::parse(uri).map(|m| self.insert(Source::Magnet(m), s.magnet.clone(), s.save_dir.clone(), s.paused)),
                    None => continue,
                },
            };
            if let Err(e) = res {
                log::warn!("could not restore {}: {e:#}", s.info_hash);
            }
        }
        Ok(())
    }

    pub fn add_torrent_bytes(&self, data: &[u8]) -> Result<String> {
        let meta = Metainfo::from_torrent_bytes(data)?;
        let dir = self.download_dir();
        let id = self.insert(Source::Meta(meta), None, dir, false);
        self.save_session();
        Ok(id)
    }

    pub fn add_magnet(&self, uri: &str) -> Result<String> {
        let m = magnet::parse(uri)?;
        let dir = self.download_dir();
        let id = self.insert(Source::Magnet(m), Some(uri.trim().to_string()), dir, false);
        self.save_session();
        Ok(id)
    }

    fn insert(&self, src: Source, magnet_uri: Option<String>, save_dir: PathBuf, paused: bool) -> String {
        let info_hash = match &src {
            Source::Meta(m) => m.info_hash,
            Source::Magnet(m) => m.info_hash,
        };
        let id = hex(&info_hash);
        let mut list = self.torrents.lock().unwrap();
        if list.iter().any(|e| e.t.info_hash == info_hash) {
            return id;
        }
        let cfg = self.config.lock().unwrap().clone();
        let (name, trackers, peers) = match &src {
            Source::Meta(m) => (m.name.clone(), m.trackers.clone(), vec![]),
            Source::Magnet(m) => (m.name.clone().unwrap_or_else(|| id.clone()), m.trackers.clone(), m.peers.clone()),
        };
        let t = Arc::new(Torrent {
            info_hash,
            peer_id: self.peer_id,
            save_dir,
            state_dir: cfg.state_dir.clone(),
            enable_dht: cfg.enable_dht,
            max_peers: cfg.max_peers_per_torrent,
            inner: Mutex::new(Inner {
                name,
                trackers,
                meta: None,
                storage: None,
                picker: None,
                peers: HashMap::new(),
                connected: HashSet::new(),
                handshaked: 0,
                connected_seeds: 0,
                swarm: HashMap::new(),
                rate: 0.0,
                error: None,
                strikes: HashMap::new(),
                banned: HashSet::new(),
            }),
            phase: watch::Sender::new(if paused { Phase::Paused } else { Phase::Checking }),
            bytes_received: AtomicU64::new(0),
            checked_pieces: AtomicU32::new(0),
            removed: AtomicBool::new(false),
        });
        if let Source::Meta(m) = src {
            t.install_metadata(m);
        }
        t.add_peers(peers);
        let task = (!paused).then(|| tokio::spawn(torrent::run(t.clone())));
        list.push(Entry { t, task, paused, magnet: magnet_uri });
        id
    }

    pub fn list(&self) -> Vec<TorrentStatus> {
        self.torrents.lock().unwrap().iter().map(|e| status(&e.t)).collect()
    }

    pub fn status(&self, id: &str) -> Option<TorrentStatus> {
        self.find(id, |e| status(&e.t))
    }

    pub fn pause(&self, id: &str) -> Result<()> {
        self.find(id, |e| {
            // Keep the aborted handle so resume can wait for it to wind down.
            if let Some(task) = &e.task {
                task.abort();
            }
            e.paused = true;
            e.t.on_stopped();
            e.t.save_resume();
            if e.t.phase() != Phase::Completed {
                e.t.set_phase(Phase::Paused);
            }
        })
        .context("no such torrent")?;
        self.save_session();
        Ok(())
    }

    pub fn resume(&self, id: &str) -> Result<()> {
        self.find(id, |e| {
            let was_paused = std::mem::replace(&mut e.paused, false);
            let running = !was_paused && e.task.as_ref().is_some_and(|t| !t.is_finished());
            if !running && e.t.phase() != Phase::Completed {
                let old = e.task.take();
                let t = e.t.clone();
                e.task = Some(tokio::spawn(async move {
                    if let Some(old) = old {
                        old.abort();
                        let _ = old.await;
                    }
                    torrent::run(t).await
                }));
            }
        })
        .context("no such torrent")?;
        self.save_session();
        Ok(())
    }

    /// Removes the torrent. With `delete_files`, its downloaded files are moved to the Trash.
    pub async fn remove(&self, id: &str, delete_files: bool) -> Result<()> {
        let entry = {
            let mut list = self.torrents.lock().unwrap();
            let pos = list.iter().position(|e| e.t.id() == id).context("no such torrent")?;
            list.remove(pos)
        };
        self.save_session();
        let t = entry.t;
        t.removed.store(true, Ordering::Relaxed);
        if let Some(s) = t.lock().storage.clone() {
            s.disable();
        }
        if let Some(task) = entry.task {
            task.abort();
            let _ = task.await;
        }
        t.on_stopped();
        t.delete_state_files();
        if delete_files {
            let target = {
                let inner = t.lock();
                inner.meta.as_ref().zip(inner.storage.as_ref()).map(|(m, s)| (m.root_path(&t.save_dir), s.file_paths()))
            };
            if let Some((root, files)) = target {
                let save_dir = t.save_dir.clone();
                let use_trash = self.config.lock().unwrap().use_trash;
                tokio::task::spawn_blocking(move || delete_download(&save_dir, &root, &files, use_trash)).await??;
            }
        }
        Ok(())
    }

    /// Per-file progress, computed from which pieces are verified.
    pub fn files(&self, id: &str) -> Option<Vec<FileStatus>> {
        self.find(id, |e| {
            let inner = e.t.lock();
            let meta = inner.meta.as_ref()?;
            let multi = meta.files.len() > 1 || meta.files[0].path != Path::new(&meta.name);
            Some(
                meta.files
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| !f.pad)
                    .map(|(index, f)| {
                        let downloaded = match &inner.picker {
                            Some(p) if f.length > 0 => {
                                let (start, end) = (f.offset, f.offset + f.length);
                                let first = (start / meta.piece_length) as u32;
                                let last = ((end - 1) / meta.piece_length) as u32;
                                (first..=last)
                                    .filter(|&i| p.has(i))
                                    .map(|i| {
                                        let ps = i as u64 * meta.piece_length;
                                        end.min(ps + meta.piece_size(i)) - start.max(ps)
                                    })
                                    .sum()
                            }
                            _ => 0,
                        };
                        let rel = if multi { f.path.strip_prefix(&meta.name).unwrap_or(&f.path) } else { &f.path };
                        FileStatus {
                            index,
                            path: rel.to_string_lossy().into_owned(),
                            length: f.length,
                            downloaded_bytes: downloaded,
                            is_video: is_video(&f.path),
                        }
                    })
                    .collect(),
            )
        })
        .flatten()
    }

    /// Absolute path of one file in the torrent.
    pub fn file_path(&self, id: &str, index: usize) -> Option<PathBuf> {
        self.find(id, |e| {
            let inner = e.t.lock();
            let f = inner.meta.as_ref()?.files.get(index)?;
            (!f.pad).then(|| e.t.save_dir.join(&f.path))
        })
        .flatten()
    }

    /// The largest video file in the torrent, if any.
    pub fn video_path(&self, id: &str) -> Option<PathBuf> {
        self.find(id, |e| {
            let inner = e.t.lock();
            let meta = inner.meta.as_ref()?;
            meta.files
                .iter()
                .filter(|f| !f.pad && is_video(&f.path))
                .max_by_key(|f| f.length)
                .map(|f| e.t.save_dir.join(&f.path))
        })
        .flatten()
    }

    /// Adds peers to try directly (useful for local swarms and tests).
    pub fn add_peers(&self, id: &str, peers: Vec<SocketAddr>) -> Result<()> {
        self.find(id, |e| e.t.add_peers(peers)).context("no such torrent")
    }

    /// Path of the downloaded file or folder, once metadata is known.
    pub fn content_path(&self, id: &str) -> Option<PathBuf> {
        self.find(id, |e| e.t.lock().meta.as_ref().map(|m| m.root_path(&e.t.save_dir))).flatten()
    }

    /// Stops all torrents and flushes resume data. Call before exiting.
    pub fn shutdown(&self) {
        for e in self.torrents.lock().unwrap().iter_mut() {
            if let Some(task) = e.task.take() {
                task.abort();
            }
            e.t.on_stopped();
            e.t.save_resume();
        }
        self.save_session();
    }

    fn find<T>(&self, id: &str, f: impl FnOnce(&mut Entry) -> T) -> Option<T> {
        self.torrents.lock().unwrap().iter_mut().find(|e| e.t.id() == id).map(f)
    }

    fn save_session(&self) {
        let Some(state) = self.config.lock().unwrap().state_dir.clone() else { return };
        let saved: Vec<SavedTorrent> = self
            .torrents
            .lock()
            .unwrap()
            .iter()
            .map(|e| SavedTorrent {
                info_hash: e.t.id(),
                save_dir: e.t.save_dir.clone(),
                paused: e.paused,
                magnet: e.magnet.clone(),
            })
            .collect();
        let tmp = state.join("session.json.tmp");
        let res = serde_json::to_vec_pretty(&saved)
            .map_err(anyhow::Error::from)
            .and_then(|json| Ok(std::fs::write(&tmp, json)?))
            .and_then(|_| Ok(std::fs::rename(&tmp, state.join("session.json"))?));
        if let Err(e) = res {
            log::warn!("could not save session: {e:#}");
        }
    }
}

enum Source {
    Meta(Metainfo),
    Magnet(magnet::Magnet),
}

fn status(t: &Torrent) -> TorrentStatus {
    let phase = t.phase();
    let inner = t.lock();
    let total = inner.meta.as_ref().map(|m| m.total_length);
    let downloaded = inner.picker.as_ref().map(|p| p.have_bytes()).unwrap_or(0);
    let progress = match (phase, &inner.meta) {
        (Phase::Checking, Some(m)) => t.checked_pieces.load(Ordering::Relaxed) as f64 / m.num_pieces().max(1) as f64,
        (_, Some(m)) if m.total_length > 0 => downloaded as f64 / m.total_length as f64,
        (Phase::Completed, _) => 1.0,
        _ => 0.0,
    };
    let rate = if phase == Phase::Downloading || phase == Phase::FetchingMetadata { inner.rate as u64 } else { 0 };
    let eta_secs = match total {
        Some(total) if rate > 0 && phase == Phase::Downloading => Some((total - downloaded) / rate),
        _ => None,
    };
    let save_path = match &inner.meta {
        Some(m) => m.root_path(&t.save_dir),
        None => t.save_dir.clone(),
    };
    let has_video = inner
        .meta
        .as_ref()
        .is_some_and(|m| m.files.iter().any(|f| !f.pad && is_video(&f.path)));
    let file_count = inner.meta.as_ref().map_or(0, |m| m.files.iter().filter(|f| !f.pad).count());
    TorrentStatus {
        file_count,
        has_video,
        id: hex(&t.info_hash),
        name: inner.name.clone(),
        phase,
        progress,
        total_bytes: total,
        downloaded_bytes: downloaded,
        download_rate: rate,
        peers: inner.handshaked,
        seeds: inner.connected_seeds,
        swarm_seeds: inner.swarm.values().filter_map(|s| s.0).max(),
        swarm_leechers: inner.swarm.values().filter_map(|s| s.1).max(),
        known_peers: inner.peers.len(),
        eta_secs,
        save_path: save_path.to_string_lossy().into_owned(),
        error: inner.error.clone(),
    }
}

const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mkv", "mov", "avi", "wmv", "webm", "flv", "mpg", "mpeg", "ts", "m2ts", "3gp", "ogv",
];

fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| VIDEO_EXTENSIONS.iter().any(|v| v.eq_ignore_ascii_case(e)))
}

/// Deletes a torrent's files without touching anything else in the folder.
/// If the torrent's folder contains only its own files, the whole folder goes in one item.
fn delete_download(save_dir: &Path, root: &Path, files: &[PathBuf], use_trash: bool) -> Result<()> {
    if root == save_dir || !root.starts_with(save_dir) || files.iter().any(|f| !f.starts_with(root)) {
        bail!("refusing to delete {}", root.display());
    }
    let mut ctx = trash::TrashContext::default();
    #[cfg(target_os = "macos")]
    {
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        // NSFileManager needs no Finder automation permission.
        ctx.set_delete_method(DeleteMethod::NsFileManager);
    }
    let delete = |paths: &[&Path]| -> Result<()> {
        if use_trash {
            ctx.delete_all(paths)?;
        } else {
            for p in paths {
                if p.is_dir() { std::fs::remove_dir_all(p)? } else { std::fs::remove_file(p)? }
            }
        }
        Ok(())
    };
    if root.is_dir() {
        let ours: HashSet<&Path> = files.iter().map(PathBuf::as_path).collect();
        let mut found = Vec::new();
        collect_files(root, &mut found)?;
        let only_ours = found
            .iter()
            .all(|f| ours.contains(f.as_path()) || f.file_name().is_some_and(|n| n == ".DS_Store"));
        if only_ours {
            return delete(&[root]);
        }
        let existing: Vec<&Path> = files.iter().map(PathBuf::as_path).filter(|f| f.exists()).collect();
        if !existing.is_empty() {
            delete(&existing)?;
        }
        // Tidy up folders the torrent created, deepest first, if they are now empty.
        let mut dirs: Vec<&Path> = files.iter().filter_map(|f| f.parent()).filter(|d| d.starts_with(root)).collect();
        dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        dirs.dedup();
        for d in dirs.into_iter().chain([root]) {
            let _ = std::fs::remove_dir(d);
        }
    } else if files.iter().any(|f| f == root) && root.exists() {
        delete(&[root])?;
    }
    Ok(())
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            collect_files(&entry.path(), out)?;
        } else {
            out.push(entry.path());
        }
    }
    Ok(())
}
