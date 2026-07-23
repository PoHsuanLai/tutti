//! Arbitrary MIDI-out to external hardware — the track/UI outbound path.
//!
//! The [`MidiBus`](tutti_midi_runtime::MidiBus) fans MIDI *inward* to synths; it
//! has no tap for sending to external gear. This module adds the outbound ring:
//! a shareable [`MidiOutHandle`](tutti_midi_runtime::MidiOutHandle) anyone off-RT
//! (a track system, a clip player, the UI) can push into, drained each frame and
//! routed to hardware through the *same* [`MidiOutRouter`](super::clock_out) the
//! clock-master pump uses — so track MIDI out gets the identical JR-stamp/UMP-vs-
//! MIDI-1 treatment (JR Timestamps reach the wire iff a native-UMP source +
//! enabled stamper are present).
//!
//! Two ways to push:
//! - [`SendMidiOut`] — a fire-and-forget ECS message, for systems that already
//!   speak messages; [`midi_out_send_system`] forwards it into the ring.
//! - [`MidiOutRes::handle`] — the raw push handle, for a hot producer (a clip
//!   player) that wants to push directly without the message round-trip.
//!
//! [`MidiOutRes`] is created and inserted by [`MidiOutPlugin`] (no engine
//! handoff needed — the ring is off-RT only), so the surface is always present
//! once the plugin is added.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::message::{Message, MessageReader};
use bevy_ecs::prelude::*;
use parking_lot::Mutex;

use tutti_midi_runtime::{shared_midi_output_channel, MidiOutHandle, MidiOutputConsumer};
use tutti_midi_types::ump::MidiEvent;

use super::clock_out::{drain_ring_through, MidiOutRouter};

/// Capacity of the track MIDI-out ring — several frames of dense output.
const MIDI_OUT_CAPACITY: usize = 1024;

/// The outbound MIDI-out ring: a push [`handle`](Self::handle) any off-RT caller
/// clones to send events to external hardware, and the consumer the pump drains.
///
/// The consumer is behind a `Mutex` because the ring's consumer half is `!Sync`
/// and a Bevy `Resource` must be `Sync`; only the single per-frame pump locks it,
/// so it's uncontended.
#[derive(Resource)]
pub struct MidiOutRes {
    handle: MidiOutHandle,
    consumer: Mutex<MidiOutputConsumer>,
}

impl Default for MidiOutRes {
    fn default() -> Self {
        let (handle, consumer) = shared_midi_output_channel(MIDI_OUT_CAPACITY);
        Self {
            handle,
            consumer: Mutex::new(consumer),
        }
    }
}

impl MidiOutRes {
    /// A cheaply-clonable push handle onto the ring, for a caller that pushes
    /// directly (a clip player, the UI) rather than via [`SendMidiOut`].
    pub fn handle(&self) -> MidiOutHandle {
        self.handle.clone()
    }
}

/// Fire-and-forget request: send these events to external MIDI hardware.
///
/// Routed like the clock master's output — JR-stamped to a native-UMP source if
/// one is present and stamping is enabled, else to the MIDI-1 port.
#[derive(Message, Debug, Clone)]
pub struct SendMidiOut(pub Vec<MidiEvent>);

/// Forward each [`SendMidiOut`] request into the outbound ring.
pub fn midi_out_send_system(
    out: Res<MidiOutRes>,
    mut requests: MessageReader<SendMidiOut>,
) {
    for SendMidiOut(events) in requests.read() {
        out.handle.push_slice(events);
    }
}

/// Per-frame: drain the track MIDI-out ring and route it to hardware, through the
/// same [`MidiOutRouter`] the clock-out pump uses.
pub fn pump_midi_out_system(
    out: Option<Res<MidiOutRes>>,
    #[cfg(feature = "midi-hardware")] midi_io: Option<Res<super::device::MidiIoRes>>,
    #[cfg(all(target_os = "macos", feature = "midi-hardware"))] ump_out: Option<
        ResMut<super::metadata::UmpOutRes>,
    >,
    #[cfg(all(target_os = "macos", feature = "midi-hardware"))] jr: Option<
        Res<super::metadata::JrStamperRes>,
    >,
) {
    let Some(out) = out else {
        return;
    };

    let mut router = MidiOutRouter {
        #[cfg(feature = "midi-hardware")]
        midi_io: midi_io.as_deref(),
        #[cfg(all(target_os = "macos", feature = "midi-hardware"))]
        jr_out: super::clock_out::jr_out_active(ump_out, jr),
        #[cfg(not(feature = "midi-hardware"))]
        _marker: std::marker::PhantomData,
    };

    let mut consumer = out.consumer.lock();
    drain_ring_through(&mut consumer, &mut router);
}

/// Wires the track MIDI-out path: inserts [`MidiOutRes`], registers
/// [`SendMidiOut`], and schedules the send + pump systems.
///
/// The pump runs *after* [`pump_clock_out_system`](super::clock_out::pump_clock_out_system)
/// so a shared native-UMP source's per-frame stamp origin advances clock-first
/// then track — a deterministic order, not a race.
pub struct MidiOutPlugin;

impl Plugin for MidiOutPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MidiOutRes>();
        app.add_message::<SendMidiOut>();
        app.add_systems(
            Update,
            (midi_out_send_system, pump_midi_out_system)
                .chain()
                .after(super::clock_out::pump_clock_out_system),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_system_forwards_into_ring() {
        let mut world = World::new();
        world.init_resource::<MidiOutRes>();
        world.init_resource::<Messages<SendMidiOut>>();

        let note = MidiEvent::note_on(0, 0, 60, 0x8000);
        world
            .resource_mut::<Messages<SendMidiOut>>()
            .write(SendMidiOut(vec![note]));

        let mut schedule = Schedule::default();
        schedule.add_systems(midi_out_send_system);
        schedule.run(&mut world);

        // The event landed in the ring: drain it out and check.
        let out = world.resource::<MidiOutRes>();
        let mut buf = [MidiEvent::noop(); 4];
        let n = out.consumer.lock().drain_into(&mut buf);
        assert_eq!(n, 1);
        assert_eq!(buf[0].note(), Some(60));
    }

    #[test]
    fn handle_pushes_directly() {
        let out = MidiOutRes::default();
        let handle = out.handle();
        assert!(handle.push(MidiEvent::note_on(0, 0, 64, 0x8000)));

        let mut buf = [MidiEvent::noop(); 4];
        let n = out.consumer.lock().drain_into(&mut buf);
        assert_eq!(n, 1);
        assert_eq!(buf[0].note(), Some(64));
    }
}
