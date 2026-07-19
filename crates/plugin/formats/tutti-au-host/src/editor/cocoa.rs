//! Cocoa view-factory plumbing for AU editors.
//!
//! Reads `kAudioUnitProperty_CocoaUI`, loads the advertised bundle, and
//! instantiates the `NSView` via the factory's `uiViewForAudioUnit:withSize:`
//! method.

#![cfg(target_os = "macos")]

use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject, Sel};
use std::ffi::CString;
use std::os::raw::c_void;

use crate::cf::{CfString, CfUrl};
use crate::error::{AuError, Result};
use crate::ffi::get_property_bytes;
use crate::types::*;

// objc_msgSend trampolines for calls where objc2's msg_send! macro rejects
// the type encoding (AudioUnit / CFURL are C opaque pointers, not ObjC objects).
//
// On ARM64, objc_msgSend is not variadic — it uses the standard calling
// convention, so struct args (NSSize) must be declared explicitly so they
// land in the correct registers. Each selector's real ABI is distinct, which
// is why we deliberately declare multiple trampolines against the same symbol.
#[allow(clashing_extern_declarations)]
extern "C" {
    #[link_name = "objc_msgSend"]
    fn msg_send_bundle_with_url(
        receiver: *mut AnyObject,
        sel: Sel,
        url: *const c_void,
    ) -> *mut AnyObject;

    #[link_name = "objc_msgSend"]
    fn msg_send_ui_view_for_au(
        receiver: *mut AnyObject,
        sel: Sel,
        unit: AudioUnit,
        size: NSSize,
    ) -> *mut AnyObject;
}

/// Top-level entry: query the AU's CocoaUI info, load the view factory bundle,
/// and instantiate the editor `NSView`.
pub(super) unsafe fn create_view(unit: AudioUnit) -> Result<*mut AnyObject> {
    let (bundle_url, class_name) = load_cocoa_view_info(unit)?;
    let bundle = load_bundle(&bundle_url)?;
    let factory = instantiate_factory(bundle, &class_name)?;
    make_view(factory, unit)
}

unsafe fn load_cocoa_view_info(unit: AudioUnit) -> Result<(CfUrl, CfString)> {
    let bytes = get_property_bytes(
        unit,
        K_AUDIO_UNIT_PROPERTY_COCOA_UI,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
    )?;
    if bytes.is_empty() {
        return Err(AuError::OsStatus {
            function: "GetProperty(CocoaUI)",
            code: K_AUDIO_UNIT_ERR_INVALID_PROPERTY,
        });
    }

    let info_ptr = bytes.as_ptr() as *const AudioUnitCocoaViewInfo;
    let url_raw = (*info_ptr).bundle_url;
    let class_raw = (*info_ptr).class_name[0];

    let bundle_url = CfUrl::from_copied(url_raw)
        .ok_or_else(|| AuError::InvalidBuffer("CocoaUI info has null bundle URL".into()))?;
    let class_name = CfString::from_copied(class_raw)
        .ok_or_else(|| AuError::InvalidBuffer("CocoaUI info has null class name".into()))?;

    Ok((bundle_url, class_name))
}

unsafe fn load_bundle(url: &CfUrl) -> Result<*mut AnyObject> {
    let ns_bundle = AnyClass::get(c"NSBundle").expect("NSBundle class must exist");
    let sel = Sel::register(c"bundleWithURL:");
    let bundle: *mut AnyObject = msg_send_bundle_with_url(
        ns_bundle as *const _ as *mut AnyObject,
        sel,
        url.as_raw() as *const c_void,
    );
    if bundle.is_null() {
        return Err(AuError::InvalidBuffer(
            "Failed to load AU view bundle".into(),
        ));
    }
    let _: bool = msg_send![bundle, load];
    Ok(bundle)
}

unsafe fn instantiate_factory(
    _bundle: *mut AnyObject,
    class_name: &CfString,
) -> Result<*mut AnyObject> {
    let factory_name = class_name.to_string();
    let factory_cstr = CString::new(factory_name.clone())
        .map_err(|_| AuError::InvalidBuffer(format!("Invalid class name: {factory_name}")))?;

    let class = AnyClass::get(&factory_cstr).ok_or_else(|| {
        AuError::InvalidBuffer(format!("ObjC class '{factory_name}' not found in bundle"))
    })?;

    let factory: *mut AnyObject = msg_send![class, alloc];
    let factory: *mut AnyObject = msg_send![factory, init];
    if factory.is_null() {
        return Err(AuError::InvalidBuffer(
            "Failed to instantiate AU view factory".into(),
        ));
    }
    Ok(factory)
}

unsafe fn make_view(factory: *mut AnyObject, unit: AudioUnit) -> Result<*mut AnyObject> {
    let size = NSSize {
        width: 800.0,
        height: 600.0,
    };
    let sel = Sel::register(c"uiViewForAudioUnit:withSize:");
    let view: *mut AnyObject = msg_send_ui_view_for_au(factory, sel, unit, size);

    let _: () = msg_send![factory, release];

    if view.is_null() {
        return Err(AuError::InvalidBuffer(
            "AU view factory returned null view".into(),
        ));
    }

    // Retain so we own it independently of the factory's autorelease pool.
    let view: *mut AnyObject = msg_send![view, retain];
    Ok(view)
}
