//! A download-only BitTorrent engine: `.torrent` files and magnet links,
//! HTTP/UDP trackers, DHT, peer exchange, rarest-first piece picking and resume.

pub mod bencode;
mod client;
mod dht;
pub mod magnet;
pub mod metainfo;
mod peer;
mod picker;
mod storage;
mod torrent;
pub mod tracker;

pub use client::{Client, Config, FileStatus, TorrentStatus};
pub use torrent::Phase;
