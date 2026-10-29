# cypher-songrequest-desktop

A Windows desktop UI and video preview for a song-request system. This repository
contains the desktop client and its local mpv playback controller; it does not
implement Twitch chat ingestion, song-request submission, or queue management.

## Architecture

- The Rust application renders its UI with [Slint](https://slint.dev/).
- It expects a separate backend API, defaulting to `http://127.0.0.1:8765`
  (configured in `config.toml` beside the executable).
- The desktop polls `GET /api/queue` and `GET /api/player/state` every 500 ms
  to display the current track, pending queue, and playback state. Queue and
  track processing happen in the backend, not in this app.
- The title bar also polls `GET /api/twitch/status` every 500 ms for Twitch
  authentication and connection state.
- The app launches `mpv` as a child process and embeds its native video window
  in the desktop UI. mpv is controlled through its named-pipe IPC interface.
- `config.toml` stores the backend address and playback-related settings,
  including the configured mpv executable path.

In short, this app depends on a separate backend service to accept Twitch
requests and maintain the queue. This repo is the desktop client; the exact
backend repository is not specified here.

## Title bar and Twitch connection

- The left side shows Twitch as **Offline**, **Connecting...**, **Online**, or
  **Connection error**. A status polling failure is displayed as a connection
  error; polling continues so the UI can recover when the backend is available
  again.
- **Connect** sends `POST /api/twitch/connect`; **Disconnect** sends
  `POST /api/twitch/disconnect`. The button is disabled while the backend
  reports that Twitch is connecting. The status poll, rather than the button
  request, determines the displayed connection state.
- When Twitch is connected, the title bar displays a greeting with the
  authenticated username.
- The title bar also provides **minimize** and **close** controls. Its empty
  space can be used to drag the borderless window.
- Twitch authentication and connection management are handled by the separate
  backend. These controls request connection changes and display the backend's
  status; they do not implement Twitch login in the desktop app.

The Twitch connection status is separate from the desktop's player/backend API
connectivity and playback status.

## Playback controls and synchronization

- **Play/pause:** the UI posts the selected action to `POST /api/player/control`.
- **Seek:** dragging the timeline updates the thumb locally; releasing it sends
  the target position in seconds as a `seek` control.
- **Volume:** moving the volume slider sends `volume` controls immediately,
  normalized from `0.0` to `1.0`.
- **Skip:** sends `POST /api/player/skip`.
- Controls are received by the desktop from player state and applied to mpv over
  IPC. A new track starts at position zero and inherits the current volume.
- mpv playback properties (position, duration, volume, and paused state) are
  observed over IPC and reported to `POST /api/player/telemetry` every 500 ms.
  At end of track, the app sends final telemetry and notifies
  `POST /api/player/ended`.

## Main libraries and components

- **Rust** — application and backend-client logic.
- **Slint** (`slint`, `slint-build`) — declarative desktop UI.
- **reqwest** — HTTP calls to the backend API.
- **Serde / serde_json** — API JSON models and payloads.
- **mpv** — external video/audio playback process, controlled using IPC.
- **raw-window-handle** and **windows-sys** — native Windows window embedding.
- **arboard** — clipboard support; **open** — opening links in the browser.
