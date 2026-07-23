//! ECS wrapping for Flex Data metadata broadcast and JR-timestamp stamping.
//!
//! Two small, related duties over the runtime primitives:
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

use tutti_midi_runtime::tutti_midi_types::ump::MidiEvent;
use tutti_midi_runtime::tutti_midi_types::ump::{ChordName, FlexTextKind, KeySharpsFlats, Tonic};
use tutti_midi_runtime::JrStamper;

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

/// Jitter-reduction timestamp stamping config, homed in the ECS.
///
/// Holds the [`JrStamper`] the hardware-out pump reads to prefix each outbound
/// event with a JR Timestamp. Inserted disabled (`enabled = false`) so nothing
/// stamps until an app opts in; the pump reads it on the native-UMP output path
/// (see [`UmpOutRes`]) — the one transport where JR Timestamps reach the wire.
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

/// A native-UMP MIDI output source (macOS), the JR-out transport.
///
/// Wraps a [`UmpVirtualSource`](crate::UmpVirtualSource) — a MIDI-2.0-protocol
/// endpoint that carries UMP words to the wire, unlike the MIDI-1.0 `MidiIoRes`
/// port that drops JR Timestamps. The clock-out pump routes here (JR-stamped)
/// when a [`JrStamperRes`] is enabled. Not inserted by default — an app that
/// wants JR-out creates the source and inserts this resource.
#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
#[derive(Resource, Debug)]
pub struct UmpOutRes {
    source: crate::UmpVirtualSource,
    /// Running absolute sample position of the next block's frame-offset zero,
    /// so JR stamps stay monotonic across pump frames.
    origin_samples: u64,
}

#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
impl UmpOutRes {
    /// Wrap a native-UMP source as the JR-out target.
    pub fn new(source: crate::UmpVirtualSource) -> Self {
        Self {
            source,
            origin_samples: 0,
        }
    }

    /// JR-stamp `events` at the running origin and send each resulting UMP
    /// message (the timestamp prefixes + the events) to the source. Advances the
    /// origin past this block's largest frame offset so the next block's stamps
    /// continue monotonically.
    pub fn send_stamped(&mut self, events: &[MidiEvent], stamper: &JrStamper) {
        let stamped = stamper.stamp_block(events, self.origin_samples);
        for ev in &stamped {
            if let Err(e) = self.source.send_ump(ev.data_words()) {
                tracing::debug!("JR-out UMP send: {e}");
            }
        }
        // Advance the origin past the furthest frame offset seen this block.
        let span = events
            .iter()
            .map(|e| e.frame_offset as u64)
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);
        self.origin_samples = self.origin_samples.wrapping_add(span);
    }
}

/// Wires Flex-metadata broadcast (and the JR-stamper config home) into the ECS.
///
/// Registers [`BroadcastFlexMetadata`] and its broadcast system. It does **not**
/// insert [`JrStamperRes`] or [`UmpOutRes`] — the sample rate and the UMP source
/// are engine/app-supplied, so an app that wants JR-out inserts both; the
/// clock-out pump ([`super::clock_out::pump_clock_out_system`]) consumes them.
pub struct MidiMetadataPlugin;

impl Plugin for MidiMetadataPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<BroadcastFlexMetadata>();
        app.add_systems(
            Update,
            flex_metadata_broadcast_system
                .run_if(tutti_core::ecs::engine_ready)
                // Fill the outbound mailbox before the pump drains it.
                .before(super::track_out::pump_midi_out_system),
        );
    }
}

#[cfg(all(test, target_os = "macos", feature = "midi-hardware"))]
mod tests {
    use super::*;

    #[test]
    fn send_stamped_advances_origin_and_sends() {
        let source = crate::UmpVirtualSource::new("Test JR-Out").expect("creates ump source");
        let mut out = UmpOutRes::new(source);
        let stamper = JrStamper::new(48_000.0, 0);

        // Two events, the later at a non-zero frame offset.
        let events = [
            MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(0),
            MidiEvent::note_off(0, 0, 60, 0).with_frame_offset(24_000),
        ];
        out.send_stamped(&events, &stamper);
        // Origin advanced past the furthest frame offset (+1).
        assert_eq!(out.origin_samples, 24_001);

        // A second block continues monotonically from the new origin.
        out.send_stamped(&events, &stamper);
        assert_eq!(out.origin_samples, 48_002);
    }
}
