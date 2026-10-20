
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
use video_preview::{VideoPreviewController, native_window_handle};

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

    /*
     * Shared between the backend worker thread,
     * the Slint event loop, and the video timer.
     */
    let playback_sync = Arc::new(Mutex::new(PlaybackSyncState::default()));

    let control_sender = start_player_control_worker(api_base_url.clone());

    setup_window_controls(&ui, video_controller.clone());
    setup_overlay_actions(&ui);
    setup_playback_controls(&ui, api_base_url.clone(), control_sender);

    start_backend_polling(&ui, playback_sync.clone(), api_base_url);

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
                .send()
                .and_then(|response| response.error_for_status());

            if let Err(error) = result {
                eprintln!("[Player] Failed to send {action} control: {error}");
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

fn setup_overlay_actions(ui: &MainWindow) {
    ui.on_copy_overlay_url(|url| match Clipboard::new() {
        Ok(mut clipboard) => match clipboard.set_text(url.to_string()) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("Failed to copy URL: {error}");
                false
            }
        },
        Err(error) => {
            eprintln!("Failed to access clipboard: {error}");
            false
        }
    });

    ui.on_open_creator_link(|| {
        if let Err(error) = that("https://www.twitch.tv/cypherusuh__") {
            eprintln!("Failed to open Twitch link: {error}");
        }
    });

    ui.on_connection_toggle_requested(|| {
        eprintln!("Twitch connection action is not wired up yet.");
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

            // Playback properties are updated from the same backend snapshot
            // that feeds the native mpv controller.
            //
            // These setters synchronize the display; they do not send
            // playback commands back to mpv.
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

fn setup_video_preview(
    ui: &MainWindow,
    video_controller: Rc<RefCell<VideoPreviewController>>,
    playback_sync: Arc<Mutex<PlaybackSyncState>>,
) -> Timer {
    let weak_ui = ui.as_weak();
    let video_timer = Timer::default();

    video_timer.start(slint::TimerMode::Repeated, VIDEO_SYNC_INTERVAL, move || {
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
    });

    video_timer
}
