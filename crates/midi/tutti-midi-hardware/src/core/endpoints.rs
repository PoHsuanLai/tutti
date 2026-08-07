//! The backend seam: enumerate and open this OS's MIDI endpoints.
//!
//! One implementation per platform — CoreMIDI, ALSA seq-UMP, and a stub where
//! neither exists. [`active`](super::backend::active) picks one, and that is the
//! **only** place in the crate a `#[cfg(target_os)]` decides anything.
//!
//! # Why this trait, and why it stops here
//!
//! Applying the repo's own test — *a trait earns its name if you can state its
//! boundary in one sentence without "and"* — this one is "enumerate and open
//! this OS's MIDI endpoints". The "and" is doing no work: enumeration exists to
//! produce the [`EndpointId`] that opening consumes, and no caller wants one
//! without the other. Splitting them would put an id's minter and its only
//! consumer in different traits.
//!
//! What is deliberately **not** here: a `UmpIn`/`UmpOut` pair mirroring
//! `AudioIn`/`AudioOut`. [`MidiEvent`](tutti_midi_types::ump::MidiEvent) already
//! *is* a UMP message — `data_words()` hands back the `&[u32]` a driver wants —
//! so a `UmpOut::send_ump` would restate `MidiOut::queue`'s boundary exactly.
//! Two names for one behaviour is not a type.
//!
//! Hence [`open_output`](MidiEndpoints::open_output) returns a
//! **`Box<dyn MidiOut>`**: a backend yields a value speaking the engine's
//! existing vocabulary, so nothing downstream needs to know which OS produced
//! it. That is what lets the output router be one field and a `match` instead of
//! a `#[cfg]` ladder.
//!
//! # Input does not return a trait object
//!
//! Asymmetric on purpose. Output is a sink the caller pushes to, so an erased
//! `MidiOut` is exactly right. Input is *pushed to us* by a driver callback on
//! its own thread, and the engine already has the destination for that: the
//! [`InputProducerHandle`] into a [`HardwareMidiInputs`] ring, which the audio
//! thread drains through `MidiIn`. A backend therefore takes the handle and
//! wires its callback to it, rather than handing back a source nobody would
//! poll.
//!
//! [`HardwareMidiInputs`]: crate::core::HardwareMidiInputs

use crate::core::capability::{EndpointId, EndpointInfo};
use crate::core::error::Result;
use crate::core::InputProducerHandle;
use tutti_midi_types::MidiOut;

/// An open input connection: a live subscription to one endpoint.
///
/// **Method-less by design.** Events flow through the [`InputProducerHandle`]
/// the backend was handed, never through this value; what it carries is a
/// *lifetime*. Dropping it closes the port, which is why
/// [`MidiEndpoints::open_input`] returns it rather than `()` and why a caller
/// must hold it.
///
/// It briefly declared `fn endpoint(&self) -> EndpointId` — directly under a doc
/// sentence saying it carried no methods. Nothing ever called it: the id is
/// already the key of the map these are stored in, so a connection is never
/// asked for its own address.
///
/// The name survives the method because `Box<dyn Send>` would say nothing about
/// what dropping the value does. This trait is where a backend author reads that
/// contract, which is a distinct thing to be even with no behaviour attached.
pub trait InputConnection: Send {}

/// This OS's MIDI endpoints.
pub trait MidiEndpoints: Send + Sync {
    /// Endpoints that can send us MIDI.
    ///
    /// A fresh snapshot per call — device lists go stale on hot-plug, and a
    /// cached one is a second owner of state the OS already holds.
    fn inputs(&self) -> Vec<EndpointInfo>;

    /// Endpoints we can send MIDI to.
    fn outputs(&self) -> Vec<EndpointInfo>;

    /// Open `id` for input, delivering events into `producer`.
    ///
    /// The returned connection must be **held**: dropping it closes the port.
    fn open_input(
        &self,
        id: EndpointId,
        producer: InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>>;

    /// Open `id` for output.
    ///
    /// The result speaks [`MidiOut`] — the same trait a per-unit mailbox
    /// implements — so callers route to a hardware wire and to a synth inbox
    /// through one vocabulary.
    fn open_output(&self, id: EndpointId) -> Result<Box<dyn MidiOut>>;
}
