//! MIDI going *out* has a destination, and says so when it does not.
//!
//! The outbound path — clock-out, track MIDI-out, the clip tap — drained its
//! mailboxes into `MidiIo::send`, which queues into a channel whether or not an
//! output port is open and reports the failure at `debug!`. With no ECS way to
//! connect an output, that was the only reachable state: every event left its
//! ring and arrived nowhere, silently, forever.
//!
//! These cover the two halves of the fix — that a drop is now counted rather
//! than swallowed, and that an output can be selected at all.

#![cfg(feature = "midi-hardware")]

use bevy_tutti::midi::{MidiOutDrops, MidiOutRouter};
use tutti_midi_runtime::tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
use tutti_midi_types::ump::MidiEvent;

/// A router with no transport at all — the state an app is in before it selects
/// an output device.
fn router_with_no_output(drops: &MidiOutDrops) -> MidiOutRouter<'_> {
    MidiOutRouter {
        midi_io: None,
        #[cfg(target_os = "macos")]
        jr_out: None,
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
        midi_io: None,
        #[cfg(target_os = "macos")]
        jr_out: None,
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
