//! ECS wrapping for Flex Data metadata broadcast and JR-timestamp stamping.
//!
//! Two small, related duties over the runtime primitives:
//!
//! - **Flex metadata** — [`BroadcastFlexMetadata`] is a fire-and-forget request
//!   to emit one of the Flex Data musical-metadata messages (chord name, key
//!   signature, or a text/metadata string like project or composition name) in
//!   band on the [`MidiBusRes`](super::bus::MidiBusRes). The `MidiBus` already has
//!   `broadcast_tempo` / `broadcast_time_signature` / `broadcast_metronome`; this
//!   covers the *rest* of Flex Data (chord/key/text), which take structured
//!   values rather than scalars and so are cleaner as an ECS message than as bus
//!   one-liners. The app raises it whenever the corresponding project metadata
//!   changes.
//!
//! - **JR timestamps** — [`JrStamperRes`] carries the jitter-reduction stamping
//!   config (reference clock + group). The *stamping itself* belongs on the
//!   hardware-out pump (it prefixes each outbound event with a JR Timestamp as it
//!   leaves for the wire), which lives on the `midi-hardware` output path — that
//!   caller is deferred with the rest of the live-transport wiring. This resource
//!   is the ECS home for the config the pump will read; it's inserted default-off
//!   so nothing stamps until an app opts in.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::message::{Message, MessageReader};
use bevy_ecs::prelude::*;

use tutti_midi_runtime::tutti_midi_types::ump::{ChordName, FlexTextKind, KeySharpsFlats, Tonic};
use tutti_midi_runtime::tutti_midi_types::ump::MidiEvent;
use tutti_midi_runtime::JrStamper;

use super::bus::MidiBusRes;

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
    KeySignature { tonic: Tonic, sharps_flats: KeySharpsFlats },
    /// One of the ~19 text/metadata messages (project / composition / clip name,
    /// lyrics, copyright, composer/performer names, …).
    Text { kind: FlexTextKind, text: String },
}

/// Broadcast each requested Flex metadata value on the bus as one or more UMP
/// packets (text may span several 128-bit packets).
pub fn flex_metadata_broadcast_system(
    bus: Option<Res<MidiBusRes>>,
    mut requests: MessageReader<BroadcastFlexMetadata>,
) {
    let Some(bus) = bus else {
        requests.clear();
        return;
    };
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
                    *kind, text, FLEX_GROUP, &mut packets,
                );
            }
        }
        for packet in &packets {
            bus.queue_system(packet);
        }
    }
}

/// Jitter-reduction timestamp stamping config, homed in the ECS.
///
/// Holds the [`JrStamper`] the hardware-out pump reads to prefix each outbound
/// event with a JR Timestamp. Inserted disabled (`enabled = false`) so nothing
/// stamps until an app opts in; the pump that consumes it lives on the
/// `midi-hardware` output path and is wired in a later pass.
#[derive(Resource)]
pub struct JrStamperRes {
    /// The stamper (reference clock + group).
    pub stamper: JrStamper,
    /// Whether outbound stamping is active.
    pub enabled: bool,
}

impl JrStamperRes {
    /// A stamper for `sample_rate` on `group`, disabled until opted in.
    pub fn new(sample_rate: f64, group: u8) -> Self {
        Self {
            stamper: JrStamper::new(sample_rate, group),
            enabled: false,
        }
    }
}

/// Wires Flex-metadata broadcast (and the JR-stamper config home) into the ECS.
///
/// Registers [`BroadcastFlexMetadata`] and its broadcast system. It does **not**
/// insert [`JrStamperRes`] — the sample rate is engine-supplied, so the app (or a
/// later hardware-out pass) inserts it; the stamper has no consumer in this crate
/// yet.
pub struct MidiMetadataPlugin;

impl Plugin for MidiMetadataPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<BroadcastFlexMetadata>();
        app.add_systems(
            Update,
            flex_metadata_broadcast_system.run_if(tutti_core::graph::engine_ready),
        );
    }
}
