//! Editor (plugin GUI) primitives shared across host crates.

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

/// A native platform window handle that a plugin editor embeds into.
///
/// Platform mapping:
/// - macOS: `NSView*`
/// - Windows: `HWND`
/// - Linux/X11: window ID cast to pointer
///
/// Serializes as the underlying pointer's `u64` numeric value — host
/// processes that ship a `WindowHandle` over IPC to the plugin subprocess
/// rely on this representation.
#[derive(Debug, Clone, Copy)]
pub struct WindowHandle(*mut c_void);

unsafe impl Send for WindowHandle {}

impl WindowHandle {
    /// Wraps a raw platform window handle.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid platform window handle for the current OS
    /// (`NSView*` on macOS, `HWND` on Windows, X11 window ID on Linux) and
    /// must outlive any editor opened against it.
    pub unsafe fn from_raw(ptr: *mut c_void) -> Self {
        Self(ptr)
    }

    /// Alias for [`from_raw`](Self::from_raw), kept for callers spelling it
    /// this way.
    ///
    /// # Safety
    ///
    /// Same requirements as [`from_raw`](Self::from_raw).
    pub unsafe fn from_ptr(ptr: *mut c_void) -> Self {
        Self(ptr)
    }

    /// Wraps a handle given as its numeric pointer value, as it travels over
    /// IPC.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid platform window handle for the current OS
    /// when interpreted as a pointer, and must outlive any editor opened
    /// against it.
    pub unsafe fn from_u64(ptr: u64) -> Self {
        Self(ptr as *mut c_void)
    }

    /// Returns the raw platform handle, for passing to a native embedding call.
    ///
    /// Safe to obtain — the pointer is opaque here. Dereferencing it requires
    /// the invariants [`from_raw`](Self::from_raw) documents, which this type
    /// carries but does not verify.
    pub fn as_ptr(&self) -> *mut c_void {
        self.0
    }

    /// Returns the pointer's numeric value, the form it takes over IPC.
    pub fn as_u64(&self) -> u64 {
        self.0 as u64
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for WindowHandle {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.as_u64())
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for WindowHandle {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u64::deserialize(deserializer)?;
        // SAFETY: the pointer is opaque at this layer; consumers that
        // dereference it must already hold the invariants documented on
        // `from_u64`. Round-tripping through IPC is sound because the
        // sending side built it from a valid `from_raw`.
        Ok(unsafe { WindowHandle::from_u64(value) })
    }
}

/// Why opening a plugin editor failed.
///
/// Each variant names a distinct cause, so a UI can say what went wrong and
/// whether retrying makes sense.
#[derive(thiserror::Error, Debug)]
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
