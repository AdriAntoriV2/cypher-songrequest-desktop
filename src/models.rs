use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct QueueResponse {
    pub current: Option<QueueEntry>,

    #[serde(default)]
    pub pending: Vec<QueueEntry>,
}

#[derive(Debug, Deserialize)]
pub struct QueueEntry {
    pub query: String,
    pub requester: Option<Requester>,
    pub track: Option<Track>,
}

#[derive(Debug, Deserialize)]
pub struct Requester {
    #[serde(rename = "displayName")]
    pub display_name: String,
}

#[derive(Debug, Deserialize)]
pub struct Track {
    pub title: String,
    pub uploader: String,
}

#[derive(Debug, Deserialize)]
pub struct PlayerState {
    pub current: Option<PlayerTrack>,

    #[serde(default)]
    pub playback: PlaybackState,

    pub control: Option<PlayerControl>,
}

#[derive(Debug, Deserialize)]
pub struct PlayerTrack {
    pub title: String,
    pub uploader: String,
    pub requester: String,

    #[serde(rename = "mediaUrl", default)]
    pub media_url: Option<String>,

    #[serde(rename = "requestId", default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlaybackState {
    #[serde(rename = "requestId")]
    pub request_id: Option<String>,

    pub position: f64,
    pub duration: f64,
    pub volume: f64,
    pub paused: bool,

    #[serde(rename = "updatedAt")]
    pub updated_at: u64,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            request_id: None,
            position: 0.0,
            duration: 0.0,
            volume: 1.0,
            paused: true,
            updated_at: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlayerControl {
    pub revision: u64,
    pub action: String,
    pub value: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct UiSnapshot {
    pub connected: bool,
    pub current_track: String,
    pub current_artist: String,
    pub current_media_url: String,
    pub current_request_id: Option<String>,
    pub playback: PlaybackState,
    pub control: Option<PlayerControl>,
    pub queue_items: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PlaybackSyncState {
    pub request_id: Option<String>,
    pub playback: PlaybackState,
    pub control: Option<PlayerControl>,
}