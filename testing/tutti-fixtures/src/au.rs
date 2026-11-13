//! The shared plugin as an Audio Unit (feature `au`, macOS).

use std::path::PathBuf;

fn shared_library() -> PathBuf {
    crate::cdylib("tutti_shared_plugin", "tutti-shared-plugin")
}

/// The shared plugin as an Audio Unit: registers it in this process (once) and
/// returns the `.component` directory a host is given to find it by.
///
/// An AU is found in the system's component registry, and the bundle only says
/// which units it installed (`AudioComponents` in its `Info.plist`). The shared
/// plugin's AU is registered in-process (`tutti_shared_plugin::register_au`), so
/// the bundle is just that declaration: no executable, nothing installed. The
/// host's own lookup, bundle then exact description in the registry, is the one
/// an installed unit takes.
///
/// # Panics
///
/// When the bundle cannot be written.
#[must_use]
pub fn au_bundle() -> PathBuf {
    use tutti_shared_plugin::{AU_MANUFACTURER, AU_NAME, AU_SUBTYPE, AU_TYPE, AU_VERSION};
    tutti_shared_plugin::register_au();
    let code = |c: [u8; 4]| String::from_utf8(c.to_vec()).expect("an ASCII code");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>dev.tutti.shared-plugin-au</string>
  <key>CFBundlePackageType</key><string>BNDL</string>
  <key>CFBundleName</key><string>tutti-shared-plugin</string>
  <key>AudioComponents</key>
  <array>
    <dict>
      <key>type</key><string>{}</string>
      <key>subtype</key><string>{}</string>
      <key>manufacturer</key><string>{}</string>
      <key>name</key><string>{AU_NAME}</string>
      <key>version</key><integer>{AU_VERSION}</integer>
    </dict>
  </array>
</dict>
</plist>
"#,
        code(AU_TYPE),
        code(AU_SUBTYPE),
        code(AU_MANUFACTURER),
    );
    let bundle = shared_library().with_extension("component");
    let contents = bundle.join("Contents");
    std::fs::create_dir_all(&contents).expect("bundle directory");
    // Staged under this process's own name and renamed in, so a process that
    // reads it never sees it half written.
    let staging = contents.join(format!(".stage{}", std::process::id()));
    std::fs::write(&staging, plist).expect("stage the Info.plist");
    std::fs::rename(&staging, contents.join("Info.plist")).expect("place the Info.plist");
    bundle
}

#[allow(
    unsafe_code,
    reason = "the Objective-C runtime and AudioToolbox are reached only through these C symbols"
)]
unsafe extern "C" {
    fn objc_msgSend(receiver: *mut std::ffi::c_void, sel: *const std::ffi::c_void) -> usize;
    fn sel_registerName(name: *const std::ffi::c_char) -> *const std::ffi::c_void;
}

/// Sends a nullary Objective-C selector and returns the word it produced: a bare
/// send, without the argument marshalling `objc2`'s `msg_send!` adds.
#[allow(unsafe_code, reason = "calls into the extern block above")]
fn objc_nullary(obj: *mut std::ffi::c_void, sel: &std::ffi::CStr) -> usize {
    if obj.is_null() {
        return 0;
    }
    // SAFETY: the callers pass a live object that responds to `retainCount` and
    // `release`; `sel_registerName` returns the registered selector for a
    // NUL-terminated name.
    unsafe { objc_msgSend(obj, sel_registerName(sel.as_ptr())) }
}

/// The retain count of the `CFString` the unit publishes as
/// `kAudioUnitProperty_ElementName` for `element` of its input or output scope,
/// read without disturbing it, or `None` when the unit gives no name.
///
/// The read takes one reference (Copy rule) and gives it back before returning,
/// so sampling does not itself leak. `unit` must be a live `AudioUnit`: the
/// raw unit behind the host's `Unit`, valid while its owner is.
#[allow(
    unsafe_code,
    clippy::not_unsafe_ptr_arg_deref,
    reason = "a test-only probe of a live unit; the caller holds the unit"
)]
#[must_use]
pub fn element_name_retain_count(
    unit: *mut std::ffi::c_void,
    is_output: bool,
    element: u32,
) -> Option<usize> {
    let mut raw: *const std::ffi::c_void = std::ptr::null();
    let mut size = std::mem::size_of::<*const std::ffi::c_void>() as u32;
    // SAFETY: `unit` is live (see above) and `raw` is a pointer-sized
    // out-parameter whose size is passed in `size`.
    let status = unsafe {
        coreaudio_sys::AudioUnitGetProperty(
            unit.cast(),
            coreaudio_sys::kAudioUnitProperty_ElementName,
            if is_output {
                coreaudio_sys::kAudioUnitScope_Output
            } else {
                coreaudio_sys::kAudioUnitScope_Input
            },
            element,
            (&mut raw as *mut *const std::ffi::c_void).cast(),
            &mut size,
        )
    };
    if status != 0 || raw.is_null() {
        return None;
    }
    let object = raw.cast_mut();
    let count = objc_nullary(object, c"retainCount");
    // Gives back the +1 this read took.
    objc_nullary(object, c"release");
    Some(count)
}
