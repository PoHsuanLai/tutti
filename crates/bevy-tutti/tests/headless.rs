//! `TuttiPlugin { disabled: true }` must build an app on a machine with no
//! sound card.
//!
//! This is what the state enum buys beyond nicer error reporting: before it,
//! adding the plugin meant opening a device, so a CI runner or a headless test
//! could not add it at all — and therefore could not add any host plugin that
//! schedules against its system sets.

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
