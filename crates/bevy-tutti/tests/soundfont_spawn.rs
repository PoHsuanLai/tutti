//! The SoundFont spawner binds its entity to the graph with one handle.
//!
//! `promote_pending_soundfonts` used to insert `AudioNode` *and* an
//! `AudioEmitter` carrying the same `NodeId`, with a comment conceding that
//! teardown keys on the former. `AudioEmitter` is gone; this pins what replaced
//! it, and covers a spawner that had no test at all — the soundfont audio tests
//! next door build their node by hand and never reach this path.
//!
//! Skipped when the soundfont is absent, following `synth/soundfont.rs`'s own
//! tests: the asset is in the repo, but a consumer checking out this crate
//! alone may not have it.

#![cfg(all(feature = "soundfont", feature = "midi"))]

use std::path::PathBuf;

use bevy_app::prelude::*;
use bevy_asset::{AssetPlugin, Assets, Handle};

use bevy_tutti::graph::{AudioConfig, AudioGraphRes, GraphReconcilePlugin, TransportRes};
use bevy_tutti::soundfont::SoundFontAsset;
use bevy_tutti::soundfont::{PlaySoundFont, TuttiSoundFontPlugin};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::Net;
use tutti_core::AudioNode;

const SAMPLE_RATE: f64 = 48_000.0;

/// The repo's test soundfont, decoded, or `None` on a checkout without it.
fn soundfont_asset() -> Option<SoundFontAsset> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()? // crates/
        .parent()? // repo root
        .join("crates/tutti/assets/soundfonts/TimGM6mb.sf2");
    let bytes = std::fs::read(path).ok()?;
    SoundFontAsset::from_bytes(&bytes).ok()
}

/// An app with the soundfont plugin and the engine resources its systems gate on.
fn app() -> App {
    let mut app = App::new();
    app.add_plugins((bevy_app::TaskPoolPlugin::default(), AssetPlugin::default()));

    let mut net = Net::new(0, 2);
    let _backend = net.backend();
    app.insert_resource(AudioGraphRes(net));
    app.insert_resource(TransportRes(tutti_core::transport::Transport::new(
        SAMPLE_RATE,
    )));
    app.insert_resource(AudioConfig {
        sample_rate: SAMPLE_RATE,
        channels: Default::default(),
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
    let Some(asset) = soundfont_asset() else {
        eprintln!("skipping: TimGM6mb.sf2 not present");
        return;
    };
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

    let node = app.world().get::<AudioNode>(entity).expect("AudioNode").0;
    assert!(
        app.world().resource::<AudioGraphRes>().0.contains(node),
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
    let Some(asset) = soundfont_asset() else {
        eprintln!("skipping: TimGM6mb.sf2 not present");
        return;
    };
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
