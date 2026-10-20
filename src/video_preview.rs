use crate::MainWindow;
use crate::backend::API_BASE_URL;
use crate::models::{PlaybackState, PlayerControl};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde_json::json;
use slint::ComponentHandle;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, SW_HIDE, SW_SHOW, SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos,
    ShowWindow, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS,
};

pub fn native_window_handle(ui: &MainWindow) -> Option<HWND> {
    let slint_handle = ui.window().window_handle();
    let handle = slint_handle.window_handle().ok()?;

    match handle.as_raw() {
        RawWindowHandle::Win32(win32) => Some(win32.hwnd.get() as HWND),
        _ => None,
    }
}

pub fn locate_mpv() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("MPV_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }

    let mut candidates = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            candidates.push(exe_dir.join("resources").join("mpv").join("mpv.exe"));
            candidates.push(exe_dir.join("mpv.exe"));
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("resources").join("mpv").join("mpv.exe"));
    }

    candidates.into_iter().find(|path| path.is_file())
}

pub fn normalize_media_url(url: &str) -> String {
    let url = url.trim();

    if url.is_empty() {
        String::new()
    } else if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else if url.starts_with('/') {
        format!("{API_BASE_URL}{url}")
    } else {
        format!("{API_BASE_URL}/{url}")
    }
}

#[derive(Default)]
pub struct VideoPreviewController {
    parent_hwnd: Option<HWND>,
    preview_hwnd: Option<HWND>,
    player: Option<Child>,
    current_url: String,
    current_request_id: Option<String>,
    ipc_pipe: Option<String>,
    last_control_revision: u64,
}

impl VideoPreviewController {
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

        // The toggle is authoritative. Closing the preview hides the
        // native child and stops playback, but retains the HWND.
        if !visible {
            self.stop_player();

            if let Some(hwnd) = self.preview_hwnd {
                unsafe {
                    ShowWindow(hwnd, SW_HIDE);
                }
            }

            self.current_url.clear();
            self.current_request_id = None;
            self.last_control_revision = control.map(|control| control.revision).unwrap_or(0);

            return;
        }

        let Some(parent) = self.parent_hwnd else {
            return;
        };

        self.ensure_preview_window(parent, x, y, width, height);

        let url = normalize_media_url(media_url);

        if url.is_empty() {
            self.stop_player();

            if let Some(hwnd) = self.preview_hwnd {
                unsafe {
                    ShowWindow(hwnd, SW_HIDE);
                }
            }

            self.current_url.clear();
            self.current_request_id = None;
            self.last_control_revision = control.map(|control| control.revision).unwrap_or(0);

            return;
        }

        let Some(hwnd) = self.preview_hwnd else {
            return;
        };

        unsafe {
            ShowWindow(hwnd, SW_SHOW);
        }

        let track_changed =
            self.current_url != url || self.current_request_id.as_deref() != request_id;

        if track_changed {
            self.start_player(&url);

            self.current_url = url;
            self.current_request_id = request_id.map(str::to_owned);
            self.last_control_revision = control.map(|control| control.revision).unwrap_or(0);

            self.apply_playback_state(playback);
            return;
        }

        if let Some(control) = control {
            if control.revision > self.last_control_revision {
                self.apply_control(control);
                self.last_control_revision = control.revision;
            }
        }
    }

    fn ensure_preview_window(&mut self, parent: HWND, x: i32, y: i32, width: i32, height: i32) {
        if self.preview_hwnd.is_none() {
            let class_name: Vec<u16> = "STATIC\0".encode_utf16().collect();
            let window_name: Vec<u16> = "Cypher Song Request Video Preview\0"
                .encode_utf16()
                .collect();

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

    fn start_player(&mut self, url: &str) {
        self.stop_player();

        let Some(mpv_path) = locate_mpv() else {
            eprintln!(
                "[Video] mpv.exe was not found. Place it in \
                 resources\\mpv or set MPV_PATH."
            );
            return;
        };

        let Some(child_hwnd) = self.preview_hwnd else {
            return;
        };

        let pipe_name = format!(r"\\.\pipe\cypher-songrequest-mpv-{}", std::process::id());

        let wid = format!("--wid={}", child_hwnd as usize);

        match Command::new(&mpv_path)
            .arg(wid)
            .args([
                "--force-window=yes",
                "--no-terminal",
                "--really-quiet",
                "--no-osc",
                "--osd-level=0",
                "--keep-open=no",
                "--input-ipc-server",
                &pipe_name,
                "--",
            ])
            .arg(url)
            .spawn()
        {
            Ok(player) => {
                self.player = Some(player);
                self.ipc_pipe = Some(pipe_name.clone());

                println!("[Video] Preview playback started: {url}");
                println!("[Video] mpv IPC: {pipe_name}");
            }
            Err(error) => {
                eprintln!("[Video] Failed to launch mpv: {error}");
                self.ipc_pipe = None;
            }
        }
    }

    fn apply_playback_state(&self, playback: &PlaybackState) {
        self.send_set_volume(playback.volume);

        if playback.position > 0.05 {
            self.send_seek(playback.position);
        }

        self.send_set_paused(playback.paused);
    }

    fn apply_control(&self, control: &PlayerControl) {
        match control.action.as_str() {
            "play" => self.send_set_paused(false),
            "pause" => self.send_set_paused(true),
            "seek" => {
                if let Some(position) = control.value {
                    self.send_seek(position);
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

    fn send_ipc_command(&self, command: serde_json::Value) {
        let Some(pipe_name) = self.ipc_pipe.as_deref() else {
            return;
        };

        let mut pipe = match std::fs::OpenOptions::new().write(true).open(pipe_name) {
            Ok(pipe) => pipe,
            Err(error) => {
                eprintln!("[Video] Failed to open mpv IPC pipe: {error}");
                return;
            }
        };

        let mut message = command.to_string();
        message.push('\n');

        if let Err(error) = pipe.write_all(message.as_bytes()) {
            eprintln!("[Video] Failed to send mpv IPC command: {error}");
        }
    }

    fn send_set_paused(&self, paused: bool) {
        self.send_ipc_command(json!({
            "command": ["set_property", "pause", paused]
        }));
    }

    fn send_set_volume(&self, volume: f64) {
        let volume = volume.clamp(0.0, 1.0) * 100.0;

        self.send_ipc_command(json!({
            "command": ["set_property", "volume", volume]
        }));
    }

    fn send_seek(&self, position: f64) {
        if !position.is_finite() || position < 0.0 {
            return;
        }

        self.send_ipc_command(json!({
            "command": ["seek", position, "absolute", "exact"]
        }));
    }

    fn stop_player(&mut self) {
        if let Some(mut player) = self.player.take() {
            let _ = player.kill();
            let _ = player.wait();
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
