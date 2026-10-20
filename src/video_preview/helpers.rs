use std::path::PathBuf;

/// Resolve mpv relative to the executable or working directory.
///
/// Bare commands such as `mpv` are resolved through Windows PATH.
pub(super) fn resolve_mpv_path(configured_path: &str) -> Option<PathBuf> {
    let configured_path = configured_path.trim();

    if configured_path.is_empty() {
        return None;
    }

    let path = PathBuf::from(configured_path);

    if path.is_absolute() {
        return path.is_file().then_some(path);
    }

    if path.components().count() == 1 {
        return Some(path);
    }

    let mut candidates = Vec::new();

    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            candidates.push(directory.join(&path));
        }
    }

    if let Ok(directory) = std::env::current_dir() {
        candidates.push(directory.join(&path));
    }

    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
}

/// Convert a relative media endpoint to an absolute backend URL.
pub fn normalize_media_url(url: &str, api_base_url: &str) -> String {
    let url = url.trim();

    if url.is_empty() {
        String::new()
    } else if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!(
            "{}/{}",
            api_base_url.trim_end_matches('/'),
            url.trim_start_matches('/')
        )
    }
}