
slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;

    // Borderless window controls.
    let weak_ui = ui.as_weak();
    ui.on_minimize_requested(move || {
        if let Some(ui) = weak_ui.upgrade() {
            ui.window().set_minimized(true);
        }
    });

    let weak_ui = ui.as_weak();
    ui.on_close_window_requested(move || {
        if let Some(ui) = weak_ui.upgrade() {
            ui.window().hide();
        }
    });

    // Copy OBS Browser Source URLs.
    ui.on_copy_overlay_url(|url| {
        match arboard::Clipboard::new() {
            Ok(mut clipboard) => match clipboard.set_text(url.to_string()) {
                Ok(()) => true,
                Err(error) => {
                    eprintln!("Failed to copy overlay URL: {error}");
                    false
                }
            },
            Err(error) => {
                eprintln!("Failed to access clipboard: {error}");
                false
            }
        }
    });

    // Open creator's Twitch channel.
    ui.on_open_creator_link(|| {
        if let Err(error) = open::that("https://www.twitch.tv/cypherusuh__") {
            eprintln!("Failed to open Twitch link: {error}");
        }
    });

    // Backend actions will be connected later.
    ui.on_connection_toggle_requested(|| {
        eprintln!("Twitch connection action is not wired up yet.");
    });

    ui.on_skip_requested(|| {
        eprintln!("Skip action is not wired up yet.");
    });

    ui.run()
}
