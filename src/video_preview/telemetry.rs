
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const TELEMETRY_INTERVAL: Duration = Duration::from_millis(250);
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Default, Clone)]
struct ObservedPlayback {
    position: Option<f64>,
    duration: Option<f64>,
    volume: Option<f64>,
    paused: Option<bool>,
}

impl ObservedPlayback {
    fn is_ready(&self) -> bool {
        self.position.is_some()
            && self.volume.is_some()
            && self.paused.is_some()
    }

    fn telemetry_payload(&self, request_id: &str) -> Option<Value> {
        if !self.is_ready() {
            return None;
        }

        let position = self.position?.max(0.0);
        let duration = self.duration.unwrap_or(0.0).max(0.0);

        Some(json!({
            "requestId": request_id,
            "position": if duration > 0.0 {
                position.min(duration)
            } else {
                position
            },
            "duration": duration,
            "volume": self.volume?.clamp(0.0, 1.0),
            "paused": self.paused?
        }))
    }
}

/// Start one IPC reader and one throttled telemetry reporter for this mpv
/// instance.
///
/// The IPC observer owns the connection used to receive mpv property events.
/// The telemetry reporter periodically publishes the latest observed state.
///
/// The generation token invalidates both workers when mpv stops or is replaced.
pub(super) fn start_observers(
    pipe_name: String,
    request_id: String,
    api_base_url: String,
    generation_source: Arc<AtomicU64>,
    generation: u64,
    telemetry_post_lock: Arc<Mutex<()>>,
) {
    let observed = Arc::new(Mutex::new(ObservedPlayback::default()));

    {
        let observed = Arc::clone(&observed);
        let generation_source = Arc::clone(&generation_source);
        let telemetry_post_lock = Arc::clone(&telemetry_post_lock);
        let request_id = request_id.clone();
        let api_base_url = api_base_url.clone();

        thread::spawn(move || {
            run_ipc_observer(
                pipe_name,
                request_id,
                api_base_url,
                generation_source,
                generation,
                observed,
                telemetry_post_lock,
            );
        });
    }

    thread::spawn(move || {
        run_telemetry_reporter(
            api_base_url,
            request_id,
            generation_source,
            generation,
            observed,
            telemetry_post_lock,
        );
    });
}

fn run_ipc_observer(
    pipe_name: String,
    request_id: String,
    api_base_url: String,
    generation_source: Arc<AtomicU64>,
    generation: u64,
    observed: Arc<Mutex<ObservedPlayback>>,
    telemetry_post_lock: Arc<Mutex<()>>,
) {
    let pipe = match OpenOptions::new()
        .read(true)
        .write(true)
        .open(&pipe_name)
    {
        Ok(pipe) => pipe,
        Err(error) => {
            eprintln!("[Video] IPC observer could not connect: {error}");
            return;
        }
    };

    let observe_commands = [
        json!({"command": ["observe_property", 1, "time-pos"]}),
        json!({"command": ["observe_property", 2, "duration"]}),
        json!({"command": ["observe_property", 3, "pause"]}),
        json!({"command": ["observe_property", 4, "volume"]}),
        json!({"command": ["observe_property", 5, "eof-reached"]}),
    ];

    let mut pipe = pipe;

    for command in observe_commands {
        let mut message = command.to_string();
        message.push('\n');

        if let Err(error) = pipe.write_all(message.as_bytes()) {
            eprintln!("[Video] Failed to subscribe to mpv properties: {error}");
            return;
        }
    }

    if let Err(error) = pipe.flush() {
        eprintln!("[Video] Failed to flush mpv IPC subscriptions: {error}");
        return;
    }

    println!("[Video] mpv playback observer started.");

    let mut reader = BufReader::new(pipe);
    let mut line = String::new();

    // Prevent duplicate track-ended notifications from repeated EOF events.
    let mut ended_sent = false;

    loop {
        if generation_source.load(Ordering::SeqCst) != generation {
            break;
        }

        line.clear();

        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                if generation_source.load(Ordering::SeqCst) == generation {
                    eprintln!("[Video] mpv IPC observer read failed: {error}");
                }
                break;
            }
        }

        if generation_source.load(Ordering::SeqCst) != generation {
            break;
        }

        let message: Value = match serde_json::from_str(line.trim()) {
            Ok(message) => message,
            Err(_) => continue,
        };

        if message["event"].as_str() != Some("property-change") {
            continue;
        }

        let Some(name) = message["name"].as_str() else {
            continue;
        };

        let data = &message["data"];

        let Ok(mut state) = observed.lock() else {
            eprintln!("[Video] Playback observer state mutex poisoned.");
            break;
        };

        match name {
            "time-pos" => {
                if let Some(value) = data.as_f64() {
                    if value.is_finite() && value >= 0.0 {
                        let previous_position = state.position;
                        state.position = Some(value);

                        // Useful for tracing seek propagation without logging
                        // every ordinary playback position update.
                        if let Some(previous) = previous_position {
                            if (value - previous).abs() >= 2.0 {
                                println!(
                                    "[Video] mpv position changed: \
                                     {:.2}s -> {:.2}s",
                                    previous, value
                                );
                            }
                        } else {
                            println!("[Video] Initial mpv position: {:.2}s", value);
                        }
                    }
                }
            }
            "duration" => {
                if let Some(value) = data.as_f64() {
                    if value.is_finite() && value >= 0.0 {
                        state.duration = Some(value);
                    }
                }
            }
            "pause" => {
                if let Some(value) = data.as_bool() {
                    state.paused = Some(value);
                }
            }
            "volume" => {
                if let Some(value) = data.as_f64() {
                    if value.is_finite() {
                        state.volume = Some((value / 100.0).clamp(0.0, 1.0));
                    }
                }
            }
            "eof-reached" => {
                if data.as_bool() == Some(true) && !ended_sent {
                    ended_sent = true;

                    if let Some(duration) = state.duration {
                        if duration > 0.0 {
                            state.position = Some(duration);
                        }
                    }

                    state.paused = Some(true);

                    println!(
                        "[Video] End of track reached for request {request_id}."
                    );

                    drop(state);

                    /*
                     * Serialize the final report with periodic telemetry.
                     * The observer thread is the only writer to `observed`,
                     * so it cannot process another property event while this
                     * final report is being sent.
                     */
                    let _post_guard = match telemetry_post_lock.lock() {
                        Ok(guard) => guard,
                        Err(error) => {
                            eprintln!(
                                "[Video] Telemetry post lock poisoned: {error}"
                            );
                            break;
                        }
                    };

                    if generation_source.load(Ordering::SeqCst) != generation {
                        break;
                    }

                    let final_payload = match observed.lock() {
                        Ok(state) => state.telemetry_payload(&request_id),
                        Err(error) => {
                            eprintln!(
                                "[Video] Playback observer state mutex poisoned: {error}"
                            );
                            break;
                        }
                    };

                    if let Some(payload) = final_payload {
                        if let Err(error) = post_json(
                            &api_base_url,
                            "/api/player/telemetry",
                            payload,
                        ) {
                            eprintln!(
                                "[Video] Failed to report final playback state: {error}"
                            );
                        }
                    }

                    if generation_source.load(Ordering::SeqCst) == generation {
                        match post_json(
                            &api_base_url,
                            "/api/player/ended",
                            json!({"requestId": request_id}),
                        ) {
                            Ok(response) => {
                                if response["accepted"].as_bool() == Some(true) {
                                    println!(
                                        "[Video] Track completion accepted by backend."
                                    );
                                } else {
                                    println!(
                                        "[Video] Track completion was stale \
                                         or already handled."
                                    );
                                }
                            }
                            Err(error) => {
                                eprintln!(
                                    "[Video] Failed to notify track completion: {error}"
                                );
                            }
                        }
                    }

                    continue;
                }
            }
            _ => {}
        }
    }

    println!("[Video] mpv playback observer stopped.");
}

fn run_telemetry_reporter(
    api_base_url: String,
    request_id: String,
    generation_source: Arc<AtomicU64>,
    generation: u64,
    observed: Arc<Mutex<ObservedPlayback>>,
    telemetry_post_lock: Arc<Mutex<()>>,
) {
    let client = match Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            eprintln!("[Video] Failed to create telemetry HTTP client: {error}");
            return;
        }
    };

    while generation_source.load(Ordering::SeqCst) == generation {
        let _post_guard = match telemetry_post_lock.lock() {
            Ok(guard) => guard,
            Err(error) => {
                eprintln!("[Video] Telemetry post lock poisoned: {error}");
                return;
            }
        };

        if generation_source.load(Ordering::SeqCst) != generation {
            break;
        }

        let payload = match observed.lock() {
            Ok(state) => state.telemetry_payload(&request_id),
            Err(error) => {
                eprintln!("[Video] Telemetry state mutex poisoned: {error}");
                return;
            }
        };

        if let Some(payload) = payload {
            if let Err(error) = post_json_with_client(
                &client,
                &api_base_url,
                "/api/player/telemetry",
                payload,
            ) {
                if generation_source.load(Ordering::SeqCst) == generation {
                    eprintln!("[Video] Playback telemetry failed: {error}");
                }
            }
        }

        drop(_post_guard);
        thread::sleep(TELEMETRY_INTERVAL);
    }
}

fn post_json(
    api_base_url: &str,
    path: &str,
    payload: Value,
) -> Result<Value, String> {
    let client = Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?;

    post_json_with_client(&client, api_base_url, path, payload)
}

fn post_json_with_client(
    client: &Client,
    api_base_url: &str,
    path: &str,
    payload: Value,
) -> Result<Value, String> {
    let url = format!(
        "{}{}",
        api_base_url.trim_end_matches('/'),
        path
    );

    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .body(payload.to_string())
        .send()
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?;

    response
        .json::<Value>()
        .map_err(|error| error.to_string())
}
