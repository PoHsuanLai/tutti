//! Flex Data metadata broadcast.
//!
//! One duty:
//!
//! - **Flex metadata** — [`BroadcastFlexMetadata`] is a fire-and-forget request
//!   to emit one of the Flex Data musical-metadata messages (chord name, key
//!   signature, or a text/metadata string like project or composition name) to
//!   external MIDI out, via [`MidiOutRes`](super::track_out::MidiOutRes). The app
//!   raises it whenever the corresponding project metadata changes.
//!
//!   Destination is the hardware-out mailbox, *not* the `MidiBus` synth fan-out:
//!   Flex metadata describes the session to downstream gear, and only the
//!   outbound mailbox is drained to the wire.
//!
//! The JR stamper and the native-UMP transport used to live here too. They are
//! not metadata — they are the wire — and now live in
//! [`hardware_out`](super::hardware_out).

use bevy_app::{App, Plugin, Update};
use bevy_ecs::message::{Message, MessageReader};
use bevy_ecs::prelude::*;

use tutti_midi_runtime::tutti_midi_types::ump::MidiEvent;
use tutti_midi_runtime::tutti_midi_types::ump::{ChordName, FlexTextKind, KeySharpsFlats, Tonic};

/// The group Flex Data metadata is broadcast on (function-block-wide).
const FLEX_GROUP: u8 = 0;

/// A Flex Data musical-metadata value to broadcast in band on the MIDI bus.
///
/// Covers the Flex Data messages that carry *structured* values — chord name,
/// key signature, and the variable-length UTF-8 text/metadata family. (Tempo,
/// time signature, and metronome are scalar and already have dedicated
/// `MidiBus::broadcast_*` one-liners.)
#[derive(Message, Debug, Clone)]
pub enum BroadcastFlexMetadata {
    /// Set Chord Name (M2-104 §7.5.10).
    ChordName(ChordName),
    /// Set Key Signature (M2-104 §7.5.9).
    KeySignature {
        tonic: Tonic,
        sharps_flats: KeySharpsFlats,
    },
    /// One of the ~19 text/metadata messages (project / composition / clip name,
    /// lyrics, copyright, composer/performer names, …).
    Text { kind: FlexTextKind, text: String },
}

/// Send each requested Flex metadata value to external MIDI out as one or more
/// UMP packets (text may span several 128-bit packets).
///
/// Destination is the hardware-out mailbox, not the synth fan-out bus: Flex
/// metadata describes the session to *downstream gear*, and only
/// [`MidiOutRes`](super::track_out::MidiOutRes) is drained to the wire.
pub fn flex_metadata_broadcast_system(
    out: Option<Res<super::track_out::MidiOutRes>>,
    mut requests: MessageReader<BroadcastFlexMetadata>,
) {
    let Some(out) = out else {
        requests.clear();
        return;
    };
    let sender = out.sender();
    let mut packets: Vec<MidiEvent> = Vec::new();
    for request in requests.read() {
        packets.clear();
        match request {
            BroadcastFlexMetadata::ChordName(chord) => {
                packets.push(MidiEvent::flex_set_chord_name(FLEX_GROUP, chord));
            }
            BroadcastFlexMetadata::KeySignature {
                tonic,
                sharps_flats,
            } => {
                packets.push(MidiEvent::flex_set_key_signature(
                    FLEX_GROUP,
                    *tonic,
                    *sharps_flats,
                ));
            }
            BroadcastFlexMetadata::Text { kind, text } => {
                tutti_midi_runtime::tutti_midi_types::ump::push_flex_text(
                    *kind,
                    text,
                    FLEX_GROUP,
                    &mut packets,
                );
            }
        }
        sender.queue(&packets);
    }
}

/// Wires Flex-metadata broadcast into the ECS.
///
/// Registers [`BroadcastFlexMetadata`] and its broadcast system. JR-out is
/// somebody else's: an app that wants it inserts
/// [`JrStamperRes`](super::hardware_out::JrStamperRes) and
/// [`UmpOutRes`](super::hardware_out::UmpOutRes), which the output pumps read.
pub struct MidiMetadataPlugin;

impl Plugin for MidiMetadataPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<BroadcastFlexMetadata>();
        app.add_systems(
            Update,
            flex_metadata_broadcast_system
                .run_if(crate::graph::engine_ready)
                // Fill the outbound mailbox before the pump drains it.
                .before(super::track_out::pump_midi_out_system),
        );
    }
}
