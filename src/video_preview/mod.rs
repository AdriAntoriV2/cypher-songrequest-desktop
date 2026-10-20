
mod helpers;
mod telemetry;

use crate::models::{PlaybackState, PlayerControl};
use crate::MainWindow;

use helpers::{normalize_media_url, resolve_mpv_path};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde_json::{json, Value};
use slint::ComponentHandle;
use std::fs::OpenOptions;
use std::io::Write;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, SW_HIDE, SW_SHOW, SWP_NOACTIVATE, SWP_NOZORDER,
    SetWindowPos, ShowWindow, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS,
};

const IPC_READY_TIMEOUT: Duration = Duration::from_millis(1500);
const IPC_RETRY_INTERVAL: Duration = Duration::from_millis(25);

pub fn native_window_handle(ui: &MainWindow) -> Option<HWND> {
    let slint_handle = ui.window().window_handle();
    let handle = slint_handle.window_handle().ok()?;

    match handle.as_raw() {
        RawWindowHandle::Win32(win32) => Some(win32.hwnd.get() as HWND),
        _ => None,
    }
}

pub struct VideoPreviewController {
    parent_hwnd: Option<HWND>,
    preview_hwnd: Option<HWND>,
    player: Option<Child>,
    current_url: String,
    current_request_id: Option<String>,
    ipc_pipe: Option<String>,
    last_control_revision: u64,
    mpv_path: String,
    api_base_url: String,

    /// Serializes periodic telemetry reports and final EOF reports.
    telemetry_post_lock: Arc<Mutex<()>>,

    /// Invalidates background workers when mpv stops or is replaced.
    observer_generation: Arc<AtomicU64>,
}

impl VideoPreviewController {
    pub fn new(mpv_path: String, api_base_url: String) -> Self {
        Self {
            parent_hwnd: None,
            preview_hwnd: None,
            player: None,
            current_url: String::new(),
            current_request_id: None,
            ipc_pipe: None,
            last_control_revision: 0,
            mpv_path,
            api_base_url,
            telemetry_post_lock: Arc::new(Mutex::new(())),
            observer_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn sync(
        &mut self,
        parent_hwnd: Option<HWND>,
        visible: bool,
        media_url: &str,
        request_id: Option<&str>,
        playback: &PlaybackState,
        control: Option<&PlayerControl>,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) {
        if self.parent_hwnd.is_none() {
            self.parent_hwnd = parent_hwnd;
        }

        let url = normalize_media_url(media_url, &self.api_base_url);

        if url.is_empty() {
            self.stop_player();

            if let Some(hwnd) = self.preview_hwnd {
                unsafe {
                    ShowWindow(hwnd, SW_HIDE);
                }
            }

            self.current_url.clear();
            self.current_request_id = None;
            self.last_control_revision =
                control.map(|control| control.revision).unwrap_or(0);

            return;
        }

        let Some(parent) = self.parent_hwnd else {
            return;
        };

        self.ensure_preview_window(parent, x, y, width, height);

        let Some(hwnd) = self.preview_hwnd else {
            return;
        };

        unsafe {
            ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });
        }

        // Restart mpv on the next sync tick if it exits unexpectedly.
        let player_exited = if let Some(player) = self.player.as_mut() {
            match player.try_wait() {
                Ok(Some(status)) => {
                    eprintln!("[Video] mpv exited unexpectedly: {status}");
                    true
                }
                Ok(None) => false,
                Err(error) => {
                    eprintln!("[Video] Failed to check mpv process: {error}");
                    false
                }
            }
        } else {
            false
        };

        // println!(
        //     "[Video] Sync: current_request_id={:?}, incoming_request_id={:?}, current_url={}, incoming_url={}",
        //     self.current_request_id,
        //     request_id,
        //     self.current_url,
        //     url
        // );

        if player_exited {
            self.stop_player();
            self.current_url.clear();
        }

        let track_changed =
            self.current_url != url || self.current_request_id.as_deref() != request_id;

        if track_changed {
            self.start_player(&url, request_id);

            self.current_url = url;
            self.current_request_id = request_id.map(str::to_owned);

            // Don't apply a stale control intended for the previous track.
            self.last_control_revision =
                control.map(|control| control.revision).unwrap_or(0);

            if self.ipc_pipe.is_some() {
                self.apply_new_track_state(playback);
            }

            return;
        }

        // Controls continue to work when the native preview is hidden.
        if let Some(control) = control {
            if control.revision > self.last_control_revision {
                println!(
                    "[Video] Consuming player control revision {}: {} {:?}",
                    control.revision,
                    control.action,
                    control.value
                );

                self.apply_control(control);
                self.last_control_revision = control.revision;
            }
        }
    }

    fn ensure_preview_window(
        &mut self,
        parent: HWND,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) {
        if self.preview_hwnd.is_none() {
            let class_name: Vec<u16> = "STATIC\0".encode_utf16().collect();
            let window_name: Vec<u16> =
                "Cypher Song Request Video Preview\0".encode_utf16().collect();

            let child = unsafe {
                CreateWindowExW(
                    0,
                    class_name.as_ptr(),
                    window_name.as_ptr(),
                    WS_CHILD | WS_CLIPSIBLINGS | WS_CLIPCHILDREN,
                    x,
                    y,
                    width.max(1),
                    height.max(1),
                    parent,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            };

            if child.is_null() {
                eprintln!("[Video] Failed to create native preview window.");
                return;
            }

            self.preview_hwnd = Some(child);
            println!("[Video] Native preview window created.");
        }

        if let Some(child) = self.preview_hwnd {
            unsafe {
                SetWindowPos(
                    child,
                    std::ptr::null_mut(),
                    x,
                    y,
                    width.max(1),
                    height.max(1),
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
    }

    fn start_player(&mut self, url: &str, request_id: Option<&str>) {
        self.stop_player();

        let Some(mpv_path) = resolve_mpv_path(&self.mpv_path) else {
            eprintln!(
                "[Video] Configured mpv executable was not found: {}",
                self.mpv_path
            );
            eprintln!(
                "[Video] Set playback.mpv_path in config.toml to a valid executable path."
            );
            return;
        };

        let Some(child_hwnd) = self.preview_hwnd else {
            return;
        };

        let pipe_name = format!(
            r"\\.\pipe\cypher-songrequest-mpv-{}",
            std::process::id()
        );

        let wid = format!("--wid={}", child_hwnd as usize);
        let ipc_argument = format!("--input-ipc-server={pipe_name}");

        match Command::new(&mpv_path)
            .arg(wid)
            .args([
                "--force-window=yes",
                "--no-terminal",
                "--really-quiet",
                "--no-osc",
                "--osd-level=0",
                "--keep-open=no",
                "--idle=yes",
            ])
            .arg(ipc_argument)
            .arg("--")
            .arg(url)
            .spawn()
        {
            Ok(player) => {
                self.player = Some(player);
                self.ipc_pipe = Some(pipe_name.clone());

                println!("[Video] mpv executable: {}", mpv_path.display());
                println!("[Video] Preview playback starting: {url}");
                println!("[Video] mpv IPC: {pipe_name}");

                if !self.wait_for_ipc_ready() {
                    eprintln!(
                        "[Video] mpv IPC did not become ready. Check the configured \
                         mpv executable and media URL."
                    );
                    self.stop_player();
                    return;
                }

                println!("[Video] mpv IPC ready.");

                if let Some(request_id) = request_id {
                    telemetry::start_observers(
                        pipe_name,
                        request_id.to_owned(),
                        self.api_base_url.clone(),
                        Arc::clone(&self.observer_generation),
                        self.observer_generation.load(Ordering::SeqCst),
                        Arc::clone(&self.telemetry_post_lock),
                    );
                } else {
                    eprintln!(
                        "[Video] Cannot report playback state without a request ID."
                    );
                }
            }
            Err(error) => {
                eprintln!(
                    "[Video] Failed to launch mpv at {}: {error}",
                    mpv_path.display()
                );
                self.ipc_pipe = None;
            }
        }
    }

    fn wait_for_ipc_ready(&mut self) -> bool {
        let Some(pipe_name) = self.ipc_pipe.clone() else {
            return false;
        };

        let deadline = Instant::now() + IPC_READY_TIMEOUT;

        loop {
            if let Some(player) = self.player.as_mut() {
                match player.try_wait() {
                    Ok(Some(status)) => {
                        eprintln!(
                            "[Video] mpv exited before IPC became ready: {status}"
                        );
                        self.player = None;
                        self.ipc_pipe = None;
                        return false;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        eprintln!("[Video] Failed to check mpv process: {error}");
                        return false;
                    }
                }
            } else {
                return false;
            }

            match OpenOptions::new()
                .read(true)
                .write(true)
                .open(&pipe_name)
            {
                Ok(pipe) => {
                    drop(pipe);
                    return true;
                }
                Err(_) if Instant::now() < deadline => {
                    thread::sleep(IPC_RETRY_INTERVAL);
                }
                Err(error) => {
                    eprintln!(
                        "[Video] Could not connect to mpv IPC pipe after waiting: {error}"
                    );
                    return false;
                }
            }
        }
    }

    /// Apply settings that should carry over to a newly launched track.
    ///
    /// Each track starts in a fresh mpv process at position zero. Do not seek
    /// using playback.position because it may still belong to the previous song.
    fn apply_new_track_state(&self, playback: &PlaybackState) {
        self.send_set_volume(playback.volume);
        self.send_set_paused(false);
    }

    fn apply_control(&self, control: &PlayerControl) {
        match control.action.as_str() {
            "play" => self.send_set_paused(false),
            "pause" => self.send_set_paused(true),
            "seek" => {
                if let Some(position) = control.value {
                    self.send_seek(position);
                } else {
                    eprintln!(
                        "[Video] Seek control revision {} has no target position.",
                        control.revision
                    );
                }
            }
            "volume" => {
                if let Some(volume) = control.value {
                    self.send_set_volume(volume);
                }
            }
            action => eprintln!("[Video] Unknown playback control: {action}"),
        }
    }

    fn send_ipc_command(&self, command: Value) {
        let Some(pipe_name) = self.ipc_pipe.as_deref() else {
            eprintln!("[Video] Cannot send mpv command: IPC pipe is unavailable.");
            return;
        };

        let mut pipe = match OpenOptions::new().write(true).open(pipe_name) {
            Ok(pipe) => pipe,
            Err(error) => {
                eprintln!("[Video] Failed to connect to mpv IPC: {error}");
                return;
            }
        };

        let mut message = command.to_string();
        message.push('\n');

        if let Err(error) = pipe.write_all(message.as_bytes()) {
            eprintln!("[Video] Failed to send mpv IPC command: {error}");
            return;
        }

        if let Err(error) = pipe.flush() {
            eprintln!("[Video] Failed to flush mpv IPC command: {error}");
        }
    }

    fn send_set_paused(&self, paused: bool) {
        println!("[Video] Setting mpv pause={paused}");

        self.send_ipc_command(json!({
            "command": ["set_property", "pause", paused]
        }));
    }

    fn send_set_volume(&self, volume: f64) {
        if !volume.is_finite() {
            return;
        }

        let volume = volume.clamp(0.0, 1.0) * 100.0;

        self.send_ipc_command(json!({
            "command": ["set_property", "volume", volume]
        }));
    }

    fn send_seek(&self, position: f64) {
        if !position.is_finite() || position < 0.0 {
            eprintln!("[Video] Ignoring invalid seek target: {position}");
            return;
        }

        println!(
            "[Video] Sending absolute seek to {:.2}s ({:.0}:{:02.0})",
            position,
            (position / 60.0).floor(),
            position % 60.0
        );

        self.send_ipc_command(json!({
            "command": ["seek", position, "absolute", "exact"]
        }));
    }

    fn stop_player(&mut self) {
        if self.player.is_some() || self.ipc_pipe.is_some() {
            self.observer_generation.fetch_add(1, Ordering::SeqCst);
        }

        if let Some(mut player) = self.player.take() {
            match player.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => {
                    let _ = player.kill();
                    let _ = player.wait();
                }
                Err(error) => {
                    eprintln!("[Video] Failed to check mpv before stopping: {error}");
                    let _ = player.kill();
                    let _ = player.wait();
                }
            }

            println!("[Video] Preview playback stopped.");
        }

        self.ipc_pipe = None;
    }

    fn hide_and_release(&mut self) {
        self.stop_player();

        self.current_url.clear();
        self.current_request_id = None;
        self.last_control_revision = 0;

        if let Some(hwnd) = self.preview_hwnd.take() {
            unsafe {
                ShowWindow(hwnd, SW_HIDE);
                DestroyWindow(hwnd);
            }

            println!("[Video] Native preview window released.");
        }
    }

    pub fn shutdown(&mut self) {
        self.hide_and_release();
        self.parent_hwnd = None;
    }
}

impl Drop for VideoPreviewController {
    fn drop(&mut self) {
        self.shutdown();
    }
}
