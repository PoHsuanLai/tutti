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

#[macro_use]
mod common;

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

    /// A promoted soundfont takes MIDI: the promotion inserts it as a graph
    /// node, whose event input a `MidiSourceInstall`, a route or a keyboard
    /// is wired to.
    ///
    /// Mutation: promoting it through `Legacy` (`graph.insert(unit)`) → a
    /// node with no event input → fails.
    #[test]
    fn a_promoted_soundfont_has_a_midi_event_input() {
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

        let node = *app.world().get::<AudioNode>(entity).expect("AudioNode");
        assert_eq!(
            app.world()
                .resource::<bevy_tutti::graph::AudioGraphRes>()
                .node_event_inputs(node),
            1
        );
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
    use bevy_ecs::prelude::Resource;

    use bevy_tutti::graph::{AudioConfig, AudioGraphRes, GraphReconcilePlugin, TransportRes};
    use bevy_tutti::midi::{LiveMidi, LiveMidiInput, MidiSourceInstall, TuttiMidiPlugin};
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
    /// Where the rolling transport is: the beat the next rendered frame is
    /// on, at 120 BPM.
    #[derive(Resource, Default)]
    struct Playhead(f64);

    /// Render `frames` from the graph, one at a time, the transport rolling
    /// from the [`Playhead`] (and moved past them).
    fn render(app: &mut App, frames: usize) -> Vec<(f32, f32)> {
        let from = app.world_mut().get_resource_or_init::<Playhead>().0;
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        let mut out = Vec::with_capacity(frames);
        for i in 0..frames {
            let t = tutti_graph::Transport::new(
                true,
                tutti_core::Bpm(120.0),
                Beat(from + i as f64 / 24_000.0),
                None,
            );
            let mut frame = [0.0f32; 2];
            graph.render_frame_at(&t, &mut frame);
            out.push((frame[0], frame[1]));
        }
        app.world_mut().resource_mut::<Playhead>().0 = from + frames as f64 / 24_000.0;
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
        let mut graph = AudioGraphRes::headless(0, 2);
        graph.set_sample_rate(SampleRate(SAMPLE_RATE));
        app.insert_resource(graph);
        app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
        app.insert_resource(AudioConfig {
            sample_rate: SampleRate(SAMPLE_RATE),
            channels: tutti_core::ChannelLayout::STEREO,
        });
        app.insert_resource(AudioEngineState::Running);
        app.insert_resource(bevy_tutti::midi::test_support::clock_master_for_test());
        // `TuttiMidiPlugin` registers the `MidiFileAsset` loader at build time,
        // which panics without an `AssetServer` — a headless app supplies it.
        app.add_plugins((
            bevy_app::TaskPoolPlugin::default(),
            bevy_asset::AssetPlugin::default(),
        ));
        app.add_plugins((GraphReconcilePlugin, TuttiMidiPlugin));

        // Inserted as a graph node, as the promotion does: its event input is
        // what the clip node and the keyboard wire to.
        let node = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            let (node, ()) = graph.insert_node(unit);
            graph.set_outputs_from(node);
            node
        };
        let synth = app.world_mut().spawn(node).id();
        (app, synth)
    }

    /// Start the playhead at beat 0.
    fn roll(app: &mut App) {
        app.world_mut().insert_resource(Playhead(0.0));
    }

    /// A note declared in the ECS makes sound.
    ///
    /// The end-to-end claim: `MidiSourceInstall` → `rebuild` → clip node →
    /// event edge → engine beat→offset → rustysynth → samples. Silence here
    /// means a break anywhere along it.
    #[test]
    fn a_declared_note_produces_audio() {
        let (mut app, synth) = app_with_soundfont();
        roll(&mut app);

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
    /// The beat is set by hand: the engine's transport drives it, and this test
    /// builds a minimal graph holding only the synth.
    #[test]
    fn the_note_waits_for_its_beat() {
        let (mut app, synth) = app_with_soundfont();
        roll(&mut app);

        app.world_mut().spawn(MidiSourceInstall::new(
            synth,
            note(60, Beat(4.0), BeatDuration(4.0)),
        ));
        app.update();

        // Still well before beat 4.
        let before = rms(&render(&mut app, 4_000));

        // Move onto the note and render again.
        app.world_mut().insert_resource(Playhead(4.0));
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

    /// Live preview still reaches a synth that has a clip installed: the
    /// keyboard's queue node and the clip node both feed its event input,
    /// merged by frame.
    ///
    /// Mutation (run): the event wiring keeping one source per sink, the
    /// first by key (`sources.truncate(1)` after the sort) → the keyboard's
    /// queue is dropped → fails. Dropping the clip instead is not caught
    /// here: the clip is silent in this window by construction, and
    /// `the_note_waits_for_its_beat` covers it.
    #[test]
    fn preview_still_sounds_under_an_installed_clip() {
        let (mut app, synth) = app_with_soundfont();
        roll(&mut app);

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

        // A keyboard on the synth, then a live note, as a keyboard sends it.
        app.world_mut().entity_mut(synth).insert(LiveMidiInput);
        app.update();
        app.world()
            .get::<LiveMidi>(synth)
            .expect("the keyboard is attached")
            .queue(&[tutti_midi_types::ump::MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                67,
                0xFFFF,
            )]);
        app.update();

        let previewed = rms(&render(&mut app, 12_000));
        assert!(
            previewed > 1e-4,
            "live preview must still sound while a clip is installed, got RMS {previewed}"
        );
    }
}

/// A note from the hardware MIDI input, routed by a rule, sounds: the input
/// node's channel port is wired to the synth's event input (`MidiRouteRule`
/// → `EventFeeds` → an event edge), rendered through the graph.
///
/// # Mutation
///
/// - (run) A channel rule wired to port 0 rather than its channel's
///   (`input_ports`) → a note on channel 2 goes nowhere → silent → fails.
/// - A rule for another channel (the check below) → silent.
mod midi_route {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use bevy_app::prelude::*;

    use bevy_tutti::graph::{AudioConfig, AudioGraphRes, GraphReconcilePlugin, TransportRes};
    use bevy_tutti::midi::{MidiEngineNodes, MidiRouteRule, TuttiMidiPlugin};
    use bevy_tutti::AudioEngineState;
    use tutti_core::transport::Transport;
    use tutti_core::SampleRate;
    use tutti_midi_runtime::MidiInputNode;
    use tutti_midi_types::ump::MidiEvent;
    use tutti_midi_types::{MidiChannel, MidiGroup, MidiIn};
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

    /// A wire that hands out what was pushed onto it, on the next poll.
    #[derive(Default)]
    struct Wire(Mutex<Vec<MidiEvent>>);

    impl MidiIn for Wire {
        fn poll_block(&self, _block_size: usize, buffer: &mut [MidiEvent]) -> usize {
            let mut pending = self.0.lock().unwrap();
            let n = pending.len().min(buffer.len());
            buffer[..n].copy_from_slice(&pending[..n]);
            pending.drain(..n);
            n
        }
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    /// Render `frames` stereo frames through the graph.
    fn render(app: &mut App, frames: usize) -> Vec<f32> {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        let mut out = Vec::with_capacity(frames * 2);
        for _ in 0..frames {
            let mut frame = [0.0f32; 2];
            graph.render_frame(&mut frame);
            out.extend_from_slice(&frame);
        }
        out
    }

    /// An app with a SoundFont synth on the master, the hardware input node
    /// over `wire`, and a rule routing `channel` to the synth. How loud a
    /// note on channel 2 through the wire comes out.
    fn routed_level(channel: MidiChannel) -> f32 {
        let mut app = App::new();
        let mut graph = AudioGraphRes::headless(0, 2);
        graph.set_sample_rate(SampleRate(SAMPLE_RATE));
        let wire = Arc::new(Wire::default());
        let (input_node, _) = graph.insert_node(MidiInputNode::new(Some(
            Arc::clone(&wire) as Arc<dyn MidiIn>
        )));
        let mut settings = SynthesizerSettings::new(SAMPLE_RATE as i32);
        settings.enable_reverb_and_chorus = false;
        let unit = SoundFontUnit::new(soundfont(), &settings).expect("build the SoundFontUnit");
        let (synth_node, ()) = graph.insert_node(unit);
        graph.set_outputs_from(synth_node);
        app.insert_resource(graph);
        app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
        app.insert_resource(AudioConfig {
            sample_rate: SampleRate(SAMPLE_RATE),
            channels: tutti_core::ChannelLayout::STEREO,
        });
        app.insert_resource(AudioEngineState::Running);
        app.insert_resource(bevy_tutti::midi::test_support::clock_master_for_test());
        app.add_plugins((
            bevy_app::TaskPoolPlugin::default(),
            bevy_asset::AssetPlugin::default(),
        ));
        app.add_plugins((GraphReconcilePlugin, TuttiMidiPlugin));
        let input = app.world_mut().spawn(input_node).id();
        let synth = app.world_mut().spawn(synth_node).id();
        app.insert_resource(MidiEngineNodes {
            input,
            input_node,
            clock: input,
            hardware_out: input,
        });
        app.world_mut()
            .spawn(MidiRouteRule::for_channel(channel).to(synth));
        app.update();

        wire.0.lock().unwrap().push(MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::new(1),
            60,
            u16::MAX,
        ));
        rms(&render(&mut app, 12_000))
    }

    #[test]
    fn a_routed_hardware_note_sounds() {
        let level = routed_level(MidiChannel::new(1));
        assert!(
            level > 1e-4,
            "a note routed to the synth must sound, got RMS {level}"
        );
        let other = routed_level(MidiChannel::new(5));
        assert!(
            other < 1e-6,
            "a note on an unrouted channel must not, got RMS {other}"
        );
    }
}
