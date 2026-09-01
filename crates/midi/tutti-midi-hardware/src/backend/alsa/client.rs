//! A UMP-mode ALSA sequencer client, and the RAII wrapper that closes it.

use std::ffi::{CStr, CString};
use std::os::raw::c_int;

use super::sys;
use crate::error::Result;

/// An open sequencer client that speaks UMP.
///
/// Owns the `snd_seq_t` and closes it on drop.
///
/// # One client per connection, not one per backend
///
/// [`snd_seq_ump_event_input`](sys::snd_seq_ump_event_input) **blocks**, so a
/// shared client would mean every input pump contending on one handle — and one
/// device going quiet would stall the others. Enumeration opens a short-lived
/// client of its own for the same reason: it must not block behind a pump.
pub struct SeqClient {
    seq: *mut sys::snd_seq_t,
}

// SAFETY: `snd_seq_t` is an opaque handle whose operations alsa-lib
// synchronises internally. Each `SeqClient` is owned by exactly one thread (a
// pump thread, or the control thread that opened it) and is never aliased —
// `Send` is what lets it move onto its pump thread at spawn.
unsafe impl Send for SeqClient {}

impl SeqClient {
    /// Open a client in UMP MIDI-2.0 mode.
    ///
    /// Three steps, and all three matter:
    /// 1. `snd_seq_open` — an ordinary sequencer client.
    /// 2. `set_client_midi_version(UMP_MIDI_2_0)` — **this is what makes it a
    ///    UMP client.** Without it `snd_seq_ump_event_*` has nothing to carry.
    /// 3. `set_client_ump_conversion(1)` — let the kernel translate at the port
    ///    boundary, so this client can talk to legacy ports and vice versa.
    ///    Kernel ≥ 6.5. Almost no Linux machine has a native-UMP device yet, so
    ///    without this the backend would be correct and useless.
    pub fn open(name: &str, nonblock: bool) -> Result<Self> {
        // `"hw"`, not `"default"`. The sequencer is a kernel device, and `hw`
        // addresses it directly; `default` routes through a config-file plugin
        // that, on at least Ubuntu, **cannot be opened non-blocking** — it
        // returns `-ENOENT` for `SND_SEQ_NONBLOCK` while succeeding blocking.
        //
        // That asymmetry is why this is worth a comment: it made input (which
        // polls, so opens non-blocking) fail while output (blocking) worked,
        // from the same function, and reads as "no MIDI inputs" rather than as
        // an open failure.
        let device = CString::new("hw").expect("literal has no NUL");
        let mut seq: *mut sys::snd_seq_t = std::ptr::null_mut();
        let mode = if nonblock { sys::SND_SEQ_NONBLOCK } else { 0 };

        // SAFETY: `seq` is a valid out-pointer; `device` outlives the call.
        sys::check(
            unsafe { sys::snd_seq_open(&mut seq, device.as_ptr(), sys::SND_SEQ_OPEN_DUPLEX, mode) },
            "snd_seq_open",
        )?;
        let client = Self { seq };

        let cname = CString::new(name).unwrap_or_else(|_| CString::new("tutti").unwrap());
        // SAFETY: `client.seq` is open; `cname` outlives the call.
        unsafe { sys::snd_seq_set_client_name(client.seq, cname.as_ptr()) };

        // SAFETY: same.
        sys::check(
            unsafe {
                sys::snd_seq_set_client_midi_version(client.seq, sys::SND_SEQ_CLIENT_UMP_MIDI_2_0)
            },
            "snd_seq_set_client_midi_version",
        )?;
        // SAFETY: same.
        sys::check(
            unsafe { sys::snd_seq_set_client_ump_conversion(client.seq, 1) },
            "snd_seq_set_client_ump_conversion",
        )?;

        Ok(client)
    }

    /// The raw `snd_seq_t` for an FFI call.
    ///
    /// Valid for as long as `&self` is; this hands out a pointer, not ownership
    /// — the handle is closed by `Drop` and must not be closed through this.
    pub fn raw(&self) -> *mut sys::snd_seq_t {
        self.seq
    }

    /// This client's sequencer id, for addressing events back to it.
    pub fn id(&self) -> Result<c_int> {
        // SAFETY: `self.seq` is open for the life of `self`.
        sys::check(
            unsafe { sys::snd_seq_client_id(self.seq) },
            "snd_seq_client_id",
        )
    }

    /// Create a port on this client.
    pub fn create_port(&self, name: &str, caps: u32, kind: u32) -> Result<c_int> {
        let cname = CString::new(name).unwrap_or_else(|_| CString::new("tutti").unwrap());
        // SAFETY: `self.seq` is open; `cname` outlives the call.
        sys::check(
            unsafe { sys::snd_seq_create_simple_port(self.seq, cname.as_ptr(), caps, kind) },
            "snd_seq_create_simple_port",
        )
    }
}

impl Drop for SeqClient {
    fn drop(&mut self) {
        if !self.seq.is_null() {
            // SAFETY: opened by `open`, closed exactly once — `seq` is private
            // and never reassigned.
            unsafe { sys::snd_seq_close(self.seq) };
        }
    }
}

/// Read a C string that alsa-lib owns, without taking ownership of it.
///
/// Returns `None` for NULL rather than an empty string: "the library reported
/// no name" and "the name is empty" are different, and the caller substitutes
/// its own label for the former.
pub(super) fn cstr(ptr: *const std::os::raw::c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: non-NULL, and alsa-lib's info getters return NUL-terminated
    // strings valid until the owning info struct is freed — copied immediately,
    // so nothing borrows past that point.
    Some(
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// Turn an ALSA error code into a readable message.
///
/// `Error::Alsa` carries the raw code, which is what a caller matches on; this
/// is for the `debug!` beside it, where "No such file or directory" beats `-2`.
pub(super) fn strerror(code: c_int) -> String {
    // SAFETY: `snd_strerror` returns a static NUL-terminated string for any int.
    cstr(unsafe { sys::snd_strerror(code) }).unwrap_or_else(|| format!("error {code}"))
}
