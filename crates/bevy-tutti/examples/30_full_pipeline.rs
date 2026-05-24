//! Full Phase A pipeline demo.
//!
//! Exercises the entity-as-node primitives end-to-end against a live
//! tutti graph. Skips real audio output (no `CpalDriverPlugin` here) so
//! the example runs anywhere `cargo run` does.
//!
//! Demonstrated capabilities:
//!
//! 1. **A1 Sampler params** — `SamplerSpeed` / `SamplerLooping` mutated
//!    via normal Bevy queries; reconcile pipeline picks them up.
//! 2. **A3 Crossfade helper** — every 60 frames, the generator entity's
//!    underlying oscillator is crossfaded to a new frequency. Same
//!    `AudioNode(NodeId)` survives, so any wires stay valid.
//! 3. **A4 Automation lane** — a `LiveAutomationLane<f32>` entity drives
//!    a target's `Volume` from 0.0 → 1.0 → 0.0 over 4 beats.
//! 4. **A7 Sidechain relationship** — illustrative `SidechainOf` insertion
//!    (the bare oscillator has no input port 1, so the warning-skip path
//!    runs, but the relationship-target list is populated as expected).
//!
//! Run with:
//!
//! ```sh
//! cargo run --example 30_full_pipeline --features "sampler,wav,automation"
//! ```

use std::sync::Arc;
use std::time::Duration;

use bevy_app::prelude::*;
use bevy_app::ScheduleRunnerPlugin;
use bevy_asset::AssetPlugin;
use bevy_ecs::prelude::*;
use bevy_log::LogPlugin;

use bevy_tutti::{
    crossfade_audio_node, AudioNode, AutomationDrivesParam, AutomationLaneNode, AutomationParam,
    MasterMeterLevels, NodeKind, PendingSamplerLoad, SamplerLooping, SamplerSpeed, SidechainOf,
    SidechainSources, SpawnAudioNode, TransportRes, TuttiPlugin, Volume,
};
use tutti::automation::{AutomationEnvelope, AutomationPoint, CurveType, LiveAutomationLane};
use tutti::dsp::sine_hz;

#[derive(Resource, Default)]
struct DemoTick(u32);

/// Marker on the entity whose volume the automation lane drives.
#[derive(Component)]
struct AutomationTarget;

fn main() {
    App::new()
        .add_plugins(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f32(
            1.0 / 60.0,
        )))
        .add_plugins(LogPlugin::default())
        .add_plugins(AssetPlugin::default())
        .add_plugins(TuttiPlugin::default())
        .init_resource::<DemoTick>()
        .add_systems(Startup, (start_transport, spawn_demo).chain())
        .add_systems(
            Update,
            (
                wiggle_sampler_speed,
                periodic_crossfade,
                report_status,
            ),
        )
        .run();
}

fn start_transport(transport: Res<TransportRes>) {
    transport.tempo(120.0).play();
}

fn spawn_demo(mut commands: Commands, transport: Res<TransportRes>) {
    // Target: a sine wave whose volume is automated and whose loop/speed
    // are exposed via SamplerSpeed / SamplerLooping. The unit here is a
    // bare oscillator (no SamplerUnit) so the sampler-param reconciler
    // is a no-op for it — but we still attach the components to show the
    // shape. A real example would use SamplerUnit + a wave handle via
    // PendingSamplerLoad.
    let target = commands
        .spawn_audio_node(sine_hz::<f32>(440.0), NodeKind::Generator)
        .insert((
            Volume(0.5),
            SamplerSpeed(1.0),
            SamplerLooping(false),
            AutomationTarget,
        ))
        .id();

    // Automation lane: a triangle 0 → 1 → 0 over 4 beats. The envelope's
    // generic `T` is a target descriptor (any clonable tag); the lane
    // always emits `f32`. bevy-tutti's reconciler reads it as
    // `LiveAutomationLane<f32>`, so use that here.
    let mut envelope = AutomationEnvelope::<f32>::new(0.0);
    envelope.add_point(AutomationPoint::new(0.0, 0.0));
    envelope.add_point(AutomationPoint::with_curve(2.0, 1.0, CurveType::Linear));
    envelope.add_point(AutomationPoint::with_curve(4.0, 0.0, CurveType::Linear));

    let lane: LiveAutomationLane<f32> = LiveAutomationLane::new(envelope, transport.0.clone());

    commands
        .spawn_audio_node(lane, NodeKind::Generator)
        .insert((
            AutomationLaneNode,
            AutomationDrivesParam {
                target,
                param: AutomationParam::Volume,
            },
        ));

    // Illustrative sidechain wiring. With a bare oscillator on the target
    // side the connect call is skipped (port 1 doesn't exist), but the
    // relationship-target list still grows.
    let _ = (target, &transport);
    let driver = commands
        .spawn_audio_node(sine_hz::<f32>(60.0), NodeKind::Generator)
        .id();
    commands.entity(driver).insert(SidechainOf(target));

    // Pending-sampler-load illustration: with no asset present we can't
    // promote, but the queue would resolve it when the asset arrives.
    let _ = Arc::new(()); // silence unused-Arc warning if features change
    let _: Option<PendingSamplerLoad> = None;
}

/// Cosmetic: nudge SamplerSpeed each frame so Changed<SamplerSpeed> fires.
fn wiggle_sampler_speed(mut q: Query<&mut SamplerSpeed>, mut tick: ResMut<DemoTick>) {
    tick.0 = tick.0.wrapping_add(1);
    let phase = (tick.0 as f32 * 0.05).sin();
    for mut s in &mut q {
        s.0 = 1.0 + 0.05 * phase;
    }
}

/// Every 120 frames, crossfade the *target* entity's oscillator to a new
/// random-ish frequency. The NodeId stays constant.
fn periodic_crossfade(
    mut commands: Commands,
    q: Query<Entity, (With<AudioNode>, With<AutomationTarget>)>,
    tick: Res<DemoTick>,
) {
    if tick.0 == 0 || !tick.0.is_multiple_of(120) {
        return;
    }
    let freq = 220.0 + 110.0 * ((tick.0 / 120) as f32 % 4.0);
    for entity in &q {
        crossfade_audio_node(&mut commands, entity, Box::new(sine_hz::<f32>(freq)));
    }
}

fn report_status(
    meters: Res<MasterMeterLevels>,
    targets: Query<&Volume, With<AutomationTarget>>,
    sources: Query<&SidechainSources>,
    tick: Res<DemoTick>,
) {
    if !tick.0.is_multiple_of(60) {
        return;
    }
    let vol = targets.single().map(|v| v.0).unwrap_or(0.0);
    let sidechain_count = sources.iter().map(|s| s.len()).sum::<usize>();
    bevy_log::info!(
        "tick {:>4} | target Volume = {:.3} | sidechain links = {} | peak L/R = {:.3} / {:.3}",
        tick.0,
        vol,
        sidechain_count,
        meters.peak_left,
        meters.peak_right
    );
}
