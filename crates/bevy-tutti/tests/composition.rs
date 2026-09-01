//! What a host can assemble, and what still works when the engine is absent.
//!
//! - `headless` — `TuttiPlugin { disabled: true }` builds an app on a machine
//!   with no audio device.
//! - `plugins_without_engine` — every subsystem plugin composes without the
//!   engine bootstrap.
//! - `master_bus` — what reaches the speakers, and who decides.
//!
//! Grouped as the composition suite: each asserts that adding plugins in some
//! order produces a working `App` rather than a panic, which is the property a
//! host depends on and no single-subsystem file covers. Bodies and test names
//! are unchanged.

/// `TuttiPlugin { disabled: true }` must build an app on a machine with no
/// sound card.
///
/// This is what the state enum buys beyond nicer error reporting: before it,
/// adding the plugin meant opening a device, so a CI runner or a headless test
/// could not add it at all — and therefore could not add any host plugin that
/// schedules against its system sets.
/// (Was `tests/headless.rs`.)
mod headless {
    use bevy_app::App;
    use bevy_tutti::{AudioEngineState, TuttiPlugin};

    /// An app with the prerequisites `TuttiPlugin` expects but nothing else.
    ///
    /// `AssetPlugin` is needed whenever a subsystem registers an asset loader
    /// (sampler, soundfont) — that is an ordinary Bevy prerequisite a real host
    /// gets from `DefaultPlugins`, not something the disabled path can skip.
    fn headless_app() -> App {
        let mut app = App::new();
        app.add_plugins((
            bevy_app::TaskPoolPlugin::default(),
            bevy_asset::AssetPlugin::default(),
        ));
        app.add_plugins(TuttiPlugin {
            disabled: true,
            ..Default::default()
        });
        app
    }

    #[test]
    fn disabled_plugin_builds_and_reports_disabled() {
        let app = headless_app();

        let state = app
            .world()
            .get_resource::<AudioEngineState>()
            .expect("the plugin always inserts its state");
        assert_eq!(*state, AudioEngineState::Disabled);
        assert!(!state.is_running());
        assert_eq!(
            state.failure(),
            None,
            "disabled is a choice, not a failure to report"
        );
    }

    /// The ECS surface is registered either way — that is the point of `Disabled`
    /// over simply not adding the plugin. Frames must run without an engine.
    #[test]
    fn disabled_app_runs_frames() {
        let mut app = headless_app();

        // Any engine-gated system would panic here if `engine_ready` let it through
        // with no engine resources present.
        app.update();
        app.update();
    }

    /// The default state of a bare `World` is `Disabled`, so a host that inspects
    /// the resource before `TuttiPlugin` runs sees something truthful rather than a
    /// claim that audio is up.
    #[test]
    fn default_is_disabled() {
        assert_eq!(AudioEngineState::default(), AudioEngineState::Disabled);
    }
}

/// Every subsystem plugin composes without the engine bootstrap.
///
/// # Why this exists
///
/// In Bevy 0.19 a missing `Res<T>` is a **parameter-validation failure that
/// panics the schedule**, not a skipped system. So any system reading a resource
/// its own plugin does not insert is a crash waiting for the right combination
/// of plugins — and the combinations are what a host picks, not what this crate
/// tests.
///
/// The trap is `engine_ready`. It reads `AudioEngineState`, which is a plain
/// resource *any host can insert*, while the dozen resources it is taken to
/// stand for come from `engine::build_into`. A doc claiming only two resources
/// needed `Option` let three MIDI systems, two graph observers, the latency
/// compensator, the soundfont promoter, the modulation driver and two plugin
/// binding systems take hard `Res` — every one of them a panic for a host that
/// declared the engine up without building one.
///
/// Six of those were found by hand, one panic at a time, each fix revealing the
/// next. This test is that loop, automated: add every plugin, claim the engine
/// is running, and run frames. It fails loudly on the whole class rather than on
/// whichever instance someone happens to hit.
///
/// # Mutating this test
///
/// Revert any `Option<Res<_>>` in a system these plugins schedule back to a hard
/// `Res<_>` and this must fail. If it still passes, the plugin whose system you
/// broke is not being added here — add it.
/// (Was `tests/plugins_without_engine.rs`.)
mod plugins_without_engine {
    use bevy_app::prelude::*;

    /// Every plugin this build has, with **no** `engine::build_into` — but with the
    /// claim that the engine is up, which is the lie that used to be load-bearing.
    fn all_plugins_no_engine() -> App {
        let mut app = App::new();
        // `AssetPlugin` because any subsystem registering an asset loader — the
        // soundfont one does, at build time — needs an `AssetServer` to register
        // into. A Bevy-side prerequisite, not part of the resource class under test.
        app.add_plugins((
            bevy_app::TaskPoolPlugin::default(),
            bevy_asset::AssetPlugin::default(),
        ));
        app.add_plugins(bevy_tutti::graph::GraphReconcilePlugin);
        app.add_plugins(bevy_tutti::LatencyCompensationPlugin);
        #[cfg(feature = "midi")]
        app.add_plugins(bevy_tutti::midi::TuttiMidiPlugin);
        #[cfg(feature = "modulation")]
        app.add_plugins(bevy_tutti::modulation::TuttiModulationPlugin);
        #[cfg(feature = "plugin")]
        app.add_plugins(bevy_tutti::plugin_host::TuttiHostingPlugin);
        #[cfg(feature = "export")]
        app.add_plugins(bevy_tutti::export::ExportPlugin);
        app.insert_resource(bevy_tutti::AudioEngineState::Running);
        app
    }

    /// The whole class, in one assertion.
    ///
    /// Three frames rather than one: some systems only touch their resources on a
    /// dirty gate, and a first frame can pass while a later one panics.
    #[test]
    fn every_plugin_survives_without_an_engine() {
        let mut app = all_plugins_no_engine();
        for _ in 0..3 {
            app.update();
        }
    }

    /// The umbrella plugin, which composes a different set than the à-la-carte path
    /// above and is what most hosts actually add.
    ///
    /// `TuttiPlugin` notably does *not* add `TuttiModulationPlugin`, so under
    /// `--features plugin,modulation` it schedules `plugin_bind_params` — which
    /// reads a resource only the modulation plugin inserts. That combination
    /// panicked on frame one.
    #[test]
    fn the_umbrella_plugin_survives_without_a_device() {
        let mut app = App::new();
        app.add_plugins((
            bevy_app::TaskPoolPlugin::default(),
            bevy_asset::AssetPlugin::default(),
        ));
        app.add_plugins(bevy_tutti::TuttiPlugin::default());
        for _ in 0..3 {
            app.update();
        }
    }
}

/// What reaches the speakers, and who decides.
///
/// `Net::pipe_output` is not a mix. It walks *every* global output channel and
/// overwrites that channel's edge, so two callers do not layer — the second
/// silently disconnects the first. Two places in this crate call it as though it
/// layered: `engine::build` for the metronome (its comment says "mixed into
/// master output") and `synth::soundfont` for every soundfont that finishes
/// loading.
///
/// These pinned that behaviour before the fix. `pipe_output` still behaves this
/// way — it is the engine's, and clobbering is a correct thing for a
/// "this node IS the master" primitive to do. What changed is that **this crate
/// no longer calls it**: what reaches the bus is declared once, in
/// `MasterSources`, where two claims cannot coexist.
///
/// They stay as the record of why that shape was chosen, and as a guard: if
/// anything here starts calling `pipe_output` again, the mechanism these tests
/// describe is what it will silently reintroduce. The declarative side is
/// covered in `graph_wire.rs`.
/// (Was `tests/master_bus.rs`.)
mod master_bus {
    // No feature gate: this drives `Net` directly and names no synth type. It
    // carried `#![cfg(feature = "synth")]` for one commit, which meant the file
    // documenting a silent bug was itself silently running zero tests in the
    // default configuration.

    use tutti_core::dsp::{sine_hz, Net, Source};

    /// Two `pipe_output` calls do not sum. The second wins outright.
    ///
    /// This is the whole defect in four lines: nothing warns, nothing logs, and the
    /// first node is simply gone from the output.
    #[test]
    fn a_second_pipe_output_silently_replaces_the_first() {
        let mut net = Net::new(0, 2);
        let first = net.push(Box::new(sine_hz::<f32>(440.0)));
        let second = net.push(Box::new(sine_hz::<f32>(880.0)));

        net.pipe_output(first);
        assert_eq!(
            net.output_source(0),
            Source::Local(first, 0),
            "the first claim lands"
        );

        net.pipe_output(second);
        assert_eq!(
            net.output_source(0),
            Source::Local(second, 0),
            "and the second overwrites it — this is the bug, not a mix"
        );
        assert_eq!(
            net.output_source(1),
            // `sine_hz` has one output, so `channel % node_outputs` wraps both
            // global channels onto port 0.
            Source::Local(second, 0),
            "on every channel, not just channel 0"
        );
    }

    /// The clobber reaches *all* global channels regardless of the source's width,
    /// which is why a mono node claiming the bus silences a stereo one on both
    /// sides rather than just the left.
    #[test]
    fn pipe_output_claims_every_channel_even_from_a_mono_source() {
        let mut net = Net::new(0, 2);
        let stereo = net.push(Box::new(sine_hz::<f32>(440.0) | sine_hz::<f32>(440.0)));
        let mono = net.push(Box::new(sine_hz::<f32>(880.0)));

        net.pipe_output(stereo);
        net.pipe_output(mono);

        // `pipe_output` wraps with `channel % node_outputs`, so a 1-output node
        // feeds both channels from its single port.
        assert_eq!(net.output_source(0), Source::Local(mono, 0));
        assert_eq!(net.output_source(1), Source::Local(mono, 0));
    }

    /// The sequence this crate used to produce: the build piped the metronome to
    /// output, then every soundfont that finished loading piped itself, taking the
    /// whole bus. Neither call site knew about the other.
    ///
    /// Both are gone — `engine/build.rs` no longer wires the click, and
    /// `synth/soundfont.rs` no longer wires the unit it promotes. This reproduces
    /// what they did, so the failure mode stays legible to whoever reads
    /// `MasterSources` and wonders why a resource rather than a helper.
    #[test]
    fn the_sequence_this_crate_used_to_produce_lost_the_metronome() {
        let mut net = Net::new(0, 2);

        // Stand-in for the click node `build_into` pipes to output.
        let click = net.push(Box::new(sine_hz::<f32>(1000.0)));
        net.pipe_output(click);

        // A soundfont finishes loading a few frames later.
        let soundfont = net.push(Box::new(sine_hz::<f32>(261.0)));
        net.pipe_output(soundfont);

        assert_ne!(
            net.output_source(0),
            Source::Local(click, 0),
            "the metronome is disconnected — silently, with nothing in the log"
        );
        assert_eq!(net.output_source(0), Source::Local(soundfont, 0));
    }
}
