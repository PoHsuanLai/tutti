//! Arbitrary MIDI-out to external hardware — the track/UI outbound path.
//!
//! The [`MidiBus`](tutti_midi_runtime::MidiBus) fans MIDI *inward* to synths; it
//! has no tap for sending to external gear. This module adds the outbound
//! mailbox: a [`MidiEventSlot`](tutti_midi_runtime::MidiEventSlot) whose
//! [`MidiSender`] anyone off-RT (a track system, the UI) — or a clip source on
//! the audio thread — can push into lock-free, drained each frame and routed to
//! hardware through the *same* [`MidiOutRouter`](super::clock_out) the
//! clock-master pump uses. So track MIDI out gets the identical JR-stamp/UMP-vs-
//! MIDI-1 treatment (JR Timestamps reach the wire iff a native-UMP source +
//! enabled stamper are present).
//!
//! **One primitive, both roles.** This mailbox is exactly the pair of MIDI
//! traits: the [`MidiSender`] is the [`MidiOut`] push half (lock-free `&self`,
//! audio-thread-safe), the [`MidiReceiver`] is the [`MidiIn`] pull half the pump
//! drains. It replaced a separate `ringbuf`-backed output ring — there is now
//! one output-mailbox type across the clock master, the track path, and the RT
//! clip tap, and no mutex anywhere on the push side.
//!
//! Two ways to push:
//! - [`SendMidiOut`] — a fire-and-forget ECS message, for systems that already
//!   speak messages; [`midi_out_send_system`] forwards it into the mailbox.
//! - [`MidiOutRes::sender`] — a clone of the [`MidiSender`], for a hot producer
//!   (a clip source's `out_tap`) that pushes directly without the message
//!   round-trip. Because it's a `dyn MidiOut`, the RT clip tap needs nothing
//!   bespoke — it holds this sender and queues stamped events straight in.
//!
//! [`MidiOutRes`] is created and inserted by [`MidiOutPlugin`] (no engine
//! handoff needed — the mailbox lives entirely off/on the audio thread via the
//! traits), so the surface is always present once the plugin is added.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::message::{Message, MessageReader};
use bevy_ecs::prelude::*;

use tutti_midi_runtime::{MidiEventSlot, MidiReceiver, MidiSender};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::MidiUnitId;

use super::clock_out::{drain_receiver_through, MidiOutRouter};

/// The outbound MIDI-out mailbox: a push [`sender`](Self::sender) any caller
/// clones to send events to external hardware (off-RT or, as a clip `out_tap`,
/// on the audio thread), and the [`MidiReceiver`] the pump drains.
#[derive(Resource)]
pub struct MidiOutRes {
    sender: MidiSender,
    receiver: MidiReceiver,
}

impl Default for MidiOutRes {
    fn default() -> Self {
        let (sender, receiver) = MidiEventSlot::pair(MidiUnitId::next());
        Self { sender, receiver }
    }
}

impl MidiOutRes {
    /// A cheaply-clonable push handle onto the mailbox, for a caller that pushes
    /// directly — the UI, or a clip source's `out_tap` (`Arc<dyn MidiOut>`).
    pub fn sender(&self) -> MidiSender {
        self.sender.clone()
    }

    /// The [`MidiUnitId`] this mailbox routes on — a caller pushing through the
    /// [`MidiOut`] trait must address this id (a [`MidiSender`] clone already
    /// carries it).
    pub fn unit_id(&self) -> MidiUnitId {
        self.sender.unit_id()
    }
}

/// Fire-and-forget request: send these events to external MIDI hardware.
///
/// Routed like the clock master's output — JR-stamped to a native-UMP source if
/// one is present and stamping is enabled, else to the MIDI-1 port.
#[derive(Message, Debug, Clone)]
pub struct SendMidiOut(pub Vec<MidiEvent>);

/// Forward each [`SendMidiOut`] request into the outbound mailbox.
pub fn midi_out_send_system(out: Res<MidiOutRes>, mut requests: MessageReader<SendMidiOut>) {
    for SendMidiOut(events) in requests.read() {
        out.sender.queue(events);
    }
}

/// Per-frame: drain the track MIDI-out mailbox and route it to hardware, through
/// the same [`MidiOutRouter`] the clock-out pump uses.
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

    drain_receiver_through(&out.receiver, &mut router);
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
    use tutti_midi_types::MidiOut;

    /// Drain the resource's receiver fully into a `Vec`.
    fn drain(out: &MidiOutRes) -> Vec<MidiEvent> {
        let mut events = Vec::new();
        let mut buf = [MidiEvent::noop(); 16];
        loop {
            let n = out.receiver.poll_into(&mut buf);
            events.extend_from_slice(&buf[..n]);
            if n < buf.len() {
                break;
            }
        }
        events
    }

    #[test]
    fn send_system_forwards_into_mailbox() {
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

        let out = world.resource::<MidiOutRes>();
        let events = drain(out);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].note(), Some(60));
    }

    #[test]
    fn sender_pushes_directly() {
        let out = MidiOutRes::default();
        let sender = out.sender();
        assert_eq!(sender.queue(&[MidiEvent::note_on(0, 0, 64, 0x8000)]), 1);

        let events = drain(&out);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].note(), Some(64));
    }

    #[test]
    fn sender_reaches_mailbox_through_the_midi_out_trait() {
        // The clip tap holds this sender as `Arc<dyn MidiOut>` and must address
        // the resource's unit id — prove the trait path lands the event.
        let out = MidiOutRes::default();
        let tap: std::sync::Arc<dyn MidiOut> = std::sync::Arc::new(out.sender());
        tap.queue(out.unit_id(), &[MidiEvent::note_on(0, 0, 67, 0x8000)]);

        let events = drain(&out);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].note(), Some(67));
    }
}
