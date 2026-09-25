//! A SoundFont voice, from the asset that spawns it to the samples it renders.
//!
//! - `soundfont_spawn` — the spawner binds its entity to the graph with one
//!   handle (`AudioNode`), and does not mint a second node on a later frame.
//! - `midi_soundfont_audio` — the whole chain rendered: an ECS declaration
//!   produces audible samples, scheduled on the beat.
//!
//! Together because they are the two halves of one path and share a fixture:
//! spawning proves the node exists, rendering proves it sounds, and neither is
//! evidence for the other. The `.sf2` both need is **committed** at
//! `assets/soundfonts/TimGM6mb.sf2`, so a missing one fails loudly
//! with the resolved path rather than skipping.

#![cfg(all(feature = "midi", feature = "soundfont"))]

/// The SoundFont spawner binds its entity to the graph with one handle.
///
/// `promote_pending_soundfonts` used to insert `AudioNode` *and* an
/// `AudioEmitter` carrying the same `NodeId`, with a comment conceding that
/// teardown keys on the former. `AudioEmitter` is gone; this pins what replaced
/// it, and covers a spawner that had no test at all — the soundfont audio tests
/// next door build their node by hand and never reach this path.
///
/// The `.sf2` these need is **committed** at
/// `assets/soundfonts/TimGM6mb.sf2`, so a missing one is a broken
/// checkout and fails loudly with the resolved path. These used to skip on the
/// `None` arm instead — the pattern the copies in `src/soundfont.rs` followed,
/// where a wrong path meant every one of them silently passed without running.
/// (Was `tests/soundfont_spawn.rs`.)
mod soundfont_spawn {
    use std::path::PathBuf;

    use bevy_app::prelude::*;
    use bevy_asset::{AssetPlugin, Assets, Handle};

    use bevy_tutti::graph::{AudioConfig, AudioGraphRes, GraphReconcilePlugin, TransportRes};
    use bevy_tutti::soundfont::SoundFontAsset;
    use bevy_tutti::soundfont::{PlaySoundFont, TuttiSoundFontPlugin};
    use bevy_tutti::AudioEngineState;
    use tutti_core::AudioNode;

    const SAMPLE_RATE: f64 = 48_000.0;

    /// The repo's test soundfont, decoded.
    ///
    /// Panics with the resolved path rather than returning `None`: the asset is
    /// committed, so its absence is a broken checkout and not a configuration this
    /// suite should quietly pass on.
    fn soundfont_asset() -> SoundFontAsset {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent() // crates/
            .and_then(|p| p.parent()) // repo root
            .expect("bevy-tutti lives two levels below the repo root")
            .join("assets/soundfonts/TimGM6mb.sf2");
        let bytes = std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "committed test soundfont missing at {}: {e}",
                path.display()
            )
        });
        SoundFontAsset::from_bytes(&bytes)
            .unwrap_or_else(|e| panic!("test soundfont at {} is malformed: {e}", path.display()))
    }

    /// An app with the soundfont plugin and the engine resources its systems gate on.
    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((bevy_app::TaskPoolPlugin::default(), AssetPlugin::default()));

        app.insert_resource(AudioGraphRes::headless(0, 2));
        app.insert_resource(TransportRes(tutti_core::transport::Transport::new(
            SAMPLE_RATE,
        )));
        app.insert_resource(AudioConfig {
            sample_rate: tutti_core::SampleRate(SAMPLE_RATE),
            channels: tutti_core::ChannelLayout::STEREO,
        });
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins((GraphReconcilePlugin, TuttiSoundFontPlugin));
        app
    }

    /// Put the decoded soundfont in the asset store and hand back a handle to it.
    fn insert_asset(app: &mut App, asset: SoundFontAsset) -> Handle<SoundFontAsset> {
        app.world_mut()
            .resource_mut::<Assets<SoundFontAsset>>()
            .add(asset)
    }

    /// Run frames until the off-thread build lands, or give up.
    ///
    /// The decode runs on the `AsyncComputeTaskPool`, so the number of frames it
    /// takes is not fixed; polling beats sleeping on a fixed guess.
    fn run_until_promoted(app: &mut App, entity: bevy_ecs::entity::Entity) -> bool {
        for _ in 0..600 {
            app.update();
            if app.world().get::<AudioNode>(entity).is_some() {
                return true;
            }
            std::thread::yield_now();
        }
        false
    }

    /// A triggered soundfont ends up bound to the graph by `AudioNode`, and the
    /// node it names is really there.
    #[test]
    fn a_triggered_soundfont_is_bound_to_the_graph_by_its_node_handle() {
        let asset = soundfont_asset();
        let mut app = app();
        let handle = insert_asset(&mut app, asset);

        let entity = app
            .world_mut()
            .spawn(PlaySoundFont {
                source: handle,
                preset: 0,
                channel: 0,
            })
            .id();

        assert!(
            run_until_promoted(&mut app, entity),
            "the off-thread build should land and insert AudioNode"
        );

        let node = *app.world().get::<AudioNode>(entity).expect("AudioNode");
        assert!(
            app.world().resource::<AudioGraphRes>().contains(node),
            "the handle must name a node that is actually in the graph"
        );
        assert!(
            app.world()
                .get::<bevy_tutti::soundfont::PendingSoundFontUnit>(entity)
                .is_none(),
            "and the pending marker is cleared"
        );
    }

    /// The trigger does not fire twice: a promoted entity keeps its original node.
    ///
    /// Honest about what this covers. `PlaySoundFontPending`'s `Without<AudioNode>`
    /// clause (which used to name `AudioEmitter`) is *not* what stops a re-trigger —
    /// `soundfont_playback_system` removes `PlaySoundFont` when it fires, so the
    /// entity leaves the trigger set either way, and breaking the filter alone does
    /// not fail this test. The clause is a second line of defence for an entity
    /// whose trigger is re-inserted by hand.
    ///
    /// What this does catch is the failure that matters: a spawner that mints a new
    /// node on a later frame, whatever the cause.
    #[test]
    fn a_promoted_soundfont_is_not_rebuilt_every_frame() {
        let asset = soundfont_asset();
        let mut app = app();
        let handle = insert_asset(&mut app, asset);

        let entity = app
            .world_mut()
            .spawn(PlaySoundFont {
                source: handle,
                preset: 0,
                channel: 0,
            })
            .id();
        assert!(run_until_promoted(&mut app, entity));

        let node = app.world().get::<AudioNode>(entity).expect("AudioNode").0;
        for _ in 0..5 {
            app.update();
        }

        assert_eq!(
            app.world().get::<AudioNode>(entity).expect("AudioNode").0,
            node,
            "the entity keeps its original node — a re-trigger would mint a new one"
        );
    }

    /// A promoted soundfont is addressable: the promotion captured its MIDI port
    /// before the unit went into the graph.
    ///
    /// `TuttiSoundFontPlugin` registers `SoundFontUnit` with the MIDI registry
    /// for exactly this, and promotion is an insertion path of its own (it adds
    /// the node directly, not through `spawn_audio_node`), so it has to run the
    /// capture itself.
    ///
    /// Mutation: making `promote_pending_soundfonts` bind
    /// `CapturedControls::default()` instead of `capture.capture(&unit)` fails
    /// this — the player gets its node and no port, and every
    /// `MidiSourceInstall` naming it would resolve to nothing.
    #[test]
    fn a_promoted_soundfont_carries_its_midi_port() {
        let asset = soundfont_asset();
        let mut app = app();
        let handle = insert_asset(&mut app, asset);

        let entity = app
            .world_mut()
            .spawn(PlaySoundFont {
                source: handle,
                preset: 0,
                channel: 0,
            })
            .id();
        assert!(run_until_promoted(&mut app, entity));

        let node = app.world().get::<AudioNode>(entity).expect("AudioNode").0;
        let target = app
            .world()
            .get::<bevy_tutti::midi::MidiTarget>(entity)
            .expect("the promotion captured the unit's MIDI port");
        assert_eq!(target.node(), node, "for the node it promoted");
    }
}

/// The whole chain, rendered: an ECS declaration produces audible samples.
///
/// Every other MIDI test asserts on the *plumbing* — a sender on the bus, an
/// event at a port. This one renders a real `SoundFontUnit` through the graph
/// and measures the output, so a break anywhere between `MidiSourceInstall` and
/// a moving speaker cone fails it. It replaces the "run the example and listen"
/// step, which no example implemented.
///
/// The `.sf2` these need is **committed** at
/// `assets/soundfonts/TimGM6mb.sf2`, so a missing one is a broken
/// checkout and fails loudly with the resolved path. These used to skip on the
/// `None` arm instead — the pattern the copies in `src/soundfont.rs` followed,
/// where a wrong path meant every one of them silently passed without running.
/// (Was `tests/midi_soundfont_audio.rs`.)
mod midi_soundfont_audio {
    use std::path::PathBuf;
    use std::sync::Arc;

    use bevy_app::prelude::*;
    use bevy_ecs::entity::Entity;

    use bevy_tutti::graph::{
        AudioConfig, AudioGraphRes, CapturedControls, GraphReconcilePlugin, TransportRes,
    };
    use bevy_tutti::midi::{MidiSourceInstall, MidiTarget, MidiTargetRegistry, TuttiMidiPlugin};
    use bevy_tutti::AudioEngineState;
    use tutti_core::transport::Transport;
    use tutti_core::{Beat, BeatDuration, SampleRate};
    use tutti_midi_runtime::TimedMidiEvent;
    use tutti_midi_types::ump::MidiEvent;
    use tutti_midi_types::{MidiChannel, MidiGroup};
    use tutti_soundfont::{SoundFont, SoundFontUnit, SynthesizerSettings};

    const SAMPLE_RATE: f64 = 48_000.0;

    /// A note-on/note-off pair at full velocity, which is what an install holds.
    ///
    /// These tests assert *audible output*, so velocity is pinned at the 16-bit
    /// maximum rather than left to a default.
    fn note(number: u8, start: Beat, duration: BeatDuration) -> Vec<TimedMidiEvent> {
        vec![
            TimedMidiEvent::new(
                start,
                MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, number, u16::MAX),
            ),
            // `Beat + BeatDuration` is the affine operator: a position plus a span
            // is a position. `Beat + Beat` deliberately does not compile.
            TimedMidiEvent::new(
                start + duration,
                MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, number, 0),
            ),
        ]
    }

    /// The repo's test soundfont.
    ///
    /// Panics with the resolved path rather than returning `None`: `TimGM6mb.sf2` is
    /// **committed**, so a checkout without it is broken, not a configuration this
    /// suite should quietly pass on. The tests here used to `return` on the `None`
    /// arm, which meant a wrong path — the exact bug the copies of this helper in
    /// `src/soundfont.rs` carried — reported success while asserting nothing.
    fn soundfont() -> Arc<SoundFont> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent() // crates/
            .and_then(|p| p.parent()) // repo root
            .expect("bevy-tutti lives two levels below the repo root")
            .join("assets/soundfonts/TimGM6mb.sf2");
        let mut file = std::fs::File::open(&path).unwrap_or_else(|e| {
            panic!(
                "committed test soundfont missing at {}: {e}",
                path.display()
            )
        });
        Arc::new(
            SoundFont::new(&mut file).unwrap_or_else(|e| {
                panic!("test soundfont at {} is malformed: {e}", path.display())
            }),
        )
    }

    /// RMS of a rendered stereo block — how loud it actually is.
    fn rms(samples: &[(f32, f32)]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_sq: f32 = samples.iter().map(|(l, r)| l * l + r * r).sum();
        (sum_sq / (samples.len() * 2) as f32).sqrt()
    }

    /// Render `frames` from the graph.
    ///
    /// Per-sample `tick` rather than a `process` block: `BufferVec` holds one SIMD
    /// block per channel, capping `process` at 64 frames, and these tests need
    /// quarter-second spans. The clip source is polled by the unit either way.
    fn render(app: &mut App, frames: usize) -> Vec<(f32, f32)> {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        let mut out = Vec::with_capacity(frames);
        for _ in 0..frames {
            let mut frame = [0.0f32; 2];
            graph.render_frame(&mut frame);
            out.push((frame[0], frame[1]));
        }
        out
    }

    /// Set up an app with a real soundfont synth wired to output.
    fn app_with_soundfont() -> (App, Entity) {
        let sf = soundfont();
        let mut settings = SynthesizerSettings::new(SAMPLE_RATE as i32);
        settings.enable_reverb_and_chorus = false;
        let unit = SoundFontUnit::new(sf, &settings).expect("build the SoundFontUnit");

        let mut app = App::new();
        // Headless, because the Commit-phase `commit_graph` needs an audio side
        // to publish to. We render the control side directly rather than through
        // that — `render_frame` on this side sees the same units.
        app.insert_resource(AudioGraphRes::headless(0, 2));
        app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
        app.insert_resource(AudioConfig {
            sample_rate: SampleRate(SAMPLE_RATE),
            channels: tutti_core::ChannelLayout::STEREO,
        });
        app.insert_resource(AudioEngineState::Running);
        app.insert_resource(bevy_tutti::midi::test_support::midi_bus_for_test());
        app.insert_resource(bevy_tutti::midi::test_support::clock_master_for_test(
            SAMPLE_RATE,
        ));
        // `engine_ready` claims every resource the engine block inserts is
        // present, and the route rebuild takes `MidiRoutingRes` as a plain
        // `ResMut` on that promise. A test asserting readiness supplies it.
        app.insert_resource(bevy_tutti::midi::test_support::routing_table_for_test().0);
        // `TuttiMidiPlugin` registers the `MidiFileAsset` loader at build time,
        // which panics without an `AssetServer` — a headless app supplies it.
        app.add_plugins((
            bevy_app::TaskPoolPlugin::default(),
            bevy_asset::AssetPlugin::default(),
        ));
        app.add_plugins((GraphReconcilePlugin, TuttiMidiPlugin));
        app.world_mut()
            .resource_mut::<MidiTargetRegistry>()
            .register::<SoundFontUnit>();

        // Registered first, then captured from the unit and pushed — the order
        // every insertion path follows, so the synth carries its MIDI port.
        let controls = CapturedControls::capture(app.world(), &unit);
        let node = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            let node = graph.insert(unit);
            graph.set_outputs_from(node);
            node
        };
        let mut synth = app.world_mut().spawn_empty();
        controls.bind(&mut synth, node);
        let synth = synth.id();
        (app, synth)
    }

    fn roll(app: &App) {
        let transport = app.world().resource::<TransportRes>().clone();
        let _ = transport
            .motion
            .try_send(tutti_core::transport::MotionEvent::Play);
        transport.motion.drain();
    }

    /// A note declared in the ECS makes sound.
    ///
    /// The end-to-end claim: `MidiSourceInstall` → `rebuild` → resolved port →
    /// installed clip → engine beat→offset → rustysynth → samples. Silence here
    /// means a break anywhere along it.
    #[test]
    fn a_declared_note_produces_audio() {
        let (mut app, synth) = app_with_soundfont();
        roll(&app);

        app.world_mut().spawn(MidiSourceInstall::new(
            synth,
            note(60, Beat(0.0), BeatDuration(2.0)),
        ));
        app.update();

        // Half a second at 120 BPM covers the note-on comfortably.
        let samples = render(&mut app, 24_000);
        let level = rms(&samples);
        assert!(
            level > 1e-4,
            "an ECS-declared note must reach the speakers, got RMS {level}"
        );
    }

    /// Silence before the note, sound after — the scheduling is real, not a note
    /// that fires the instant the clip is installed.
    ///
    /// Without this, "it made noise" would pass equally for a source that ignored
    /// the beat entirely, which is what the old path effectively did.
    ///
    /// The beat is set by hand: it is normally advanced by a `TransportClock` node
    /// the engine adds to the graph, and this test builds a minimal graph holding
    /// only the synth. Setting it is what the audio thread would have done.
    #[test]
    fn the_note_waits_for_its_beat() {
        let (mut app, synth) = app_with_soundfont();
        roll(&app);

        app.world_mut().spawn(MidiSourceInstall::new(
            synth,
            note(60, Beat(4.0), BeatDuration(4.0)),
        ));
        app.update();

        // Still well before beat 4.
        let before = rms(&render(&mut app, 4_000));

        // Advance onto the note and render again.
        let transport = app.world().resource::<TransportRes>().clone();
        transport.settings.set_beat(tutti_core::Beat(4.0));
        let after = rms(&render(&mut app, 12_000));

        assert!(
            before < 1e-5,
            "nothing should sound before the note's beat, got RMS {before}"
        );
        assert!(
            after > 1e-4,
            "the note should sound once its beat arrives: {before} then {after}"
        );
    }

    /// Live preview still reaches a synth that has a clip installed.
    ///
    /// This is what commit `01ad5b006` bought — `MidiInPort::poll` layers the
    /// installed source over the mailbox rather than replacing it. Before that, a
    /// synth playing a clip went deaf to the keyboard, and the pushed events sat in
    /// the mailbox and popped out stale on the next `clear()`.
    #[test]
    fn preview_still_sounds_under_an_installed_clip() {
        let (mut app, synth) = app_with_soundfont();
        roll(&app);

        // A clip whose first note is far in the future, so anything audible in the
        // next quarter-second can only be the preview.
        app.world_mut().spawn(MidiSourceInstall::new(
            synth,
            note(60, Beat(100.0), BeatDuration(1.0)),
        ));
        app.update();

        let quiet = rms(&render(&mut app, 6_000));
        assert!(
            quiet < 1e-5,
            "the clip's note is far away; expected silence"
        );

        // Push a live note straight at the synth's mailbox, as a keyboard would.
        {
            let target = app.world().get::<MidiTarget>(synth).unwrap();
            target
                .port()
                .sender()
                .queue(&[tutti_midi_types::ump::MidiEvent::note_on(
                    MidiGroup::FIRST,
                    MidiChannel::FIRST,
                    67,
                    0xFFFF,
                )]);
        }

        let previewed = rms(&render(&mut app, 12_000));
        assert!(
            previewed > 1e-4,
            "live preview must still sound while a clip is installed, got RMS {previewed}"
        );
    }
}

/// A crossfaded synth still plays: MIDI sent the way the inbound phase sends it
/// reaches the **incoming** unit, rendered through the audio-thread backend.
///
/// A crossfade replaces the unit — and with it the MIDI port and its id — under
/// a surviving `NodeId`. Registration used to key on the first port's id and
/// never revisit it, and the route table rebuilt only on a new `AudioNode`, so
/// after a crossfade the bus held the outgoing unit's sender, the routes named
/// the outgoing unit's id, and the synth the listener hears was unreachable.
///
/// Rendered through the `NetBackend`, not the frontend `Net`: a commit hands
/// the frontend's vertices to the backend along with the crossfade edit, so the
/// frontend keeps rendering the outgoing unit and could never show this.
///
/// # Mutation
///
/// - Dropping `Changed<MidiTarget>` from `register_midi_senders`' filter (the
///   bug as it was) leaves the new id off the bus: the `contains` assertion
///   fails, and without it the render is silent.
/// - Dropping the `recaptured` arm from the route `rebuild`'s dirty check
///   leaves the routes naming the outgoing unit: the route assertion fails.
mod midi_crossfade {
    use std::path::PathBuf;
    use std::sync::Arc;

    use bevy_app::prelude::*;

    use bevy_tutti::graph::{
        crossfade_audio_node, AudioConfig, AudioGraphRes, GraphReconcilePlugin, MasterSources,
        SpawnAudioNode, TransportRes,
    };
    use bevy_tutti::midi::{
        MidiBusRes, MidiRouteRule, MidiTarget, MidiTargetRegistry, TuttiMidiPlugin,
    };
    use bevy_tutti::AudioEngineState;
    use tutti_core::transport::Transport;
    use tutti_core::{AudioUnit, SampleRate};
    use tutti_midi_types::ump::MidiEvent;
    use tutti_midi_types::{MidiChannel, MidiGroup, MidiUnitId};
    use tutti_soundfont::{SoundFont, SoundFontUnit, SynthesizerSettings};

    const SAMPLE_RATE: f64 = 48_000.0;

    fn soundfont() -> Arc<SoundFont> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("bevy-tutti lives two levels below the repo root")
            .join("assets/soundfonts/TimGM6mb.sf2");
        let mut file = std::fs::File::open(&path).unwrap_or_else(|e| {
            panic!(
                "committed test soundfont missing at {}: {e}",
                path.display()
            )
        });
        Arc::new(SoundFont::new(&mut file).expect("test soundfont parses"))
    }

    fn unit(sf: &Arc<SoundFont>) -> SoundFontUnit {
        let mut settings = SynthesizerSettings::new(SAMPLE_RATE as i32);
        settings.enable_reverb_and_chorus = false;
        SoundFontUnit::new(Arc::clone(sf), &settings).expect("build the SoundFontUnit")
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    /// Render `frames` stereo frames from the backend the audio thread would own.
    fn render(backend: &mut impl AudioUnit, frames: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames * 2);
        for _ in 0..frames {
            let mut frame = [0.0f32; 2];
            backend.tick(&[], &mut frame);
            out.extend_from_slice(&frame);
        }
        out
    }

    #[test]
    fn a_crossfaded_synth_is_reached_through_the_bus() {
        let sf = soundfont();
        let mut graph = AudioGraphRes::unattached(0, 2);
        graph.set_sample_rate(SampleRate(SAMPLE_RATE));
        let mut backend = graph.take_audio_side();

        let mut app = App::new();
        app.insert_resource(graph);
        app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
        app.insert_resource(AudioConfig {
            sample_rate: SampleRate(SAMPLE_RATE),
            channels: tutti_core::ChannelLayout::STEREO,
        });
        app.insert_resource(AudioEngineState::Running);
        app.insert_resource(bevy_tutti::midi::test_support::midi_bus_for_test());
        app.insert_resource(bevy_tutti::midi::test_support::clock_master_for_test(
            SAMPLE_RATE,
        ));
        let (routing, rt_view) = bevy_tutti::midi::test_support::routing_table_for_test();
        app.insert_resource(routing);
        app.add_plugins((
            bevy_app::TaskPoolPlugin::default(),
            bevy_asset::AssetPlugin::default(),
        ));
        app.add_plugins((GraphReconcilePlugin, TuttiMidiPlugin));
        app.world_mut()
            .resource_mut::<MidiTargetRegistry>()
            .register::<SoundFontUnit>();

        let synth = app.world_mut().commands().spawn_audio_node(unit(&sf)).id();
        app.insert_resource(MasterSources::from(synth));
        app.world_mut()
            .spawn(MidiRouteRule::for_channel(MidiChannel::FIRST).to(synth));
        app.update();
        let first = app
            .world()
            .get::<MidiTarget>(synth)
            .unwrap()
            .port()
            .unit_id();

        crossfade_audio_node(&mut app.world_mut().commands(), synth, Box::new(unit(&sf)));
        app.update();
        app.update();
        let second = app
            .world()
            .get::<MidiTarget>(synth)
            .unwrap()
            .port()
            .unit_id();
        assert_ne!(first, second, "the incoming unit has its own port");

        let bus = app.world().resource::<MidiBusRes>().clone();
        assert!(
            bus.contains(second) && !bus.contains(first),
            "the bus must carry the incoming unit's sender, not the outgoing one's"
        );

        // Let the backend take the commit and finish the 5 ms fade.
        render(&mut backend, 2_048);

        // What the inbound phase does: route by the RT snapshot, queue on the bus.
        let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, u16::MAX);
        let targets: Vec<MidiUnitId> = rt_view.read().route(&note).collect();
        assert_eq!(targets, vec![second], "the routes name the incoming unit");
        for id in targets {
            bus.queue(id, &[note]);
        }

        let level = rms(&render(&mut backend, 12_000));
        assert!(
            level > 1e-4,
            "a note routed to the crossfaded synth must sound, got RMS {level}"
        );
    }
}
