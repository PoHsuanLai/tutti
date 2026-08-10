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
use tutti_plugin_types::EditorSize;

/// Top-level entry: query the AU's CocoaUI info, load the view factory bundle,
/// and instantiate the editor `NSView`.
///
/// Returns a **retained** `NSView*` — ownership passes to the caller, which is
/// why [`super::AuEditor`]'s `Drop` is the thing that releases it.
///
/// # Safety
/// `unit` must be a live `AudioUnit`. Must be called on the macOS main thread:
/// every step below is AppKit, and the plugin's own factory assumes it.
///
/// # Errors
/// [`AuError::OsStatus`] if the AU publishes no `kAudioUnitProperty_CocoaUI`,
/// and [`AuError::InvalidBuffer`] at each later step that can come up empty —
/// null bundle URL, null class name, bundle that will not load, ObjC class not
/// found, factory that returns a null view.
pub(super) unsafe fn create_view(unit: AudioUnit, preferred: EditorSize) -> Result<*mut AnyObject> {
    let (bundle_url, class_name) = load_cocoa_view_info(unit)?;
    let bundle = load_bundle(&bundle_url)?;
    let factory = instantiate_factory(&bundle, &class_name)?;
    make_view(factory, unit, preferred)
}

/// Read `kAudioUnitProperty_CocoaUI` and decode the bundle URL plus the view
/// factory's ObjC class name.
///
/// Both come back **owned** (`from_copied` takes the +1), so the caller drops
/// them rather than the AU.
///
/// # Safety
/// `unit` must be a live `AudioUnit`. The property's bytes are reinterpreted as
/// an `AudioUnitCocoaViewInfo`, so the AU must have written at least that
/// struct's fixed head — an emptiness check guards the zero-length case, but a
/// unit reporting a short non-zero size is trusted.
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
    // LIMITATION (intentional): `AudioUnitCocoaViewInfo` carries a
    // variable-length `mCocoaAUViewClass` array — an AU may advertise several
    // candidate view-factory classes. Only `class[0]` is read — the AU's
    // preferred/first factory. Multi-class AUs are rare in practice; supporting
    // fallback across the remaining classes is deferred until a real plugin is
    // found that requires it (don't build speculative fan-out).
    let class_raw = (*info_ptr).mCocoaAUViewClass[0];

    let bundle_url = CfUrl::from_copied(url_raw)
        .ok_or_else(|| AuError::InvalidBuffer("CocoaUI info has null bundle URL".into()))?;
    let class_name = CfString::from_copied(class_raw)
        .ok_or_else(|| AuError::InvalidBuffer("CocoaUI info has null class name".into()))?;

    Ok((bundle_url, class_name))
}

/// Load and link the `NSBundle` at `url`, the AU's advertised view-factory
/// bundle.
///
/// # Safety
/// `url` must wrap a live `CFURLRef`. The pointer is handed to `bundleWithURL:`
/// under the toll-free `CFURL`/`NSURL` bridge, and the message send is
/// unchecked. Must be called on the macOS main thread.
unsafe fn load_bundle(url: &CfUrl) -> Result<Retained<NSBundle>> {
    // `CFURLRef` is toll-free bridged with `NSURL`; the pointer is a valid
    // `NSURL*` at the ObjC boundary, so pass it straight to `bundleWithURL:`.
    // Kept as a dynamic `msg_send!` (rather than the typed `NSBundle::
    // bundleWithURL`) to avoid round-tripping the CFURL through an
    // owned `NSURL` just to borrow it.
    let ns_url = url.as_raw() as *const AnyObject;
    let bundle: Option<Retained<NSBundle>> = msg_send![NSBundle::class(), bundleWithURL: ns_url];
    let bundle =
        bundle.ok_or_else(|| AuError::InvalidBuffer("Failed to load AU view bundle".into()))?;
    let _: bool = msg_send![&*bundle, load];
    Ok(bundle)
}

/// `alloc`/`init` the view-factory class the AU named.
///
/// The class is looked up in the **global** ObjC runtime rather than through
/// `_bundle`: loading the bundle is what registers its classes, so by this point
/// the name resolves globally. The bundle is still taken by reference to tie
/// this call to a bundle that has been loaded.
///
/// Returns an object with a +1 retain the caller must balance — [`make_view`]
/// `release`s it once the view is out.
///
/// # Safety
/// `class_name` must name a class that is `alloc`/`init`-constructible with no
/// arguments and conforms to `AUCocoaUIBase`; it comes from the AU's own
/// `CocoaUI` property, which is the only sanctioned source. Must be called on
/// the macOS main thread.
unsafe fn instantiate_factory(_bundle: &NSBundle, class_name: &CfString) -> Result<*mut AnyObject> {
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

/// Send `uiViewForAudioUnit:withSize:` to the factory and take ownership of the
/// resulting `NSView`.
///
/// Consumes `factory`: it is `release`d before this returns, whether or not a
/// view came back. The view is retained on the way out, so it survives the
/// factory's autorelease pool.
///
/// # Safety
/// `factory` must be a live, +1-retained instance of an `AUCocoaUIBase` factory
/// class — what [`instantiate_factory`] returns — and must not be used again by
/// the caller afterwards. `unit` must be a live `AudioUnit`, since the factory
/// stores it and drives it for the view's whole lifetime. The message send is
/// unchecked in release builds; see the encoding note below for what the debug
/// verifier does and does not catch. Must be called on the macOS main thread.
unsafe fn make_view(
    factory: *mut AnyObject,
    unit: AudioUnit,
    preferred: EditorSize,
) -> Result<*mut AnyObject> {
    // `inPreferredSize` is what the *host* would like, and
    // `AUCocoaUIView.h:47-48` calls it exactly that — a preference. A plugin
    // may return a view of any size, which is why the caller reads back the
    // real frame afterwards rather than assuming it got what it asked for.
    //
    // It must be the host's real window size. A fixed figure here tells every AU
    // the host wants that size regardless of the window it is about to live in.
    let size = NSSize {
        width: f64::from(preferred.width),
        height: f64::from(preferred.height),
    };
    // `uiViewForAudioUnit:withSize:` lives on the plugin-provided factory class,
    // so it has no typed binding — dispatch dynamically. objc2's `msg_send!`
    // encodes the `AudioUnit` and `NSSize` (`CGSize`, an `Encode` struct)
    // arguments into the correct ARM64 registers. `AudioUnit` is an opaque
    // `*mut ComponentInstanceRecord`; erase it to `*mut c_void`, the encodable
    // pointer type, before sending.
    //
    // Debug builds additionally verify the argument encodings against the
    // *plugin's* method signature, and that check only passes because this
    // crate enables objc2's `relax-void-encoding`. There is no single encoding
    // that would satisfy it otherwise: measured on this machine, TDR Nova's
    // JUCE factory declares the parameter `^{ComponentInstanceRecord=[1q]}`
    // while Apple's own AUBandpassViewFactory declares
    // `^{OpaqueAudioComponentInstance=}` — the same 8-byte pointer under two
    // incompatible declared types, because each SDK generation binds a
    // different opaque struct name. Without the feature the verifier aborted on
    // whichever of the two is not hardcoded, i.e. `AuEditor::open` could
    // never open a custom Cocoa view in a debug build. See Cargo.toml.
    let unit_ptr = unit as *mut c_void;
    let view: *mut AnyObject = msg_send![factory, uiViewForAudioUnit: unit_ptr, withSize: size];

    let _: () = msg_send![factory, release];

    if view.is_null() {
        return Err(AuError::InvalidBuffer(
            "AU view factory returned null view".into(),
        ));
    }

    // Retain so ownership is independent of the factory's autorelease pool.
    let view: *mut AnyObject = msg_send![view, retain];
    Ok(view)
}
