//! The outbound wire's switch: whether drained MIDI is JR-stamped.
//!
//! The wire itself is the session's (design 015 PR 5): the output a host
//! connects ([`UmpOut`](tutti_session::UmpOut), through
//! [`AudioGraphRes::midi`](crate::graph::AudioGraphRes::midi)'s
//! `set_hardware_out`), the drop count, and the one router both producers
//! drain through, the engine's hardware out (the clock and whatever is wired
//! to its node, [`clock_out`](super::clock_out)) and the track MIDI-out
//! mailbox ([`track_out`](super::track_out)). So the stamping decision lives
//! in exactly one place, and both producers advance one stamp origin.
//!
//! **JR-out:** with an output connected and [`JrStamperRes`] enabled, each
//! event is JR-stamped (a JR Timestamp prefix derived from its
//! `frame_offset`) and sent as UMP words. With the stamper disabled the
//! events still go out, unstamped: `JrStamperRes` gates timestamps, not
//! delivery. The pumps mirror it into the session each frame.

use bevy_ecs::prelude::*;

use crate::graph::AudioGraphRes;

/// Jitter-reduction stamping config.
///
/// Inserted disabled so nothing stamps until an app opts in.
///
/// It gates **stamping, not sending**: with it off, the connected output
/// still delivers, just without timestamps.
///
/// It holds only the *switch*: the stamper and its running origin belong to
/// the stream being stamped, which is the session's output's.
#[derive(Resource, Debug)]
pub struct JrStamperRes {
    /// Whether outbound stamping is active.
    pub enabled: bool,
}

impl JrStamperRes {
    /// Creates the switch with stamping off, the default.
    pub fn disabled() -> Self {
        Self { enabled: false }
    }

    /// Creates the switch with stamping on.
    pub fn enabled() -> Self {
        Self { enabled: true }
    }
}

impl Default for JrStamperRes {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Mirrors [`JrStamperRes`] onto the session's output (absent: off).
pub(crate) fn mirror_stamping(graph: &mut AudioGraphRes, jr: Option<&JrStamperRes>) {
    graph.midi().set_jr_stamping(jr.is_some_and(|j| j.enabled));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::midi::{MidiOutRes, UmpOut};
    use bevy_ecs::system::RunSystemOnce;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tutti_midi_types::ump::MidiEvent;
    use tutti_midi_types::{MidiChannel, MidiGroup};

    /// Counts what reached the wire.
    struct CountingSink(Arc<AtomicUsize>);
    impl tutti_midi_types::MidiOut for CountingSink {
        fn queue(&self, events: &[MidiEvent]) -> usize {
            self.0.fetch_add(events.len(), Ordering::SeqCst);
            events.len()
        }
    }

    /// Queues one note on the track mailbox and runs its pump: what reached
    /// the wire.
    fn pump_one(world: &mut World, count: &AtomicUsize) -> usize {
        let before = count.load(Ordering::SeqCst);
        world
            .resource::<MidiOutRes>()
            .sender()
            .queue(&[MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                60,
                0x8000,
            )]);
        world
            .run_system_once(super::super::track_out::pump_midi_out_system)
            .expect("the pump runs");
        count.load(Ordering::SeqCst) - before
    }

    /// **[`JrStamperRes`] reaches the session's output, and gates stamping,
    /// not delivery** (the adapter half of the session's stamping tests):
    /// disabled, the note goes out alone; enabled, with its JR Timestamp.
    ///
    /// Mutations (run), each fails: `mirror_stamping` writing `true` (the
    /// disabled note carries a stamp); writing `false` (the enabled note
    /// carries none); the pump draining without the graph's output (nothing
    /// reaches the wire).
    #[test]
    fn the_stamper_switch_reaches_the_sessions_output() {
        let mut world = World::new();
        let count = Arc::new(AtomicUsize::new(0));
        let mut graph = AudioGraphRes::headless(0, 1);
        graph.midi().set_hardware_out(Some(UmpOut::new(
            Box::new(CountingSink(count.clone())),
            48_000.0,
        )));
        world.insert_resource(graph);
        world.init_resource::<MidiOutRes>();

        world.insert_resource(JrStamperRes::disabled());
        assert_eq!(
            pump_one(&mut world, &count),
            1,
            "a disabled stamper still delivers, unstamped"
        );

        world.insert_resource(JrStamperRes::enabled());
        let sent = pump_one(&mut world, &count);
        assert!(sent > 1, "an enabled stamper adds the stamp, sent {sent}");
    }
}
