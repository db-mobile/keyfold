//! The desktop's light or dark preference on Linux.
//!
//! The webview's `prefers-color-scheme` follows the GTK 3 theme. GNOME's
//! dark style only sets the `color-scheme` preference and leaves the GTK
//! theme alone, so on a dark desktop the webview still reports light. The
//! freedesktop settings portal has the real answer, and says when it changes.

use gtk::gio;
use gtk::glib;
use tauri::{AppHandle, Emitter, Runtime};

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const SETTINGS: &str = "org.freedesktop.portal.Settings";
const NAMESPACE: &str = "org.freedesktop.appearance";
const KEY: &str = "color-scheme";
/// Emitted with "dark" or "light" when the desktop preference changes.
pub const EVENT: &str = "theme:system";

/// The portal wraps the value in one or two variants depending on its version.
fn unwrap_u32(mut v: glib::Variant) -> Option<u32> {
    for _ in 0..4 {
        if let Some(n) = v.get::<u32>() {
            return Some(n);
        }
        v = v.as_variant()?;
    }
    None
}

/// 1 is "prefer dark"; 0 (no preference) and 2 (prefer light) are light.
fn name(value: u32) -> &'static str {
    if value == 1 { "dark" } else { "light" }
}

/// `None` when there is no portal to ask; the caller falls back to the webview.
pub fn current() -> Option<&'static str> {
    let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok()?;
    let reply = bus
        .call_sync(
            Some(PORTAL),
            PATH,
            SETTINGS,
            "Read",
            Some(&(NAMESPACE, KEY).into()),
            None,
            gio::DBusCallFlags::NONE,
            1000,
            gio::Cancellable::NONE,
        )
        .ok()?;
    unwrap_u32(reply.child_value(0)).map(name)
}

/// Forwards changes of the preference to the webview for as long as the app runs.
pub fn watch<R: Runtime>(app: &AppHandle<R>) {
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        return;
    };
    let app = app.clone();
    let _subscription = bus.signal_subscribe(
        Some(PORTAL),
        Some(SETTINGS),
        Some("SettingChanged"),
        Some(PATH),
        Some(NAMESPACE),
        gio::DBusSignalFlags::NONE,
        move |_bus, _sender, _path, _iface, _signal, params| {
            if params.child_value(1).get::<String>().as_deref() != Some(KEY) {
                return;
            }
            if let Some(value) = unwrap_u32(params.child_value(2)) {
                let _ = app.emit(EVENT, name(value));
            }
        },
    );
}
