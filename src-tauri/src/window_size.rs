//! Window size persistence on Linux.
//!
//! `tauri-plugin-window-state` saves `inner_size()` in physical pixels and
//! restores it before the window is mapped. Under GTK that does not round
//! trip: the scale factor still reads 1 at restore time, so the saved
//! physical size is applied as a logical one, and `inner_size()` includes
//! the client-side decorations. On a scaled Wayland display the window grows
//! on every launch until the compositor maximises it.
//!
//! GTK's own `gtk_window_get_size` / `gtk_window_resize` pair is exact: both
//! are logical pixels without decorations. So on Linux the plugin keeps
//! position, maximised and fullscreen state, and the size is handled here.

use std::path::PathBuf;

use gtk::prelude::GtkWindowExt;
use serde::{Deserialize, Serialize};
use tauri::{LogicalSize, Manager, Runtime, WebviewWindow, Window};

const FILE: &str = "window-size.json";
/// Smaller than this is a broken save, not a size anyone chose.
const MIN_EDGE: i32 = 200;

#[derive(Serialize, Deserialize)]
struct Saved {
    width: i32,
    height: i32,
}

fn path<R: Runtime>(app: &tauri::AppHandle<R>) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|dir| dir.join(FILE))
}

/// Applies the saved size, if there is a usable one. Call from `setup`.
pub fn restore<R: Runtime>(window: &WebviewWindow<R>) {
    let Some(path) = path(window.app_handle()) else { return };
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let Ok(saved) = serde_json::from_str::<Saved>(&text) else { return };
    if saved.width < MIN_EDGE || saved.height < MIN_EDGE {
        return;
    }
    if let Err(err) = window.set_size(LogicalSize::new(saved.width, saved.height)) {
        tracing::warn!(%err, "window size not restored");
    }
}

/// Stores the current size. A maximised or fullscreen window keeps the last
/// normal size on file, so leaving that state later returns to it.
pub fn save<R: Runtime>(window: &Window<R>) {
    if window.is_maximized().unwrap_or(false) || window.is_fullscreen().unwrap_or(false) {
        return;
    }
    let Ok(gtk_window) = window.gtk_window() else { return };
    let (width, height) = gtk_window.size();
    if width < MIN_EDGE || height < MIN_EDGE {
        return;
    }
    let Some(path) = path(window.app_handle()) else { return };
    let result = serde_json::to_string(&Saved { width, height })
        .map_err(std::io::Error::other)
        .and_then(|text| {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, text)
        });
    if let Err(err) = result {
        tracing::warn!(%err, path = %path.display(), "window size not saved");
    }
}
