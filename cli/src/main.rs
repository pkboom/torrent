//! Command-line downloader: `torrent <file.torrent | magnet:...> [download-dir]`.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;
use torrent_engine::{Client, Config, Phase};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let source = args.next().context("usage: torrent <file.torrent | magnet:?...> [download-dir]")?;
    let dir = args.next().map(PathBuf::from).unwrap_or(std::env::current_dir()?);

    let client = Client::new(Config { download_dir: dir, ..Config::default() });
    let id = if source.starts_with("magnet:") {
        client.add_magnet(&source)?
    } else {
        client.add_torrent_bytes(&std::fs::read(&source).with_context(|| format!("reading {source}"))?)?
    };

    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let s = client.status(&id).context("torrent vanished")?;
        let total = s.total_bytes.map(mb).unwrap_or_else(|| "?".into());
        print!(
            "\r\x1b[2K{:?} {:5.1}%  {}/{} MB  {:.2} MB/s  peers {}/{}  {}",
            s.phase,
            s.progress * 100.0,
            mb(s.downloaded_bytes),
            total,
            s.download_rate as f64 / 1e6,
            s.peers,
            s.known_peers,
            s.name,
        );
        std::io::stdout().flush()?;
        match s.phase {
            Phase::Completed => {
                println!("\nSaved to {}", s.save_path);
                return Ok(());
            }
            Phase::Error => anyhow::bail!("\n{}", s.error.unwrap_or_default()),
            _ => {}
        }
    }
}

fn mb(b: u64) -> String {
    format!("{:.1}", b as f64 / 1e6)
}
