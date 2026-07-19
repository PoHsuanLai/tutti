//! Editor (plugin GUI) primitives shared across host crates.

use std::ffi::c_void;

/// Pixel dimensions of a plugin editor window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EditorSize {
    pub width: u32,
    pub height: u32,
}

/// Static capabilities of a plugin editor view. Superset of the fields
/// expressed by VST3 (`canResize`), CLAP (`gui_resize_hints`), and VST2
/// (which leans on AppKit auto-resize semantics on macOS).
///
/// Grouped into [`ResizeHints`] (which axes the view supports) and
/// [`AspectRatio`] (whether the view wants its aspect ratio preserved),
/// plus the macOS-specific autoresize flag.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EditorCapabilities {
    pub resize: ResizeHints,
    pub aspect: AspectRatio,
    /// `true` if the plugin's NSView resizes correctly when AppKit
    /// auto-sizes it (e.g. VST3 / JUCE). `false` for formats like
    /// CLAP that pin their NSView and require explicit `set_size`
    /// calls to reflow internal layout. VST2-specific surface.
    pub appkit_autoresize_friendly: bool,
}

/// Which axes the plugin editor view supports resizing along.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ResizeHints {
    /// `true` if the view can be resized at all (VST3 `canResize`, CLAP
    /// `gui_can_resize`).
    pub resizable: bool,
    pub can_resize_horizontally: bool,
    pub can_resize_vertically: bool,
}

/// Aspect-ratio preferences for the editor view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AspectRatio {
    /// `true` if the host should keep `aspect_ratio` constant on resize.
    pub preserve: bool,
    /// `(width, height)` ratio the plugin would like maintained. `None`
    /// means "no preference".
    pub ratio: Option<(u32, u32)>,
}

/// Native platform window handle used to embed plugin editors.
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
    /// # Safety
    /// `ptr` must be a valid platform window handle for the current OS
    /// (`NSView*` on macOS, `HWND` on Windows, X11 window ID on Linux) and
    /// must outlive any editor opened against it.
    pub unsafe fn from_raw(ptr: *mut c_void) -> Self {
        Self(ptr)
    }

    /// Alias for [`from_raw`](Self::from_raw) — vst2-host originally
    /// exposed the constructor under this name.
    ///
    /// # Safety
    /// Same requirements as [`from_raw`](Self::from_raw).
    pub unsafe fn from_ptr(ptr: *mut c_void) -> Self {
        Self(ptr)
    }

    /// Construct from a numeric pointer value. Used by the plugin-server
    /// IPC path, which transports `WindowHandle` as a `u64`.
    ///
    /// # Safety
    /// `ptr` must be a valid platform window handle for the current OS
    /// when interpreted as a pointer, and must outlive any editor opened
    /// against it.
    pub unsafe fn from_u64(ptr: u64) -> Self {
        Self(ptr as *mut c_void)
    }

    pub fn as_ptr(&self) -> *mut c_void {
        self.0
    }

    /// The pointer's numeric value, used by the IPC serializer.
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

/// Structured failures from opening an editor. Each variant tells the
/// caller exactly what went wrong so UI can surface a meaningful message
/// instead of falling back to "failed to open" for every case.
#[derive(thiserror::Error, Debug)]
pub enum EditorError {
    #[error("plugin subprocess has crashed")]
    PluginCrashed,

    #[error("no in-process GUI support compiled for {format}")]
    GuiNotSupported { format: String },

    #[error("parent window platform not supported by this plugin host: {got}")]
    UnsupportedPlatform { got: String },

    #[error("plugin failed to open editor: {0}")]
    PluginError(String),

    #[error("could not acquire GUI lock (another open_editor in progress?)")]
    Busy,
}
