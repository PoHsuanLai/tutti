//! Every type this crate's docs demonstrate must be nameable through this crate.
//!
//! `MetronomeRes::set_mode` takes a `MetronomeMode`; `transport.motion.try_send`
//! takes a `MotionEvent`. Both arrive through a `Deref` to a tutti-transport type, so
//! without the re-export a host would need a direct `tutti-transport`
//! dependency to spell an argument this crate's own examples pass. Handing out a
//! method whose parameter type the caller cannot name is an incomplete forward.
//!
//! # Why most of this file is one `fn`, not a suite of `#[test]`s
//!
//! Re-export coverage is a **compile-time** property: a name that left the
//! prelude is a compile error, and this file then does not build at all. A
//! `#[test]` wrapper around that adds nothing — it cannot fail at runtime, and
//! a green count of eight such tests claims eight checks where there is really
//! one link step. So the nameability half lives in [`_surface_compiles`], which
//! is never called and is the assertion by existing.
//!
//! What stays a real test is what has real behaviour: the timeline handle being
//! live rather than a snapshot, the loop region's validation, and the unit
//! vocabulary's *conversions*. Those can fail with everything still compiling,
//! which is exactly the line between the two halves.

use bevy_tutti::prelude::*;

/// Names every type the prelude must forward, in a position where a missing
/// re-export is a compile error.
///
/// Never called. Each block below was a separate `#[test]` whose body only
/// mentioned types; they are merged here because they were all one assertion —
/// "this file links" — split eight ways.
#[allow(dead_code)]
fn _surface_compiles() {
    use bevy_ecs::prelude::Commands;
    use std::sync::Arc;

    // --- The enums a host must pass to a method it can already call. ---
    //
    // `MetronomeRes::set_mode` and `transport.motion.try_send` both take a type
    // that arrives through a `Deref` to tutti-transport.
    let _: MetronomeMode = MetronomeMode::Always;
    let _: MetronomeMode = MetronomeMode::Off;
    // `Locate` carries the richest payload of the variants, so naming it proves
    // the whole enum came across rather than a stub.
    let _: MotionEvent = MotionEvent::locate(Beat(4.0));
    let _: MotionEvent = MotionEvent::Play;

    // `MotionEvent`'s own payload types, and what `motion()` gives back.
    // Re-exporting `MotionEvent` alone left a host able to call the convenience
    // constructors but unable to write `MotionEvent::Stop { fade }` — the
    // incomplete forward one level down.
    let _: MotionEvent = MotionEvent::Stop {
        fade: FadeOut::Immediate,
    };
    let _: MotionEvent = MotionEvent::Locate {
        beat: Beat(8.0),
        fade: FadeOut::Declick,
        then: Then::Roll,
    };
    let _: MotionState = MotionState::Stopped;
    let _: MotionState = MotionState::Rolling;

    // A timed program: `schedule` takes an `At` and anything that becomes a
    // `TransportCommand`, and `Meter` is the time-signature trait.
    fn _timed(t: &TransportRes) -> Result<(), tutti_transport::ScheduleFull> {
        t.motion.schedule(At::Frame(Frame(0)), MotionEvent::Play)?;
        t.motion
            .schedule(At::NextBlock, TransportCommand::Tempo(Bpm::of(90.0)))
    }
    fn _bar(m: &dyn Meter, bar: BarNumber) -> Beat {
        m.bar_start(bar)
    }
    // Mutation (run): dropping `At`, `TransportCommand`, `Meter` or
    // `BarNumber` from the prelude stops this file compiling.

    // --- The types the prelude's own signatures are spelled in. ---
    //
    // The incomplete-forward rule applied to itself. A previous pass added
    // `TransportRes::timeline()` returning `Arc<dyn Timeline>` without
    // re-exporting `Timeline`, so a host could call the method but not write a
    // function whose signature mentions it. `SessionNode` is the same: without
    // it a host cannot write a generic spawn helper at all.
    fn _timeline(t: &TransportRes) -> Arc<dyn Timeline> {
        t.timeline()
    }
    // The bound on `spawn_audio_node` and `crossfade_audio_node`, and the
    // wrapper a plain node goes in.
    fn _spawn<N>(commands: &mut Commands, node: N)
    where
        N: SessionNode,
        N::Controls: Send + Sync + 'static,
    {
        commands.spawn_audio_node(node);
    }
    fn _crossfade<N: tutti_graph::Node + Clone + Send + 'static>(
        commands: &mut Commands,
        e: bevy_ecs::entity::Entity,
        node: N,
    ) {
        crossfade_audio_node(commands, e, ForkByClone(node));
    }
    // Deref targets: nameable in a signature, which is what a host needs to
    // write a helper taking one.
    fn _deref<'a>(t: &'a TransportRes, m: &'a MetronomeRes) -> (&'a Transport, &'a ClickState) {
        (t, m)
    }
    fn _payloads(l: &GraphLatency, t: &TransportRes) -> (Samples, Beat, Bpm) {
        (l.0, t.settings.beat(), t.settings.tempo())
    }
    // The crossfade curve — the engine's, not the fork's `Fade`.
    let _: CrossfadeCurve = CrossfadeCurve::EqualAmplitude;

    // --- The disk-streaming handle. ---
    //
    // `DiskStreamerRes` is named for what it holds, like every sibling resource
    // (`AudioGraphRes`, over the graph, `MeteringRes(MeterReader)`,
    // `TransportRes(Transport)`): a host that cannot guess the name cannot ask
    // for the resource.
    // A `DiskStreamer` needs a butler thread, so constructing one here would be
    // an engine test; naming it in a signature is the whole point.
    #[cfg(feature = "sampler")]
    fn _takes_the_streamer(_: &DiskStreamerRes) {}
    // The plugin that registers the `.wav` loader travels with it.
    #[cfg(feature = "sampler")]
    let _ = TuttiPlaybackPlugin;

    // --- The whole capture surface, writable from the prelude alone. ---
    //
    // Each of these three is a path this crate's own docs demonstrate, and each
    // was reachable only by naming an engine crate directly until the types
    // below were forwarded.

    // Recording the graph's output: `io/mod.rs`'s "Recording what the graph is
    // playing". Needs `TapIn` and `TapReader` (what `open_heard_tap` returns,
    // so a host can write the signature of a function that holds one).
    #[cfg(feature = "audio-io")]
    fn _record_master(
        graph: &mut AudioGraphRes,
        path: std::path::PathBuf,
    ) -> Result<AudioPump, HeardTapError> {
        let reader: TapReader = graph.open_heard_tap(Samples(48_000))?;
        // The sink's width must match the source's: the tap's own layout, not a
        // bare `2`. `Recorder::start` rejects a mismatch outright, and `pump`
        // debug-asserts it.
        let wav =
            WavOut::create(&path, 48_000.0, reader.layout(), Pcm::Float32).expect("sink opens");
        Ok(AudioPump::start(TapIn::new(reader), wav, Samples(1024)))
    }

    // Recording a mic: `io/mod.rs`'s `matching_sink` example.
    //
    // `io::Result` rather than `Option`: `matching_sink` reports *why* the sink
    // would not open, and a host writing this signature should be able to pass
    // that on. The path goes in bare — it is `impl AsRef<Path>`.
    #[cfg(feature = "audio-io")]
    fn _record_mic(path: std::path::PathBuf, rate: SampleRate) -> std::io::Result<AudioPump> {
        let mic = MicIn::open(None, rate).map_err(std::io::Error::other)?;
        let wav = mic.matching_sink(path, Pcm::Float32)?;
        Ok(AudioPump::start(mic, wav, Samples(1024)))
    }

    // Live monitoring: `io/mod.rs`'s graph-wiring example.
    #[cfg(feature = "audio-io")]
    fn _wire_monitor(graph: &mut AudioGraphRes, rate: SampleRate) -> Option<()> {
        let (_mic, monitor) = MicIn::open_with_monitor(None, rate).ok()?;
        let (_id, ()) = graph.graph().insert(monitor);
        Some(())
    }

    // --- The names app code was importing through `bevy_tutti::<module>::`. ---
    //
    // Thirty distinct names reach into `graph`, `midi`, `modulation`, `sampler`
    // and `soundfont` from the app side; sixteen of them had no prelude entry,
    // so `use bevy_tutti::prelude::*` was not enough to write a host and every
    // caller went to the module path instead.

    // Scalar params: the component and the registration that makes it reconcile.
    // `AudioParam` is generic over unit and port, so it is spelled with both.
    #[cfg(feature = "modulation")]
    fn _declare_param(app: &mut bevy_app::App) {
        app.add_audio_param::<tutti_types::Db, 0>();
    }
    #[cfg(feature = "modulation")]
    fn _param_component() -> AudioParam<tutti_types::Db, 0> {
        AudioParam::new(tutti_types::Db(-6.0))
    }

    // Modulation declared from the prelude alone: a range, a source and a
    // route on a param named by `ParamRef`.
    #[cfg(feature = "modulation")]
    fn _declare_modulation(mut mods: Mods, filter: bevy_ecs::entity::Entity) {
        let cutoff = ParamRef::unit(filter, UnitParam::Cutoff);
        mods.range(cutoff, 200.0, 4_000.0);
        let lfo = mods.lfo(Lfo::sine());
        mods.route(lfo, cutoff, tutti_types::Depth(0.3));
    }

    // Sampler: build a voice, and clamp a file width to what it will play.
    // Every type in this signature comes from the prelude — `Playback`,
    // `VoiceWindow`, `Voice` and `PlaceError` are memory_voice's arguments and return, so a
    // prelude carrying only the function would be an incomplete forward.
    #[cfg(feature = "sampler")]
    fn _build_voice(
        wave: std::sync::Arc<tutti_codec::Wave>,
        width: ChannelLayout,
    ) -> Result<Voice, PlaceError> {
        memory_voice(
            wave,
            voice_width(width),
            Playback::default(),
            VoiceWindow::default(),
        )
    }

    // SoundFont: the module re-exported nothing anywhere before this.
    #[cfg(feature = "soundfont")]
    let _: TuttiSoundFontPlugin = TuttiSoundFontPlugin;

    // Param addressing — how a modulation target names the scalar it moves.
    let _: ParamAddr = ParamAddr::Unit(UnitParam::GainDb);
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
    use tutti_transport::Transport;

    let res = TransportRes(Transport::new(48_000.0));
    let timeline = res.timeline();

    let mut audio = res.audio(48_000.0).expect("the only playhead writer");
    audio.set_playhead(4.0);
    assert_eq!(timeline.beat().get(), 4.0);

    // Move it again through the resource; the handed-out handle follows.
    audio.set_playhead(12.5);
    assert_eq!(
        timeline.beat().get(),
        12.5,
        "the handle shares state — a snapshot would still read 4.0"
    );

    res.settings.set_tempo(Bpm::of(140.0));
    assert_eq!(timeline.tempo().get(), 140.0);
}

/// The loop region a host edits, and the form it reads back.
///
/// An edit is a scheduled `LoopEdit`; the region is the engine's, so a read
/// shows an edit only once a block has applied it. `LoopRange::new` is where
/// an inverted brace is refused, so every type in the round trip has to be
/// nameable from the prelude.
#[test]
fn the_loop_region_round_trips_through_the_prelude() {
    let range: LoopRange = LoopRange::new(4.0, 8.0).expect("4..8 is a usable region");
    assert_eq!(range.start().get(), 4.0);
    assert_eq!(range.end().get(), 8.0);
    assert!(
        LoopRange::new(8.0, 4.0).is_none(),
        "an inverted brace is not a loop"
    );

    let region = LoopRegion::default().with(LoopEdit::over(range));
    assert_eq!(region.active(), Some(range));
    assert_eq!(region.with(LoopEdit::armed(false)).bounds(), range);
}

/// The engine's measurement vocabulary arrives whole, not as a hand-picked subset.
///
/// The names are what app code imports from `tutti-metering` / `tutti-types`
/// alongside this prelude, most-used first. This stays a `#[test]` rather than
/// moving into [`_surface_compiles`] because it asserts the *conversions*, not
/// just the names: `to_samples`, `to_amplitude` and `to_semitones` are the
/// converters the units rule says never to hand-roll, and each can be wrong
/// while still compiling.
#[test]
fn the_engine_measurement_vocabulary_converts_as_documented() {
    // Musical position and span — the pair a placement is spelled in.
    assert_eq!(Beat(1.0) + BeatDuration(3.0), Beat(4.0));
    assert_eq!(Bpm::of(120.0).get(), 120.0);

    // Frequency, time, and the conversion between them. `SampleRate` is the
    // argument that conversion takes, so it has to come across with them.
    assert_eq!(
        Seconds(0.5).to_samples(SampleRate(48_000.0)),
        Samples(24_000)
    );
    assert_eq!(Hz(440.0).get(), 440.0);

    // Level: the log and linear halves, plus the named converter between them.
    assert_eq!(Db(0.0).to_amplitude(), Amplitude(1.0));

    // Pitch offsets and their converter.
    assert_eq!(Cents(1200.0).to_semitones(), Semitones(12.0));

    // Modulation depth and phase.
    assert_eq!(Depth(0.5).get(), 0.5);
    assert_eq!(PhaseIncrement(0.25).get(), 0.25);

    // Channel width, ungated: the I/O traits report it at runtime, so a host
    // reads a source's layout without enabling `audio-io`.
    assert_eq!(ChannelLayout::STEREO.count(), 2);

    // MIDI addressing, for a host installing a sequence source.
    assert_eq!(MidiChannel::FIRST.get(), 0);
    assert_eq!(MidiGroup::FIRST.get(), 0);
}
