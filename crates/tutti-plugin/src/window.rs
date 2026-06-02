//! Editor window types + platform handle extraction.
//!
//! `PluginHandle::open_editor` takes `impl HasWindowHandle` directly —
//! no wrapper. [`extract_platform_ptr`] reduces a [`RawWindowHandle`]
//! to the platform-native child-window pointer that each plugin format
//! expects (`NSView*`, `HWND`, X11 `Window`, `wl_surface*`).
//!
//! `EditorSize`, `EditorCapabilities`, and `WindowHandle` come from
//! `tutti-plugin-types` so the host crates and the IPC protocol share
//! the same shape.

use crate::error::EditorError;
use raw_window_handle::RawWindowHandle;
use std::ffi::c_void;

pub use tutti_plugin_types::{EditorCapabilities, EditorSize, WindowHandle};

/// Reduce a [`RawWindowHandle`] to the platform-native child-window
/// pointer plugin formats expect:
///
/// - macOS: `NSView*`
/// - Windows: `HWND`
/// - X11: `Window` (widened to pointer)
/// - Wayland: `wl_surface*`
pub(crate) fn extract_platform_ptr(handle: RawWindowHandle) -> Result<*mut c_void, EditorError> {
    match handle {
        RawWindowHandle::AppKit(h) => Ok(h.ns_view.as_ptr()),
        RawWindowHandle::Win32(h) => Ok(isize::from(h.hwnd) as *mut c_void),
        RawWindowHandle::Xlib(h) => Ok(h.window as *mut c_void),
        RawWindowHandle::Xcb(h) => Ok(h.window.get() as *mut c_void),
        RawWindowHandle::Wayland(h) => Ok(h.surface.as_ptr()),
        other => Err(EditorError::UnsupportedPlatform {
            got: format!("{other:?}"),
        }),
    }
}
