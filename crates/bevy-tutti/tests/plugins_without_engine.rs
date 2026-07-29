//! Every subsystem plugin composes without the engine bootstrap.
//!
//! # Why this exists
//!
//! In Bevy 0.19 a missing `Res<T>` is a **parameter-validation failure that
//! panics the schedule**, not a skipped system. So any system reading a resource
//! its own plugin does not insert is a crash waiting for the right combination
//! of plugins — and the combinations are what a host picks, not what this crate
//! tests.
//!
//! The trap is `engine_ready`. It reads `AudioEngineState`, which is a plain
//! resource *any host can insert*, while the dozen resources it is taken to
//! stand for come from `engine::build_into`. A doc claiming only two resources
//! needed `Option` let three MIDI systems, two graph observers, the latency
//! compensator, the soundfont promoter, the modulation driver and two plugin
//! binding systems take hard `Res` — every one of them a panic for a host that
//! declared the engine up without building one.
//!
//! Six of those were found by hand, one panic at a time, each fix revealing the
//! next. This test is that loop, automated: add every plugin, claim the engine
//! is running, and run frames. It fails loudly on the whole class rather than on
//! whichever instance someone happens to hit.
//!
//! # Mutating this test
//!
//! Revert any `Option<Res<_>>` in a system these plugins schedule back to a hard
//! `Res<_>` and this must fail. If it still passes, the plugin whose system you
//! broke is not being added here — add it.

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
