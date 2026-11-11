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
};
use vst3::{ComPtr, ComWrapper};

use tutti_plugin_types::{ContentScale, EditorSize, MainThread, WindowHandle};

use crate::com::HostPlugFrame;
use crate::error::{Vst3Error, check, is_ok};

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
pub fn detach_view(_main: &MainThread, view: &ComPtr<IPlugView>) {
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
#[must_use]
pub fn send_key_down(
    _main: &MainThread,
    view: &ComPtr<IPlugView>,
    key: u16,
    virtual_key: i16,
    modifiers: i16,
) -> bool {
    // SAFETY: a plain call on a live view.
    is_ok(unsafe { view.onKeyDown(key, virtual_key, modifiers) })
}

/// Offer a key release to the open editor. See
/// [`send_key_down`] — identical contract, and a host
/// that forwards one without the other leaves plugins holding keys down.
#[must_use]
pub fn send_key_up(
    _main: &MainThread,
    view: &ComPtr<IPlugView>,
    key: u16,
    virtual_key: i16,
    modifiers: i16,
) -> bool {
    // SAFETY: a plain call on a live view.
    is_ok(unsafe { view.onKeyUp(key, virtual_key, modifiers) })
}

/// Offer a mouse-wheel movement to the open editor, `distance` in wheel
/// notches. Returns `true` if the plugin consumed it.
///
/// `IPlugView::onWheel` takes a single scalar, so horizontal scroll and
/// modifier-qualified scroll have nowhere to go — a caller with either must
/// handle it host-side. Main/UI thread.
#[must_use]
pub fn send_wheel(_main: &MainThread, view: &ComPtr<IPlugView>, distance: f32) -> bool {
    // SAFETY: a plain call on a live view.
    is_ok(unsafe { view.onWheel(distance) })
}

/// Tells a view whether it now has keyboard focus.
///
/// Unlike the key forwarders this has no consumed/not-consumed answer — it is
/// a notification, and the spec defines no meaning for a refusal.
pub fn set_view_focus(_main: &MainThread, view: &ComPtr<IPlugView>, focused: bool) {
    // SAFETY: a plain call on a live view.
    unsafe {
        view.onFocus(u8::from(focused));
    }
}

/// The extent of a rect a plugin filled in, or `None` for one that is not a
/// size: a negative extent, or one whose subtraction overflows.
fn size_of_rect(rect: &ViewRect) -> Option<EditorSize> {
    let extent = |high: i32, low: i32| high.checked_sub(low).and_then(|n| u32::try_from(n).ok());
    Some(EditorSize {
        width: extent(rect.right, rect.left)?,
        height: extent(rect.bottom, rect.top)?,
    })
}

/// The rect for `size`, each extent saturating at `i32::MAX` (a rect cannot
/// hold more, and a wrap would be negative).
fn rect_of_size(size: EditorSize) -> ViewRect {
    ViewRect {
        left: 0,
        top: 0,
        right: i32::try_from(size.width).unwrap_or(i32::MAX),
        bottom: i32::try_from(size.height).unwrap_or(i32::MAX),
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
        // (`setFrame(nullptr)`) before the view is released: in `close`, or
        // below when `attached` fails.
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
        if let Err(error) = check(result) {
            // The view was never attached, but it holds the frame: withdraw it
            // so a plugin that kept the pointer does not keep a frame nothing
            // will pump.
            // SAFETY: a plain call on a live view; null is "no frame".
            unsafe {
                let _ = view.setFrame(std::ptr::null_mut());
            }
            return Err(EditorError::Attach(error));
        }

        let size = {
            let mut rect = ViewRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            // SAFETY: `rect` is a valid out-parameter.
            let result = unsafe { view.getSize(&mut rect) };
            is_ok(result).then(|| size_of_rect(&rect)).flatten()
        };
        Ok((Self { view, plug_frame }, size))
    }

    /// Withdraws the frame and removes the view.
    pub(crate) fn close(self, main: &MainThread) {
        detach_view(main, &self.view);
    }

    /// Whether the view says it can be resized (`kResultOk`).
    pub(crate) fn resizable(&self) -> bool {
        // SAFETY: a plain call on a live view.
        is_ok(unsafe { self.view.canResize() })
    }

    /// What the view makes of `requested`: the size it would snap to, if it
    /// answers.
    pub(crate) fn constrain(&self, requested: EditorSize) -> (EditorSize, bool) {
        let mut rect = rect_of_size(requested);
        // SAFETY: `rect` is a valid in/out parameter.
        let snapped = is_ok(unsafe { self.view.checkSizeConstraint(&mut rect) });
        // A rect that is not a size is no answer: keep the request, unsnapped.
        match size_of_rect(&rect) {
            Some(size) => (size, snapped),
            None => (requested, false),
        }
    }

    /// Resizes the view to `requested`, or to what it snaps to.
    pub(crate) fn resize(&self, requested: EditorSize) -> Result<EditorSize, Vst3Error> {
        let requested_rect = rect_of_size(requested);
        let mut constrained = requested_rect;
        // SAFETY: `constrained` is a valid in/out parameter.
        let snapped = is_ok(unsafe { self.view.checkSizeConstraint(&mut constrained) });
        let mut rect = if snapped { constrained } else { requested_rect };
        // SAFETY: `rect` is a valid in-parameter for the call.
        check(unsafe { self.view.onSize(&mut rect) })?;
        size_of_rect(&rect).ok_or(Vst3Error::InvalidArgument)
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

#[cfg(test)]
mod rect_tests {
    use super::*;

    const fn rect(left: i32, top: i32, right: i32, bottom: i32) -> ViewRect {
        ViewRect {
            left,
            top,
            right,
            bottom,
        }
    }

    /// **A rect a plugin fills in is a size only if both extents are
    /// non-negative and their subtraction fits.** A negative extent used to
    /// become about four billion, and a wide span overflowed (a panic in debug,
    /// a wrap in release).
    #[test]
    fn a_rect_that_is_not_a_size_is_refused() {
        assert_eq!(
            size_of_rect(&rect(10, 20, 110, 70)),
            Some(EditorSize {
                width: 100,
                height: 50
            })
        );
        assert_eq!(size_of_rect(&rect(100, 0, 10, 10)), None, "negative width");
        assert_eq!(size_of_rect(&rect(0, 100, 10, 10)), None, "negative height");
        assert_eq!(
            size_of_rect(&rect(i32::MIN, 0, i32::MAX, 10)),
            None,
            "the span overflows"
        );
    }

    /// **A size past `i32::MAX` saturates; it does not wrap negative.**
    #[test]
    fn a_size_past_the_rect_saturates() {
        let r = rect_of_size(EditorSize {
            width: u32::MAX,
            height: 3,
        });
        assert_eq!((r.right, r.bottom), (i32::MAX, 3));
    }
}

#[cfg(test)]
mod attach_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use vst3::Class;
    use vst3::Steinberg::{TBool, char16, int16, kResultOk, kResultTrue, tresult};

    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    enum Call {
        SetFrame,
        ClearFrame,
        Removed,
    }

    /// A view whose `attached` fails, recording the frame calls.
    struct Refusing(Arc<Mutex<Vec<Call>>>);

    impl Class for Refusing {
        type Interfaces = (IPlugView,);
    }

    impl IPlugViewTrait for Refusing {
        unsafe fn isPlatformTypeSupported(&self, _t: FIDString) -> tresult {
            kResultTrue
        }
        unsafe fn attached(&self, _p: *mut std::ffi::c_void, _t: FIDString) -> tresult {
            kInvalidArgument
        }
        unsafe fn removed(&self) -> tresult {
            self.0.lock().unwrap().push(Call::Removed);
            kResultOk
        }
        unsafe fn onWheel(&self, _d: f32) -> tresult {
            kResultFalse
        }
        unsafe fn onKeyDown(&self, _k: char16, _c: int16, _m: int16) -> tresult {
            kResultFalse
        }
        unsafe fn onKeyUp(&self, _k: char16, _c: int16, _m: int16) -> tresult {
            kResultFalse
        }
        unsafe fn getSize(&self, _s: *mut ViewRect) -> tresult {
            kResultFalse
        }
        unsafe fn onSize(&self, _s: *mut ViewRect) -> tresult {
            kResultOk
        }
        unsafe fn onFocus(&self, _s: TBool) -> tresult {
            kResultOk
        }
        unsafe fn setFrame(&self, frame: *mut IPlugFrame) -> tresult {
            self.0.lock().unwrap().push(if frame.is_null() {
                Call::ClearFrame
            } else {
                Call::SetFrame
            });
            kResultOk
        }
        unsafe fn canResize(&self) -> tresult {
            kResultFalse
        }
        unsafe fn checkSizeConstraint(&self, _r: *mut ViewRect) -> tresult {
            kResultFalse
        }
    }

    /// **A view that refuses `attached` is not left holding the host's frame.**
    /// The frame is set before `attached`; on the failure path nothing used to
    /// withdraw it, so a plugin that kept the pointer held a frame nothing
    /// would ever pump.
    #[test]
    fn a_failed_attach_withdraws_the_frame() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let view = ComWrapper::new(Refusing(calls.clone()))
            .to_com_ptr::<IPlugView>()
            .expect("the view implements IPlugView");
        let frame = HostPlugFrame::new(
            #[cfg(target_os = "linux")]
            crate::com::RunLoop::new(),
            #[cfg(target_os = "linux")]
            None,
        );
        // SAFETY: the handle is never dereferenced by the stub view.
        let parent = unsafe { WindowHandle::from_u64(1) };
        let result = Editor::open(view, parent, ContentScale::default(), frame);
        assert!(matches!(result, Err(EditorError::Attach(_))));
        assert_eq!(*calls.lock().unwrap(), [Call::SetFrame, Call::ClearFrame]);
    }
}
