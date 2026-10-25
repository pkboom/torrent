# Torrent

A download-only BitTorrent client for macOS, written in Rust.

- **engine/**: the BitTorrent engine (library). It handles `.torrent` files and magnet links, HTTP/UDP trackers, DHT, peer exchange, metadata exchange, rarest-first piece picking with endgame mode, SHA-1 verification, and resume.
- **app/**: the Mac app, built with Tauri. The UI lives in `app/ui`.

The client never uploads: it doesn't accept incoming connections, unchoke peers or serve data.

## Using it

There are no add buttons. Click a `magnet:` link in your browser or double-click a `.torrent` file and the download starts; dropping a `.torrent` onto the window also works. Downloads go to `~/Downloads`.

- Each row has a Start/Pause button and a Delete button. Delete moves the files to the Trash, or removes the torrent from the list only.
- Once a video has finished, a Play button opens it in your default player.
- Multi-file torrents expand to show every file with its own progress.
- Select rows with the checkboxes (or ⌘A) to start, pause or delete them together.
- The Seeds column shows the seeds you're connected to and the seeds in the whole swarm.
- Double-click a row to play it or show it in Finder.

Closing the window keeps downloads running; quit with ⌘Q. The torrent list and progress are saved in `~/Library/Application Support/com.keunbae.torrent/`, so downloads resume after a restart.

## Build

```sh
cargo install tauri-cli --version "^2" --locked   # once
cd app/src-tauri && cargo tauri build
```

This produces `target/release/bundle/macos/Torrent.app` and a `.dmg` next to it. Copy the app into `/Applications` so macOS uses it for magnet links and `.torrent` files.

## Tests

```sh
cargo test -p torrent-engine
```

The integration tests download a multi-file torrent from an in-process seeder, both from a `.torrent` file and from a magnet link. They also cover:

- hash-checking existing data
- resuming a session
- pause and resume
- slow and misbehaving peers
