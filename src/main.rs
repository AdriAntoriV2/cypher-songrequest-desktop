
slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;

    // Resize the main window when the inline video preview is toggled.
    let weak_ui = ui.as_weak();
    let mut collapsed_size: Option<slint::PhysicalSize> = None;

    ui.on_video_visibility_changed(move |visible| {
        if let Some(ui) = weak_ui.upgrade() {
            let window = ui.window();

            if visible {
                let current_size = window.size();
                let scale_factor = window.scale_factor();

                collapsed_size = Some(current_size);

                // The preview uses the available content width in a 16:9 ratio.
                let logical_width =
                    current_size.width as f32 / scale_factor - 18.0;
                let preview_height = (logical_width.max(0.0) * 9.0 / 16.0) + 8.0;
                let extra_physical_height =
                    (preview_height * scale_factor).ceil() as u32;

                window.set_size(slint::PhysicalSize::new(
                    current_size.width,
                    current_size.height.saturating_add(extra_physical_height),
                ));
            } else if let Some(original_size) = collapsed_size.take() {
                window.set_size(original_size);
            }
        }
    });

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
