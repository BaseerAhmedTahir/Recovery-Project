//! RECOVERY-CORE GUI: a thin shell (SPEC.md 0: "the GUI is a thin client over
//! the same library"). Scans, the results grid, previews, hex and restore call
//! `rc-results`; everything else the CLI can do is reachable through
//! `run_cli`, which runs the `rc` binary beside this one with `--json`.
//!
//! Long work runs on its own thread and reports through `progress` events; the
//! store is locked only for the short moments rows are added or read, so the
//! grid keeps scrolling during a carve.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use base64::Engine;
use rc_results::{Filter, Found, Progress, Row, ScanSummary, Store};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};

struct App {
    store: Mutex<Store>,
    stop: Arc<AtomicBool>,
    busy: AtomicBool,
    data_dir: PathBuf,
}

type Res<T> = Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[derive(Serialize)]
struct DeviceDto {
    path: String,
    kind: String,
    model: Option<String>,
    size_bytes: u64,
    readable: bool,
    note: Option<String>,
    removable: Option<bool>,
    rotational: Option<bool>,
}

#[tauri::command]
fn devices() -> Res<Vec<DeviceDto>> {
    let list = rc_device::enumerate_devices().map_err(err)?;
    Ok(list
        .into_iter()
        .map(|d| DeviceDto {
            path: d.path.to_string_lossy().to_string(),
            kind: format!("{:?}", d.kind),
            size_bytes: d.total_bytes(),
            model: d.model,
            readable: d.readable,
            note: d.access_note,
            removable: d.removable,
            rotational: d.rotational,
        })
        .collect())
}

#[derive(Clone, Serialize)]
struct Done {
    ok: bool,
    summary: Option<ScanSummary>,
    error: Option<String>,
}

fn run_scan(
    app: AppHandle,
    state: &App,
    work: impl FnOnce(&AtomicBool, &mut dyn FnMut(Progress)) -> rc_results::Result<Found>
        + Send
        + 'static,
) -> Res<()> {
    if state.busy.swap(true, Ordering::SeqCst) {
        return Err("a scan is already running".into());
    }
    state.stop.store(false, Ordering::SeqCst);
    let stop = state.stop.clone();
    std::thread::spawn(move || {
        let emitter = app.clone();
        let mut last = std::time::Instant::now() - std::time::Duration::from_secs(1);
        let result = work(&stop, &mut |p: Progress| {
            if last.elapsed().as_millis() >= 100 {
                last = std::time::Instant::now();
                let _ = emitter.emit("progress", p);
            }
        });
        let st = app.state::<App>();
        let done = match result.and_then(|found| st.store.lock().expect("store").add(found)) {
            Ok(summary) => Done {
                ok: true,
                summary: Some(summary),
                error: None,
            },
            Err(e) => Done {
                ok: false,
                summary: None,
                error: Some(e.to_string()),
            },
        };
        st.busy.store(false, Ordering::SeqCst);
        let _ = app.emit("scan-done", done);
    });
    Ok(())
}

#[tauri::command]
fn scan_filesystems(app: AppHandle, state: State<App>, source: String) -> Res<()> {
    run_scan(app, &state, move |stop, progress| {
        rc_results::scan_filesystems(Path::new(&source), stop, progress)
    })
}

#[tauri::command]
fn carve(app: AppHandle, state: State<App>, source: String) -> Res<()> {
    let dir = state.data_dir.join("indexes");
    std::fs::create_dir_all(&dir).map_err(err)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let index = dir.join(format!("carve-{stamp}.rcindex"));
    run_scan(app, &state, move |stop, progress| {
        rc_results::carve(Path::new(&source), &index, stop, progress)
    })
}

#[tauri::command]
fn stop(state: State<App>) {
    state.stop.store(true, Ordering::SeqCst);
}

#[tauri::command]
async fn set_view(app: AppHandle, filter: Filter) -> Res<u64> {
    tauri::async_runtime::spawn_blocking(move || {
        let st = app.state::<App>();
        let mut store = st.store.lock().map_err(err)?;
        store.set_view(&filter).map_err(err)
    })
    .await
    .map_err(err)?
}

#[tauri::command]
fn rows(state: State<App>, start: u64, count: u64) -> Res<Vec<Row>> {
    state
        .store
        .lock()
        .map_err(err)?
        .rows(start, count.min(500))
        .map_err(err)
}

#[tauri::command]
fn view_len(state: State<App>) -> Res<u64> {
    Ok(state.store.lock().map_err(err)?.view_len())
}

#[tauri::command]
async fn synthetic(app: AppHandle, count: u64) -> Res<u64> {
    tauri::async_runtime::spawn_blocking(move || {
        let st = app.state::<App>();
        let mut store = st.store.lock().map_err(err)?;
        store.add_synthetic(count.min(20_000_000)).map_err(err)?;
        Ok(store.view_len())
    })
    .await
    .map_err(err)?
}

#[tauri::command]
fn clear(state: State<App>) -> Res<()> {
    state.store.lock().map_err(err)?.clear().map_err(err)
}

#[tauri::command]
async fn preview(app: AppHandle, id: i64) -> Res<String> {
    tauri::async_runtime::spawn_blocking(move || {
        let st = app.state::<App>();
        let store = st.store.lock().map_err(err)?;
        let png = rc_results::preview(&store, id, 480).map_err(err)?;
        Ok(format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png)
        ))
    })
    .await
    .map_err(err)?
}

#[tauri::command]
fn hex(state: State<App>, id: i64, skip: u64, length: u64) -> Res<rc_preview::HexView> {
    let store = state.store.lock().map_err(err)?;
    rc_results::hex(&store, id, skip, length).map_err(err)
}

#[tauri::command]
async fn restore(
    app: AppHandle,
    ids: Vec<i64>,
    out: String,
    reassemble: bool,
) -> Res<Vec<rc_restore_dto::Written>> {
    tauri::async_runtime::spawn_blocking(move || {
        let st = app.state::<App>();
        let store = st.store.lock().map_err(err)?;
        let written = rc_results::restore(&store, &ids, Path::new(&out), reassemble).map_err(err)?;
        Ok(written
            .into_iter()
            .map(|w| rc_restore_dto::Written {
                source: w.source,
                dest: w.dest.to_string_lossy().to_string(),
                bytes: w.bytes,
                sha256: w.sha256,
                notes: w.notes,
            })
            .collect())
    })
    .await
    .map_err(err)?
}

mod rc_restore_dto {
    #[derive(serde::Serialize)]
    pub struct Written {
        pub source: String,
        pub dest: String,
        pub bytes: u64,
        pub sha256: String,
        pub notes: Vec<String>,
    }
}

#[derive(Serialize)]
struct CliOutput {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    program: String,
}

/// The `rc` binary: beside this executable when installed, else in the
/// workspace's build output during development, else on PATH.
fn rc_binary() -> PathBuf {
    let exe = if cfg!(windows) { "rc.exe" } else { "rc" };
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            let beside = dir.join(exe);
            if beside.is_file() {
                return beside;
            }
            for up in dir.ancestors() {
                for profile in ["release", "debug"] {
                    let p = up.join("target").join(profile).join(exe);
                    if p.is_file() && !p.starts_with(up.join("gui")) {
                        return p;
                    }
                }
            }
        }
    }
    PathBuf::from(exe)
}

/// Run any `rc` subcommand with `--json`. This is how every CLI capability
/// without a dedicated screen stays reachable.
#[tauri::command]
async fn run_cli(args: Vec<String>) -> Res<CliOutput> {
    tauri::async_runtime::spawn_blocking(move || {
        let program = rc_binary();
        let mut cmd = std::process::Command::new(&program);
        cmd.arg("--json").args(&args);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let out = cmd
            .output()
            .map_err(|e| format!("could not run {}: {e}", program.display()))?;
        Ok(CliOutput {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            program: program.display().to_string(),
        })
    })
    .await
    .map_err(err)?
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let data_dir = rc_results::default_store_path()
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(std::env::temp_dir);
            std::fs::create_dir_all(&data_dir)?;
            let store = Store::open(&data_dir.join("results.sqlite"))?;
            app.manage(App {
                store: Mutex::new(store),
                stop: Arc::new(AtomicBool::new(false)),
                busy: AtomicBool::new(false),
                data_dir,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            devices,
            scan_filesystems,
            carve,
            stop,
            set_view,
            rows,
            view_len,
            synthetic,
            clear,
            preview,
            hex,
            restore,
            run_cli
        ])
        .run(tauri::generate_context!())
        .expect("error while running RECOVERY-CORE");
}
