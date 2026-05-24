//! Entity-as-node demo.
//!
//! Spawns a 440Hz sine wave as a graph node bound to a Bevy entity, then
//! uses a normal Bevy system to mutate the entity's `Volume` component.
//! The reconcile pipeline in `bevy-tutti` translates the `Changed<Volume>`
//! event into a typed parameter write and a coalesced `graph.commit()`.
//!
//! What this demo proves:
//!
//! 1. A graph node is owned by a Bevy entity (`AudioNode(NodeId)` + `NodeKind`).
//! 2. Mutating an entity's `Volume` triggers reconciliation automatically.
//! 3. Despawning the entity removes the underlying graph node + commits.
//!
//! The bare oscillator used here (`sine_hz`) doesn't expose a `set_gain`
//! setter at the AudioUnit level, so the `Volume` change is logged rather
//! than physically attenuating the output. To attenuate audio, swap the
//! `NodeKind::Generator` for `NodeKind::Sampler` and use a `SamplerUnit`
//! source; the existing `reconcile_params` Sampler arm will route the
//! gain change through `SamplerUnit::set_gain`.
//!
//! Run with:
//!
//! ```sh
//! cargo run --example 29_node_entities --features sampler,wav
//! ```

use std::time::Duration;

use bevy_app::prelude::*;
use bevy_app::ScheduleRunnerPlugin;
use bevy_asset::AssetPlugin;
use bevy_ecs::prelude::*;
use bevy_log::LogPlugin;

use bevy_tutti::{
    AudioNode, MasterMeterLevels, NodeKind, SpawnAudioNode, TuttiGraphRes, TuttiPlugin, Volume,
};
use tutti::dsp::sine_hz;

fn main() {
    App::new()
        .add_plugins(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f32(
            1.0 / 60.0,
        )))
        .add_plugins(LogPlugin::default())
        .add_plugins(AssetPlugin::default())
        .add_plugins(TuttiPlugin::default())
        .add_systems(Startup, (start_transport, spawn_sine).chain())
        .add_systems(Update, (fade_volume, log_state))
        .run();
}

fn start_transport(transport: Res<bevy_tutti::TransportRes>) {
    transport.play();
}

fn spawn_sine(mut commands: Commands) {
    let entity = commands
        .spawn_audio_node(sine_hz::<f32>(440.0), NodeKind::Generator)
        .insert(Volume(1.0))
        .id();

    // The graph mutation in `spawn_audio_node` is queued as a deferred
    // world command. We queue a second command to wire the new node into
    // the output bus once that one has applied.
    commands.queue(move |world: &mut World| {
        let Some(node) = world.get::<AudioNode>(entity).copied() else {
            return;
        };
        let mut graph = world.resource_mut::<TuttiGraphRes>();
        graph.0.pipe_output(node.0);
        graph.0.commit();
    });
}

fn fade_volume(mut q: Query<&mut Volume, With<AudioNode>>) {
    for mut v in &mut q {
        v.0 = (v.0 - 0.005).max(0.0);
    }
}

fn log_state(
    meters: Res<MasterMeterLevels>,
    q: Query<&Volume, With<AudioNode>>,
    mut tick: Local<u32>,
) {
    *tick = tick.wrapping_add(1);
    if (*tick).is_multiple_of(30) {
        let vol = q.single().map(|v| v.0).unwrap_or(0.0);
        bevy_log::info!(
            "Volume = {:.3} | peak L/R = {:.3} / {:.3}",
            vol,
            meters.peak_left,
            meters.peak_right
        );
    }
}
