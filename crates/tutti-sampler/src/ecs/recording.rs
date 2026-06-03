//! Sampler recording: `StartRecording` / `StopRecording` triggers.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::message::{Message, MessageReader};
use bevy_ecs::prelude::*;
use bevy_ecs::schedule::IntoScheduleConfigs;

use tutti_core::ecs::{engine_ready, TransportRes};

use super::SamplerRes;

/// Fire-and-forget request to start recording on a channel.
///
/// `recording_start_system` reads each `StartRecording`, calls
/// `engine.sampler().start_recording()`, and spawns an entity carrying
/// `RecordingActive`.
///
/// Not `Reflect`: `Source` / `Mode` are foreign types from `tutti-sampler`.
#[derive(Message, Debug, Clone, Copy)]
pub struct StartRecording {
    pub channel_index: usize,
    pub source: crate::capture::Source,
    pub mode: crate::capture::Mode,
}

impl StartRecording {
    pub fn new(channel_index: usize, source: crate::capture::Source) -> Self {
        Self {
            channel_index,
            source,
            mode: crate::capture::Mode::Replace,
        }
    }

    pub fn mode(mut self, mode: crate::capture::Mode) -> Self {
        self.mode = mode;
        self
    }
}

/// Fire-and-forget request to stop recording on a channel.
///
/// `recording_stop_system` reads each `StopRecording`, calls
/// `engine.sampler().stop_recording()`, removes the matching
/// `RecordingActive`, and spawns the `RecordingResult`.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StopRecording {
    pub channel_index: usize,
}

/// Marks an entity as having an active recording session.
///
/// Added automatically by `recording_start_system`. Removed when
/// `StopRecording` is processed or recording stops.
///
/// Not `Reflect`: `Source` / `Mode` are foreign types from `tutti-sampler`.
#[derive(Component, Debug, Clone, Copy)]
pub struct RecordingActive {
    pub channel_index: usize,
    pub source: crate::capture::Source,
    pub mode: crate::capture::Mode,
}

/// Holds the recorded data after a recording session completes.
///
/// Spawned by `recording_stop_system` on its own entity. Consume and
/// despawn the entity to process the recorded data.
#[derive(Component)]
pub struct RecordingResult(pub crate::capture::Recorded);

/// Processes `StartRecording` messages.
///
/// Calls `sampler.recording().start_recording()` with the current transport
/// beat, then spawns a `RecordingActive` entity to track the session.
pub fn recording_start_system(
    mut commands: Commands,
    sampler: Res<SamplerRes>,
    transport: Res<TransportRes>,
    mut events: MessageReader<StartRecording>,
) {
    for start in events.read() {
        match sampler.0.recording().start_recording(
            start.channel_index,
            start.source,
            start.mode,
            transport.current_beat(),
        ) {
            Ok(()) => {
                commands.spawn(RecordingActive {
                    channel_index: start.channel_index,
                    source: start.source,
                    mode: start.mode,
                });
                bevy_log::info!(
                    "Recording started on channel {} ({:?}, {:?})",
                    start.channel_index,
                    start.source,
                    start.mode
                );
            }
            Err(e) => {
                bevy_log::error!(
                    "Failed to start recording on channel {}: {}",
                    start.channel_index,
                    e
                );
            }
        }
    }
}

/// Processes `StopRecording` messages.
///
/// Calls `sampler.recording().stop_recording()`, removes the matching
/// `RecordingActive`, and spawns a `RecordingResult` carrying the captured
/// data.
pub fn recording_stop_system(
    mut commands: Commands,
    sampler: Res<SamplerRes>,
    mut events: MessageReader<StopRecording>,
    active_query: Query<(Entity, &RecordingActive)>,
) {
    for stop in events.read() {
        match sampler.0.recording().stop_recording(stop.channel_index) {
            Ok(data) => {
                bevy_log::info!(
                    "Recording stopped on channel {}, data captured",
                    stop.channel_index
                );
                for (active_entity, active) in active_query.iter() {
                    if active.channel_index == stop.channel_index {
                        commands.entity(active_entity).remove::<RecordingActive>();
                    }
                }
                commands.spawn(RecordingResult(data));
            }
            Err(e) => {
                bevy_log::error!(
                    "Failed to stop recording on channel {}: {}",
                    stop.channel_index,
                    e
                );
            }
        }
    }
}

/// Bevy plugin: sampler recording control.
pub struct TuttiRecordingPlugin;

impl Plugin for TuttiRecordingPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<StartRecording>()
            .add_message::<StopRecording>()
            .add_systems(
                Update,
                (recording_start_system, recording_stop_system).run_if(engine_ready),
            );
    }
}
