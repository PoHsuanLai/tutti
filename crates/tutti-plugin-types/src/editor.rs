//! Editor (plugin GUI) primitives shared across host crates.

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::ffi::c_void;

/// Pixel dimensions of a plugin editor window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EditorSize {
    /// Width in pixels, in the platform's own coordinate space — not scaled for
    /// a HiDPI backing factor.
    pub width: u32,
    /// Height in pixels, in the same space as [`width`](Self::width).
    pub height: u32,
}

/// The static resize capabilities of a plugin editor view.
///
/// Covers what VST3 (`canResize`), CLAP (`gui_resize_hints`) and VST2 (which
/// leans on AppKit auto-resize on macOS) can express.
/// Grouped into [`ResizeHints`] (which axes the view supports) and
/// [`AspectRatio`] (whether the view wants its aspect ratio preserved),
/// plus the macOS-specific autoresize flag.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EditorCapabilities {
    /// Which axes the view may be resized along.
    pub resize: ResizeHints,
    /// Whether the view wants its aspect ratio preserved, and at what.
    pub aspect: AspectRatio,
    /// Whether the plugin's `NSView` reflows correctly when AppKit auto-sizes
    /// it (typical of VST3 and JUCE plugins).
    ///
    /// `false` for views that must be resized through an explicit `set_size`
    /// call, as CLAP views are. Only the VST2 editor path reads it.
    pub appkit_autoresize_friendly: bool,
}

/// Which axes the plugin editor view supports resizing along.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ResizeHints {
    /// Whether the view can be resized at all (VST3 `canResize`, CLAP
    /// `gui_can_resize`).
    pub resizable: bool,
    /// Whether the width may change. Meaningful only when
    /// [`resizable`](Self::resizable) is set.
    pub can_resize_horizontally: bool,
    /// Whether the height may change. Meaningful only when
    /// [`resizable`](Self::resizable) is set.
    pub can_resize_vertically: bool,
}

/// Aspect-ratio preferences for the editor view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AspectRatio {
    /// Whether the host should keep [`ratio`](Self::ratio) constant on resize.
    pub preserve: bool,
    /// The `(width, height)` ratio the plugin would like maintained, or `None`
    /// for no preference.
    pub ratio: Option<(u32, u32)>,
}

/// The backing-store scale factor of the window an editor embeds into:
/// physical pixels per logical pixel (1.0 at standard density, 2.0 on a
/// Retina display or at 200 % on Windows).
///
/// The host knows it from its window (`winit::Window::scale_factor`, Bevy's
/// `Window::scale_factor`) and passes it when it opens an editor; each format
/// forwards it where its ABI takes it: CLAP `clap_plugin_gui::set_scale` and
/// VST3 `IPlugViewContentScaleSupport::setContentScaleFactor`, on the
/// physical-pixel windowing systems only (Win32, X11). On macOS the window
/// system already reports logical pixels and a plugin reads the factor from
/// its own view, so it is not sent (both specs say so). VST2 and AU have no
/// such call.
///
/// Sent once, when the editor opens. A window moved to a display of another
/// scale while its editor is open keeps the scale it opened at: nothing
/// carries a scale to an open editor (there is no message for it on the
/// plugin server's wire), so the editor lays out at the new scale only once
/// it is closed and reopened. On macOS this does not arise, since a plugin
/// reads its own view's scale there.
///
/// Finite and positive by construction: a plugin multiplies its geometry by
/// it.
///
/// # Examples
///
/// ```
/// use tutti_plugin_types::ContentScale;
///
/// assert_eq!(ContentScale::new(2.0).map(ContentScale::get), Some(2.0));
/// assert_eq!(ContentScale::new(0.0), None);
/// assert_eq!(ContentScale::default(), ContentScale::IDENTITY);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "f64", into = "f64")
)]
pub struct ContentScale(f64);

impl ContentScale {
    /// 1.0: a standard-density display.
    pub const IDENTITY: Self = Self(1.0);

    /// The scale `factor`, or `None` unless it is finite and positive.
    #[must_use]
    pub fn new(factor: f64) -> Option<Self> {
        (factor.is_finite() && factor > 0.0).then_some(Self(factor))
    }

    /// The factor. A raw `f64`: it is handed straight to a C ABI (CLAP takes
    /// `double`, VST3 `float`).
    #[must_use]
    pub fn get(self) -> f64 {
        self.0
    }
}

impl Default for ContentScale {
    /// [`ContentScale::IDENTITY`].
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl TryFrom<f64> for ContentScale {
    type Error = InvalidContentScale;

    fn try_from(factor: f64) -> Result<Self, Self::Error> {
        Self::new(factor).ok_or(InvalidContentScale(factor))
    }
}

impl From<ContentScale> for f64 {
    fn from(scale: ContentScale) -> f64 {
        scale.0
    }
}

/// A scale factor that is not finite and positive (see [`ContentScale::new`]).
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq)]
#[error("a content scale must be finite and positive, got {0}")]
pub struct InvalidContentScale(pub f64);

/// A native platform window that a plugin editor embeds into.
///
/// Platform mapping:
/// - macOS: `NSView*`
/// - Windows: `HWND`
/// - X11: the window ID
/// - Wayland: `wl_surface*`
///
/// Held as the handle's number (as `raw-window-handle` holds an `HWND`), so it
/// is `Copy` and `Send` with no `unsafe impl`. Only a format's embed call, in
/// its `-ffi` crate, turns it back into a pointer that is used, and that call
/// relies on what the constructors promise: the number names a live window of
/// this process. [`from_window`](Self::from_window) has it from
/// `raw-window-handle` (a borrowed window handle cannot be made without that
/// promise); [`from_u64`](Self::from_u64), for a number that crossed a process
/// edge, is `unsafe` because it has it only from its caller.
///
/// No type here can check that the window outlives the editor opened on it: a
/// host closes the editor before it destroys the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowHandle(u64);

impl WindowHandle {
    /// The native window behind `window` (a winit or Bevy window, a wgpu
    /// surface's window, ...).
    ///
    /// # Errors
    ///
    /// [`EditorError::PluginError`] when `window` has no handle to give, and
    /// [`EditorError::UnsupportedPlatform`] for a windowing system no plugin
    /// format embeds into.
    pub fn from_window(window: &impl HasWindowHandle) -> Result<Self, EditorError> {
        let raw = window
            .window_handle()
            .map_err(|e| EditorError::PluginError(format!("failed to get window handle: {e}")))?
            .as_raw();
        let number = match raw {
            RawWindowHandle::AppKit(h) => h.ns_view.as_ptr() as u64,
            RawWindowHandle::Win32(h) => h.hwnd.get() as u64,
            #[allow(
                clippy::useless_conversion,
                reason = "an X11 `Window` is a `c_ulong`: `u64` here, `u32` on Windows"
            )]
            RawWindowHandle::Xlib(h) => u64::from(h.window),
            RawWindowHandle::Xcb(h) => u64::from(h.window.get()),
            RawWindowHandle::Wayland(h) => h.surface.as_ptr() as u64,
            other => {
                return Err(EditorError::UnsupportedPlatform {
                    got: format!("{other:?}"),
                });
            }
        };
        Ok(Self(number))
    }

    /// The window a number names, as it arrives over IPC.
    ///
    /// # Safety
    ///
    /// `raw` must be [`as_u64`](Self::as_u64) of a window that is live in this
    /// process (or the number of one a plugin is known never to embed into,
    /// as a test's stand-in), and the window must outlive any editor opened on
    /// it.
    #[must_use]
    #[allow(
        unsafe_code,
        reason = "the one constructor on its caller's word; see `# Safety`"
    )]
    pub const unsafe fn from_u64(raw: u64) -> Self {
        Self(raw)
    }

    /// The handle as the platform pointer a native embedding call takes.
    #[must_use]
    pub fn as_ptr(self) -> *mut c_void {
        self.0 as *mut c_void
    }

    /// The handle's number, the form it takes over IPC.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Why opening a plugin editor failed.
///
/// Each variant names a distinct cause, so a UI can say what went wrong and
/// whether retrying makes sense.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum EditorError {
    /// The plugin-server subprocess died, so there is nothing left to embed.
    /// Not retryable without reloading the plugin.
    #[error("plugin subprocess has crashed")]
    PluginCrashed,

    /// This build has no in-process editor host for the plugin's format — a
    /// missing host capability, not a plugin fault.
    #[error("no in-process GUI support compiled for {format}")]
    GuiNotSupported {
        /// The format's short name, from
        /// [`PluginClass::format_name`](crate::PluginClass::format_name).
        format: String,
    },

    /// The parent window handle is for a platform this host cannot embed into.
    #[error("parent window platform not supported by this plugin host: {got}")]
    UnsupportedPlatform {
        /// What was received, for the message.
        got: String,
    },

    /// The plugin itself refused or failed to open its editor, carrying
    /// whatever it reported.
    #[error("plugin failed to open editor: {0}")]
    PluginError(String),

    /// Another `open_editor` holds the GUI lock. Retryable once it completes.
    #[error("could not acquire GUI lock (another open_editor in progress?)")]
    Busy,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A content scale is finite and positive**: anything else is refused,
    /// on construction and on the wire, since a plugin multiplies its
    /// geometry by it.
    ///
    /// Mutation: drop `factor > 0.0` from `ContentScale::new` → 0.0 and
    /// -2.0 are accepted → fails. Mutation: drop `is_finite` → infinity is
    /// accepted → fails.
    #[test]
    fn a_content_scale_is_finite_and_positive() {
        for good in [0.5, 1.0, 1.25, 2.0, 3.0] {
            assert_eq!(ContentScale::new(good).map(ContentScale::get), Some(good));
            assert_eq!(ContentScale::try_from(good).map(f64::from), Ok(good));
        }
        for bad in [0.0, -0.0, -2.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(ContentScale::new(bad), None, "{bad}");
            assert!(ContentScale::try_from(bad).is_err(), "{bad}");
        }
    }

    /// The range contract (`new`, `TryFrom`, `Into`, serde), checked by the
    /// shared helper. `ContentScale` has no `of`.
    ///
    /// Mutation: drop `is_finite` from `ContentScale::new` → the helper
    /// reports "new accepted inf". `try_from` refusing above 100 → "try_from(
    /// 1.7976931348623157e308)" fails; `new` refusing at or below 1e-300 → "new refused valid
    /// 2.2250738585072014e-308"; `InvalidContentScale(0.0)` in `try_from` →
    /// "the error for -0.0 does not carry it".
    #[cfg(feature = "serde")]
    #[test]
    fn a_content_scale_keeps_the_range_contract() {
        tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
            new: ContentScale::new,
            get: ContentScale::get,
            of: None,
            valid: &[f64::MIN_POSITIVE, 0.5, 1.0, 2.0, 3.0, 100.0, f64::MAX],
            invalid: &[0.0, -0.0, -2.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY],
        });
    }
}
