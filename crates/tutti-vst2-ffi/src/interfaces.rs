//! The host's `audioMaster` dispatch and the two string helpers the host and the plugin side share.
//!
//! The plugin-side entry points (`dispatch`, `process_replacing`, ...) are
//! `tutti-vst2-author`'s: this crate is the host half.

#![doc(hidden)]

use std::cell::Cell;
use std::os::raw::{c_char, c_void};

use crate::{
    api::{self, AEffect, Supported, TimeInfo, consts::*},
    host::Host,
};

/// Copy a string into a destination buffer.
///
/// String will be cut at `max` characters.
/// Returns `0` (not handled) for a null `dst`.
///
/// `max` counts the NUL, as the SDK's `kVstMax*Len` constants do. What is written is the
/// string up to its first NUL, cut to at most `max - 1` bytes at a char boundary, then a
/// NUL: always terminated, and nothing is written past that NUL, so a destination that
/// is shorter than `max` but holds the string is not overrun by a clearing pass.
pub fn copy_string(dst: *mut c_void, src: &str, max: usize) -> isize {
    if dst.is_null() || max == 0 {
        return 0;
    }
    let src = src.split('\0').next().unwrap_or("");
    let mut len = src.len().min(max - 1);
    while !src.is_char_boundary(len) {
        len -= 1;
    }
    // SAFETY: `dst` is non-null and the opcode's buffer, which the VST2 ABI has hold at
    // least `max` writable bytes (the callers pass the SDK's fixed size for the opcode;
    // a host that offers less is outside the ABI, and this fn does not and cannot check
    // it). Exactly `len + 1 <= max` bytes are written. `src` is a live `&str`, read for
    // `len`, and a plugin's buffer does not overlap it.
    unsafe {
        let dst = dst as *mut u8;
        std::ptr::copy_nonoverlapping(src.as_ptr(), dst, len);
        dst.add(len).write(0);
    }

    1 // Success
}

/// Dispatch one `audioMaster` opcode to the host.
///
/// Takes `&dyn Host`, not `&mut`: every `Host` method takes `&self`, and this
/// runs on the plugin's audio thread as often as the UI thread — see
/// [`crate::host::PluginLoader::load`] for why no lock guards it.
pub fn host_dispatch(
    host: &dyn Host,
    effect: *mut AEffect,
    opcode: i32,
    index: i32,
    value: isize,
    ptr: *mut c_void,
    opt: f32,
) -> isize {
    use crate::host::OpCode;

    let opcode = OpCode::try_from(opcode);
    match opcode {
        Ok(OpCode::Version) => return 2400,
        Ok(OpCode::Automate) => host.automate(index, opt),
        Ok(OpCode::BeginEdit) => host.begin_edit(index),
        Ok(OpCode::EndEdit) => host.end_edit(index),

        Ok(OpCode::Idle) => host.idle(),

        // ...
        // `audioMasterCanDo`: 1 = yes, -1 = no, 0 = don't know. A null string
        // is a malformed query and answers "don't know".
        Ok(OpCode::CanDo) => {
            if ptr.is_null() {
                return 0;
            }
            let query = read_string(ptr);
            info!("Plugin is asking if host can: {}.", query);
            return match host.can_do(&query) {
                Supported::Yes => 1,
                Supported::Maybe => 0,
                Supported::No => -1,
                Supported::Custom(n) => n,
            };
        }

        Ok(OpCode::GetVendorVersion) => return host.get_info().0,
        Ok(OpCode::GetVendorString) => {
            return copy_string(ptr, &host.get_info().1, MAX_VENDOR_STR_LEN);
        }
        Ok(OpCode::GetProductString) => {
            return copy_string(ptr, &host.get_info().2, MAX_PRODUCT_STR_LEN);
        }
        Ok(OpCode::ProcessEvents) => {
            // A null or misaligned `VstEvents` is a malformed call: not handled.
            if ptr.is_null() || !(ptr as *const api::Events).is_aligned() {
                return 0;
            }
            // SAFETY: VST2 ABI: for `audioMasterProcessEvents`, `ptr` is the plugin's
            // `VstEvents`, valid for the call; non-null and aligned (checked above). What
            // it holds beyond the header (the count's worth of event pointers) is the
            // plugin's word, which the view reads and `PluginEvents::iter` bounds as it can.
            let events = unsafe { &*(ptr as *const api::Events) };
            host.process_plugin_events(api::PluginEvents::new(events));
        }

        Ok(OpCode::GetTime) => {
            return match host.get_time_info(value as i32) {
                None => 0,
                Some(result) => {
                    thread_local! {
                        static TIME_INFO: Cell<TimeInfo> =
                            Cell::new(TimeInfo::default());
                    }
                    TIME_INFO.with(|time_info| {
                        (*time_info).set(result);
                        time_info.as_ptr() as isize
                    })
                }
            };
        }
        Ok(OpCode::GetBlockSize) => return host.get_block_size(),

        // `audioMasterGetSampleRate` returns the rate via the intptr result
        // (same channel as `GetBlockSize`), truncated from f32 to isize.
        Ok(OpCode::GetSampleRate) => return host.get_sample_rate() as isize,
        Ok(OpCode::SizeWindow) => return host.size_window(index, value) as isize,
        Ok(OpCode::IOChanged) => return host.io_changed() as isize,
        Ok(OpCode::GetInputLatency) => return host.get_input_latency(),
        Ok(OpCode::GetOutputLatency) => return host.get_output_latency(),
        Ok(OpCode::GetCurrentProcessLevel) => return host.get_process_level(),
        Ok(OpCode::GetAutomationState) => return host.get_automation_state(),

        // `audioMasterUpdateDisplay`. A plugin fires this after changing preset
        // or program from its own editor: the parameter set the host is showing
        // is now stale and must be re-read. `Host::update_display` was declared
        // with nothing routing to it, so the plugin got the fall-through's `0`
        // and the host's parameter list silently drifted from the plugin's.
        Ok(OpCode::UpdateDisplay) => {
            host.update_display();
            // 1 = handled. A plugin that reads this as "host will not refresh"
            // may fall back to forcing its own repaint.
            return 1;
        }

        // `audioMasterCurrentId`. A shell plugin (a bundle exposing several
        // effects behind one binary) calls this during `VSTPluginMain` to learn
        // which sub-plugin the host wants. The fall-through's `0` means "no
        // particular id", so every shell loaded its default effect — and
        // `Host::get_plugin_id` was overridden to a real value that nothing
        // ever asked for.
        Ok(OpCode::CurrentId) => return host.get_plugin_id() as isize,

        // `audioMasterGetLanguage`. `HostLanguage` is 1-based (`English = 1`),
        // so the fall-through's `0` is not a language — a plugin indexing a
        // string table by it reads slot 0. English is the honest answer for a
        // host with no localization rather than a placeholder.
        Ok(OpCode::GetLanguage) => return api::HostLanguage::English as isize,

        _ => {
            trace!("VST: Got unimplemented host opcode ({:?})", opcode);
            trace!(
                "Arguments; effect: {:?}, index: {}, value: {}, ptr: {:?}, opt: {}",
                effect, index, value, ptr, opt
            );
        }
    }
    0
}

/// Read a string from the `ptr` buffer. A null `ptr` reads as the empty string.
pub fn read_string(ptr: *mut c_void) -> String {
    use std::ffi::CStr;

    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: `ptr` is non-null (checked above), and the VST2 ABI has every opcode that
    // passes a string send a NUL-terminated C string that the sender keeps valid for the
    // call. The length is found by scanning for the NUL, so a sender that breaks that
    // is the only way to read past the buffer; it cannot be checked from here.
    String::from_utf8_lossy(unsafe { CStr::from_ptr(ptr as *mut c_char).to_bytes() }).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// **`copy_string` writes the string, a NUL, and nothing past it.** It used to
    /// clear `max` bytes first (a destination that holds the string but not `max`
    /// bytes was overrun) and to leave a string of `max` bytes or more with no NUL.
    ///
    /// Mutation (run): clear `max` bytes before the copy again (`memset`): the
    /// sentinel past the NUL is cleared and the first assertion fails. Mutation
    /// (run): copy `min(max, len)` bytes instead of `max - 1`: the long string has
    /// no NUL and the third assertion fails.
    #[test]
    fn copy_string_stops_at_its_nul_and_always_has_one() {
        let mut dst = [0xAAu8; 16];
        assert_eq!(copy_string(dst.as_mut_ptr().cast(), "hi", 16), 1);
        assert_eq!(&dst[..3], b"hi\0");
        assert!(dst[3..].iter().all(|b| *b == 0xAA), "wrote past the NUL");

        // A string at least `max` long is cut to `max - 1` and terminated.
        let mut dst = [0xAAu8; 16];
        assert_eq!(copy_string(dst.as_mut_ptr().cast(), "abcdefghij", 8), 1);
        assert_eq!(&dst[..8], b"abcdefg\0");
        assert_eq!(dst[8], 0xAA, "wrote past max");

        // Not cut inside a char.
        let mut dst = [0xAAu8; 8];
        copy_string(dst.as_mut_ptr().cast(), "aé", 3);
        assert_eq!(&dst[..2], b"a\0");

        // An interior NUL ends it.
        let mut dst = [0xAAu8; 8];
        copy_string(dst.as_mut_ptr().cast(), "a\0b", 8);
        assert_eq!(&dst[..2], b"a\0");
        assert_eq!(dst[2], 0xAA);
    }

    /// A null destination is not handled (`0`), not written to.
    ///
    /// Mutation (run): remove the null check: `copy_string` writes to address 0
    /// and the test aborts on a signal.
    #[test]
    fn copy_string_to_null_is_not_handled() {
        assert_eq!(copy_string(std::ptr::null_mut(), "x", 8), 0);
        assert_eq!(
            copy_string(std::ptr::NonNull::<u8>::dangling().as_ptr().cast(), "x", 0),
            0
        );
    }

    struct Counting(AtomicUsize);

    impl Host for Counting {
        fn process_plugin_events(&self, _events: api::PluginEvents<'_>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// **A plugin's malformed host calls are answered `0`, not dereferenced.** A
    /// null `VstEvents`, a misaligned one, and a null string buffer.
    ///
    /// Mutation (run): drop the null check on `audioMasterProcessEvents`: the null
    /// call dereferences address 0 and the test aborts on a signal. Mutation (run):
    /// drop the alignment check: the misaligned call reaches the host and the
    /// count assertion fails.
    #[test]
    fn malformed_host_calls_are_not_followed() {
        let host = Counting(AtomicUsize::new(0));
        let process_events = crate::host::OpCode::ProcessEvents as i32;
        let null: *mut c_void = std::ptr::null_mut();
        let no_effect: *mut AEffect = std::ptr::null_mut();

        assert_eq!(
            host_dispatch(&host, no_effect, process_events, 0, 0, null, 0.0),
            0
        );
        let misaligned = (std::mem::align_of::<api::Events>() as isize + 1) as *mut c_void;
        assert_eq!(
            host_dispatch(&host, no_effect, process_events, 0, 0, misaligned, 0.0),
            0
        );
        assert_eq!(
            host.0.load(Ordering::SeqCst),
            0,
            "a bad pointer was followed"
        );

        for opcode in [
            crate::host::OpCode::GetVendorString as i32,
            crate::host::OpCode::GetProductString as i32,
        ] {
            assert_eq!(host_dispatch(&host, no_effect, opcode, 0, 0, null, 0.0), 0);
        }
    }
}
