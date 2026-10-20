use crate::models::{
    PlaybackSyncState, PlayerState, QueueResponse, UiSnapshot,
};
use reqwest::blocking::Client;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const API_BASE_URL: &str = "http://127.0.0.1:8765";

pub const POLL_INTERVAL: Duration = Duration::from_millis(500);
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

pub fn create_client() -> Result<Client, String> {
    Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())
}

pub fn fetch_snapshot(client: &Client) -> Result<UiSnapshot, String> {
    let queue: QueueResponse = client
        .get(format!("{API_BASE_URL}/api/queue"))
        .send()
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .json()
        .map_err(|error| error.to_string())?;

    let player: PlayerState = client
        .get(format!("{API_BASE_URL}/api/player/state"))
        .send()
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .json()
        .map_err(|error| error.to_string())?;

    let current_request_id = player
        .current
        .as_ref()
        .and_then(|track| track.request_id.clone());

    let current_media_url = player
        .current
        .as_ref()
        .and_then(|track| track.media_url.clone())
        .unwrap_or_default();

    let current_track = player
        .current
        .as_ref()
        .map(|track| track.title.clone())
        .or_else(|| {
            queue
                .current
                .as_ref()
                .and_then(|entry| entry.track.as_ref())
                .map(|track| track.title.clone())
        })
        .unwrap_or_else(|| "Nothing playing".to_string());

    let current_artist = player
        .current
        .as_ref()
        .map(|track| {
            format!(
                "{} · requested by {}",
                track.uploader, track.requester
            )
        })
        .or_else(|| {
            queue.current.as_ref().and_then(|entry| {
                entry.track.as_ref().map(|track| {
                    let requester = entry
                        .requester
                        .as_ref()
                        .map(|requester| requester.display_name.as_str())
                        .unwrap_or("Unknown");

                    format!(
                        "{} · requested by {}",
                        track.uploader, requester
                    )
                })
            })
        })
        .unwrap_or_else(|| {
            "Request a song to start playback".to_string()
        });

    let queue_items = queue
        .pending
        .iter()
        .map(|entry| {
            let title = entry
                .track
                .as_ref()
                .map(|track| track.title.as_str())
                .unwrap_or(entry.query.as_str());

            let requester = entry
                .requester
                .as_ref()
                .map(|requester| requester.display_name.as_str())
                .unwrap_or("Unknown");

            format!("{title} · {requester}")
        })
        .collect();

    Ok(UiSnapshot {
        connected: true,
        current_track,
        current_artist,
        current_media_url,
        current_request_id,
        playback: player.playback,
        control: player.control,
        queue_items,
    })
}

pub fn start_polling(
    playback_sync: Arc<Mutex<PlaybackSyncState>>,
    on_snapshot: impl Fn(UiSnapshot) + Send + 'static,
) {
    thread::spawn(move || {
        let client = match create_client() {
            Ok(client) => client,

            Err(error) => {
                eprintln!(
                    "[Backend] Failed to create HTTP client: {error}"
                );
                return;
            }
        };

        loop {
            match fetch_snapshot(&client) {
                Ok(snapshot) => {
                    {
                        let mut sync = match playback_sync.lock() {
                            Ok(sync) => sync,

                            Err(error) => {
                                eprintln!(
                                    "[Backend] Playback state mutex poisoned: {error}"
                                );
                                break;
                            }
                        };

                        sync.request_id =
                            snapshot.current_request_id.clone();

                        sync.playback = snapshot.playback.clone();
                        sync.control = snapshot.control.clone();
                    }

                    on_snapshot(snapshot);
                }

                Err(error) => {
                    eprintln!(
                        "[Backend] API request failed: {error}"
                    );
                }
            }

            thread::sleep(POLL_INTERVAL);
        }
    });
}

pub fn skip() {
    thread::spawn(|| {
        let result = create_client().and_then(|client| {
            client
                .post(format!("{API_BASE_URL}/api/player/skip"))
                .send()
                .and_then(|response| response.error_for_status())
                .map(|_| ())
                .map_err(|error| error.to_string())
        });

        match result {
            Ok(()) => {
                println!("[Playback] Skip request accepted.");
            }

            Err(error) => {
                eprintln!(
                    "[Playback] Skip request failed: {error}"
                );
            }
        }
    });
}