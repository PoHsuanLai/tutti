//! A **native-UMP** virtual MIDI source (macOS / CoreMIDI).
//!
//! The safe `coremidi` wrapper only exposes a MIDI-1.0 virtual source
//! (`MIDISourceCreate`), whose only send path is legacy packet bytes — which
//! forces every event through `to_midi1_bytes`, dropping any
//! MIDI-2-only message (per-note controllers, **and JR Timestamps**). Publishing
//! a genuine UMP stream — the one transport where a JR Timestamp actually
//! reaches the wire — needs a MIDI-2.0-protocol endpoint, created via
//! `MIDISourceCreateWithProtocol` and fed `MIDIReceivedEventList` with raw UMP
//! words. That FFI isn't in the safe wrapper, so this module owns the small
//! `coremidi-sys` bridge for it.
//!
//! [`UmpVirtualSource`] creates one such endpoint and sends `&[u32]` UMP words
//! directly. It owns a `MIDIEndpointRef`, disposed on drop, and sends through a
//! process-wide `MIDIClientRef` of this module's own, never disposed.

use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use coremidi_sys::{
    MIDIClientCreate, MIDIClientRef, MIDIEndpointDispose, MIDIEndpointRef, MIDIEventList,
    MIDIEventListAdd, MIDIEventListInit, MIDIProtocolID, MIDIReceivedEventList,
    MIDISourceCreateWithProtocol, MIDITimeStamp, kMIDIProtocol_2_0,
};
use std::mem::MaybeUninit;
use std::sync::Mutex;
use tracing::debug;

use crate::error::{Error, Result};

/// The process-wide raw client every source's endpoint belongs to.
///
/// The safe wrapper hides its `MIDIClientRef`, and `MIDISourceCreateWithProtocol`
/// needs the raw one, so this module keeps a client of its own (one of the two this
/// backend creates, beside [`shared_client`](super::shared_client); both start the
/// device watch's client first, see [`hub_first`](super::hub_first)).
/// It is never disposed: Apple warns that disposing the last client may make the
/// MIDI server exit and fail every later client creation, and coremidi 0.9 stopped
/// disposing its own for the same reason. A failed creation is not cached.
fn shared_raw_client() -> std::result::Result<MIDIClientRef, i32> {
    super::hub_first();
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
/// Unlike a MIDI-1.0 virtual source (byte packets, whose only send path forces
/// every event through `to_midi1_bytes`), this endpoint carries UMP
/// words end to end, so JR Timestamps and other MIDI-2-only messages survive to
/// the wire. Owns its endpoint, which is disposed on drop.
#[must_use = "dropping it removes the virtual source"]
pub struct UmpVirtualSource {
    source: MIDIEndpointRef,
    name: String,
}

impl core::fmt::Debug for UmpVirtualSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Owns opaque CoreMIDI FFI handles; report the endpoint name only.
        f.debug_struct("UmpVirtualSource")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

// SAFETY: `MIDIClientRef` / `MIDIEndpointRef` are opaque `UInt32` handles into
// CoreMIDI; the framework's own objects are internally synchronized and the
// handles are just identifiers. Sending them across threads (the pump runs on a
// Bevy system thread, not the creating thread) is sound — the same contract the
// safe wrapper's `VirtualSource` relies on.
unsafe impl Send for UmpVirtualSource {}
unsafe impl Sync for UmpVirtualSource {}

impl UmpVirtualSource {
    /// Creates a MIDI-2.0 virtual source named `name`, visible to other apps as a
    /// UMP-capable MIDI source.
    ///
    /// Keep the value alive: dropping it disposes the endpoint.
    ///
    /// # Errors
    ///
    /// [`Error::CoreMidi`] if CoreMIDI refuses either the client or the
    /// endpoint.
    pub fn new(name: &str) -> Result<Self> {
        let source_name = CFString::new(name);
        let client = shared_raw_client().map_err(|status| Error::CoreMidi {
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
            return Err(Error::CoreMidi {
                operation: "create ump virtual source",
                status,
            });
        }

        debug!(name, "Created native-UMP virtual MIDI source");
        Ok(Self {
            source,
            name: name.to_string(),
        })
    }

    /// Publish one UMP message (1–4 words) to the endpoint's subscribers.
    ///
    /// `words` is a complete UMP message; its length must match the message type
    /// (a JR Timestamp is 1 word, a note is 1–2, SysEx8 is 4). Empty input is a
    /// no-op. CoreMIDI copies the words, so no lifetime escapes this call.
    ///
    /// # Errors
    ///
    /// [`Error::CoreMidi`] when the message will not fit the event list, or when
    /// `MIDIReceivedEventList` returns a non-zero `OSStatus`.
    pub fn send_ump(&self, words: &[u32]) -> Result<()> {
        if words.is_empty() {
            return Ok(());
        }

        // A `MIDIEventList` holds one packet of up to 64 words inline, so a single
        // ≤4-word UMP message always fits. Build it the framework-sanctioned way
        // (`Init` + `Add`) rather than hand-packing the C struct.
        let mut list = MaybeUninit::<MIDIEventList>::uninit();
        let list_ptr = list.as_mut_ptr();
        let list_size = std::mem::size_of::<MIDIEventList>();

        // SAFETY: `list_ptr` addresses the whole `MIDIEventList`, and `Init`
        // is what initialises it — `assume_init_ref` below is reached only
        // after both calls have run. `list_size` is that allocation's real size,
        // so `Add` cannot write past it, and `words` outlives the call.
        let status = unsafe {
            let mut packet = MIDIEventListInit(list_ptr, kMIDIProtocol_2_0 as MIDIProtocolID);
            packet = MIDIEventListAdd(
                list_ptr,
                list_size as coremidi_sys::ByteCount,
                packet,
                MIDI_TIMESTAMP_NOW,
                words.len() as coremidi_sys::ByteCount,
                words.as_ptr(),
            );
            if packet.is_null() {
                // The message didn't fit (only possible if `words` is absurdly
                // long) — nothing was queued.
                return Err(Error::CoreMidi {
                    operation: "add ump packet",
                    status: -1,
                });
            }
            MIDIReceivedEventList(self.source, list.assume_init_ref() as *const MIDIEventList)
        };

        if status == 0 {
            Ok(())
        } else {
            Err(Error::CoreMidi {
                operation: "send ump event list",
                status,
            })
        }
    }

    /// The source's display name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for UmpVirtualSource {
    fn drop(&mut self) {
        debug!(name = %self.name, "Dropping native-UMP virtual MIDI source");
        // SAFETY: the endpoint was created in `new` and is disposed exactly once,
        // here. Its client is process-wide and outlives it.
        unsafe { MIDIEndpointDispose(self.source) };
    }
}

#[cfg(test)]
mod tests;
