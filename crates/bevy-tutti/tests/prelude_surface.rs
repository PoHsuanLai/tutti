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
