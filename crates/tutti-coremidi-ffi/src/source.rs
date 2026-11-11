//! A **native-UMP** virtual MIDI source.
//!
//! The safe `coremidi` wrapper only exposes a MIDI-1.0 virtual source
//! (`MIDISourceCreate`), whose only send path is legacy packet bytes, which
//! carries MIDI 1.0 messages only, so a MIDI-2-only message (per-note
//! controllers, **and JR Timestamps**) cannot reach the wire through it. Publishing a genuine UMP
//! stream, the one transport where a JR Timestamp actually reaches the wire,
//! needs a MIDI-2.0-protocol endpoint, created via
//! `MIDISourceCreateWithProtocol` and fed `MIDIReceivedEventList` with raw UMP
//! words. That is what [`UmpSource`] does.

use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use coremidi_sys::{
    ByteCount, MIDIClientCreate, MIDIClientRef, MIDIEndpointDispose, MIDIEndpointRef,
    MIDIEventList, MIDIEventListAdd, MIDIEventListInit, MIDIProtocolID, MIDIReceivedEventList,
    MIDISourceCreateWithProtocol, MIDITimeStamp, kMIDIProtocol_2_0,
};
use std::fmt;
use std::mem::MaybeUninit;
use std::sync::Mutex;

/// A CoreMIDI call that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error {
    /// What was being done, as a static label.
    pub operation: &'static str,
    /// The `OSStatus` CoreMIDI returned (`-1` where no call returned one).
    pub status: i32,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CoreMIDI error: {} ({})", self.operation, self.status)
    }
}

impl std::error::Error for Error {}

/// The process-wide raw client every source's endpoint belongs to.
///
/// The safe wrapper hides its `MIDIClientRef`, and `MIDISourceCreateWithProtocol`
/// needs the raw one, so this module keeps a client of its own. It is never
/// disposed: Apple warns that disposing the last client may make the MIDI server
/// exit and fail every later client creation, and coremidi 0.9 stopped disposing
/// its own for the same reason. A failed creation is not cached.
fn shared_raw_client() -> Result<MIDIClientRef, i32> {
    static CLIENT: Mutex<Option<MIDIClientRef>> = Mutex::new(None);
    let mut slot = CLIENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(client) = *slot {
        return Ok(client);
    }
    let name = CFString::new("tutti-ump-src");
    let mut client: MIDIClientRef = 0;
    // SAFETY: `name` outlives the call; `client` is a valid out-pointer. A NULL
    // notify proc with a NULL context is the documented "no notifications" form.
    let status = unsafe {
        MIDIClientCreate(
            name.as_concrete_TypeRef(),
            None,
            std::ptr::null_mut(),
            &mut client,
        )
    };
    if status != 0 {
        return Err(status);
    }
    *slot = Some(client);
    Ok(client)
}

/// Send this timestamp to mean "now" (CoreMIDI treats 0 as immediate delivery).
const MIDI_TIMESTAMP_NOW: MIDITimeStamp = 0;

/// A CoreMIDI virtual source that speaks the **MIDI 2.0 (UMP) protocol**.
///
/// It carries UMP words end to end, so JR Timestamps and other MIDI-2-only
/// messages survive to the wire. Owns its endpoint, which is disposed on drop.
///
/// `Send` and `Sync` without an `unsafe impl`: the endpoint is an opaque `u32`
/// handle into CoreMIDI, whose objects are internally synchronised.
#[derive(Debug)]
#[must_use = "dropping it removes the virtual source"]
pub struct UmpSource {
    source: MIDIEndpointRef,
}

impl UmpSource {
    /// Creates a MIDI-2.0 virtual source named `name`, visible to other apps as
    /// a UMP-capable MIDI source.
    ///
    /// # Errors
    ///
    /// CoreMIDI's status if it refuses the client or the endpoint.
    pub fn new(name: &str) -> Result<Self, Error> {
        let source_name = CFString::new(name);
        let client = shared_raw_client().map_err(|status| Error {
            operation: "create client (ump source)",
            status,
        })?;

        let mut source: MIDIEndpointRef = 0;
        // SAFETY: `client` is the live process-wide client; `source_name` outlives
        // the call and `source` is a valid out-pointer.
        let status = unsafe {
            MIDISourceCreateWithProtocol(
                client,
                source_name.as_concrete_TypeRef(),
                kMIDIProtocol_2_0 as MIDIProtocolID,
                &mut source,
            )
        };
        if status != 0 {
            return Err(Error {
                operation: "create ump virtual source",
                status,
            });
        }
        Ok(Self { source })
    }

    /// Publishes one UMP message to the endpoint's subscribers.
    ///
    /// `words` is a complete UMP message; its length must match the message type
    /// (a JR Timestamp is 1 word, a note is 1 to 2, SysEx8 is 4). Empty input is
    /// a no-op. CoreMIDI copies the words, so no lifetime escapes this call.
    ///
    /// # Errors
    ///
    /// When the message will not fit the event list, or when
    /// `MIDIReceivedEventList` returns a non-zero `OSStatus`.
    pub fn send(&self, words: &[u32]) -> Result<(), Error> {
        if words.is_empty() {
            return Ok(());
        }

        // A `MIDIEventList` holds one packet of up to 64 words inline, so a single
        // message of up to 4 words always fits. Build it the framework-sanctioned
        // way (`Init` + `Add`) rather than hand-packing the C struct.
        let mut list = MaybeUninit::<MIDIEventList>::uninit();
        let list_ptr = list.as_mut_ptr();
        let list_size = std::mem::size_of::<MIDIEventList>();

        // SAFETY: `list_ptr` addresses the whole `MIDIEventList`, and `Init`
        // is what initialises it; `assume_init_ref` below is reached only
        // after both calls have run. `list_size` is that allocation's real size,
        // so `Add` cannot write past it, and `words` outlives the call.
        let status = unsafe {
            let mut packet = MIDIEventListInit(list_ptr, kMIDIProtocol_2_0 as MIDIProtocolID);
            packet = MIDIEventListAdd(
                list_ptr,
                list_size as ByteCount,
                packet,
                MIDI_TIMESTAMP_NOW,
                words.len() as ByteCount,
                words.as_ptr(),
            );
            if packet.is_null() {
                // The message didn't fit (only possible if `words` is absurdly
                // long): nothing was queued.
                return Err(Error {
                    operation: "add ump packet",
                    status: -1,
                });
            }
            MIDIReceivedEventList(self.source, list.assume_init_ref() as *const MIDIEventList)
        };

        if status == 0 {
            Ok(())
        } else {
            Err(Error {
                operation: "send ump event list",
                status,
            })
        }
    }
}

impl Drop for UmpSource {
    fn drop(&mut self) {
        // SAFETY: the endpoint was created in `new` and is disposed exactly once,
        // here. Its client is process-wide and outlives it.
        unsafe { MIDIEndpointDispose(self.source) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    /// The source crosses threads with no `unsafe impl`: if a field ever stopped
    /// being a plain handle, this stops compiling.
    #[test]
    fn a_source_is_send_and_sync() {
        assert_send_sync::<UmpSource>();
    }

    /// A message longer than the event list holds is refused with the packet
    /// error, not written past it; an empty one is a no-op.
    #[test]
    fn a_message_that_does_not_fit_is_refused() {
        let source = UmpSource::new("tutti-ffi-test").expect("CoreMIDI");
        assert_eq!(source.send(&[]), Ok(()));
        let err = source.send(&[0; 100_000]).unwrap_err();
        assert_eq!(err.operation, "add ump packet");
        assert_eq!(source.send(&[0x2090_3C64]), Ok(()));
    }
}
