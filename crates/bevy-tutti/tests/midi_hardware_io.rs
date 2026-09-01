//! The OS MIDI edge: events leaving for a device, and events arriving from one.
//!
//! - `midi_out_device` — MIDI going *out* has a destination, and says so when
//!   it does not.
//! - `midi_hardware_frame_offset` — an inbound hardware event is timed against
//!   the device's real sample rate.
//!
//! The two directions of one boundary, and both need the same `midi-hardware`
//! feature — which is why they are one file rather than two.

#![cfg(feature = "midi-hardware")]

/// MIDI going *out* has a destination, and says so when it does not.
///
/// The outbound path — clock-out, track MIDI-out, the clip tap — drained its
/// mailboxes into `MidiIo::send`, which queues into a channel whether or not an
/// output port is open and reports the failure at `debug!`. With no ECS way to
/// connect an output, that was the only reachable state: every event left its
/// ring and arrived nowhere, silently, forever.
///
/// These cover the two halves of the fix — that a drop is now counted rather
/// than swallowed, and that an output can be selected at all.
/// (Was `tests/midi_out_device.rs`.)
mod midi_out_device {
    use bevy_tutti::midi::{MidiOutDrops, MidiOutRouter};
    use tutti_midi_types::ump::MidiEvent;
    use tutti_midi_types::{MidiChannel, MidiGroup};

    /// A router with no transport at all — the state an app is in before it selects
    /// an output device.
    fn router_with_no_output(drops: &MidiOutDrops) -> MidiOutRouter<'_> {
        MidiOutRouter {
            out: None,
            drops: Some(drops),
        }
    }

    /// Events with nowhere to go are counted, not silently swallowed.
    #[test]
    fn midi_out_drops_are_counted_when_no_device_is_connected() {
        let drops = MidiOutDrops::default();
        assert_eq!(drops.count(), 0, "nothing dropped yet");

        {
            let mut router = router_with_no_output(&drops);
            router.route(&[
                MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
                MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 64, 0x8000),
            ]);
        }

        assert_eq!(
            drops.count(),
            2,
            "both events went nowhere, and the tally says so"
        );
    }

    /// The count accumulates across calls, so a long run of silence is visible as
    /// one growing number rather than a burst that scrolls past.
    #[test]
    fn drops_accumulate_across_batches() {
        let drops = MidiOutDrops::default();
        for _ in 0..3 {
            let mut router = router_with_no_output(&drops);
            router.route(&[MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                60,
                0x8000,
            )]);
        }
        assert_eq!(drops.count(), 3);
    }

    /// An empty batch is not a drop. The pumps run every frame whether or not
    /// anything was queued, so counting empty drains would make the tally
    /// meaningless within seconds.
    #[test]
    fn an_empty_batch_is_not_a_drop() {
        let drops = MidiOutDrops::default();
        let mut router = router_with_no_output(&drops);
        router.route(&[]);
        assert_eq!(drops.count(), 0);
    }

    /// A caller that does not want the tally passes `None`, and nothing panics.
    #[test]
    fn dropping_without_a_counter_is_allowed() {
        let mut router = MidiOutRouter {
            out: None,
            drops: None,
        };
        router.route(&[MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            60,
            0x8000,
        )]);
    }

    /// The request messages exist and are registered, so a host can select an output.
    ///
    /// The connection itself needs real hardware, so what is asserted here is the
    /// ECS surface: the messages are registered types a host can send, and the
    /// system that services them runs without a device present rather than panicking
    /// on the missing `MidiIoRes`.
    #[test]
    fn an_output_connect_request_is_serviced_without_hardware() {
        use bevy_app::prelude::*;
        use bevy_tutti::midi::{ConnectMidiOutput, DisconnectMidiOutput, MidiDevicePlugin};

        let mut app = App::new();
        app.insert_resource(bevy_tutti::AudioEngineState::Running);
        app.add_plugins(MidiDevicePlugin);

        // No `MidiIoRes`: the engine may build without one, which is why the device
        // systems take it as `Option<Res<_>>`.
        app.world_mut().write_message(ConnectMidiOutput {
            name: "nonexistent device".into(),
        });
        app.world_mut().write_message(DisconnectMidiOutput);
        app.update();

        // Reaching here at all is the claim: the readers drained, nothing panicked,
        // and no event was announced for a connection that did not happen.
        assert_eq!(
            app.world()
                .resource::<bevy_ecs::message::Messages<bevy_tutti::midi::MidiDeviceEvent>>()
                .len(),
            0,
            "a request the driver cannot service must not announce a connection"
        );
    }
}

/// An inbound hardware event is timed against the device's real sample rate.
///
/// The port manager turns a wall-clock delta into a `frame_offset`
/// (`manager.rs`'s `read_inputs`), and that conversion needs the rate the
/// device is actually running at. `HardwareMidiInputs` is constructed before a
/// device exists, so it starts at a placeholder 44100 and documents
/// `set_sample_rate` as "call before starting the audio stream" — which nothing
/// did. At 48 kHz every inbound event landed ~8.8% early.
///
/// # What these cover, and what they do not
///
/// They prove `set_sample_rate` *works* — it was previously untested, being
/// called from nowhere. They do **not** prove `build_into` calls it: that
/// function opens a real CPAL device, so it cannot run headless, and the
/// timestamps here come from the wall clock rather than anything injectable
/// through the engine. The call site is covered by inspection.
/// (Was `tests/midi_hardware_frame_offset.rs`.)
mod midi_hardware_frame_offset {
    use std::time::{Duration, Instant};

    use tutti_midi_hardware::HardwareMidiInputs;
    use tutti_midi_types::ump::MidiEvent;
    use tutti_midi_types::{MidiChannel, MidiGroup};

    /// A block long enough that a 1 ms offset is a large, unambiguous fraction of
    /// it, and short enough to stay a plausible audio block.
    const NFRAMES: usize = 1024;

    /// How old the event is when the block starts.
    const AGE: Duration = Duration::from_millis(1);

    /// Push one event aged `AGE` into a fresh manager and read back its
    /// `frame_offset`.
    ///
    /// `push` takes the timestamp explicitly (unlike `push_input_event`, which
    /// stamps `Instant::now()`), which is what makes the age injectable at all.
    fn offset_at(sample_rate: Option<f64>) -> u32 {
        let inputs = HardwareMidiInputs::new(256);
        if let Some(rate) = sample_rate {
            inputs.set_sample_rate(rate);
        }
        let port = inputs.create_input_port("test in");
        let handle = inputs
            .get_input_producer_handle(port)
            .expect("the port we just created has a producer");

        assert!(
            handle.push(
                MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
                Instant::now() - AGE
            ),
            "the fifo has room for one event"
        );

        // The drain hands each event to a visitor rather than returning a slice, so
        // nothing borrows into the audio-thread-only scratch.
        let mut seen: Vec<MidiEvent> = Vec::new();
        inputs.cycle_start_read_all_inputs(NFRAMES, |_port, event| seen.push(event));
        assert_eq!(seen.len(), 1, "exactly the event we pushed");
        seen[0].frame_offset
    }

    /// The rate the device reports is the rate the offset is computed against.
    ///
    /// At 48 kHz, 1 ms is 48 samples, so the event belongs 48 frames before the end
    /// of the block: 1024 - 48 = 976. The assertion band deliberately **excludes
    /// 980**, the answer the stale 44100 default gives — a test that accepted both
    /// would pass with the bug in place.
    #[test]
    fn an_inbound_event_is_timed_against_the_device_sample_rate() {
        let offset = offset_at(Some(48_000.0));
        assert!(
            (970..=978).contains(&offset),
            "1 ms before a 1024-frame block at 48 kHz is ~976, got {offset} \
             (980 would mean the 44100 default is still in force)"
        );
    }

    /// The placeholder rate mistimes the same event, and this pins the exact wrong
    /// answer.
    ///
    /// Without this, someone changing `CycleScratch::new`'s default would silently
    /// widen the band the test above tolerates rather than failing anything. Here
    /// the wrong value is named, so the default cannot drift unnoticed.
    #[test]
    fn the_default_rate_mistimes_an_event() {
        let offset = offset_at(None);
        assert!(
            (976..=984).contains(&offset),
            "1 ms at the 44100 placeholder is ~980, got {offset}"
        );
    }
}
