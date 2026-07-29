//! The tap the audio callback pushes into must be reachable from the ECS.
//!
//! `build_into` created an `AudioTap`, handed a clone to the RT callback, and
//! dropped the local. Nothing else held a handle, so `open()` — the only way to
//! get the consumer end — was unreachable, and the callback pushed every block
//! into a ring nobody could ever read. `tutti-analysis`, the documented
//! consumer, had no route in at all.
//!
//! These drive the wrapper directly. The build-time publish itself needs a real
//! audio device (`TuttiPlugin { disabled: true }` skips `build_into` entirely),
//! so it is covered by the ignored test at the bottom rather than claimed here.

use bevy_app::App;
use bevy_tutti::graph::AudioTapRes;
use ringbuf::traits::Consumer as _;

/// A published tap starts closed: opening is the host's decision, not the
/// engine's, and while closed the audio thread pays one atomic load.
#[test]
fn a_published_tap_starts_closed() {
    let mut app = App::new();
    app.insert_resource(AudioTapRes::default());

    assert!(!app.world().resource::<AudioTapRes>().is_open());
}

/// The whole point of the seam: a host can get the consumer end, and it sees
/// what the callback pushed.
#[test]
fn opening_the_tap_yields_a_consumer_that_sees_the_pushed_frames() {
    let mut app = App::new();
    app.insert_resource(AudioTapRes::default());

    // The callback's half — the clone `build_into` hands to `AudioCallbackState`.
    let callback_side = app.world().resource::<AudioTapRes>().0.clone();
    let mut consumer = app.world().resource::<AudioTapRes>().open();
    assert!(app.world().resource::<AudioTapRes>().is_open());

    // Two interleaved stereo frames, as a block would arrive.
    callback_side.push(&[0.25, -0.5, 0.75, -1.0], 2);

    assert_eq!(consumer.try_pop(), Some((0.25, -0.5)));
    assert_eq!(consumer.try_pop(), Some((0.75, -1.0)));
    assert_eq!(consumer.try_pop(), None, "and nothing more than was pushed");
}

/// Closing stops the copy, so a host that finishes analysing gets its
/// atomic-load-only path back.
#[test]
fn closing_the_tap_stops_the_copy() {
    let mut app = App::new();
    app.insert_resource(AudioTapRes::default());

    let callback_side = app.world().resource::<AudioTapRes>().0.clone();
    let mut consumer = app.world().resource::<AudioTapRes>().open();
    app.world().resource::<AudioTapRes>().close();
    assert!(!app.world().resource::<AudioTapRes>().is_open());

    callback_side.push(&[0.25, -0.5], 1);

    assert_eq!(consumer.try_pop(), None, "a closed tap copies nothing");
}

/// A closed tap accepts pushes without panicking — the state the audio thread
/// is in for every block until a host opens it.
#[test]
fn pushing_into_a_closed_tap_is_a_no_op() {
    let tap = AudioTapRes::default();
    tap.0.push(&[0.1, 0.2, 0.3, 0.4], 2);
    assert!(!tap.is_open());
}

/// The engine publishes its tap, so a host can reach the one the callback holds
/// rather than a fresh disconnected one.
///
/// Ignored: `build_into` opens a real CPAL device. This is the assertion the
/// other tests in this file cannot make — they prove the wrapper's shape, not
/// that `build_into` inserts it — so it is recorded here rather than left
/// implicit, and run by hand on a machine with audio.
#[test]
#[ignore = "requires an audio device"]
fn the_engine_publishes_its_tap() {
    let mut app = App::new();
    app.add_plugins(bevy_tutti::TuttiPlugin::default());

    assert!(
        app.world().get_resource::<AudioTapRes>().is_some(),
        "build_into must publish the tap it hands the callback"
    );
}
