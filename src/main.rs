use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use arboard::Clipboard;
use open::that;
use slint::{ComponentHandle, ModelRc, SharedString, Timer, VecModel};

mod backend;
mod models;
mod video_preview;

use backend::start_polling;
use models::PlaybackSyncState;
use video_preview::{VideoPreviewController, native_window_handle};

slint::include_modules!();

const VIDEO_SYNC_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

fn main() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;

    let video_controller = Rc::new(RefCell::new(VideoPreviewController::default()));

    /*
     * Shared between:
     *
     * backend worker thread
     *        ↓
     * Slint event loop
     *        ↓
     * video timer
     *
     * Arc<Mutex> is used because the backend poller
     * runs on a separate OS thread.
     */
    let playback_sync = Arc::new(Mutex::new(PlaybackSyncState::default()));

    setup_window_controls(&ui, video_controller.clone());

    setup_overlay_actions(&ui);

    setup_playback_controls(&ui);

    start_backend_polling(&ui, playback_sync.clone());

    let _video_timer = setup_video_preview(&ui, video_controller, playback_sync);

    ui.run()
}

fn setup_window_controls(ui: &MainWindow, video_controller: Rc<RefCell<VideoPreviewController>>) {
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

fn setup_playback_controls(ui: &MainWindow) {
    ui.on_skip_requested(|| {
        backend::skip();
    });
}

fn start_backend_polling(ui: &MainWindow, playback_sync: Arc<Mutex<PlaybackSyncState>>) {
    let weak_ui = ui.as_weak();

    start_polling(playback_sync, move |snapshot| {
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
        });

        if let Err(error) = result {
            eprintln!("[Backend] Failed to schedule UI update: {error}");
        }
    });
}

fn setup_video_preview(
    ui: &MainWindow,
    video_controller: Rc<
        RefCell<VideoPreviewController>,
    >,
    playback_sync: Arc<
        Mutex<PlaybackSyncState>,
    >,
) -> Timer {
    let weak_ui = ui.as_weak();

    let video_timer = Timer::default();

    video_timer.start(
        slint::TimerMode::Repeated,
        VIDEO_SYNC_INTERVAL,
        move || {
            let Some(ui) =
                weak_ui.upgrade()
            else {
                video_controller
                    .borrow_mut()
                    .shutdown();

                return;
            };

            let scale =
                ui.window().scale_factor();

            let sync =
                match playback_sync.lock() {
                    Ok(sync) => sync.clone(),

                    Err(error) => {
                        eprintln!(
                            "[Video] Playback sync mutex poisoned: {error}"
                        );
                        return;
                    }
                };

            video_controller
                .borrow_mut()
                .sync(
                    native_window_handle(&ui),
                    ui.get_video_visible(),
                    ui.get_current_media_url()
                        .as_str(),
                    sync.request_id
                        .as_deref(),
                    &sync.playback,
                    sync.control.as_ref(),
                    (
                        ui.get_video_preview_x()
                            * scale
                    )
                        .round()
                        as i32,
                    (
                        ui.get_video_preview_y()
                            * scale
                    )
                        .round()
                        as i32,
                    (
                        ui.get_video_preview_width()
                            * scale
                    )
                        .round()
                        as i32,
                    (
                        ui.get_video_preview_height()
                            * scale
                    )
                        .round()
                        as i32,
                );
        },
    );

    video_timer
}
