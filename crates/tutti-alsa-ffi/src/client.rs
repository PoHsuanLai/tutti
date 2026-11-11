//! A UMP-mode ALSA sequencer client, and the queue clock it can start.

use std::ffi::CString;
use std::time::Duration;

use crate::error::{Error, Result, check};
use crate::event::Received;
use crate::records::{PortRecord, StatusRecord};
use crate::sys;

/// `-EAGAIN`: nothing queued yet on a non-blocking client.
const EAGAIN: i32 = -11;
/// `-EINVAL`.
const EINVAL: i32 = -22;

/// A name as a C string; one with an interior NUL is called `tutti` instead.
fn c_name(name: &str) -> CString {
    CString::new(name).unwrap_or_else(|_| c"tutti".to_owned())
}

/// What [`Client::read_event`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    /// One event.
    Event(Received),
    /// Nothing queued (`-EAGAIN`; only a non-blocking client says so).
    Empty,
    /// The read succeeded and returned no event; read again.
    Skipped,
}

/// An open sequencer client that speaks UMP. Owns the `snd_seq_t` and closes it
/// on drop.
///
/// It is `Send` and not `Sync`: one thread uses it at a time, and it moves to
/// its pump thread at spawn.
///
/// # One client per connection, not one per backend
///
/// [`read_event`](Client::read_event) **blocks** on a blocking client, so a
/// shared client would mean every input pump contending on one handle, and one
/// device going quiet would stall the others.
#[derive(Debug)]
pub struct Client {
    seq: *mut sys::snd_seq_t,
    id: i32,
}

// SAFETY: `snd_seq_t` is an opaque handle whose operations alsa-lib
// synchronises internally, and a `Client` is not `Sync`, so it is used from one
// thread at a time: moving it to another is the same use.
unsafe impl Send for Client {}

impl Client {
    /// Opens a client in UMP MIDI-2.0 mode, named `name` to other clients.
    ///
    /// Three steps, and all three matter:
    /// 1. `snd_seq_open`: an ordinary sequencer client.
    /// 2. `snd_seq_set_client_midi_version(UMP_MIDI_2_0)`: **this is what makes
    ///    it a UMP client.** Without it the UMP event calls have nothing to
    ///    carry.
    /// 3. `snd_seq_set_client_ump_conversion(1)`: let the kernel translate at
    ///    the port boundary, so this client can talk to legacy ports and vice
    ///    versa. Kernel 6.5 or newer. Almost no Linux machine has a native-UMP
    ///    device yet, so without this the backend would be correct and useless.
    ///
    /// `nonblock` opens it so a read answers [`Read::Empty`] rather than
    /// blocking.
    ///
    /// # Errors
    ///
    /// The step that failed, with alsa-lib's code.
    pub fn open(name: &str, nonblock: bool) -> Result<Self> {
        // `"hw"`, not `"default"`. The sequencer is a kernel device, and `hw`
        // addresses it directly; `default` routes through a config-file plugin
        // that, on at least Ubuntu, **cannot be opened non-blocking**: it
        // returns `-ENOENT` for `SND_SEQ_NONBLOCK` while succeeding blocking.
        //
        // That asymmetry made input (which polls, so opens non-blocking) fail
        // while output (blocking) worked, from the same function, and read as
        // "no MIDI inputs" rather than as an open failure.
        let device = c"hw";
        let mut seq: *mut sys::snd_seq_t = std::ptr::null_mut();
        let mode = if nonblock { sys::SND_SEQ_NONBLOCK } else { 0 };

        // SAFETY: `seq` is a valid out-pointer; `device` is a NUL-terminated
        // literal that outlives the call.
        let rc =
            unsafe { sys::snd_seq_open(&mut seq, device.as_ptr(), sys::SND_SEQ_OPEN_DUPLEX, mode) };
        check(rc, "snd_seq_open")?;
        // Owned from here: any failure below closes it by `Drop`.
        let mut client = Self { seq, id: -1 };

        let cname = c_name(name);
        // SAFETY: `client.seq` is open; `cname` outlives the call.
        unsafe { sys::snd_seq_set_client_name(client.seq, cname.as_ptr()) };

        // SAFETY: `client.seq` is open.
        let rc = unsafe {
            sys::snd_seq_set_client_midi_version(client.seq, sys::SND_SEQ_CLIENT_UMP_MIDI_2_0)
        };
        check(rc, "snd_seq_set_client_midi_version")?;
        // SAFETY: `client.seq` is open.
        let rc = unsafe { sys::snd_seq_set_client_ump_conversion(client.seq, 1) };
        check(rc, "snd_seq_set_client_ump_conversion")?;
        // SAFETY: `client.seq` is open.
        let rc = unsafe { sys::snd_seq_client_id(client.seq) };
        client.id = check(rc, "snd_seq_client_id")?;
        Ok(client)
    }

    /// This client's sequencer id, for addressing events back to it.
    #[must_use]
    pub fn id(&self) -> i32 {
        self.id
    }

    /// Creates a port on this client with `caps` (`CAP_*`) and `kind`
    /// (`TYPE_*`), and returns its number.
    ///
    /// # Errors
    ///
    /// `snd_seq_create_simple_port`'s code.
    pub fn open_port(&self, name: &str, caps: u32, kind: u32) -> Result<i32> {
        let cname = c_name(name);
        // SAFETY: `self.seq` is open; `cname` outlives the call.
        let rc = unsafe { sys::snd_seq_create_simple_port(self.seq, cname.as_ptr(), caps, kind) };
        check(rc, "snd_seq_create_simple_port")
    }

    /// Subscribes `port` to what `src_client:src_port` sends.
    ///
    /// # Errors
    ///
    /// `snd_seq_connect_from`'s code: the address is not one that exists.
    pub fn connect_from(&self, port: i32, src_client: i32, src_port: i32) -> Result<()> {
        // SAFETY: `self.seq` is open; the arguments are plain integers the
        // sequencer validates.
        let rc = unsafe { sys::snd_seq_connect_from(self.seq, port, src_client, src_port) };
        check(rc, "snd_seq_connect_from").map(drop)
    }

    /// Subscribes `dest_client:dest_port` to what `port` sends.
    ///
    /// # Errors
    ///
    /// `snd_seq_connect_to`'s code: the address is not one that exists.
    pub fn connect_to(&self, port: i32, dest_client: i32, dest_port: i32) -> Result<()> {
        // SAFETY: `self.seq` is open; the arguments are plain integers the
        // sequencer validates.
        let rc = unsafe { sys::snd_seq_connect_to(self.seq, port, dest_client, dest_port) };
        check(rc, "snd_seq_connect_to").map(drop)
    }

    /// Subscribes `port` to the system announce port, which sends an event
    /// whenever a client or port starts, exits or changes.
    ///
    /// # Errors
    ///
    /// `snd_seq_connect_from`'s code.
    pub fn connect_from_announce(&self, port: i32) -> Result<()> {
        self.connect_from(
            port,
            sys::SND_SEQ_CLIENT_SYSTEM,
            sys::SND_SEQ_PORT_SYSTEM_ANNOUNCE,
        )
    }

    /// Reads one event. A blocking client waits for one; a non-blocking client
    /// answers [`Read::Empty`] when none is queued.
    ///
    /// # Errors
    ///
    /// `snd_seq_ump_event_input`'s code for anything but an empty queue, an
    /// overrun among them.
    pub fn read_event(&self) -> Result<Read> {
        let mut ev: *mut sys::snd_seq_ump_event_t = std::ptr::null_mut();
        // SAFETY: `self.seq` is open; `ev` is a valid out-pointer. On success
        // alsa-lib points it at a buffer it owns, valid until the next call on
        // this client, which `&self` cannot make while `ev` is read below.
        let rc = unsafe { sys::snd_seq_ump_event_input(self.seq, &mut ev) };
        if rc == EAGAIN {
            return Ok(Read::Empty);
        }
        check(rc, "snd_seq_ump_event_input")?;
        // SAFETY: `rc >= 0` means alsa-lib wrote a complete event at `ev`, which
        // `as_ref` checks for NULL; it is copied out by value before any other
        // call.
        match unsafe { ev.as_ref() } {
            Some(ev) => Ok(Read::Event(Received::from_event(ev))),
            None => Ok(Read::Skipped),
        }
    }

    /// Sends one UMP message to everything subscribed to `port` (see
    /// [`connect_to`](Self::connect_to)), immediately, bypassing any queue.
    ///
    /// `words` is one complete UMP message; only its first 4 words are sent,
    /// which is the whole of any UMP message. Empty input is a no-op.
    ///
    /// # Errors
    ///
    /// `snd_seq_ump_event_output_direct`'s code, or `-EINVAL` when this
    /// client's id is not an address byte.
    pub fn send_to_subscribers(&self, port: u8, words: &[u32]) -> Result<()> {
        if words.is_empty() {
            return Ok(());
        }
        let operation = "snd_seq_ump_event_output_direct";
        let client = u8::try_from(self.id).map_err(|_| Error {
            operation,
            code: EINVAL,
        })?;
        let mut ev = sys::snd_seq_ump_event_t {
            // `SND_SEQ_EVENT_NONE`. For a UMP event the type field is unused:
            // the message type lives in the first word's top nibble.
            type_: 0,
            source: sys::snd_seq_addr_t { client, port },
            // **Not** the destination's own address, even though it is known.
            //
            // The port is already subscribed (`connect_to`), and an explicit
            // `dest` on top of a subscription is rejected outright:
            // `snd_seq_ump_event_output_direct` returns `-EINVAL`, not a partial
            // send. `SUBSCRIBERS` means "everyone connected to my port", which
            // is exactly the set `connect_to` established.
            dest: sys::snd_seq_addr_t {
                client: sys::SND_SEQ_ADDRESS_SUBSCRIBERS,
                port: 0,
            },
            // Dispatch now rather than through a queue. Zero here is queue 0, a
            // real queue this client never created, which is the other half of
            // that same `-EINVAL`.
            queue: sys::SND_SEQ_QUEUE_DIRECT,
            ..Default::default()
        };
        copy_words(&mut ev.ump, words);

        // SAFETY: `self.seq` is open; `ev` is a fully initialised event of the
        // measured layout (the compile-time asserts in `sys`), which alsa-lib
        // reads and does not retain.
        let rc = unsafe { sys::snd_seq_ump_event_output_direct(self.seq, &mut ev) };
        check(rc, operation).map(drop)
    }

    /// Starts a queue of this client's own and makes `port` stamp each event it
    /// receives with that queue's real time, at the moment the kernel delivers
    /// it. The returned clock reads that queue, and the queue closes with this
    /// client.
    ///
    /// # Errors
    ///
    /// The code of the call that failed.
    pub fn stamp_port(&self, port: i32) -> Result<QueueClock> {
        // SAFETY: `self.seq` is open.
        let rc = unsafe { sys::snd_seq_alloc_queue(self.seq) };
        let queue = check(rc, "snd_seq_alloc_queue")?;
        // SAFETY: `self.seq` is open; a NULL event is the call's documented
        // "send directly".
        let rc = unsafe {
            sys::snd_seq_control_queue(
                self.seq,
                queue,
                sys::SND_SEQ_EVENT_START,
                0,
                std::ptr::null_mut(),
            )
        };
        check(rc, "snd_seq_control_queue")?;
        // The control event is buffered, not sent, until the buffer is drained.
        // SAFETY: `self.seq` is open.
        let rc = unsafe { sys::snd_seq_drain_output(self.seq) };
        check(rc, "snd_seq_drain_output")?;
        self.edit_port(port, |info| {
            // SAFETY: `info` is a live record, filled from this client's port.
            unsafe {
                sys::snd_seq_port_info_set_timestamping(info, 1);
                sys::snd_seq_port_info_set_timestamp_real(info, 1);
                sys::snd_seq_port_info_set_timestamp_queue(info, queue);
            }
        })?;
        Ok(QueueClock {
            queue,
            status: StatusRecord::new()?,
        })
    }

    /// Reads one of this client's ports into a record, lets `edit` change it,
    /// and writes it back; the sequencer announces the change.
    fn edit_port(&self, port: i32, edit: impl FnOnce(*mut sys::snd_seq_port_info_t)) -> Result<()> {
        let info = PortRecord::new()?;
        // SAFETY: `self.seq` is open and `info` is a live record.
        let rc = unsafe { sys::snd_seq_get_port_info(self.seq, port, info.as_ptr()) };
        check(rc, "snd_seq_get_port_info")?;
        edit(info.as_ptr());
        // SAFETY: `self.seq` is open and `info` is a live record.
        let rc = unsafe { sys::snd_seq_set_port_info(self.seq, port, info.as_ptr()) };
        check(rc, "snd_seq_set_port_info").map(drop)
    }

    /// The sequencer handle, for the crate's other modules.
    pub(crate) fn raw(&self) -> *mut sys::snd_seq_t {
        self.seq
    }

    /// Renames this client; the sequencer announces the change.
    #[cfg(feature = "test-support")]
    pub fn rename(&self, name: &str) {
        let cname = c_name(name);
        // SAFETY: `self.seq` is open; `cname` outlives the call.
        unsafe { sys::snd_seq_set_client_name(self.seq, cname.as_ptr()) };
    }

    /// Sets the capability bits of one of this client's ports; the sequencer
    /// announces the change.
    ///
    /// # Errors
    ///
    /// The code of the call that failed.
    #[cfg(feature = "test-support")]
    pub fn set_port_caps(&self, port: i32, caps: u32) -> Result<()> {
        self.edit_port(port, |info| {
            // SAFETY: `info` is a live record, filled from this client's port.
            unsafe { sys::snd_seq_port_info_set_capability(info, caps) };
        })
    }
}

/// Copies a message's words into an event's four, and leaves the rest zero; a
/// message longer than four words is cut to them.
fn copy_words(field: &mut [u32; 4], words: &[u32]) {
    let n = words.len().min(field.len());
    field[..n].copy_from_slice(&words[..n]);
}

impl Drop for Client {
    fn drop(&mut self) {
        // SAFETY: opened by `open`, closed exactly once: `seq` is private and
        // never reassigned.
        unsafe { sys::snd_seq_close(self.seq) };
    }
}

/// A queue's real-time clock, read through a status record kept for the
/// purpose. Made by [`Client::stamp_port`].
#[derive(Debug)]
pub struct QueueClock {
    queue: i32,
    status: StatusRecord,
}

impl QueueClock {
    /// The queue's real time now, as the time since it started; `None` if the
    /// sequencer will not say. `client` is the one that made the queue: another
    /// client has no such queue and reads `None`.
    pub fn now(&mut self, client: &Client) -> Option<Duration> {
        // SAFETY: `client.seq` is open and `status` is a live record.
        let rc =
            unsafe { sys::snd_seq_get_queue_status(client.seq, self.queue, self.status.as_ptr()) };
        if rc < 0 {
            return None;
        }
        // SAFETY: `status` was just filled; the pointer is into it and read at
        // once, before the record is used again.
        let time =
            unsafe { sys::snd_seq_queue_status_get_real_time(self.status.as_ptr()).as_ref() }?;
        Some(Duration::new(u64::from(time.tv_sec), time.tv_nsec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A message shorter than four words leaves the rest zero, and a longer
    /// one is cut to four. Mutations: dropping `.min(field.len())` panics on
    /// the five-word message; copying into `field[4 - n..]` fails the first.
    #[test]
    fn a_message_fills_the_front_of_the_four_words() {
        let mut field = [9; 4];
        copy_words(&mut field, &[1, 2]);
        assert_eq!(field, [1, 2, 9, 9]);
        let mut field = [0; 4];
        copy_words(&mut field, &[1, 2, 3, 4, 5]);
        assert_eq!(field, [1, 2, 3, 4]);
    }

    /// A name with a NUL is not refused: the client is called `tutti`.
    #[test]
    fn a_name_with_a_nul_falls_back() {
        assert_eq!(c_name("a\0b").to_str(), Ok("tutti"));
        assert_eq!(c_name("ok").to_str(), Ok("ok"));
    }
}
