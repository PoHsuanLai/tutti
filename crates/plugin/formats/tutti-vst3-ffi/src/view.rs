//! The plugin's editor view (`IPlugView`) and the frame the host gives it.

#[cfg(target_os = "windows")]
use vst3::Steinberg::kPlatformTypeHWND;
#[cfg(target_os = "macos")]
use vst3::Steinberg::kPlatformTypeNSView;
#[cfg(target_os = "linux")]
use vst3::Steinberg::kPlatformTypeX11EmbedWindowID;
use vst3::Steinberg::{
    FIDString, IPlugFrame, IPlugView, IPlugViewContentScaleSupport,
    IPlugViewContentScaleSupportTrait, IPlugViewTrait, ViewRect, kInvalidArgument, kResultFalse,
    kResultOk, kResultTrue,
};
use vst3::{ComPtr, ComWrapper};

use tutti_plugin_types::{ContentScale, EditorSize, WindowHandle};

use crate::com::HostPlugFrame;
use crate::error::{Vst3Error, check};

/// Why an editor could not be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EditorError {
    /// The plugin has no edit controller to ask for a view.
    NoController,
    /// `createView` returned nothing.
    NoView,
    /// `isPlatformTypeSupported` refused this platform's window type; the field
    /// is its name.
    PlatformRefused(String),
    /// `IPlugView::attached` failed.
    Attach(Vst3Error),
}

/// The editor of an open plugin: its view and the frame it was handed.
pub(crate) struct Editor {
    pub(crate) view: ComPtr<IPlugView>,
    pub(crate) plug_frame: ComWrapper<HostPlugFrame>,
}

/// The window type this platform's editors attach to.
fn native_platform_type() -> FIDString {
    #[cfg(target_os = "macos")]
    return kPlatformTypeNSView;
    #[cfg(target_os = "windows")]
    return kPlatformTypeHWND;
    #[cfg(target_os = "linux")]
    return kPlatformTypeX11EmbedWindowID;
}

/// A platform-type id as text.
fn platform_type_name(platform_type: FIDString) -> String {
    if platform_type.is_null() {
        return "<null>".to_string();
    }
    // SAFETY: the SDK's platform-type ids are static NUL-terminated strings.
    unsafe { std::ffi::CStr::from_ptr(platform_type) }
        .to_str()
        .unwrap_or("<invalid utf-8>")
        .to_string()
}

/// The factor `setContentScaleFactor` sends for a view embedded as
/// `platform_type` in a window of backing scale `scale`, or `None` where it
/// must not be sent.
///
/// The interface exists "on systems where plug-ins cannot get this
/// information directly like Microsoft Windows"
/// (`iplugviewcontentscalesupport.h`). An `NSView` (or `UIView`) already
/// carries its backing scale in logical-pixel geometry, so a plugin told the
/// factor as well applies it twice and opens at 2× on a Retina display: the
/// CLAP host withholds `set_scale` on cocoa for the same reason. VST3 takes a
/// `float`; a display's scale is far inside its precision.
fn content_scale_for(platform_type: FIDString, scale: ContentScale) -> Option<f32> {
    use vst3::Steinberg::{kPlatformTypeNSView, kPlatformTypeUIView};
    if platform_type.is_null() {
        return None;
    }
    let name = platform_type_name(platform_type);
    let logical = [kPlatformTypeNSView, kPlatformTypeUIView]
        .iter()
        .any(|&t| name == platform_type_name(t));
    (!logical).then_some(scale.get() as f32)
}

/// Does this `isPlatformTypeSupported` result mean "no"?
///
/// Only an explicit denial counts. `editorhost` treats anything but
/// `kResultTrue` as fatal, and copying that would break working plugins: the
/// SDK's own `CPluginView` — base class of `EditorView`, and so of a large
/// share of shipping plugins — returns `kNotImplemented` unconditionally
/// (`public.sdk/source/common/pluginview.cpp`) while its `attached` succeeds
/// anyway. A view that declines to answer is not a view that said no.
///
/// `kInvalidArgument` *is* a refusal: it is what `VSTGUIEditor` returns for a
/// type it does not handle (`vstguieditor.cpp`), which is precisely the
/// Wayland-only-view-handed-an-X11-id case this check exists to catch.
///
/// The asymmetry is deliberate — a false "unsupported" costs the user their
/// editor, while a false "supported" only reaches the failure the check was
/// meant to catch, which is where an unchecked call ends up anyway.
#[must_use]
pub fn platform_type_refused(result: i32) -> bool {
    result == kResultFalse || result == kInvalidArgument
}

/// Tears a view down: retract the host frame, then tell the view it is removed.
///
/// The order is the point, and it matches `editorhost`'s `closePlugView`. The
/// `HostPlugFrame` this view was given dies with the `EditorState` that owned
/// it, so a plugin still holding the pointer during `removed()` — to report a
/// final `resizeView`, say — would call through memory about to be freed.
/// Retracting first makes that unrepresentable rather than merely unlikely.
///
/// Split out of `close_editor_unchecked` so the ordering is reachable from a
/// test without a loaded plugin: the state this runs on can only be built by
/// `open_editor`, but the sequence itself is what needs pinning.
pub fn detach_view(view: &ComPtr<IPlugView>) {
    // SAFETY: `view` is a live view; null is the documented "no frame" argument.
    unsafe {
        view.setFrame(std::ptr::null_mut());
        view.removed();
    }
}

/// Offer a key press to the open editor. Returns `true` if the plugin
/// **consumed** it, in which case the host must not act on it too.
///
/// # The two key codes
///
/// VST3 carries a character *and* a virtual key, and a caller supplies one
/// while zeroing the other. `key` is the UTF-16 character the keystroke
/// produced; `virtual_key` is a `VirtualKeyCodes` value for keystrokes that
/// produce no character (arrows, function keys). Sending both, or neither,
/// is what plugins disagree about handling — pick one.
///
/// `modifiers` is an OR of `KeyModifier` bits. Note the SDK's own
/// convention is platform-dependent: `kCommandKey` is Cmd on macOS and
/// Ctrl on Windows and Linux, so a caller mapping from a neutral modifier
/// set has to know which platform it is on.
///
/// # Arbitration
///
/// Honour the return value rather than filtering keys before the call. A
/// host that reserves keys for itself — space for transport, say — takes
/// them from every plugin that legitimately uses them, and cannot know
/// which those are. The spec's answer is this bool, and it applies only
/// while the editor holds focus.
///
/// Returns `false` if no editor is open. Main/UI thread.
///
/// # Panics
///
/// In debug builds, panics if called off the thread registered with
/// [`tutti_plugin_types::mark_main_thread`].
#[must_use]
pub fn send_key_down(view: &ComPtr<IPlugView>, key: u16, virtual_key: i16, modifiers: i16) -> bool {
    // SAFETY: a plain call on a live view.
    unsafe { view.onKeyDown(key, virtual_key, modifiers) == kResultTrue }
}

/// Offer a key release to the open editor. See
/// [`send_key_down`] — identical contract, and a host
/// that forwards one without the other leaves plugins holding keys down.
///
/// # Panics
///
/// In debug builds, panics if called off the thread registered with
/// [`tutti_plugin_types::mark_main_thread`].
#[must_use]
pub fn send_key_up(view: &ComPtr<IPlugView>, key: u16, virtual_key: i16, modifiers: i16) -> bool {
    // SAFETY: a plain call on a live view.
    unsafe { view.onKeyUp(key, virtual_key, modifiers) == kResultTrue }
}

/// Offer a mouse-wheel movement to the open editor, `distance` in wheel
/// notches. Returns `true` if the plugin consumed it.
///
/// `IPlugView::onWheel` takes a single scalar, so horizontal scroll and
/// modifier-qualified scroll have nowhere to go — a caller with either must
/// handle it host-side. Main/UI thread.
///
/// # Panics
///
/// In debug builds, panics if called off the thread registered with
/// [`tutti_plugin_types::mark_main_thread`].
#[must_use]
pub fn send_wheel(view: &ComPtr<IPlugView>, distance: f32) -> bool {
    // SAFETY: a plain call on a live view.
    unsafe { view.onWheel(distance) == kResultTrue }
}

/// Tells a view whether it now has keyboard focus.
///
/// Unlike the key forwarders this has no consumed/not-consumed answer — it is
/// a notification, and the spec defines no meaning for a refusal.
pub fn set_view_focus(view: &ComPtr<IPlugView>, focused: bool) {
    // SAFETY: a plain call on a live view.
    unsafe {
        view.onFocus(u8::from(focused));
    }
}

fn size_of_rect(rect: &ViewRect) -> EditorSize {
    EditorSize {
        width: (rect.right - rect.left) as u32,
        height: (rect.bottom - rect.top) as u32,
    }
}

fn rect_of_size(size: EditorSize) -> ViewRect {
    ViewRect {
        left: 0,
        top: 0,
        right: size.width as i32,
        bottom: size.height as i32,
    }
}

impl Editor {
    /// Builds the frame, attaches the view to `parent` and reads its size.
    pub(crate) fn open(
        view: ComPtr<IPlugView>,
        parent: WindowHandle,
        scale: ContentScale,
        plug_frame: ComWrapper<HostPlugFrame>,
    ) -> Result<(Self, Option<EditorSize>), EditorError> {
        let platform_type = native_platform_type();
        // SAFETY: a plain call on a live view with a static platform-type id.
        let supported = unsafe { view.isPlatformTypeSupported(platform_type) };
        if platform_type_refused(supported) {
            return Err(EditorError::PlatformRefused(platform_type_name(
                platform_type,
            )));
        }

        let frame_ptr = plug_frame
            .as_com_ref::<IPlugFrame>()
            .map(|r| r.as_ptr())
            .unwrap_or(std::ptr::null_mut());
        // SAFETY: `frame_ptr` is the frame `plug_frame` keeps alive, and the
        // editor stores that wrapper beside the view. It is withdrawn
        // (`setFrame(nullptr)`) before the view is released, in `close`.
        unsafe {
            let _ = view.setFrame(frame_ptr);
        }

        if let Some(factor) = content_scale_for(platform_type, scale)
            && let Some(support) = view.cast::<IPlugViewContentScaleSupport>()
        {
            // SAFETY: a plain call on a live interface.
            unsafe {
                let _ = support.setContentScaleFactor(factor);
            }
        }

        // SAFETY: `parent` is a valid window handle by `WindowHandle`'s
        // constructor contract, and outlives the editor.
        let result = unsafe { view.attached(parent.as_ptr(), platform_type) };
        check(result).map_err(EditorError::Attach)?;

        let size = {
            let mut rect = ViewRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            // SAFETY: `rect` is a valid out-parameter.
            let result = unsafe { view.getSize(&mut rect) };
            (result == kResultOk).then(|| size_of_rect(&rect))
        };
        Ok((Self { view, plug_frame }, size))
    }

    /// Withdraws the frame and removes the view.
    pub(crate) fn close(self) {
        detach_view(&self.view);
    }

    /// Whether the view says it can be resized (`kResultOk`).
    pub(crate) fn resizable(&self) -> bool {
        // SAFETY: a plain call on a live view.
        unsafe { self.view.canResize() == kResultOk }
    }

    /// What the view makes of `requested`: the size it would snap to, if it
    /// answers.
    pub(crate) fn constrain(&self, requested: EditorSize) -> (EditorSize, bool) {
        let mut rect = rect_of_size(requested);
        // SAFETY: `rect` is a valid in/out parameter.
        let snapped = unsafe { self.view.checkSizeConstraint(&mut rect) } == kResultTrue;
        (size_of_rect(&rect), snapped)
    }

    /// Resizes the view to `requested`, or to what it snaps to.
    pub(crate) fn resize(&self, requested: EditorSize) -> Result<EditorSize, Vst3Error> {
        let requested_rect = rect_of_size(requested);
        let mut constrained = requested_rect;
        // SAFETY: `constrained` is a valid in/out parameter.
        let snapped = unsafe { self.view.checkSizeConstraint(&mut constrained) } == kResultTrue;
        let mut rect = if snapped { constrained } else { requested_rect };
        // SAFETY: `rect` is a valid in-parameter for the call.
        check(unsafe { self.view.onSize(&mut rect) })?;
        Ok(size_of_rect(&rect))
    }
}

#[cfg(test)]
mod content_scale_tests {
    use super::content_scale_for;
    use tutti_plugin_types::ContentScale;
    use vst3::Steinberg::{
        kPlatformTypeHWND, kPlatformTypeNSView, kPlatformTypeUIView, kPlatformTypeX11EmbedWindowID,
    };

    /// **The window's scale reaches a view on a physical-pixel window system,
    /// and never one on a logical-pixel system.** An HWND or X11 view is told
    /// 2.0; an `NSView` or `UIView`, which already lays out in logical pixels,
    /// is told nothing (it would apply the factor twice).
    ///
    /// Mutation: return `Some(1.0)` → the HWND view reads 1.0 → fails.
    /// Mutation: drop the logical-pixel check → the `NSView` is told 2.0 →
    /// fails.
    #[test]
    fn the_scale_is_sent_only_on_a_physical_pixel_window_system() {
        let two = ContentScale::new(2.0).expect("a scale");
        assert_eq!(content_scale_for(kPlatformTypeHWND, two), Some(2.0));
        assert_eq!(
            content_scale_for(kPlatformTypeX11EmbedWindowID, two),
            Some(2.0)
        );
        assert_eq!(content_scale_for(kPlatformTypeNSView, two), None);
        assert_eq!(content_scale_for(kPlatformTypeUIView, two), None);
        assert_eq!(content_scale_for(std::ptr::null(), two), None);
    }
}
