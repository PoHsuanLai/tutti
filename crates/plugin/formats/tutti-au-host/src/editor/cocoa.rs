//! Cocoa view-factory plumbing for AU editors.
//!
//! Reads `kAudioUnitProperty_CocoaUI`, loads the advertised bundle, and
//! instantiates the `NSView` via the factory's `uiViewForAudioUnit:withSize:`
//! method.

#![cfg(target_os = "macos")]

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::ClassType;
use objc2_foundation::{NSBundle, NSSize};
use std::os::raw::c_void;

use crate::cf::{CfString, CfUrl};
use crate::error::{AuError, Result};
use crate::ffi::get_property_bytes;
use crate::types::*;

/// Top-level entry: query the AU's CocoaUI info, load the view factory bundle,
/// and instantiate the editor `NSView`.
pub(super) unsafe fn create_view(unit: AudioUnit) -> Result<*mut AnyObject> {
    let (bundle_url, class_name) = load_cocoa_view_info(unit)?;
    let bundle = load_bundle(&bundle_url)?;
    let factory = instantiate_factory(&bundle, &class_name)?;
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
    let url_raw = (*info_ptr).mCocoaAUViewBundleLocation;
    let class_raw = (*info_ptr).mCocoaAUViewClass[0];

    let bundle_url = CfUrl::from_copied(url_raw)
        .ok_or_else(|| AuError::InvalidBuffer("CocoaUI info has null bundle URL".into()))?;
    let class_name = CfString::from_copied(class_raw)
        .ok_or_else(|| AuError::InvalidBuffer("CocoaUI info has null class name".into()))?;

    Ok((bundle_url, class_name))
}

unsafe fn load_bundle(url: &CfUrl) -> Result<Retained<NSBundle>> {
    // `CFURLRef` is toll-free bridged with `NSURL`; the pointer is a valid
    // `NSURL*` at the ObjC boundary, so pass it straight to `bundleWithURL:`.
    // Kept as a dynamic `msg_send!` (rather than the typed `NSBundle::
    // bundleWithURL`) so we don't have to round-trip the CFURL through an
    // owned `NSURL` just to borrow it.
    let ns_url = url.as_raw() as *const AnyObject;
    let bundle: Option<Retained<NSBundle>> =
        msg_send![NSBundle::class(), bundleWithURL: ns_url];
    let bundle =
        bundle.ok_or_else(|| AuError::InvalidBuffer("Failed to load AU view bundle".into()))?;
    let _: bool = msg_send![&*bundle, load];
    Ok(bundle)
}

unsafe fn instantiate_factory(
    _bundle: &NSBundle,
    class_name: &CfString,
) -> Result<*mut AnyObject> {
    let factory_name = class_name.to_string();

    let class = AnyClass::get(
        &std::ffi::CString::new(factory_name.clone())
            .map_err(|_| AuError::InvalidBuffer(format!("Invalid class name: {factory_name}")))?,
    )
    .ok_or_else(|| {
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
    // `uiViewForAudioUnit:withSize:` lives on the plugin-provided factory class,
    // so it has no typed binding — dispatch dynamically. objc2's `msg_send!`
    // encodes the `AudioUnit` and `NSSize` (`CGSize`, an `Encode` struct)
    // arguments into the correct ARM64 registers. `AudioUnit` is an opaque
    // `*mut ComponentInstanceRecord`; erase it to `*mut c_void` (the encodable
    // pointer type the AU view protocol actually expects) before sending.
    let unit_ptr = unit as *mut c_void;
    let view: *mut AnyObject =
        msg_send![factory, uiViewForAudioUnit: unit_ptr, withSize: size];

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
