//! The Linux backend: ALSA's UMP sequencer.
//!
//! Present only when `build.rs` found alsa-lib ≥ 1.2.10 (`cfg(alsa_ump)`).
//!
//! # UMP over a sequencer that predates it
//!
//! `snd_seq_set_client_midi_version(SND_SEQ_CLIENT_UMP_MIDI_2_0)` turns an
//! ordinary seq client into a **UMP** client: `snd_seq_ump_event_input` /
//! `_output_direct` then carry four-word UMP messages instead of the legacy
//! byte-oriented events.
//!
//! Paired with `snd_seq_set_client_ump_conversion(1)`, the kernel (≥ 6.5)
//! translates in both directions at the port boundary, so a UMP client can talk
//! to legacy hardware and a legacy app can talk to us — neither side needing to
//! know. That is what makes this backend useful today, when almost no Linux
//! machine has a native-UMP device: the conversion is the kernel's job, not
//! ours, and unlike a userspace MIDI-1.0 fallback it does not silently drop
//! MIDI-2-only messages that *are* representable.
//!
//! # Threading
//!
//! `snd_seq_ump_event_input` blocks, so each open input runs a pump thread that
//! reads and pushes into the connection's [`InputProducerHandle`] — the same
//! ring CoreMIDI's callback feeds. The thread exits when its connection drops.

mod client;
mod enumerate;
mod input;
mod output;
pub mod sys;

use crate::core::capability::{EndpointId, EndpointInfo};
use crate::core::endpoints::{InputConnection, MidiEndpoints};
use crate::core::error::Result;
use crate::core::InputProducerHandle;
use client::SeqClient;
use enumerate::Direction;
use tutti_midi_types::MidiOut;

/// The Linux [`MidiEndpoints`] implementation.
///
/// Holds no client of its own: enumeration opens a short-lived one per call, and
/// each connection owns the client it pumps or sends through. A shared handle
/// would serialise a blocking read against every other operation.
#[derive(Debug, Default)]
pub struct AlsaEndpoints;

impl AlsaEndpoints {
    pub fn new() -> Self {
        Self
    }

    /// A fresh snapshot in `direction`.
    ///
    /// An unopenable sequencer means no endpoints, not a panic — the same shape
    /// CoreMIDI's failed-client path takes. Callers see an empty list and the
    /// reason lands in `debug!`.
    fn list(direction: Direction) -> Vec<EndpointInfo> {
        match SeqClient::open("tutti-enum", true) {
            Ok(seq) => enumerate::endpoints(&seq, direction),
            Err(e) => {
                tracing::debug!("ALSA enumeration: {e}");
                Vec::new()
            }
        }
    }
}

impl MidiEndpoints for AlsaEndpoints {
    fn inputs(&self) -> Vec<EndpointInfo> {
        Self::list(Direction::Input)
    }

    fn outputs(&self) -> Vec<EndpointInfo> {
        Self::list(Direction::Output)
    }

    fn open_input(
        &self,
        id: EndpointId,
        producer: InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        input::open(id, producer)
    }

    fn open_output(&self, id: EndpointId) -> Result<Box<dyn MidiOut>> {
        output::open(id)
    }
}
