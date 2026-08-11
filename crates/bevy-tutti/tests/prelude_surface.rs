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
    let event = MotionEvent::locate(Beat(4.0));
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
    assert_eq!(beat_from_ports(4.0, 0.25), Beat(4.25));
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
        beat: Beat(8.0),
        fade: FadeOut::Declick,
        then: Then::Roll,
    };
    assert_ne!(locate, MotionEvent::locate(Beat(8.0)));

    let state: MotionState = MotionState::Stopped;
    assert_ne!(state, MotionState::Rolling);
}

/// Every type the prelude's own signatures are spelled in is nameable from it.
///
/// This is the incomplete-forward rule applied to itself, and it is a *compile*
/// test — the bodies do nothing, the point is that the annotations resolve. A
/// previous pass added `TransportRes::timeline()` returning `Arc<dyn Timeline>`
/// without re-exporting `Timeline`, so a host could call the method but not
/// write a function whose signature mentions it. `AudioUnit` was worse: without
/// it a host cannot write a generic spawn helper at all.
#[test]
fn the_types_the_prelude_is_spelled_in_are_nameable_from_it() {
    use bevy_ecs::prelude::Commands;
    use std::sync::Arc;

    // Return of `TransportRes::timeline()`.
    fn _timeline(t: &TransportRes) -> Arc<dyn Timeline> {
        t.timeline()
    }
    // Bound on `spawn_audio_node`, and the boxed param of `crossfade_audio_node`.
    fn _spawn<U: AudioUnit + 'static>(commands: &mut Commands, unit: U) {
        commands.spawn_audio_node(unit);
    }
    fn _crossfade(commands: &mut Commands, e: bevy_ecs::entity::Entity, u: Box<dyn AudioUnit>) {
        crossfade_audio_node(commands, e, u);
    }
    // Deref targets: nameable in a signature, which is what a host needs to
    // write a helper taking one.
    fn _deref<'a>(t: &'a TransportRes, m: &'a MetronomeRes) -> (&'a Transport, &'a ClickState) {
        (t, m)
    }
    fn _payloads(l: &GraphLatency, t: &TransportRes) -> (Samples, Beat, Bpm) {
        (l.0, t.settings.beat(), t.settings.tempo())
    }
    // The crossfade curve.
    let _: Fade = Fade::Smooth;
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
    assert_eq!(
        span.bounds(),
        (Beat(8.0), Beat(4.0)),
        "raw bounds are what a drag shows"
    );
    assert!(span.range().is_none(), "but it is not a loop");
}

/// The disk-streaming handle, nameable through the prelude and named after the
/// engine type it wraps.
///
/// It used to be `SamplerRes` — a name with no referent, since `tutti-sampler`
/// has no `Sampler` type. Every sibling resource (`AudioGraphRes(Net)`,
/// `MeteringRes(MasterMeter)`, `TransportRes(Transport)`) is named for what it
/// holds, and a host that cannot guess the name cannot ask for the resource.
#[cfg(feature = "sampler")]
#[test]
fn the_disk_streamer_resource_is_nameable_from_the_prelude() {
    // Naming the type in a signature is the whole assertion: a `DiskStreamer`
    // needs a butler thread, so constructing one here would be an engine test.
    fn takes_the_resource(_: &DiskStreamerRes) {}
    let _ = takes_the_resource;

    // The plugin that registers the `.wav` loader travels with it.
    let _ = TuttiPlaybackPlugin;
}

/// The whole capture surface is writable from the prelude alone.
///
/// Each of these three is a path this crate's own docs demonstrate, and each
/// was reachable only by naming an engine crate directly until the types below
/// were forwarded. The assertion is that they *compile* — every one is a
/// signature a host would write, and a missing re-export is a compile error
/// rather than a runtime surprise.
#[cfg(feature = "audio-io")]
#[test]
fn the_capture_paths_the_docs_demonstrate_are_writable_from_the_prelude() {
    // Recording the master output: `io/mod.rs`'s "Recording what the graph is
    // playing". Needs `TapIn` *and* `TapBusy` — the second because `open`
    // returns it, and a host that cannot name it cannot write this signature at
    // all, only call the method inside someone else's.
    fn record_master(tap: &AudioTapRes, path: std::path::PathBuf) -> Result<AudioPump, TapBusy> {
        // The sink's width must match the source's — `TapIn` is stereo, so this
        // is `ChannelLayout::STEREO` and not a bare `2`. Naming the layout is
        // what makes the pairing legible; `Recorder::start` rejects a mismatch
        // outright, and `pump` debug-asserts it.
        let wav = WavOut::create(&path, 48_000.0, ChannelLayout::STEREO, BitDepth::Float32)
            .expect("sink opens");
        Ok(AudioPump::start(TapIn::new(tap.open()?), wav, 1024))
    }

    // Recording a mic: `io/mod.rs`'s `matching_sink` example.
    //
    // `io::Result` rather than `Option`: `matching_sink` reports *why* the sink
    // would not open, and a host writing this signature should be able to pass
    // that on. The path goes in bare — it is `impl AsRef<Path>`.
    fn record_mic(path: std::path::PathBuf) -> std::io::Result<AudioPump> {
        let mic = MicIn::open(None).map_err(std::io::Error::other)?;
        let wav = mic.matching_sink(path, BitDepth::Float32)?;
        Ok(AudioPump::start(mic, wav, 1024))
    }

    // Live monitoring: `io/mod.rs`'s graph-wiring example.
    fn wire_monitor(graph: &mut AudioGraphRes) -> Option<()> {
        let (_mic, monitor) = MicIn::open_with_monitor(None).ok()?;
        let _id = graph.0.add(monitor);
        Some(())
    }

    // Naming them is the assertion — calling them would open a real device.
    let _ = record_master;
    let _ = record_mic;
    let _ = wire_monitor;
}
