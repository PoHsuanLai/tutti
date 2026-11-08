//! Cocoa view-factory plumbing for AU editors.
//!
//! Reads `kAudioUnitProperty_CocoaUI`, loads the advertised bundle, and
//! instantiates the `NSView` via the factory's `uiViewForAudioUnit:withSize:`
//! method.

use objc2::ClassType;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::{NSBundle, NSSize};
use std::os::raw::c_void;

use tutti_plugin_types::EditorSize;

use crate::cf::{CfString, CfUrl};
use crate::error::{Error, OsError, Result};
use crate::unit::Unit;

const K_AUDIO_UNIT_ERR_INVALID_PROPERTY: i32 = coreaudio_sys::kAudioUnitErr_InvalidProperty;

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
/// `Error::Os` if the AU publishes no `kAudioUnitProperty_CocoaUI`,
/// and `Error::Invalid` at each later step that can come up empty —
/// null bundle URL, null class name, bundle that will not load, ObjC class not
/// found, factory that returns a null view.
pub(super) fn create_view(unit: &Unit, preferred: EditorSize) -> Result<*mut AnyObject> {
    let (bundle_url, class_name) = load_cocoa_view_info(unit)?;
    let bundle = load_bundle(&bundle_url)?;
    let factory = instantiate_factory(&bundle, &class_name)?;
    make_view(factory, unit, preferred)
}

/// Reads `kAudioUnitProperty_CocoaUI` and decode the bundle URL plus the view
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
fn load_cocoa_view_info(unit: &Unit) -> Result<(CfUrl, CfString)> {
    let bytes = unit.get_bytes(
        coreaudio_sys::kAudioUnitProperty_CocoaUI,
        coreaudio_sys::kAudioUnitScope_Global,
        0,
    )?;
    // `AudioUnitCocoaViewInfo` is a URL followed by a variable-length array of
    // class names. All of them are ours to release, so every pointer is adopted
    // (and dropped) and only the first class, the AU's preferred factory, is used.
    let word = size_of::<usize>();
    let pointer_at = |i: usize| {
        bytes
            .get(i * word..(i + 1) * word)
            .map(|w| usize::from_ne_bytes(w.try_into().expect("a word")))
    };
    let Some(url_word) = pointer_at(0) else {
        return Err(Error::Os(OsError {
            function: "GetProperty(CocoaUI)",
            status: K_AUDIO_UNIT_ERR_INVALID_PROPERTY,
        }));
    };
    // SAFETY: the property returns its URL under the Copy rule (the header: "the
    // host owns these"): one retain to adopt.
    let bundle_url = unsafe { CfUrl::from_copied(url_word as coreaudio_sys::CFURLRef) }
        .ok_or_else(|| Error::Invalid("CocoaUI info has null bundle URL".into()))?;
    let classes: Vec<Option<CfString>> = (1..bytes.len() / word)
        .filter_map(pointer_at)
        .map(|w| {
            // SAFETY: each class name is returned under the same Copy rule.
            unsafe { CfString::from_copied(w as coreaudio_sys::CFStringRef) }
        })
        .collect();
    let class_name = classes
        .into_iter()
        .next()
        .flatten()
        .ok_or_else(|| Error::Invalid("CocoaUI info has no class name".into()))?;
    Ok((bundle_url, class_name))
}

/// Loads and links the `NSBundle` at `url`, the AU's advertised view-factory
/// bundle.
///
/// # Safety
/// `url` must wrap a live `CFURLRef`. The pointer is handed to `bundleWithURL:`
/// under the toll-free `CFURL`/`NSURL` bridge, and the message send is
/// unchecked. Must be called on the macOS main thread.
fn load_bundle(url: &CfUrl) -> Result<Retained<NSBundle>> {
    // `CFURLRef` is toll-free bridged with `NSURL`; the pointer is a valid
    // `NSURL*` at the ObjC boundary, so pass it straight to `bundleWithURL:`.
    // Kept as a dynamic `msg_send!` (rather than the typed `NSBundle::
    // bundleWithURL`) to avoid round-tripping the CFURL through an
    // owned `NSURL` just to borrow it.
    let ns_url = url.as_raw() as *const AnyObject;
    // SAFETY: `ns_url` is a live CFURL, toll-free bridged to NSURL, which
    // `bundleWithURL:` accepts; the result is an autoreleased NSBundle or nil,
    // and `Retained` retains it.
    let bundle: Option<Retained<NSBundle>> =
        unsafe { msg_send![NSBundle::class(), bundleWithURL: ns_url] };
    let bundle = bundle.ok_or_else(|| Error::Invalid("Failed to load AU view bundle".into()))?;
    // SAFETY: `bundle` is a live NSBundle; `load` takes no arguments.
    let _: bool = unsafe { msg_send![&*bundle, load] };
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
fn instantiate_factory(_bundle: &NSBundle, class_name: &CfString) -> Result<*mut AnyObject> {
    let factory_name = class_name.to_string();

    let class = AnyClass::get(
        &std::ffi::CString::new(factory_name.clone())
            .map_err(|_| Error::Invalid(format!("Invalid class name: {factory_name}")))?,
    )
    .ok_or_else(|| Error::Invalid(format!("ObjC class '{factory_name}' not found in bundle")))?;

    // SAFETY: `class` is a registered Objective-C class; `alloc` then `init` on
    // the result is the NSObject construction protocol, and a nil result is
    // checked below.
    let factory: *mut AnyObject = unsafe { msg_send![class, alloc] };
    // SAFETY: as above.
    let factory: *mut AnyObject = unsafe { msg_send![factory, init] };
    if factory.is_null() {
        return Err(Error::Invalid(
            "Failed to instantiate AU view factory".into(),
        ));
    }
    Ok(factory)
}

/// Sends `uiViewForAudioUnit:withSize:` to the factory and take ownership of the
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
fn make_view(
    factory: *mut AnyObject,
    unit: &Unit,
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
    let unit_ptr = unit.raw() as *mut c_void;
    // SAFETY: `factory` is a live instance of the AUCocoaUIBase class the
    // bundle registered; `uiViewForAudioUnit:withSize:` takes the unit and a
    // size and returns an autoreleased (+0) view or nil; the `retain` below takes
    // the reference the editor owns.
    let view: *mut AnyObject =
        unsafe { msg_send![factory, uiViewForAudioUnit: unit_ptr, withSize: size] };

    // SAFETY: `factory` is the +1 object `init` returned and is not used after.
    let _: () = unsafe { msg_send![factory, release] };

    if view.is_null() {
        return Err(Error::Invalid("AU view factory returned null view".into()));
    }

    // Retain so ownership is independent of the factory's autorelease pool.
    // SAFETY: `view` is a live, non-nil object; `retain` takes the reference
    // the editor owns until `close`.
    let view: *mut AnyObject = unsafe { msg_send![view, retain] };
    Ok(view)
}
