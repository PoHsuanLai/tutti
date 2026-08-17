//! Flex Data metadata broadcast.
//!
//! One duty:
//!
//! - **Flex metadata** — [`BroadcastFlexMetadata`] is a fire-and-forget request
//!   to emit one of the Flex Data musical-metadata messages (chord name, key
//!   signature, tempo, time signature, metronome, or a text/metadata string like
//!   project or composition name) to external MIDI out, via
//!   [`MidiOutRes`](super::track_out::MidiOutRes). The app raises it whenever the
//!   corresponding project metadata changes.
//!
//!   Destination is the hardware-out mailbox, *not* the `MidiBus` synth fan-out:
//!   Flex metadata describes the session to downstream gear, and only the
//!   outbound mailbox is drained to the wire.
//!
//! The JR stamper and the native-UMP transport are not metadata — they are the
//! wire — and live in [`hardware_out`](super::hardware_out).

use bevy_app::{App, Plugin, Update};
use bevy_ecs::message::{Message, MessageReader};
use bevy_ecs::prelude::*;

use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::ump::{BarAccents, ChordName, FlexTextKind, KeySharpsFlats, Tonic};
use tutti_midi_types::MidiGroup;

/// The group Flex Data metadata is broadcast on (function-block-wide).
const FLEX_GROUP: MidiGroup = MidiGroup::FIRST;

/// A Flex Data musical-metadata value to broadcast in band on the MIDI bus.
///
/// Covers chord name, key signature, tempo, time signature, metronome, and the
/// variable-length UTF-8 text/metadata family.
///
/// # Describing state, not owning it
///
/// [`Tempo`](Self::Tempo) and [`TimeSignature`](Self::TimeSignature) name values
/// the [`Transport`](tutti_core::transport::Transport) also holds, which reads
/// like a second writer and is not one: every arm of
/// [`flex_metadata_broadcast_system`] builds a packet and queues it into the
/// hardware-out mailbox. There is no path from here back into the transport, so
/// this describes the session to downstream gear and cannot change it.
///
/// What that does leave to the caller is *when* to raise it. Chord name has no
/// engine-side owner, so an app is its only possible source; tempo has one, so
/// an app that broadcasts a figure the transport disagrees with has told the
/// wire a lie. Raise these where the transport's tempo is set, not from a
/// separate store.
#[derive(Message, Debug, Clone)]
pub enum BroadcastFlexMetadata {
    /// Set Chord Name (M2-104 §7.5.10).
    ChordName(ChordName),
    /// Set Key Signature (M2-104 §7.5.9).
    KeySignature {
        /// The tonic (root pitch class) of the key.
        tonic: Tonic,
        /// The accidental count as `Sharps(n)` / `Flats(n)` / `NonStandard` —
        /// the key-signature flavour, not the chord-name one.
        sharps_flats: KeySharpsFlats,
    },
    /// Set Tempo (M2-104 §7.5.7), in beats per minute.
    ///
    /// A non-positive rate encodes upstream as the "no valid tempo" sentinel
    /// rather than being rejected here.
    Tempo(f64),
    /// Set Time Signature (M2-104 §7.5.8).
    TimeSignature {
        /// Beats per bar.
        numerator: u8,
        /// The beat unit (4 = quarter, 8 = eighth, …).
        denominator: u8,
        /// 1/32 notes per quarter note — 8 in the ordinary case.
        num_32nd_notes: u8,
    },
    /// Set Metronome (M2-104 §7.5.8).
    Metronome {
        /// MIDI clocks per primary click.
        clocks_per_click: u8,
        /// Which bar subdivisions are accented.
        accents: BarAccents,
    },
    /// One of the ~19 text/metadata messages (project / composition / clip name,
    /// lyrics, copyright, composer/performer names, …).
    Text {
        /// Which of the text messages this is — the kind is the only thing
        /// distinguishing a lyric from a copyright notice on the wire.
        kind: FlexTextKind,
        /// UTF-8, any length: the encoder splits it across as many 128-bit
        /// packets as it needs.
        text: String,
    },
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
            BroadcastFlexMetadata::Tempo(bpm) => {
                packets.push(MidiEvent::flex_set_tempo(FLEX_GROUP, *bpm));
            }
            BroadcastFlexMetadata::TimeSignature {
                numerator,
                denominator,
                num_32nd_notes,
            } => {
                packets.push(MidiEvent::flex_set_time_signature(
                    FLEX_GROUP,
                    *numerator,
                    *denominator,
                    *num_32nd_notes,
                ));
            }
            BroadcastFlexMetadata::Metronome {
                clocks_per_click,
                accents,
            } => {
                packets.push(MidiEvent::flex_set_metronome(
                    FLEX_GROUP,
                    *clocks_per_click,
                    *accents,
                ));
            }
            BroadcastFlexMetadata::Text { kind, text } => {
                tutti_midi_types::ump::push_flex_text(*kind, text, FLEX_GROUP, &mut packets);
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
