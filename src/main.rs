
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use arboard::Clipboard;
use open::that;
use slint::{ComponentHandle, ModelRc, SharedString, Timer, VecModel};

mod backend;
mod config;
mod models;
mod video_preview;

use models::PlaybackSyncState;
use video_preview::{native_window_handle, VideoPreviewController};

slint::include_modules!();

const VIDEO_SYNC_INTERVAL: Duration = Duration::from_millis(100);

type PlayerControlRequest = (String, Option<f64>);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = config::AppConfig::load_or_create()?;
    let api_base_url = config.api_base_url();

    println!("[Config] Backend address: {api_base_url}");

    let ui = MainWindow::new()?;

    let video_controller = Rc::new(RefCell::new(VideoPreviewController::new(
        config.playback.mpv_path.clone(),
        api_base_url.clone(),
    )));

    // Shared between the backend worker, Slint event loop, and video timer.
    let playback_sync = Arc::new(Mutex::new(PlaybackSyncState::default()));

    let control_sender = start_player_control_worker(api_base_url.clone());

    setup_window_controls(&ui, video_controller.clone());
    setup_overlay_actions(&ui, api_base_url.clone());
    setup_playback_controls(&ui, api_base_url.clone(), control_sender);

    // Player/backend status, queue, and playback synchronization.
    start_backend_polling(&ui, playback_sync.clone(), api_base_url.clone());

    // Actual Twitch connection status, independently polled from the backend.
    start_twitch_status_polling(&ui, api_base_url);

    let _video_timer = setup_video_preview(&ui, video_controller, playback_sync);

    Ok(ui.run()?)
}

fn start_player_control_worker(api_base_url: String) -> Sender<PlayerControlRequest> {
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
                                "[Player] Control response: action={action}, status={status}, body={body}"
                            );

                            if !status.is_success() {
                                eprintln!(
                                    "[Player] Backend rejected {action}: HTTP {status}"
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

    let weak_ui = ui.as_weak();

    ui.on_close_window_requested(move || {
        video_controller.borrow_mut().shutdown();

        if let Some(ui) = weak_ui.upgrade() {
            let _ = ui.window().hide();
        }
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

    // The button requests a state change. The polling thread determines
    // the actual connection state and updates the UI.
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
    let skip_api_url = api_base_url;

    ui.on_skip_requested(move || {
        backend::skip(skip_api_url.clone());
    });

    let sender = control_sender.clone();

    ui.on_seek_requested(move |position| {
        send_player_control(&sender, "seek", Some(f64::from(position)));
    });

    let sender = control_sender.clone();

    ui.on_volume_requested(move |volume| {
        let volume = f64::from(volume).clamp(0.0, 1.0);
        send_player_control(&sender, "volume", Some(volume));
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

    backend::start_polling(api_base_url, playback_sync, move |snapshot| {
        let weak_ui = weak_ui.clone();

        let result = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak_ui.upgrade() else {
                return;
            };

            // This property means player/backend API connectivity only.
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

            // These properties synchronize the UI with the backend.
            // They do not send playback commands to mpv.
            ui.set_playback_position(snapshot.playback.position as f32);
            ui.set_playback_duration(snapshot.playback.duration as f32);
            ui.set_playback_volume(snapshot.playback.volume as f32);
            ui.set_playback_paused(snapshot.playback.paused);
        });

        if let Err(error) = result {
            eprintln!("[Backend] Failed to schedule UI update: {error}");
        }
    });
}

fn start_twitch_status_polling(ui: &MainWindow, api_base_url: String) {
    let weak_ui = ui.as_weak();

    backend::start_twitch_status_polling(api_base_url, move |status| {
        let weak_ui = weak_ui.clone();

        let result = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak_ui.upgrade() else {
                return;
            };

            // Show an explicit error state if the status endpoint fails.
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
            eprintln!("[Twitch] Failed to schedule UI update: {error}");
        }
    });
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
                    eprintln!("[Video] Playback sync mutex poisoned: {error}");
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
