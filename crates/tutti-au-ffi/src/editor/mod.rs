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
use crate::unit::Unit;

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

// SAFETY: the view is released only by this type's `Drop`, and the raw pointer
// leaves only through `view_ptr()`, so there is no aliasing across threads.
// SAFETY: the view is only touched from the main thread (every method asserts
// it), so the value may be moved between threads between calls; the `Unit` it
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
    /// # Safety
    /// `unit` must be a valid, initialized `AudioUnit`. If `parent` is
    /// `Some`, its underlying pointer must be a valid `NSView*` owned by
    /// the caller. Must be called on the macOS main thread.
    ///
    /// # Errors
    /// Returns `InvalidBuffer` if the AU does not
    /// advertise a Cocoa view bundle or the view factory fails to load.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn open(unit: &Unit, parent: Option<WindowHandle>, preferred: EditorSize) -> Result<Self> {
        tutti_plugin_types::assert_main_thread();
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
        let info = unit.property_info(
            coreaudio_sys::kAudioUnitProperty_CocoaUI,
            coreaudio_sys::kAudioUnitScope_Global,
            0,
        );
        matches!(info, Ok(i) if i.size > 0)
    }

    /// Removes the view from its superview (if any) and release it. Safe to
    /// call multiple times; subsequent calls are no-ops. Must be called on the
    /// macOS main thread — `removeFromSuperview`/`release` are AppKit calls and
    /// touching AppKit off the main thread is undefined behavior. This is the
    /// single teardown path; [`Drop`](Self::drop) routes through it too.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn close(&mut self) {
        tutti_plugin_types::assert_main_thread();
        if !self.view.is_null() {
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
        if self.view.is_null() {
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
        // Single teardown path: route through `close`, which asserts main-thread
        // affinity (debug builds). Dropping an `AuEditor` off the main thread is
        // an AppKit UB hazard — the assert turns that latent corruption into a
        // located panic rather than silently bypassing the check.
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
}
