//! End-to-end downloads against an in-process seeder on localhost.

use sha1::{Digest, Sha1};
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use torrent_engine::bencode::{encode, Value};
use torrent_engine::{Client, Config, Phase};

const PIECE: usize = 32 * 1024;
const SEEDER_UT_METADATA: u8 = 3;

struct Fixture {
    data: Vec<u8>,
    info: Vec<u8>,
    files: Vec<(String, usize)>,
}

fn fixture() -> Fixture {
    // Odd sizes so files straddle piece and block boundaries; includes an empty file.
    let files = vec![("a.bin".to_string(), 100_003), ("sub/b.bin".to_string(), 0), ("sub/c.bin".to_string(), 250_017)];
    let total: usize = files.iter().map(|f| f.1).sum();
    let data: Vec<u8> = (0..total).map(|i| (i * 31 % 251) as u8).collect();
    let pieces: Vec<u8> = data.chunks(PIECE).flat_map(|c| Sha1::digest(c).to_vec()).collect();
    let file_list = files
        .iter()
        .map(|(p, len)| {
            Value::dict([
                ("length", Value::Int(*len as i64)),
                ("path", Value::List(p.split('/').map(|s| Value::bytes(s)).collect())),
            ])
        })
        .collect();
    let info = encode(&Value::dict([
        ("name", Value::bytes("fixture")),
        ("piece length", Value::Int(PIECE as i64)),
        ("pieces", Value::bytes(pieces)),
        ("files", Value::List(file_list)),
    ]));
    Fixture { data, info, files }
}

fn torrent_file(f: &Fixture) -> Vec<u8> {
    let mut out = b"d4:info".to_vec();
    out.extend_from_slice(&f.info);
    out.push(b'e');
    out
}

#[derive(Clone, Copy, Default)]
struct Behaviour {
    per_block: Duration,
    /// Wait before sending the bitfield, like slow real-world peers.
    bitfield_delay: Duration,
    /// Answer the first request with a short block, like a buggy or hostile peer.
    short_first_block: bool,
}

async fn seeder(f: &Fixture) -> SocketAddr {
    seeder_with(f, Behaviour::default()).await
}

async fn slow_seeder(f: &Fixture, per_block: Duration) -> SocketAddr {
    seeder_with(f, Behaviour { per_block, ..Default::default() }).await
}

async fn seeder_with(f: &Fixture, b: Behaviour) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (data, info) = (f.data.clone(), f.info.clone());
    let info_hash: [u8; 20] = Sha1::digest(&info).into();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            let (data, info) = (data.clone(), info.clone());
            tokio::spawn(async move {
                let _ = serve(sock, &data, &info, info_hash, b).await;
            });
        }
    });
    addr
}

async fn send_msg(s: &mut TcpStream, id: u8, body: &[u8]) -> std::io::Result<()> {
    let mut m = ((body.len() + 1) as u32).to_be_bytes().to_vec();
    m.push(id);
    m.extend_from_slice(body);
    s.write_all(&m).await
}

async fn serve(mut s: TcpStream, data: &[u8], info: &[u8], info_hash: [u8; 20], b: Behaviour) -> std::io::Result<()> {
    let mut hs = [0u8; 68];
    s.read_exact(&mut hs).await?;
    assert_eq!(&hs[28..48], &info_hash);
    let mut reply = vec![19];
    reply.extend_from_slice(b"BitTorrent protocol");
    reply.extend_from_slice(&[0, 0, 0, 0, 0, 0x10, 0, 0]);
    reply.extend_from_slice(&info_hash);
    reply.extend_from_slice(b"-SEED00-000000000000");
    s.write_all(&reply).await?;

    let ext = encode(&Value::dict([
        ("m", Value::dict([("ut_metadata", Value::Int(SEEDER_UT_METADATA as i64))])),
        ("metadata_size", Value::Int(info.len() as i64)),
    ]));
    let mut body = vec![0];
    body.extend_from_slice(&ext);
    send_msg(&mut s, 20, &body).await?;
    let npieces = data.len().div_ceil(PIECE);
    let mut bits = vec![0u8; npieces.div_ceil(8)];
    for i in 0..npieces {
        bits[i / 8] |= 0x80 >> (i % 8);
    }
    tokio::time::sleep(b.bitfield_delay).await;
    send_msg(&mut s, 5, &bits).await?;
    send_msg(&mut s, 1, &[]).await?;

    let mut client_ut_metadata = 0;
    let mut first = true;
    loop {
        let len = s.read_u32().await? as usize;
        if len == 0 {
            continue;
        }
        let mut buf = vec![0u8; len];
        s.read_exact(&mut buf).await?;
        match buf[0] {
            6 => {
                let u = |i: usize| u32::from_be_bytes(buf[i..i + 4].try_into().unwrap()) as usize;
                let (index, begin, length) = (u(1), u(5), u(9));
                tokio::time::sleep(b.per_block).await;
                let start = index * PIECE + begin;
                let mut body = buf[1..9].to_vec();
                let length = if b.short_first_block && std::mem::take(&mut first) { 100 } else { length };
                body.extend_from_slice(&data[start..start + length]);
                send_msg(&mut s, 7, &body).await?;
            }
            20 if buf[1] == 0 => {
                let hs = torrent_engine::bencode::decode(&buf[2..]).unwrap();
                client_ut_metadata = hs.get("m").unwrap().get("ut_metadata").unwrap().as_int().unwrap() as u8;
            }
            20 if buf[1] == SEEDER_UT_METADATA => {
                let req = torrent_engine::bencode::decode(&buf[2..]).unwrap();
                let piece = req.get("piece").unwrap().as_int().unwrap() as usize;
                let chunk = &info[piece * 16384..info.len().min((piece + 1) * 16384)];
                let mut body = vec![client_ut_metadata];
                body.extend_from_slice(&encode(&Value::dict([
                    ("msg_type", Value::Int(1)),
                    ("piece", Value::Int(piece as i64)),
                    ("total_size", Value::Int(info.len() as i64)),
                ])));
                body.extend_from_slice(chunk);
                send_msg(&mut s, 20, &body).await?;
            }
            _ => {}
        }
    }
}

fn config(download: &Path, state: Option<&Path>) -> Config {
    Config {
        download_dir: download.to_path_buf(),
        state_dir: state.map(Path::to_path_buf),
        enable_dht: false,
        max_peers_per_torrent: 10,
        use_trash: false,
    }
}

async fn wait_for(client: &Client, id: &str, phase: Phase) {
    for _ in 0..300 {
        let s = client.status(id).unwrap();
        if s.phase == phase {
            return;
        }
        assert_ne!(s.phase, Phase::Error, "torrent failed: {:?}", s.error);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {phase:?}: {:?}", client.status(id));
}

fn assert_files(f: &Fixture, dir: &Path) {
    let mut offset = 0;
    for (path, len) in &f.files {
        let got = std::fs::read(dir.join("fixture").join(path)).unwrap();
        assert_eq!(got.len(), *len, "{path}");
        assert!(got == f.data[offset..offset + len], "{path} content differs");
        offset += len;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_from_torrent_file_and_resumes() {
    let f = fixture();
    let seed = seeder(&f).await;
    let dl = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();

    let client = Client::new(config(dl.path(), Some(state.path())));
    let id = client.add_torrent_bytes(&torrent_file(&f)).unwrap();
    client.add_peers(&id, vec![seed]).unwrap();
    wait_for(&client, &id, Phase::Completed).await;
    let st = client.status(&id).unwrap();
    assert_eq!(st.progress, 1.0);
    assert_eq!(st.downloaded_bytes, f.data.len() as u64);
    assert_files(&f, dl.path());
    let files = client.files(&id).unwrap();
    assert_eq!(files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["a.bin", "sub/b.bin", "sub/c.bin"]);
    assert!(files.iter().all(|f| f.downloaded_bytes == f.length));
    assert_eq!(client.file_path(&id, 2).unwrap(), dl.path().join("fixture/sub/c.bin"));
    client.shutdown();

    // A fresh client restores the session and recognises the finished download without any peers.
    let again = Client::new(config(dl.path(), Some(state.path())));
    again.restore().unwrap();
    wait_for(&again, &id, Phase::Completed).await;
    assert_eq!(again.list().len(), 1);

    // A file the torrent didn't create must survive deleting the download.
    std::fs::write(dl.path().join("fixture/sub/mine.txt"), b"keep").unwrap();
    again.remove(&id, true).await.unwrap();
    assert!(dl.path().join("fixture/sub/mine.txt").exists());
    assert!(!dl.path().join("fixture/a.bin").exists());
    assert!(!dl.path().join("fixture/sub/c.bin").exists());
    assert!(again.list().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_from_magnet_via_metadata_exchange() {
    let f = fixture();
    let seed = seeder(&f).await;
    let dl = tempfile::tempdir().unwrap();
    let hash: [u8; 20] = Sha1::digest(&f.info).into();
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();

    let client = Client::new(config(dl.path(), None));
    let id = client.add_magnet(&format!("magnet:?xt=urn:btih:{hex}&dn=fixture&x.pe={seed}")).unwrap();
    assert_eq!(id, hex);
    wait_for(&client, &id, Phase::Completed).await;
    assert_eq!(client.status(&id).unwrap().name, "fixture");
    assert_files(&f, dl.path());
}

#[tokio::test(flavor = "multi_thread")]
async fn rechecks_partial_data_without_resume_file() {
    let f = fixture();
    let dl = tempfile::tempdir().unwrap();
    // Pre-place the first file only; the hash check should credit exactly the pieces it fully covers.
    std::fs::create_dir_all(dl.path().join("fixture")).unwrap();
    std::fs::write(dl.path().join("fixture/a.bin"), &f.data[..100_003]).unwrap();

    let client = Client::new(config(dl.path(), None));
    let id = client.add_torrent_bytes(&torrent_file(&f)).unwrap();
    wait_for(&client, &id, Phase::Downloading).await;
    let st = client.status(&id).unwrap();
    assert_eq!(st.downloaded_bytes, (100_003 / PIECE * PIECE) as u64);
    // a.bin is only credited for the pieces that lie entirely inside it.
    let files = client.files(&id).unwrap();
    assert_eq!(files[0].downloaded_bytes, (100_003 / PIECE * PIECE) as u64);
    assert_eq!(files[2].downloaded_bytes, 0);

    let seed = seeder(&f).await;
    client.add_peers(&id, vec![seed]).unwrap();
    wait_for(&client, &id, Phase::Completed).await;
    assert_files(&f, dl.path());
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_and_resume_mid_download() {
    let f = fixture();
    let seed = slow_seeder(&f, Duration::from_millis(40)).await;
    let dl = tempfile::tempdir().unwrap();
    let client = Client::new(config(dl.path(), None));
    let id = client.add_torrent_bytes(&torrent_file(&f)).unwrap();
    client.add_peers(&id, vec![seed]).unwrap();

    for _ in 0..200 {
        if client.status(&id).unwrap().downloaded_bytes > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    client.pause(&id).unwrap();
    let paused = client.status(&id).unwrap();
    assert_eq!(paused.phase, Phase::Paused);
    assert!(paused.downloaded_bytes > 0 && paused.downloaded_bytes < f.data.len() as u64);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(client.status(&id).unwrap().phase, Phase::Paused);

    // Requests outstanding at pause time must be re-requested after resuming.
    client.resume(&id).unwrap();
    client.add_peers(&id, vec![seed]).unwrap();
    wait_for(&client, &id, Phase::Completed).await;
    assert_files(&f, dl.path());
}

#[tokio::test(flavor = "multi_thread")]
async fn seed_with_late_bitfield_is_used() {
    let f = fixture();
    let seed = seeder_with(&f, Behaviour { bitfield_delay: Duration::from_millis(300), ..Default::default() }).await;
    let dl = tempfile::tempdir().unwrap();
    let client = Client::new(config(dl.path(), None));
    let id = client.add_torrent_bytes(&torrent_file(&f)).unwrap();
    client.add_peers(&id, vec![seed]).unwrap();
    wait_for(&client, &id, Phase::Completed).await;
    assert_files(&f, dl.path());
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_length_block_does_not_stall_download() {
    let f = fixture();
    let bad = seeder_with(&f, Behaviour { short_first_block: true, ..Default::default() }).await;
    let dl = tempfile::tempdir().unwrap();
    let client = Client::new(config(dl.path(), None));
    let id = client.add_torrent_bytes(&torrent_file(&f)).unwrap();
    client.add_peers(&id, vec![bad]).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let good = seeder(&f).await;
    client.add_peers(&id, vec![good]).unwrap();
    wait_for(&client, &id, Phase::Completed).await;
    assert_files(&f, dl.path());
}
