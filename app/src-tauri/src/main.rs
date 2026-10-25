#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::{AppHandle, DragDropEvent, Manager, RunEvent, State, WindowEvent};
use torrent_engine::{Client, Config, FileStatus, TorrentStatus};

struct AppState {
    client: Client,
}

#[derive(Serialize, Deserialize)]
struct Settings {
    download_dir: PathBuf,
}

type CmdResult<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn open_path(args: &[&std::ffi::OsStr]) -> CmdResult<()> {
    std::process::Command::new("open").args(args).spawn().map_err(err)?;
    Ok(())
}

#[tauri::command]
async fn list_torrents(state: State<'_, AppState>) -> CmdResult<Vec<TorrentStatus>> {
    Ok(state.client.list())
}

#[tauri::command]
async fn pause(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.client.pause(&id).map_err(err)
}

#[tauri::command]
async fn resume(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.client.resume(&id).map_err(err)
}

/// Removes torrents and moves their downloaded files to the Trash.
#[tauri::command]
async fn delete(state: State<'_, AppState>, ids: Vec<String>, delete_files: bool) -> CmdResult<()> {
    let mut errors = Vec::new();
    for id in &ids {
        if let Err(e) = state.client.remove(id, delete_files).await {
            errors.push(e.to_string());
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(errors.join("\n")) }
}

/// Opens the torrent's main video in the default player.
#[tauri::command]
async fn play(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let path = state.client.video_path(&id).ok_or("This torrent has no video file.")?;
    if !path.exists() {
        return Err("The video file is missing. It may have been moved or deleted.".into());
    }
    open_path(&[path.as_os_str()])
}

#[tauri::command]
async fn list_files(state: State<'_, AppState>, id: String) -> CmdResult<Vec<FileStatus>> {
    Ok(state.client.files(&id).unwrap_or_default())
}

/// Opens one file with its default app (e.g. a video in the default player).
#[tauri::command]
async fn open_file(state: State<'_, AppState>, id: String, index: usize) -> CmdResult<()> {
    let path = state.client.file_path(&id, index).ok_or("No such file.")?;
    if !path.exists() {
        return Err("The file is missing. It may have been moved or deleted.".into());
    }
    open_path(&[path.as_os_str()])
}

#[tauri::command]
async fn reveal_file(state: State<'_, AppState>, id: String, index: usize) -> CmdResult<()> {
    let path = state.client.file_path(&id, index).ok_or("No such file.")?;
    if path.exists() {
        open_path(&["-R".as_ref(), path.as_os_str()])
    } else {
        let dir = state.client.content_path(&id).unwrap_or_else(|| state.client.download_dir());
        open_path(&[dir.as_os_str()])
    }
}

#[tauri::command]
async fn reveal(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let path = state.client.content_path(&id).unwrap_or_else(|| state.client.download_dir());
    if path.exists() {
        open_path(&["-R".as_ref(), path.as_os_str()])
    } else {
        open_path(&[path.parent().unwrap_or(&path).as_os_str()])
    }
}

/// Links and files macOS hands us while the app is still starting up (e.g. a magnet
/// click that launches it). They are added once setup has created the client.
static PENDING: Mutex<Option<Vec<String>>> = Mutex::new(Some(Vec::new()));

/// Adds `.torrent` files and magnet links handed to us by Finder, the browser, or drag & drop.
fn open_inputs(app: &AppHandle, inputs: Vec<String>) {
    if let Some(queue) = PENDING.lock().unwrap().as_mut() {
        queue.extend(inputs);
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        for input in inputs {
            let res = if input.starts_with("magnet:") {
                state.client.add_magnet(&input)
            } else {
                std::fs::read(Path::new(&input))
                    .map_err(Into::into)
                    .and_then(|bytes| state.client.add_torrent_bytes(&bytes))
            };
            if let Err(e) = res {
                eprintln!("could not add {input}: {e:#}");
            }
        }
        if let Some(w) = app.get_webview_window("main") {
            let _ = w.unminimize();
            let _ = w.show();
            let _ = w.set_focus();
        }
    });
}

fn main() {
    let app = tauri::Builder::default()
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let settings_path = data_dir.join("settings.json");
            let download_dir = std::fs::read(&settings_path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Settings>(&b).ok())
                .map(|s| s.download_dir)
                .or_else(|| app.path().download_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));

            let client = Client::new(Config {
                download_dir,
                state_dir: Some(data_dir.join("session")),
                ..Config::default()
            });
            let restore = client.clone();
            tauri::async_runtime::block_on(async move { restore.restore() })?;
            app.manage(AppState { client });
            let queued = PENDING.lock().unwrap().take().unwrap_or_default();
            if !queued.is_empty() {
                open_inputs(app.handle(), queued);
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps downloads running; quit with ⌘Q.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
            if let WindowEvent::DragDrop(DragDropEvent::Drop { paths, .. }) = event {
                let files = paths
                    .iter()
                    .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("torrent")))
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                open_inputs(window.app_handle(), files);
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_torrents,
            pause,
            resume,
            delete,
            play,
            reveal,
            list_files,
            open_file,
            reveal_file,
        ])
        .build(tauri::generate_context!())
        .expect("error while building app");

    app.run(|handle, event| match event {
        #[cfg(target_os = "macos")]
        RunEvent::Opened { urls } => {
            let inputs = urls
                .into_iter()
                .filter_map(|u| match u.scheme() {
                    "file" => u.to_file_path().ok().map(|p| p.to_string_lossy().into_owned()),
                    "magnet" => Some(u.to_string()),
                    _ => None,
                })
                .collect();
            open_inputs(handle, inputs);
        }
        // Clicking the Dock icon brings the window back after it was closed.
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => {
            if let Some(w) = handle.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }
        RunEvent::Exit => {
            if let Some(state) = handle.try_state::<AppState>() {
                state.client.shutdown();
            }
        }
        _ => {}
    });
}
