//! Every type this crate's docs demonstrate must be nameable through this crate.
//!
//! `MetronomeRes::set_mode` takes a `MetronomeMode`; `transport.motion.try_send`
//! takes a `MotionEvent`. Both arrive through a `Deref` to a tutti-core type, so
//! before these were re-exported a host had to add a direct `tutti-core`
//! dependency to spell an argument this crate's own examples pass. Handing out a
//! method whose parameter type the caller cannot name is an incomplete forward.

use bevy_tutti::prelude::*;

/// The metronome's mode enum, reachable without naming tutti-core.
#[test]
fn the_metronome_mode_a_host_must_pass_is_nameable_from_the_prelude() {
    let mode: MetronomeMode = MetronomeMode::Always;
    assert_ne!(mode, MetronomeMode::Off);
}

/// The transport command enum, likewise.
#[test]
fn the_motion_event_a_host_must_send_is_nameable_from_the_prelude() {
    // `Locate` carries the richest payload of the variants, so it is the one
    // that proves the whole enum came across rather than a stub.
    let event = MotionEvent::locate(4.0);
    assert_ne!(event, MotionEvent::Play);
}

/// The beat-port convention a host needs to wire anything to the transport clock.
///
/// The clock emits whole beats on port 0 and the fraction on port 1;
/// `beat_from_ports` is the inverse. A host reading those ports needs both names,
/// and the constant is what says how many there are.
#[test]
fn the_clocks_beat_port_convention_is_nameable_from_the_prelude() {
    assert_eq!(BEAT_PORTS, 2);
    assert_eq!(beat_from_ports(4.0, 0.25), 4.25);
}

/// `MotionEvent`'s own payload types, and what `motion()` gives back.
///
/// Re-exporting `MotionEvent` alone left a host able to call the convenience
/// constructors but unable to write `MotionEvent::Stop { fade }` or to name the
/// state it read — the incomplete forward one level down.
#[test]
fn a_motion_events_payload_types_are_nameable_from_the_prelude() {
    let immediate = MotionEvent::Stop {
        fade: FadeOut::Immediate,
    };
    assert_ne!(immediate, MotionEvent::stop());

    let locate = MotionEvent::Locate {
        beat: 8.0,
        fade: FadeOut::Declick,
        then: Then::Roll,
    };
    assert_ne!(locate, MotionEvent::locate(8.0));

    let state: MotionState = MotionState::Stopped;
    assert_ne!(state, MotionState::Rolling);
}

/// `TransportRes::timeline()` hands over a live handle, not a snapshot.
///
/// This is the property the whole per-frame/per-block seam rests on: a
/// beat-scheduled source is given the timeline once, at install time, and reads
/// the beat itself every block. If the clone were a snapshot, every source would
/// be frozen at the frame it was installed on.
#[test]
fn the_timeline_handle_tracks_the_live_transport() {
    use bevy_tutti::graph::TransportRes;
    use tutti_core::transport::Transport;

    let res = TransportRes(Transport::new(48_000.0));
    let timeline = res.timeline();

    res.settings.set_beat(4.0);
    assert_eq!(timeline.beat().get(), 4.0);

    // Move it again through the resource; the handed-out handle follows.
    res.settings.set_beat(12.5);
    assert_eq!(
        timeline.beat().get(),
        12.5,
        "the handle shares state — a snapshot would still read 4.0"
    );

    res.settings.set_tempo(140.0);
    assert_eq!(timeline.tempo().get(), 140.0);
}

/// The loop region a host arms, and the validated form it reads back.
///
/// `set_range` stores raw bounds — an inverted pair is a legitimate transient
/// while a user drags a brace — and `range()` is where validation happens,
/// returning `None` for disabled, empty or inverted. Both types have to be
/// nameable for that round trip to be writable.
#[test]
fn the_loop_region_round_trips_through_the_prelude() {
    let span = LoopSpan::new(4.0, 8.0);
    span.set_enabled(true);

    let armed: Option<LoopRange> = span.range();
    let armed = armed.expect("4..8 is a usable region");
    assert_eq!(armed.start().get(), 4.0);
    assert_eq!(armed.end().get(), 8.0);

    // Inverted: stored happily, reported as no region.
    span.set_range(8.0, 4.0);
    assert_eq!(span.bounds(), (8.0, 4.0), "raw bounds are what a drag shows");
    assert!(span.range().is_none(), "but it is not a loop");
}
