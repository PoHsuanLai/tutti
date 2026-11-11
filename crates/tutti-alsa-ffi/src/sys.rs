//! Raw declarations for the part of ALSA's sequencer and **UMP** API
//! (alsa-lib >= 1.2.10) that `alsa-sys` does not bind.
//!
//! Not a general ALSA binding: it declares the functions, types and constants
//! the UMP path calls, with alsa-lib's C names so they can be looked up in its
//! headers. Private to the crate: the safe handles above it are the only
//! callers, and each `unsafe` call is argued where it is made.
//!
//! # The struct is measured, not guessed
//!
//! [`snd_seq_ump_event_t`] mirrors a C struct whose last member is a union. A
//! wrong layout is not a compile error, it is silent memory corruption. Every
//! offset was read off the real header with `offsetof` on the target
//! (alsa-lib 1.2.14, x86_64), and compile-time asserts below re-check them.
//!
//! # Version floors
//!
//! Everything declared here needs alsa-lib 1.2.10, reported by `build.rs` as
//! `cfg(alsa_ump)` (and `DEP_ASOUND_ALSA_UMP` to dependents). It also detects
//! 1.2.13 (`DEP_ASOUND_ALSA_UMP_CREATE`), which adds
//! `snd_seq_create_ump_endpoint`; that is not declared here.

#![allow(
    non_camel_case_types,
    reason = "FFI type names mirror alsa-lib's C spelling, so a reader can grep the header"
)]

use std::os::raw::{c_char, c_int, c_short, c_uint, c_void};

/// Opaque sequencer handle.
#[derive(Debug)]
pub enum snd_seq_t {}

/// Opaque `snd_ump_block_info_t`. Accessed only through its getters, so the
/// layout never needs to be declared — the reason those are preferred over
/// declaring the struct.
#[derive(Debug)]
pub enum snd_ump_block_info_t {}

/// Opaque `snd_seq_client_info_t`.
#[derive(Debug)]
pub enum snd_seq_client_info_t {}

/// Opaque `snd_seq_port_info_t`.
#[derive(Debug)]
pub enum snd_seq_port_info_t {}

/// Opaque `snd_seq_queue_status_t`.
#[derive(Debug)]
pub enum snd_seq_queue_status_t {}

/// `snd_seq_real_time_t`: a time on a queue's real-time clock.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct snd_seq_real_time_t {
    /// Whole seconds.
    pub tv_sec: c_uint,
    /// Nanoseconds past the second.
    pub tv_nsec: c_uint,
}

/// A sequencer address: `(client, port)`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct snd_seq_addr_t {
    /// Sequencer client id.
    pub client: u8,
    /// Port number within the client.
    pub port: u8,
}

/// A UMP sequencer event.
///
/// Layout mirrors alsa-lib's `snd_seq_ump_event_t` exactly — same prefix as the
/// legacy `snd_seq_event_t`, with the data union widened to carry four UMP
/// words. Measured offsets (x86_64, alsa-lib 1.2.14):
///
/// ```text
/// type 0  flags 1  tag 2  queue 3  time 4..12  source 12  dest 14  ump 16..32
/// size 32, align 4
/// ```
///
/// `time` is declared as an opaque 8-byte blob rather than the real
/// `snd_seq_timestamp_t` union. A sent event is "direct" (no queue), so the
/// field is zero; a received one carries the port's timestamp, read through
/// [`real_time`](snd_seq_ump_event_t::real_time) when `flags` say it is real
/// time. Declaring the union would be layout risk for nothing.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct snd_seq_ump_event_t {
    /// Event type (`SND_SEQ_EVENT_*`).
    pub type_: u8,
    /// Event flags; timestamp and queue modes.
    pub flags: u8,
    /// Application-defined tag.
    pub tag: u8,
    /// Queue id; `SND_SEQ_QUEUE_DIRECT` for immediate dispatch.
    pub queue: u8,
    /// `snd_seq_timestamp_t`: a union of a tick and a [`snd_seq_real_time_t`].
    /// Read through [`snd_seq_ump_event_t::real_time`].
    pub time: [u8; 8],
    /// Sending address.
    pub source: snd_seq_addr_t,
    /// Destination address.
    pub dest: snd_seq_addr_t,
    /// The UMP words. A message is 1–4 of them; the rest are zero.
    pub ump: [u32; 4],
}

impl snd_seq_ump_event_t {
    /// The event's timestamp on its queue's real-time clock, when `flags` mark
    /// it as one (`SND_SEQ_TIME_STAMP_REAL`; a tick stamp, or none, is `None`).
    #[must_use]
    pub fn real_time(&self) -> Option<snd_seq_real_time_t> {
        if self.flags & SND_SEQ_TIME_STAMP_MASK != SND_SEQ_TIME_STAMP_REAL {
            return None;
        }
        let word = |at: usize| {
            u32::from_ne_bytes([
                self.time[at],
                self.time[at + 1],
                self.time[at + 2],
                self.time[at + 3],
            ])
        };
        Some(snd_seq_real_time_t {
            tv_sec: word(0),
            tv_nsec: word(4),
        })
    }
}

impl Default for snd_seq_ump_event_t {
    /// The "unset" event the send path fills in: every field zero.
    fn default() -> Self {
        Self {
            type_: 0,
            flags: 0,
            tag: 0,
            queue: 0,
            time: [0; 8],
            source: snd_seq_addr_t::default(),
            dest: snd_seq_addr_t::default(),
            ump: [0; 4],
        }
    }
}

// --- Constants -------------------------------------------------------------

/// `SND_SEQ_OPEN_DUPLEX` — read and write.
pub const SND_SEQ_OPEN_DUPLEX: c_int = 3;
/// Non-blocking open.
pub const SND_SEQ_NONBLOCK: c_int = 1;

/// `SND_SEQ_CLIENT_UMP_MIDI_2_0` — the client speaks UMP with MIDI 2.0 protocol.
pub const SND_SEQ_CLIENT_UMP_MIDI_2_0: c_int = 2;

/// `SND_SEQ_CLIENT_SYSTEM` — the kernel's own client, which owns the announce
/// port.
pub const SND_SEQ_CLIENT_SYSTEM: c_int = 0;
/// `SND_SEQ_PORT_SYSTEM_ANNOUNCE` — the system client's port that sends an event
/// whenever a client or port starts, exits or changes.
pub const SND_SEQ_PORT_SYSTEM_ANNOUNCE: c_int = 1;

/// `SND_SEQ_EVENT_START` — start a queue.
pub const SND_SEQ_EVENT_START: c_int = 30;

/// `SND_SEQ_EVENT_CLIENT_START` — announced when a client opens; the event's data
/// begins with its `snd_seq_addr_t`.
pub const SND_SEQ_EVENT_CLIENT_START: u8 = 60;
/// `SND_SEQ_EVENT_CLIENT_EXIT` — announced when a client closes; the event's data
/// begins with its `snd_seq_addr_t`.
pub const SND_SEQ_EVENT_CLIENT_EXIT: u8 = 61;
/// `SND_SEQ_EVENT_CLIENT_CHANGE` — announced when a client sets its info (its
/// name among it); the data begins with its address.
pub const SND_SEQ_EVENT_CLIENT_CHANGE: u8 = 62;
/// `SND_SEQ_EVENT_PORT_START` — announced when a port is created; the data begins
/// with its address.
pub const SND_SEQ_EVENT_PORT_START: u8 = 63;
/// `SND_SEQ_EVENT_PORT_EXIT` — announced when a port is deleted; the data begins
/// with its address.
pub const SND_SEQ_EVENT_PORT_EXIT: u8 = 64;
/// `SND_SEQ_EVENT_PORT_CHANGE` — announced when a port's info is set (its name or
/// capabilities); the data begins with its address.
pub const SND_SEQ_EVENT_PORT_CHANGE: u8 = 65;

/// `SND_SEQ_ADDRESS_SUBSCRIBERS` — deliver to everything subscribed to the
/// sending port, rather than to one named address.
///
/// This is the correct `dest` for a port that used `snd_seq_connect_to`: naming
/// the destination explicitly *on top of* a subscription is rejected with
/// `-EINVAL`.
pub const SND_SEQ_ADDRESS_SUBSCRIBERS: u8 = 254;

/// `SND_SEQ_TIME_STAMP_MASK`: the bit of an event's `flags` that says whether
/// `time` is a tick or a real time.
pub const SND_SEQ_TIME_STAMP_MASK: u8 = 1 << 0;

/// `SND_SEQ_TIME_STAMP_REAL`: `time` is a [`snd_seq_real_time_t`].
pub const SND_SEQ_TIME_STAMP_REAL: u8 = 1 << 0;

/// `SND_SEQ_QUEUE_DIRECT` — dispatch immediately instead of scheduling.
///
/// Must be set explicitly: a zeroed `queue` field means queue 0, a real queue
/// this client never created, and the send fails.
pub const SND_SEQ_QUEUE_DIRECT: u8 = 253;

/// Port capabilities.
pub const SND_SEQ_PORT_CAP_READ: c_uint = 1 << 0;
/// Other clients may write events to the port.
pub const SND_SEQ_PORT_CAP_WRITE: c_uint = 1 << 1;
/// Other clients may subscribe to read from the port.
pub const SND_SEQ_PORT_CAP_SUBS_READ: c_uint = 1 << 5;
/// Other clients may subscribe to write to the port.
pub const SND_SEQ_PORT_CAP_SUBS_WRITE: c_uint = 1 << 6;

/// Port types.
pub const SND_SEQ_PORT_TYPE_MIDI_GENERIC: c_uint = 1 << 1;
/// A port owned by an application.
pub const SND_SEQ_PORT_TYPE_APPLICATION: c_uint = 1 << 20;

// --- Functions -------------------------------------------------------------

// Only declared when `build.rs` found alsa-lib >= 1.2.10 (`cfg(alsa_ump)`); the
// types and constants above stay unconditional.
#[cfg(alsa_ump)]
unsafe extern "C" {
    // Client lifecycle.
    /// Open a sequencer client handle; `streams` and `mode` are alsa-lib's `SND_SEQ_OPEN_*` / `SND_SEQ_NONBLOCK` flags.
    pub fn snd_seq_open(
        seq: *mut *mut snd_seq_t,
        name: *const c_char,
        streams: c_int,
        mode: c_int,
    ) -> c_int;
    /// Close a client opened by `snd_seq_open`.
    pub fn snd_seq_close(seq: *mut snd_seq_t) -> c_int;
    /// This client's id on the sequencer.
    pub fn snd_seq_client_id(seq: *mut snd_seq_t) -> c_int;
    /// Set the name other clients see for this client.
    pub fn snd_seq_set_client_name(seq: *mut snd_seq_t, name: *const c_char) -> c_int;
    /// Static message for a negative alsa-lib error code.
    pub fn snd_strerror(errnum: c_int) -> *const c_char;

    /// Declare this client's MIDI version — `SND_SEQ_CLIENT_UMP_MIDI_2_0` here.
    /// **This is what makes the client a UMP client** rather than a legacy one.
    /// @@ALSA_1.2.10
    pub fn snd_seq_set_client_midi_version(seq: *mut snd_seq_t, midi_version: c_int) -> c_int;

    /// Enable kernel UMP↔legacy conversion, so a UMP client can talk to legacy
    /// ports (and vice versa) without either side knowing. Kernel ≥ 6.5.
    /// @@ALSA_1.2.10
    pub fn snd_seq_set_client_ump_conversion(seq: *mut snd_seq_t, enable: c_int) -> c_int;

    // Ports.
    /// Create a port with `caps` (`SND_SEQ_PORT_CAP_*`) and `type_` (`SND_SEQ_PORT_TYPE_*`); returns the port number or a negative error.
    pub fn snd_seq_create_simple_port(
        seq: *mut snd_seq_t,
        name: *const c_char,
        caps: c_uint,
        type_: c_uint,
    ) -> c_int;
    /// Subscribe `my_port` to events sent by `src_client:src_port`.
    pub fn snd_seq_connect_from(
        seq: *mut snd_seq_t,
        my_port: c_int,
        src_client: c_int,
        src_port: c_int,
    ) -> c_int;
    /// Subscribe `dest_client:dest_port` to events sent by `my_port`.
    pub fn snd_seq_connect_to(
        seq: *mut snd_seq_t,
        my_port: c_int,
        dest_client: c_int,
        dest_port: c_int,
    ) -> c_int;

    // UMP event I/O. @@ALSA_1.2.10
    /// Read one UMP event. Blocks unless the client was opened non-blocking, in
    /// which case it returns `-EAGAIN` when the queue is empty.
    ///
    /// The event is owned by alsa-lib and valid only until the next call — it
    /// must be copied out, never retained.
    pub fn snd_seq_ump_event_input(seq: *mut snd_seq_t, ev: *mut *mut snd_seq_ump_event_t)
    -> c_int;
    /// How many poll descriptors the client needs for `events` (`POLLIN`).
    pub fn snd_seq_poll_descriptors_count(seq: *mut snd_seq_t, events: c_short) -> c_int;
    /// Fill `pfds` (an array of `struct pollfd`, `space` long) for `events`;
    /// returns how many it filled.
    pub fn snd_seq_poll_descriptors(
        seq: *mut snd_seq_t,
        pfds: *mut c_void,
        space: c_uint,
        events: c_short,
    ) -> c_int;
    /// Send one UMP event immediately, bypassing the queue.
    pub fn snd_seq_ump_event_output_direct(
        seq: *mut snd_seq_t,
        ev: *mut snd_seq_ump_event_t,
    ) -> c_int;

    // Client enumeration.
    /// Allocate a client-info record; free it with `snd_seq_client_info_free`.
    pub fn snd_seq_client_info_malloc(ptr: *mut *mut snd_seq_client_info_t) -> c_int;
    /// Free a record from `snd_seq_client_info_malloc`.
    pub fn snd_seq_client_info_free(ptr: *mut snd_seq_client_info_t);
    /// Set the client id a query starts from (`-1` for the first).
    pub fn snd_seq_client_info_set_client(info: *mut snd_seq_client_info_t, client: c_int);
    /// The client id in the record.
    pub fn snd_seq_client_info_get_client(info: *const snd_seq_client_info_t) -> c_int;
    /// The client name in the record; valid while the record lives.
    pub fn snd_seq_client_info_get_name(info: *mut snd_seq_client_info_t) -> *const c_char;
    /// Advance `info` to the next client; negative when there is none.
    pub fn snd_seq_query_next_client(
        seq: *mut snd_seq_t,
        info: *mut snd_seq_client_info_t,
    ) -> c_int;

    // Port enumeration.
    /// Allocate a port-info record; free it with `snd_seq_port_info_free`.
    pub fn snd_seq_port_info_malloc(ptr: *mut *mut snd_seq_port_info_t) -> c_int;
    /// Free a record from `snd_seq_port_info_malloc`.
    pub fn snd_seq_port_info_free(ptr: *mut snd_seq_port_info_t);
    /// Set the client whose ports a query walks.
    pub fn snd_seq_port_info_set_client(info: *mut snd_seq_port_info_t, client: c_int);
    /// Set the port a query starts from (`-1` for the first).
    pub fn snd_seq_port_info_set_port(info: *mut snd_seq_port_info_t, port: c_int);
    /// The port number in the record.
    pub fn snd_seq_port_info_get_port(info: *const snd_seq_port_info_t) -> c_int;
    /// The port name in the record; valid while the record lives.
    pub fn snd_seq_port_info_get_name(info: *const snd_seq_port_info_t) -> *const c_char;
    /// The port's `SND_SEQ_PORT_CAP_*` bits.
    pub fn snd_seq_port_info_get_capability(info: *const snd_seq_port_info_t) -> c_uint;
    /// Fill `info` from one of `seq`'s own ports.
    pub fn snd_seq_get_port_info(
        seq: *mut snd_seq_t,
        port: c_int,
        info: *mut snd_seq_port_info_t,
    ) -> c_int;
    /// Change one of `seq`'s own ports to `info`; the sequencer announces it.
    pub fn snd_seq_set_port_info(
        seq: *mut snd_seq_t,
        port: c_int,
        info: *mut snd_seq_port_info_t,
    ) -> c_int;
    /// Whether events delivered to the port carry a timestamp.
    pub fn snd_seq_port_info_set_timestamping(info: *mut snd_seq_port_info_t, enable: c_int);
    /// Whether that timestamp is real time (not ticks).
    pub fn snd_seq_port_info_set_timestamp_real(info: *mut snd_seq_port_info_t, real: c_int);
    /// The queue the timestamp is read from.
    pub fn snd_seq_port_info_set_timestamp_queue(info: *mut snd_seq_port_info_t, queue: c_int);

    // Queues.
    /// Create a queue owned by `seq`; its id, or negative. The sequencer frees
    /// it when the client closes.
    pub fn snd_seq_alloc_queue(seq: *mut snd_seq_t) -> c_int;
    /// Send queue `q` the control event `type_` (`SND_SEQ_EVENT_START`...) with
    /// `value`. `ev` may be NULL, which sends it directly. (`snd_seq_start_queue`
    /// is a header macro over this, so it is not a symbol.)
    pub fn snd_seq_control_queue(
        seq: *mut snd_seq_t,
        q: c_int,
        type_: c_int,
        value: c_int,
        ev: *mut c_void,
    ) -> c_int;
    /// Send what `snd_seq_control_queue` and other buffered output left in the
    /// client's output buffer; the count still buffered, or negative.
    pub fn snd_seq_drain_output(seq: *mut snd_seq_t) -> c_int;
    /// Allocate a queue-status record; free it with `snd_seq_queue_status_free`.
    pub fn snd_seq_queue_status_malloc(ptr: *mut *mut snd_seq_queue_status_t) -> c_int;
    /// Free a record from `snd_seq_queue_status_malloc`.
    pub fn snd_seq_queue_status_free(ptr: *mut snd_seq_queue_status_t);
    /// Fill `status` with queue `q`'s state now.
    pub fn snd_seq_get_queue_status(
        seq: *mut snd_seq_t,
        q: c_int,
        status: *mut snd_seq_queue_status_t,
    ) -> c_int;
    /// The queue's real time in `status`: a pointer into `status`, valid until
    /// it is freed.
    pub fn snd_seq_queue_status_get_real_time(
        status: *const snd_seq_queue_status_t,
    ) -> *const snd_seq_real_time_t;

    /// Advance `info` to the client's next port; negative when there is none.
    pub fn snd_seq_query_next_port(seq: *mut snd_seq_t, info: *mut snd_seq_port_info_t) -> c_int;

    // Function blocks. Opaque + getters, so no struct layout to get wrong.
    // @@ALSA_1.2.10
    /// Allocate a function-block record; free it with `snd_ump_block_info_free`.
    pub fn snd_ump_block_info_malloc(ptr: *mut *mut snd_ump_block_info_t) -> c_int;
    /// Free a record from `snd_ump_block_info_malloc`.
    pub fn snd_ump_block_info_free(ptr: *mut snd_ump_block_info_t);
    /// @@ALSA_1.2.10
    pub fn snd_seq_get_ump_block_info(
        seq: *mut snd_seq_t,
        client: c_int,
        blk: c_int,
        info: *mut c_void,
    ) -> c_int;
}

#[cfg(all(alsa_ump, feature = "test-support"))]
unsafe extern "C" {
    /// Set the port's `SND_SEQ_PORT_CAP_*` bits.
    pub fn snd_seq_port_info_set_capability(info: *mut snd_seq_port_info_t, capability: c_uint);
}

/// The measured `snd_seq_ump_event_t` size, alignment and field offsets, checked
/// at compile time (they need no libasound).
const _: () = {
    use std::mem::{align_of, offset_of, size_of};
    assert!(size_of::<snd_seq_ump_event_t>() == 32);
    assert!(align_of::<snd_seq_ump_event_t>() == 4);
    assert!(size_of::<snd_seq_addr_t>() == 2);
    assert!(size_of::<snd_seq_real_time_t>() == 8);
    assert!(offset_of!(snd_seq_ump_event_t, type_) == 0);
    assert!(offset_of!(snd_seq_ump_event_t, flags) == 1);
    assert!(offset_of!(snd_seq_ump_event_t, tag) == 2);
    assert!(offset_of!(snd_seq_ump_event_t, queue) == 3);
    assert!(offset_of!(snd_seq_ump_event_t, time) == 4);
    assert!(offset_of!(snd_seq_ump_event_t, source) == 12);
    assert!(offset_of!(snd_seq_ump_event_t, dest) == 14);
    assert!(offset_of!(snd_seq_ump_event_t, ump) == 16);
};

#[cfg(test)]
mod tests;
