//! Host-side support for AU Cocoa editor views.
//!
//! [`AuEditor`] loads the `NSView` the plugin advertises via
//! `kAudioUnitProperty_CocoaUI`, retains it, and optionally attaches it as a
//! subview of a caller-provided parent `NSView`.

#![cfg(target_os = "macos")]
// AudioUnit is a raw opaque C pointer; every public helper in this module
// takes one and delegates to AudioToolbox calls that expect a valid unit.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod cocoa;

use objc2::msg_send;
use objc2::runtime::AnyObject;
use std::os::raw::c_void;

use tutti_plugin_types::{EditorSize, WindowHandle};

use crate::error::Result;
use crate::ffi::property_size;
use crate::types::*;

/// Owned handle to an AU's Cocoa editor view.
///
/// The editor view is released when the handle is dropped. The handle is
/// `Send` because the retained `NSView` pointer is self-contained, but all
/// AppKit calls must still originate from the main thread.
pub struct AuEditor {
    view: *mut AnyObject,
    unit: AudioUnit,
}

// SAFETY: the view is released only by our Drop; we never hand out the raw
// pointer except through `view_ptr()`, so there is no aliasing across threads.
unsafe impl Send for AuEditor {}

impl AuEditor {
    /// Instantiate the AU's Cocoa editor view and attach it as a subview of
    /// the caller's parent `NSView`. Pass `None` to instantiate the view
    /// without attaching it to a parent hierarchy.
    ///
    /// # Safety
    /// `unit` must be a valid, initialized `AudioUnit`. If `parent` is
    /// `Some`, its underlying pointer must be a valid `NSView*` owned by
    /// the caller. Must be called on the macOS main thread.
    ///
    /// # Errors
    /// Returns [`crate::error::AuError::InvalidBuffer`] if the AU does not
    /// advertise a Cocoa view bundle or the view factory fails to load.
    pub unsafe fn open(unit: AudioUnit, parent: Option<WindowHandle>) -> Result<Self> {
        tutti_plugin_types::assert_main_thread();
        let view = cocoa::create_view(unit)?;

        if let Some(handle) = parent {
            let parent_obj = handle.as_ptr() as *mut AnyObject;
            if !parent_obj.is_null() {
                let _: () = msg_send![parent_obj, addSubview: view];
            }
        }

        Ok(Self { view, unit })
    }

    /// Whether the AU advertises a Cocoa editor view.
    ///
    /// Calls `AudioUnitGetPropertyInfo(kAudioUnitProperty_CocoaUI)` and
    /// reports `true` if the property exists with a non-zero size.
    pub fn has_editor(unit: AudioUnit) -> bool {
        let size = unsafe {
            property_size(
                unit,
                K_AUDIO_UNIT_PROPERTY_COCOA_UI,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
            )
        };
        matches!(size, Ok(n) if n > 0)
    }

    /// Remove the view from its superview (if any) and release it. Safe to
    /// call multiple times; subsequent calls are no-ops. Must be called on the
    /// macOS main thread.
    pub fn close(&mut self) {
        tutti_plugin_types::assert_main_thread();
        self.close_inner();
    }

    /// View teardown without the main-thread assertion — used by `Drop`, which
    /// can run on the audio thread when the graph releases the editor. The
    /// public [`close`](Self::close) asserts; this does not.
    fn close_inner(&mut self) {
        if !self.view.is_null() {
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
    pub fn unit(&self) -> AudioUnit {
        self.unit
    }
}

impl Drop for AuEditor {
    fn drop(&mut self) {
        // No main-thread assert: Drop can run on the audio thread when the
        // graph releases the editor. See `close_inner`.
        self.close_inner();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::*;

    #[test]
    fn test_has_editor() {
        let desc = AudioComponentDescription {
            component_type: K_AUDIO_UNIT_TYPE_EFFECT,
            component_sub_type: u32::from_be_bytes(*b"dely"),
            component_manufacturer: u32::from_be_bytes(*b"appl"),
            component_flags: 0,
            component_flags_mask: 0,
        };
        let comp = find_component(&desc).expect("AUDelay should be present");
        let mut instance: AudioComponentInstance = std::ptr::null_mut();
        let status = unsafe { AudioComponentInstanceNew(comp, &mut instance) };
        assert_eq!(status, NO_ERR);
        unsafe { AudioUnitInitialize(instance) };

        let _has = AuEditor::has_editor(instance);

        unsafe {
            AudioUnitUninitialize(instance);
            AudioComponentInstanceDispose(instance);
        }
    }
}
