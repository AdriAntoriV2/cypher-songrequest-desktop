use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const TELEMETRY_INTERVAL: Duration = Duration::from_millis(500);
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Default, Clone)]
struct ObservedPlayback {
    position: Option<f64>,
    duration: Option<f64>,
    volume: Option<f64>,
    paused: Option<bool>,
}

struct HttpJsonResponse {
    status: u16,
    body: Value,
}

impl ObservedPlayback {
    fn is_ready(&self) -> bool {
        self.position.is_some() && self.volume.is_some() && self.paused.is_some()
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
    // Retry briefly if another connection currently occupies the mpv pipe.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);

    let pipe = loop {
        if generation_source.load(Ordering::SeqCst) != generation {
            println!("[Video] IPC observer cancelled before connecting: request_id={request_id}");
            return;
        }

        match OpenOptions::new().read(true).write(true).open(&pipe_name) {
            Ok(pipe) => break pipe,
            Err(error)
                if error.raw_os_error() == Some(231) && std::time::Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                eprintln!(
                    "[Video] IPC observer could not connect for request \
                     {request_id}: {error}"
                );
                return;
            }
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
            eprintln!(
                "[Video] Failed to subscribe to mpv properties for \
                 request {request_id}: {error}"
            );
            return;
        }
    }

    if let Err(error) = pipe.flush() {
        eprintln!(
            "[Video] Failed to flush mpv IPC subscriptions for \
             request {request_id}: {error}"
        );
        return;
    }

    println!("[Video] mpv playback observer started: request_id={request_id}");

    let mut reader = BufReader::new(pipe);
    let mut line = String::new();

    // Prevent duplicate track-ended notifications from repeated EOF events.
    let mut ended_sent = false;

    loop {
        if generation_source.load(Ordering::SeqCst) != generation {
            println!("[Video] Observer generation invalidated: request_id={request_id}");
            break;
        }

        line.clear();

        match reader.read_line(&mut line) {
            Ok(0) => {
                eprintln!("[Video] mpv IPC stream closed: request_id={request_id}");
                break;
            }
            Ok(_) => {}
            Err(error) => {
                if generation_source.load(Ordering::SeqCst) == generation {
                    eprintln!(
                        "[Video] mpv IPC observer read failed for \
                         request {request_id}: {error}"
                    );
                }
                break;
            }
        }

        if generation_source.load(Ordering::SeqCst) != generation {
            break;
        }

        let message: Value = match serde_json::from_str(line.trim()) {
            Ok(message) => message,
            Err(error) => {
                eprintln!(
                    "[Video] Invalid mpv IPC JSON for request {request_id}: \
                     {error}; line={}",
                    line.trim()
                );
                continue;
            }
        };

        // mpv also sends command replies, which are not property events.
        if message["event"].as_str() != Some("property-change") {
            continue;
        }
        if message["event"].as_str() != Some("property-change") {
    if message["request_id"].is_number() || message["error"].is_string() {
        println!(
            "[Video] mpv IPC response: request_id={request_id}, message={message}"
        );
    }
    continue;
}

        let Some(name) = message["name"].as_str() else {
            eprintln!(
                "[Video] Property event missing name: request_id={request_id}, \
                 event={message}"
            );
            continue;
        };

        let data = &message["data"];

        // Log EOF events before any handler conditions can skip them.
        if name == "eof-reached" {
            println!(
                "[Video] EOF property received: request_id={request_id}, \
                 value={data}, ended_sent={ended_sent}"
            );
        }

        let Ok(mut state) = observed.lock() else {
            eprintln!("[Video] Playback observer state mutex poisoned.");
            break;
        };

        match name {
            "time-pos" => {
    if let Some(value) = data.as_f64() {
        if value.is_finite() && value >= 0.0 {
            state.position = Some(value);
        }
    } else {
        eprintln!(
            "[Video] Ignoring non-numeric time-pos event: \
             request_id={request_id}, data={data}"
        );
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
                let eof_value = data.as_bool();

                println!(
                    "[Video] EOF property received: request_id={}, value={:?}, ended_sent={}",
                    request_id, eof_value, ended_sent
                );

                let reached_duration = match (state.position, state.duration) {
                    (Some(position), Some(duration)) if duration > 0.0 => {
                        position >= duration - 0.5
                    }
                    _ => false,
                };

                let reached_eof = eof_value == Some(true) || (data.is_null() && reached_duration);

                if reached_eof && !ended_sent {
                    ended_sent = true;

                    if let Some(duration) = state.duration {
                        if duration > 0.0 {
                            state.position = Some(duration);
                        }
                    }

                    state.paused = Some(true);

                    println!("[Video] Track ended for request {request_id}; notifying backend.");

                    drop(state);

                    let _post_guard = match telemetry_post_lock.lock() {
                        Ok(guard) => guard,
                        Err(error) => {
                            eprintln!("[Video] Telemetry post lock poisoned: {error}");
                            break;
                        }
                    };

                    if generation_source.load(Ordering::SeqCst) != generation {
                        break;
                    }

                    let final_payload = match observed.lock() {
                        Ok(state) => state.telemetry_payload(&request_id),
                        Err(error) => {
                            eprintln!("[Video] Playback observer state mutex poisoned: {error}");
                            break;
                        }
                    };

                    if let Some(payload) = final_payload {
                        if let Err(error) =
                            post_json(&api_base_url, "/api/player/telemetry", payload)
                        {
                            eprintln!("[Video] Failed to report final playback state: {error}");
                        }
                    }

                    if generation_source.load(Ordering::SeqCst) == generation {
                        match post_json(
                            &api_base_url,
                            "/api/player/ended",
                            json!({"requestId": request_id}),
                        ) {
                            Ok(response) => {
                                if response.body["accepted"].as_bool() == Some(true) {
                                    println!("[Video] Track completion accepted by backend.");
                                } else {
                                    eprintln!(
                                        "[Video] Backend did not accept track completion: {:?}",
                                        response.body
                                    );
                                }
                            }
                            Err(error) => {
                                eprintln!("[Video] Failed to notify track completion: {error}");
                            }
                        }
                    }

                    continue;
                }
            }
            _ => {}
        }
    }

    println!("[Video] mpv playback observer stopped: request_id={request_id}");
}

fn run_telemetry_reporter(
    api_base_url: String,
    request_id: String,
    generation_source: Arc<AtomicU64>,
    generation: u64,
    observed: Arc<Mutex<ObservedPlayback>>,
    telemetry_post_lock: Arc<Mutex<()>>,
) {
    let client = match Client::builder().timeout(HTTP_TIMEOUT).build() {
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
            let position = payload["position"].as_f64().unwrap_or(-1.0);

            match post_json_with_client(&client, &api_base_url, "/api/player/telemetry", payload) {
                Ok(response) => {
                    if response.body["accepted"].as_bool() != Some(true) {
                        eprintln!(
                            "[Video] Backend rejected telemetry at {:.2}s: \
                             HTTP {}, body={}",
                            position, response.status, response.body
                        );
                    }
                }
                Err(error) => {
                    eprintln!("[Video] Telemetry POST failed at {:.2}s: {error}", position);
                }
            }
        }

        drop(_post_guard);
        thread::sleep(TELEMETRY_INTERVAL);
    }

    println!("[Video] Telemetry reporter stopped: request_id={request_id}");
}

fn post_json(api_base_url: &str, path: &str, payload: Value) -> Result<HttpJsonResponse, String> {
    let client = Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|error| format!("Failed to create HTTP client: {error}"))?;

    post_json_with_client(&client, api_base_url, path, payload)
}

fn post_json_with_client(
    client: &Client,
    api_base_url: &str,
    path: &str,
    payload: Value,
) -> Result<HttpJsonResponse, String> {
    let url = format!("{}{}", api_base_url.trim_end_matches('/'), path);

    let response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(payload.to_string())
        .send()
        .map_err(|error| format!("POST {url} failed: {error}"))?;

    let status = response.status();
    let status_code = status.as_u16();

    // Read the body before checking HTTP success so error responses include
    // their actual status and response body in the diagnostic message.
    let body_text = response
        .text()
        .map_err(|error| format!("POST {url}: HTTP {status_code}, failed to read body: {error}"))?;

    if !status.is_success() {
        return Err(format!(
            "POST {url} returned HTTP {status_code}: {body_text}"
        ));
    }

    let body: Value = serde_json::from_str(&body_text).map_err(|error| {
        format!(
            "POST {url} returned HTTP {status_code}, but its body was not valid JSON: \
             {error}; body={body_text}"
        )
    })?;

    Ok(HttpJsonResponse {
        status: status_code,
        body,
    })
}
