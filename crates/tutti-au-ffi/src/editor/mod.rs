//! Host-side support for AU Cocoa editor views.
//!
//! [`AuEditor`] loads the `NSView` the plugin advertises via
//! `kAudioUnitProperty_CocoaUI`, retains it, and optionally attaches it as a
//! subview of a caller-provided parent `NSView`.

// AudioUnit is a raw opaque C pointer; every public helper in this module
// takes one and delegates to AudioToolbox calls that expect a valid unit.

mod cocoa;

use objc2::msg_send;
use objc2::runtime::AnyObject;
use objc2_foundation::NSRect;
use std::os::raw::c_void;

use tutti_plugin_types::{EditorSize, WindowHandle};

use crate::error::Result;
use crate::property::{CocoaUi, Element, Scope};
use crate::unit::Unit;

unsafe extern "C" {
    fn pthread_main_np() -> std::os::raw::c_int;
}

/// Whether the caller is AppKit's main thread, by the OS's own answer (no
/// marking needed, so it cannot be fooled by marking the wrong thread).
fn on_main_thread() -> bool {
    // SAFETY: `pthread_main_np` takes no arguments and reads no memory.
    unsafe { pthread_main_np() != 0 }
}

static LEAKED_VIEWS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many editor views were leaked because they were closed off the main thread.
pub fn leaked_editor_views() -> usize {
    LEAKED_VIEWS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Owned handle to an AU's Cocoa editor view.
///
/// The editor view is released when the handle is dropped. The handle is
/// `Send` because the retained `NSView` pointer is self-contained, but all
/// AppKit calls must still originate from the main thread.
pub struct AuEditor {
    view: *mut AnyObject,
    unit: Unit,
}

impl std::fmt::Debug for AuEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuEditor").finish_non_exhaustive()
    }
}

// SAFETY: the view is touched only on the main thread: `open` refuses off it,
// and `close` (so `Drop`) leaks the view instead of releasing it off it, so a
// moved value never makes an AppKit call from another thread. The `Unit` it
// holds is `Send`.
unsafe impl Send for AuEditor {}

impl AuEditor {
    /// Instantiates the AU's Cocoa editor view and attach it as a subview of
    /// the caller's parent `NSView`. Pass `None` to instantiate the view
    /// without attaching it to a parent hierarchy.
    ///
    /// `preferred` is the size the host would like, passed to the factory as
    /// `inPreferredSize` (`AUCocoaUIView.h:47-48`). It is a *hint*: the plugin
    /// may return a view of any size, so read the actual frame back afterwards
    /// rather than assuming the request was honoured. It must be the host's real
    /// window size: a fixed figure tells every AU the host wants that size
    /// whatever window the editor is about to live in.
    ///
    /// `parent` is a `WindowHandle`, whose constructor carries the claim that
    /// its pointer is a live `NSView`.
    ///
    /// # Errors
    /// `Error::Invalid` off the OS main thread (`pthread_main_np`), or if the AU
    /// advertises no Cocoa view or the view factory fails to load.
    pub fn open(unit: &Unit, parent: Option<WindowHandle>, preferred: EditorSize) -> Result<Self> {
        if !on_main_thread() {
            return Err(crate::Error::Invalid(
                "an AU editor opens only on the main thread".into(),
            ));
        }
        let view = cocoa::create_view(unit, preferred)?;

        if let Some(handle) = parent {
            let parent_obj = handle.as_ptr() as *mut AnyObject;
            if !parent_obj.is_null() {
                // SAFETY: `WindowHandle` carries its constructor's claim that
                // the parent is a live NSView; `view` is the live view just
                // created, and `addSubview:` retains it.
                let _: () = unsafe { msg_send![parent_obj, addSubview: view] };
            }
        }

        Ok(Self {
            view,
            unit: unit.clone(),
        })
    }

    /// Whether the AU advertises a Cocoa editor view.
    ///
    /// Calls `AudioUnitGetPropertyInfo(kAudioUnitProperty_CocoaUI)` and
    /// reports `true` if the property exists with a non-zero size.
    pub fn has_editor(unit: &Unit) -> bool {
        let info = unit.property_info::<CocoaUi>(Scope::GLOBAL, Element::ZERO);
        matches!(info, Ok(i) if i.size > 0)
    }

    /// Removes the view from its superview (if any) and releases it. Later calls
    /// are no-ops. AppKit is touched only on the main thread: off it the view is
    /// leaked instead. This is the single teardown path; `Drop` routes through it.
    pub fn close(&mut self) {
        if !self.view.is_null() {
            if !on_main_thread() {
                self.leak_view();
                return;
            }
            // SAFETY: `view` is the live +1 view this editor owns (non-null,
            // checked); it leaves its superview and the reference is released
            // once, then the pointer is nulled below.
            unsafe {
                let _: () = msg_send![self.view, removeFromSuperview];
                let _: () = msg_send![self.view, release];
            }
            self.view = std::ptr::null_mut();
        }
    }

    /// Editor view frame size in points. Returns
    /// `EditorSize { width: 0, height: 0 }` after [`close`](Self::close).
    pub fn editor_size(&self) -> EditorSize {
        if self.view.is_null() || !on_main_thread() {
            return EditorSize {
                width: 0,
                height: 0,
            };
        }
        // SAFETY: `view` is the live view this editor owns (non-null, checked).
        unsafe {
            let frame: NSRect = msg_send![self.view, frame];
            EditorSize {
                width: frame.size.width as u32,
                height: frame.size.height as u32,
            }
        }
    }

    /// Gives up the view without touching AppKit, off the main thread.
    ///
    /// The view stays in its window and may still talk to the unit, so a clone of
    /// the unit is leaked too: the instance must outlive the view. Counted in
    /// [`leaked_editor_views`] and logged once, so it is never silent.
    fn leak_view(&mut self) {
        std::mem::forget(self.unit.clone());
        if LEAKED_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
            eprintln!(
                "[tutti-au-ffi] an AU editor was closed off the main thread; its view is leaked"
            );
        }
        self.view = std::ptr::null_mut();
    }

    /// Raw `NSView*` pointer, useful for embedding the editor in a host-owned
    /// window hierarchy.
    pub fn view_ptr(&self) -> *mut c_void {
        self.view as *mut c_void
    }

    /// Raw `AudioUnit` this editor was created for.
    pub fn unit(&self) -> &Unit {
        &self.unit
    }
}

impl Drop for AuEditor {
    fn drop(&mut self) {
        // Single teardown path. Off the main thread `close` leaks the view rather
        // than call AppKit from the wrong thread.
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::ComponentRef;

    #[test]
    fn test_has_editor() {
        let desc = coreaudio_sys::AudioComponentDescription {
            componentType: coreaudio_sys::kAudioUnitType_Effect,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let comp = ComponentRef::first_matching(&desc).expect("AUDelay should be present");
        let unit = Unit::new(comp).expect("instantiate");
        unit.initialize().expect("initialize");
        let _has = AuEditor::has_editor(&unit);
    }

    /// `open` off the main thread is refused, whatever thread was "marked".
    #[test]
    fn open_off_the_main_thread_is_refused() {
        let desc = coreaudio_sys::AudioComponentDescription {
            componentType: coreaudio_sys::kAudioUnitType_Effect,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let comp = ComponentRef::first_matching(&desc).expect("AUDelay should be present");
        let unit = Unit::new(comp).expect("instantiate");
        let refused = std::thread::spawn(move || {
            let size = EditorSize {
                width: 100,
                height: 100,
            };
            match AuEditor::open(&unit, None, size) {
                Err(crate::Error::Invalid(why)) => why.contains("main thread"),
                _ => false,
            }
        })
        .join()
        .expect("thread");
        assert!(refused);
    }
}
