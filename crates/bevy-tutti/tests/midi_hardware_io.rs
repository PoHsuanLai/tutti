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
/// (`manager.rs`'s `read_inputs`), and that conversion needs the rate the device
/// is actually running at. `HardwareMidiInputs` is constructed before a device
/// exists, so it starts at a placeholder 44100 and documents `set_sample_rate`
/// as "call before starting the audio stream" — which nothing did. At 48 kHz
/// every inbound event landed ~8.8% early.
///
/// # The block start is supplied, so these are equalities
///
/// `cycle_start_read_all_inputs_at` takes the block's start instant instead of
/// reading it from the clock, which removes the only term a test could not pin.
/// The earlier version of this file asserted `(970..=978).contains(&offset)` —
/// a band sized to absorb however long the scheduler took between pushing an
/// event and draining it. Now the answer is arithmetic and the assertions say
/// so.
///
/// # What these cover, and what they do not
///
/// They prove `set_sample_rate` *works*. They do **not** prove `build_into`
/// calls it: that function opens a real CPAL device, so it cannot run headless.
/// The call site is covered by inspection.
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

    /// Push one event aged [`AGE`] into a fresh manager and read back its
    /// `frame_offset`.
    ///
    /// Both instants are the test's: `push` takes the arrival stamp explicitly,
    /// and `cycle_start_read_all_inputs_at` takes the block start. So the age is
    /// exactly [`AGE`] and nothing in between can change it.
    fn offset_at(sample_rate: Option<f64>) -> u32 {
        let inputs = HardwareMidiInputs::new(256);
        if let Some(rate) = sample_rate {
            inputs.set_sample_rate(rate);
        }
        let port = inputs.create_input_port("test in");
        let handle = inputs
            .get_input_producer_handle(port)
            .expect("the port we just created has a producer");

        let block_start = Instant::now();
        assert!(
            handle.push(
                MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
                block_start - AGE
            ),
            "the fifo has room for one event"
        );

        // The drain hands each event to a visitor rather than returning a slice, so
        // nothing borrows into the audio-thread-only scratch.
        let mut seen: Vec<MidiEvent> = Vec::new();
        inputs
            .cycle_start_read_all_inputs_at(NFRAMES, block_start, |_port, event| seen.push(event));
        assert_eq!(seen.len(), 1, "exactly the event we pushed");
        seen[0].frame_offset
    }

    /// The rate the device reports is the rate the offset is computed against.
    ///
    /// 1 ms at 48 kHz is 48 samples, so the event belongs 48 frames before the
    /// end of the block: 1024 - 48 = 976, exactly.
    #[test]
    fn an_inbound_event_is_timed_against_the_device_sample_rate() {
        assert_eq!(
            offset_at(Some(48_000.0)),
            976,
            "1 ms before a 1024-frame block at 48 kHz is frame 976"
        );
    }

    /// The placeholder rate mistimes the same event, and this pins the exact
    /// wrong answer.
    ///
    /// 1 ms at 44100 is 44.1 samples, truncated to 44: frame 980. Without this,
    /// someone changing `CycleScratch::new`'s default would move what the test
    /// above is compared against rather than failing anything.
    #[test]
    fn the_default_rate_mistimes_an_event() {
        assert_eq!(
            offset_at(None),
            980,
            "1 ms at the 44100 placeholder is frame 980 -- 4 frames late"
        );
    }
}

/// The session layer is drivable with no MIDI hardware present.
///
/// `MidiSession::with_backend` takes a `MidiEndpoints`, and
/// `tutti_midi_hardware::test_support::FakeBackend` is one with a fixed device
/// list and no OS behind it. Before it was exported, every path through this
/// layer was reachable from `bevy-tutti` only against a real port — which a
/// headless CI box does not have, so these paths had no coverage here at all and
/// the alternative was a test asserting "no device found".
mod midi_session_without_hardware {
    use std::sync::Arc;

    use tutti_midi_hardware::test_support::FakeBackend;
    use tutti_midi_hardware::{EndpointId, HardwareMidiInputs, MidiSession};
    use tutti_midi_types::ump::MidiEvent;
    use tutti_midi_types::{MidiChannel, MidiGroup};

    fn session() -> (MidiSession, tutti_midi_hardware::test_support::FakeCounters) {
        let (backend, counters) = FakeBackend::build();
        let ports = Arc::new(HardwareMidiInputs::new(64));
        (MidiSession::with_backend(backend, ports), counters)
    }

    /// A session enumerates, connects and sends, all without a device.
    ///
    /// One test over the whole round trip rather than four: what is being
    /// asserted here is that the *seam* carries a `bevy-tutti` consumer through
    /// it, and the individual behaviours are already covered inside
    /// `tutti-midi-hardware`. Duplicating those assertions would mean two places
    /// to update when the session's contract moves.
    #[test]
    fn a_faked_backend_carries_a_session_end_to_end() {
        let (s, counters) = session();

        assert_eq!(s.inputs().len(), 2, "the fake list is enumerated as given");
        assert_eq!(s.outputs().len(), 2);

        assert!(!s.is_any_input_connected());
        s.connect_input(EndpointId::from_raw(1))
            .expect("the fake list contains input 1");
        assert!(s.is_any_input_connected());
        assert_eq!(
            counters.opens(),
            1,
            "connecting opens the port exactly once"
        );

        s.connect_output(EndpointId::from_raw(10))
            .expect("the fake list contains output 10");
        let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000);
        assert_eq!(
            s.send(&[note, note]),
            2,
            "a connected output accepts what it is handed"
        );
        assert_eq!(counters.sent(), 2, "and the sink saw both events");
    }

    /// A device that is not in the list is an error, not a silent no-op.
    ///
    /// The failure this guards is a session that reports itself connected to
    /// something it never opened — every later send then vanishes with no error
    /// anywhere, which is indistinguishable from a quiet device.
    #[test]
    fn connecting_an_absent_device_reports_it() {
        let (s, counters) = session();
        assert!(s.connect_input(EndpointId::from_raw(999)).is_err());
        assert!(!s.is_any_input_connected());
        assert_eq!(counters.opens(), 0, "a refused connect opens nothing");
    }
}
