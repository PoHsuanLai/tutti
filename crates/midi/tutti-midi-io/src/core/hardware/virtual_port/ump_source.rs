//! A **native-UMP** virtual MIDI source (macOS / CoreMIDI).
//!
//! The safe `coremidi` 0.8 wrapper only exposes a MIDI-1.0 virtual source
//! (`MIDISourceCreate`), whose only send path is legacy packet bytes — which
//! forces every event through `MidiEvent::to_midi1_bytes`, dropping any
//! MIDI-2-only message (per-note controllers, **and JR Timestamps**). To publish
//! a genuine UMP stream — the one transport where a JR Timestamp actually reaches
//! the wire — we need a MIDI-2.0-protocol endpoint, created via
//! `MIDISourceCreateWithProtocol` and fed `MIDIReceivedEventList` with raw UMP
//! words. That FFI isn't in the safe wrapper, so this module owns the small
//! `coremidi-sys` bridge for it.
//!
//! [`UmpVirtualSource`] creates one such endpoint and sends `&[u32]` UMP words
//! directly. It manages its own `MIDIClientRef` + `MIDIEndpointRef` and disposes
//! both on drop.

use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use coremidi_sys::{
    kMIDIProtocol_2_0, MIDIClientCreate, MIDIClientDispose, MIDIClientRef, MIDIEndpointDispose,
    MIDIEndpointRef, MIDIEventList, MIDIEventListAdd, MIDIEventListInit, MIDIProtocolID,
    MIDIReceivedEventList, MIDISourceCreateWithProtocol, MIDITimeStamp,
};
use std::mem::MaybeUninit;
use tracing::debug;

use crate::core::error::{Error, Result};

/// Send this timestamp to mean "now" (CoreMIDI treats 0 as immediate delivery).
const MIDI_TIMESTAMP_NOW: MIDITimeStamp = 0;

/// A CoreMIDI virtual source that speaks the **MIDI 2.0 (UMP) protocol**.
///
/// Unlike [`super::VirtualMidiSource`] (MIDI 1.0, byte packets), this endpoint
/// carries UMP words end to end, so JR Timestamps and other MIDI-2-only messages
/// survive to the wire. Owns its client + endpoint; both are disposed on drop.
pub struct UmpVirtualSource {
    client: MIDIClientRef,
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
    /// Create a MIDI-2.0 virtual source named `name`, visible to other apps as a
    /// UMP-capable MIDI source.
    pub fn new(name: &str) -> Result<Self> {
        let client_name = CFString::new(&format!("tutti-ump-src-{name}"));
        let source_name = CFString::new(name);

        // Create our own client (the safe wrapper hides its `MIDIClientRef`).
        let mut client: MIDIClientRef = 0;
        let status = unsafe {
            MIDIClientCreate(
                client_name.as_concrete_TypeRef(),
                None,
                std::ptr::null_mut(),
                &mut client,
            )
        };
        if status != 0 {
            return Err(Error::CoreMidi {
                operation: "create client (ump source)",
                status,
            });
        }

        let mut source: MIDIEndpointRef = 0;
        let status = unsafe {
            MIDISourceCreateWithProtocol(
                client,
                source_name.as_concrete_TypeRef(),
                kMIDIProtocol_2_0 as MIDIProtocolID,
                &mut source,
            )
        };
        if status != 0 {
            // Roll back the client we just made so we don't leak it.
            unsafe { MIDIClientDispose(client) };
            return Err(Error::CoreMidi {
                operation: "create ump virtual source",
                status,
            });
        }

        debug!(name, "Created native-UMP virtual MIDI source");
        Ok(Self {
            client,
            source,
            name: name.to_string(),
        })
    }

    /// Publish one UMP message (1–4 words) to the endpoint's subscribers.
    ///
    /// `words` is a complete UMP message; its length must match the message type
    /// (a JR Timestamp is 1 word, a note is 1–2, SysEx8 is 4). Empty input is a
    /// no-op. CoreMIDI copies the words, so no lifetime escapes this call.
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
        unsafe {
            MIDIEndpointDispose(self.source);
            MIDIClientDispose(self.client);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};

    #[test]
    fn create_ump_source() {
        let src = UmpVirtualSource::new("Test UMP Source").expect("creates");
        assert_eq!(src.name(), "Test UMP Source");
    }

    #[test]
    fn send_empty_is_noop() {
        let src = UmpVirtualSource::new("Test UMP Empty").expect("creates");
        assert!(src.send_ump(&[]).is_ok());
    }

    #[test]
    fn send_jr_timestamp_word() {
        // A JR Timestamp is a single UMP Utility word — the whole point of this
        // endpoint (it has no MIDI 1.0 form). It must send without error.
        let src = UmpVirtualSource::new("Test UMP JR").expect("creates");
        let jr = tutti_midi_types::ump::MidiEvent::jr_timestamp(0x1234).data_words()[0];
        assert!(src.send_ump(&[jr]).is_ok());
    }

    #[test]
    fn send_multiword_note_on() {
        // A MIDI 2.0 channel-voice note-on is two words.
        let src = UmpVirtualSource::new("Test UMP Note").expect("creates");
        let note = tutti_midi_types::ump::MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            60,
            0x8000,
        );
        assert!(src.send_ump(note.data_words()).is_ok());
    }
}
