#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::RefCell;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};
use std::rc::Rc;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use arboard::Clipboard;
use open::that;
use slint::{ComponentHandle, ModelRc, SharedString, Timer, VecModel};

use windows_sys::Win32::Foundation::{
    CloseHandle,
    GetLastError,
    HANDLE,
    INVALID_HANDLE_VALUE,
    ERROR_ALREADY_EXISTS,
};

use windows_sys::Win32::System::Threading::{
    CreateMutexW,
    OpenThread,
    ResumeThread,
    CREATE_SUSPENDED,
    THREAD_SUSPEND_RESUME,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, THREADENTRY32, TH32CS_SNAPTHREAD,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JobObjectExtendedLimitInformation,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

mod backend;
mod config;
mod models;
mod video_preview;
mod desktop_updater;

use models::PlaybackSyncState;
use video_preview::{native_window_handle, VideoPreviewController};

slint::include_modules!();

const VIDEO_SYNC_INTERVAL: Duration = Duration::from_millis(100);
const PATCHER_EXE: &str = "CypherSR.exe";
const DATA_DIRECTORY: &str = "data";
const SONG_REQUEST_HOME_ENV: &str = "SONG_REQUEST_HOME";

type PlayerControlRequest = (String, Option<f64>);

/// Owns the updater and its Windows Job Object.
///
/// When the Job Object handle closes, Windows terminates processes in the job,
/// including the core backend if the updater has already exited.
struct BackendPatcher {
    _child: Child,
    _job: OwnedHandle,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Acquire the single-instance lock BEFORE starting the updater.
    let _single_instance = match acquire_single_instance() {
        Ok(handle) => handle,

        Err(error)
            if error.kind() == std::io::ErrorKind::AlreadyExists =>
        {
            show_error_dialog(
                "CypherSR Desktop is already running.\n\n\
                 Check your taskbar or system tray for the existing window."
            );

            return Ok(());
        }

        Err(error) => {
            show_error_dialog(&format!(
                "Could not enforce single-instance mode:\n\n{error}"
            ));

            return Err(error.into());
        }
    };

    // Only one desktop instance may reach this point.
    if desktop_updater::check_and_apply_update() {
        // PowerShell is waiting for this process to exit before replacing the EXE.
        return Ok(());
    }

    // Only the instance holding the mutex can reach this point.
    // Start the updater after the single-instance check.
    let backend_patcher = match launch_backend_patcher() {
        Ok(process) => process,
        Err(error) => {
            show_error_dialog(&format!(
                "Could not start CypherSR updater:\n\n{error}"
            ));

            return Err(error.into());
        }
    };

    let config = config::AppConfig::load_or_create()?;
    let api_base_url = config.api_base_url();

    println!("[Config] Backend address: {api_base_url}");

    let ui = MainWindow::new()?;

    let video_controller = Rc::new(RefCell::new(VideoPreviewController::new(
        config.playback.mpv_path.clone(),
        api_base_url.clone(),
    )));

    let playback_sync = Arc::new(Mutex::new(PlaybackSyncState::default()));

    let control_sender = start_player_control_worker(api_base_url.clone());

    setup_window_controls(&ui, video_controller.clone());
    setup_overlay_actions(&ui, api_base_url.clone());
    setup_playback_controls(&ui, api_base_url.clone(), control_sender);

    start_backend_polling(&ui, playback_sync.clone(), api_base_url.clone());
    start_twitch_status_polling(&ui, api_base_url);

    let video_timer = setup_video_preview(
        &ui,
        video_controller.clone(),
        playback_sync,
    );

    let run_result = ui.run();

    // Stop GUI-owned resources before the Job Object is dropped.
    video_timer.stop();
    video_controller.borrow_mut().shutdown();
    drop(backend_patcher);

    run_result?;
    Ok(())
}

const SINGLE_INSTANCE_MUTEX: &str = "Local\\CypherSR.Desktop.SingleInstance";

fn acquire_single_instance() -> std::io::Result<OwnedHandle> {
    let mutex_name: Vec<u16> = SINGLE_INSTANCE_MUTEX
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let handle = unsafe {
        CreateMutexW(
            std::ptr::null(),
            0,
            mutex_name.as_ptr(),
        )
    };

    if handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }

    // Check immediately after CreateMutexW.
    let last_error = unsafe { GetLastError() };

    if last_error == ERROR_ALREADY_EXISTS {
        unsafe {
            CloseHandle(handle);
        }

        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "Another CypherSR Desktop instance is already running.",
        ));
    }

    // Keep this handle alive for the entire desktop application lifetime.
    Ok(unsafe {
        OwnedHandle::from_raw_handle(handle as RawHandle)
    })
}

/// Launches CypherSR.exe from the same directory as the desktop executable.
fn launch_backend_patcher() -> Result<BackendPatcher, Box<dyn std::error::Error>> {
    let executable = std::env::current_exe()?;

    let bundle_dir = executable.parent().ok_or_else(|| {
        io::Error::other("Could not determine the desktop executable directory")
    })?;

    let patcher_path = bundle_dir.join(PATCHER_EXE);

    if !patcher_path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "Patcher executable was not found: {}",
                patcher_path.display()
            ),
        )
        .into());
    }

    // Persistent portable data lives beside the executables.
    let data_dir = bundle_dir.join(DATA_DIRECTORY);
    std::fs::create_dir_all(&data_dir).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "Could not create portable data directory {}: {error}",
                data_dir.display()
            ),
        )
    })?;

    println!("[Launcher] Updater: {}", patcher_path.display());
    println!("[Launcher] Data directory: {}", data_dir.display());

    // Create the Job Object.
    let job_handle = unsafe {
        CreateJobObjectW(std::ptr::null(), std::ptr::null())
    };

    if job_handle.is_null() {
        return Err(io::Error::other(format!(
            "CreateJobObjectW failed: {}",
            io::Error::last_os_error()
        ))
        .into());
    }

    // Immediately take ownership so all subsequent error paths close the handle.
    let job = unsafe {
        OwnedHandle::from_raw_handle(job_handle as RawHandle)
    };

    // Configure the Job Object to terminate its processes when the handle closes.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
        unsafe { zeroed() };

    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

    let configured = unsafe {
        SetInformationJobObject(
            job_handle,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };

    if configured == 0 {
        return Err(io::Error::other(format!(
            "SetInformationJobObject failed: {}",
            io::Error::last_os_error()
        ))
        .into());
    }

    // Start suspended so the updater cannot spawn the core before being
    // assigned to the Job Object.
    let mut child = Command::new(&patcher_path)
    .current_dir(bundle_dir)
    .env(SONG_REQUEST_HOME_ENV, &data_dir)
    .creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW)
    .spawn()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "Could not spawn updater {}: {error}",
                    patcher_path.display()
                ),
            )
        })?;

    println!("[Launcher] Updater created suspended (PID {}).", child.id());

    // Assign the suspended updater to the Job Object.
    let assigned = unsafe {
        AssignProcessToJobObject(
            job_handle,
            child.as_raw_handle() as HANDLE,
        )
    };

    if assigned == 0 {
        let error = io::Error::other(format!(
            "AssignProcessToJobObject failed: {}",
            io::Error::last_os_error()
        ));

        terminate_child(&mut child);
        return Err(error.into());
    }

    println!("[Launcher] Updater assigned to Job Object.");

    // IMPORTANT: OpenThread needs a thread ID, not child.id() (the process ID).
    resume_suspended_process(child.id()).map_err(|error| {
        terminate_child(&mut child);
        error
    })?;

    println!("[Launcher] Updater resumed successfully.");

    Ok(BackendPatcher {
        _child: child,
        _job: job,
    })
}

/// Finds and resumes the suspended updater's primary thread.
///
/// The updater was created with CREATE_SUSPENDED, so its primary thread
/// should be present in the system thread snapshot.
fn resume_suspended_process(process_id: u32) -> io::Result<()> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);

        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::other(format!(
                "CreateToolhelp32Snapshot failed: {}",
                io::Error::last_os_error()
            )));
        }

        let mut entry: THREADENTRY32 = zeroed();
        entry.dwSize = size_of::<THREADENTRY32>() as u32;

        if Thread32First(snapshot, &mut entry) == 0 {
            let error = io::Error::last_os_error();
            CloseHandle(snapshot);

            return Err(io::Error::other(format!(
                "Thread32First failed: {error}"
            )));
        }

        loop {
            if entry.th32OwnerProcessID == process_id {
                let thread_id = entry.th32ThreadID;

                let thread_handle = OpenThread(
                    THREAD_SUSPEND_RESUME,
                    0,
                    thread_id,
                );

                if thread_handle.is_null() {
                    let error = io::Error::last_os_error();
                    CloseHandle(snapshot);

                    return Err(io::Error::other(format!(
                        "OpenThread failed for updater PID {process_id}, \
                         thread ID {thread_id}: {error}"
                    )));
                }

                let resume_result = ResumeThread(thread_handle);
                let resume_error = if resume_result == u32::MAX {
                    Some(io::Error::last_os_error())
                } else {
                    None
                };

                CloseHandle(thread_handle);
                CloseHandle(snapshot);

                return match resume_error {
                    Some(error) => Err(io::Error::other(format!(
                        "ResumeThread failed for updater PID {process_id}, \
                         thread ID {thread_id}: {error}"
                    ))),
                    None => Ok(()),
                };
            }

            entry.dwSize = size_of::<THREADENTRY32>() as u32;

            if Thread32Next(snapshot, &mut entry) == 0 {
                let error = io::Error::last_os_error();
                CloseHandle(snapshot);

                return Err(io::Error::other(format!(
                    "Could not find a thread for updater PID {process_id}: \
                     {error}"
                )));
            }
        }
    }
}

/// Stops a child process on a startup failure and waits for termination.
fn terminate_child(child: &mut Child) {
    if let Err(error) = child.kill() {
        eprintln!("[Launcher] Failed to terminate updater: {error}");
    }

    if let Err(error) = child.wait() {
        eprintln!("[Launcher] Failed to wait for updater: {error}");
    }
}

fn show_error_dialog(message: &str) {
    let title: Vec<u16> = "CypherSR Desktop"
        .encode_utf16()
        .chain(Some(0))
        .collect();

    let message: Vec<u16> = message
        .encode_utf16()
        .chain(Some(0))
        .collect();

    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn start_player_control_worker(
    api_base_url: String,
) -> Sender<PlayerControlRequest> {
    let (sender, receiver) = mpsc::channel::<PlayerControlRequest>();

    thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                eprintln!("[Player] Failed to create HTTP client: {error}");
                return;
            }
        };

        let endpoint = format!(
            "{}/api/player/control",
            api_base_url.trim_end_matches('/')
        );

        while let Ok((action, value)) = receiver.recv() {
            let payload = match value {
                Some(value) => serde_json::json!({
                    "action": action,
                    "value": value
                }),
                None => serde_json::json!({
                    "action": action
                }),
            };

            let result = client
                .post(&endpoint)
                .header("Content-Type", "application/json")
                .body(payload.to_string())
                .send();

            match result {
                Ok(response) => {
                    let status = response.status();

                    match response.text() {
                        Ok(body) => {
                            println!(
                                "[Player] Control response: \
                                 action={action}, status={status}, body={body}"
                            );

                            if !status.is_success() {
                                eprintln!(
                                    "[Player] Backend rejected {action}: \
                                     HTTP {status}"
                                );
                            }
                        }
                        Err(error) => {
                            eprintln!(
                                "[Player] Failed to read {action} response: {error}"
                            );
                        }
                    }
                }
                Err(error) => {
                    eprintln!(
                        "[Player] Failed to send {action} control: {error}"
                    );
                }
            }
        }
    });

    sender
}

fn send_player_control(
    sender: &Sender<PlayerControlRequest>,
    action: &str,
    value: Option<f64>,
) {
    if let Err(error) = sender.send((action.to_owned(), value)) {
        eprintln!("[Player] Failed to queue control request: {error}");
    }
}

fn setup_window_controls(
    ui: &MainWindow,
    video_controller: Rc<RefCell<VideoPreviewController>>,
) {
    let weak_ui = ui.as_weak();

    ui.on_minimize_requested(move || {
        if let Some(ui) = weak_ui.upgrade() {
            ui.window().set_minimized(true);
        }
    });

    ui.on_close_window_requested(move || {
        video_controller.borrow_mut().shutdown();
        let _ = slint::quit_event_loop();
    });
}

fn setup_overlay_actions(ui: &MainWindow, api_base_url: String) {
    ui.on_copy_overlay_url(|url| match Clipboard::new() {
        Ok(mut clipboard) => match clipboard.set_text(url.to_string()) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("[Overlay] Failed to copy URL: {error}");
                false
            }
        },
        Err(error) => {
            eprintln!("[Overlay] Failed to access clipboard: {error}");
            false
        }
    });

    ui.on_open_creator_link(|| {
        if let Err(error) = that("https://www.twitch.tv/cypherusuh__") {
            eprintln!("[Twitch] Failed to open creator link: {error}");
        }
    });

    let weak_ui = ui.as_weak();

    ui.on_connection_toggle_requested(move || {
        let Some(ui) = weak_ui.upgrade() else {
            return;
        };

        if ui.get_twitch_connecting() {
            return;
        }

        let should_connect = !ui.get_twitch_connected();

        backend::request_twitch_connection(
            api_base_url.clone(),
            should_connect,
        );
    });
}

fn setup_playback_controls(
    ui: &MainWindow,
    api_base_url: String,
    control_sender: Sender<PlayerControlRequest>,
) {
    ui.on_skip_requested(move || {
        backend::skip(api_base_url.clone());
    });

    let sender = control_sender.clone();

    ui.on_seek_requested(move |position| {
        send_player_control(
            &sender,
            "seek",
            Some(f64::from(position)),
        );
    });

    let sender = control_sender.clone();

    ui.on_volume_requested(move |volume| {
        let volume = f64::from(volume).clamp(0.0, 1.0);

        send_player_control(
            &sender,
            "volume",
            Some(volume),
        );
    });

    let sender = control_sender;
    let weak_ui = ui.as_weak();

    ui.on_playback_toggle_requested(move || {
        let Some(ui) = weak_ui.upgrade() else {
            return;
        };

        if ui.get_current_media_url().is_empty() {
            return;
        }

        let action = if ui.get_playback_paused() {
            "play"
        } else {
            "pause"
        };

        send_player_control(&sender, action, None);
    });
}

fn start_backend_polling(
    ui: &MainWindow,
    playback_sync: Arc<Mutex<PlaybackSyncState>>,
    api_base_url: String,
) {
    let weak_ui = ui.as_weak();

    backend::start_polling(
        api_base_url,
        playback_sync,
        move |snapshot| {
            let weak_ui = weak_ui.clone();

            let result = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };

                ui.set_connected(snapshot.connected);
                ui.set_current_track(snapshot.current_track.into());
                ui.set_current_artist(snapshot.current_artist.into());
                ui.set_current_media_url(snapshot.current_media_url.into());

                let items: Vec<SharedString> = snapshot
                    .queue_items
                    .into_iter()
                    .map(SharedString::from)
                    .collect();

                let model = Rc::new(VecModel::from(items));
                ui.set_queue_items(ModelRc::from(model));

                ui.set_playback_position(
                    snapshot.playback.position as f32,
                );
                ui.set_playback_duration(
                    snapshot.playback.duration as f32,
                );
                ui.set_playback_volume(
                    snapshot.playback.volume as f32,
                );
                ui.set_playback_paused(snapshot.playback.paused);
            });

            if let Err(error) = result {
                eprintln!(
                    "[Backend] Failed to schedule UI update: {error}"
                );
            }
        },
    );
}

fn start_twitch_status_polling(
    ui: &MainWindow,
    api_base_url: String,
) {
    let weak_ui = ui.as_weak();

    backend::start_twitch_status_polling(
        api_base_url,
        move |status| {
            let weak_ui = weak_ui.clone();

            let result = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak_ui.upgrade() else {
                    return;
                };

                let status_label = if status.error.is_some() {
                    "error"
                } else if status.connecting {
                    "connecting"
                } else if status.connected {
                    "connected"
                } else {
                    "offline"
                };

                ui.set_twitch_connected(status.connected);
                ui.set_twitch_connecting(status.connecting);
                ui.set_twitch_status(status_label.into());

                if let Some(username) = status.username {
                    if !username.trim().is_empty() {
                        ui.set_username(username.into());
                    }
                }
            });

            if let Err(error) = result {
                eprintln!(
                    "[Twitch] Failed to schedule UI update: {error}"
                );
            }
        },
    );
}

fn setup_video_preview(
    ui: &MainWindow,
    video_controller: Rc<RefCell<VideoPreviewController>>,
    playback_sync: Arc<Mutex<PlaybackSyncState>>,
) -> Timer {
    let weak_ui = ui.as_weak();
    let video_timer = Timer::default();

    video_timer.start(
        slint::TimerMode::Repeated,
        VIDEO_SYNC_INTERVAL,
        move || {
            let Some(ui) = weak_ui.upgrade() else {
                video_controller.borrow_mut().shutdown();
                return;
            };

            let scale = ui.window().scale_factor();

            let sync = match playback_sync.lock() {
                Ok(sync) => sync.clone(),
                Err(error) => {
                    eprintln!(
                        "[Video] Playback sync mutex poisoned: {error}"
                    );
                    return;
                }
            };

            video_controller.borrow_mut().sync(
                native_window_handle(&ui),
                ui.get_video_visible(),
                ui.get_current_media_url().as_str(),
                sync.request_id.as_deref(),
                &sync.playback,
                sync.control.as_ref(),
                (ui.get_video_preview_x() * scale).round() as i32,
                (ui.get_video_preview_y() * scale).round() as i32,
                (ui.get_video_preview_width() * scale).round() as i32,
                (ui.get_video_preview_height() * scale).round() as i32,
            );
        },
    );

    video_timer
}