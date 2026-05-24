use bevy_ecs::prelude::*;

use crate::resources::SamplerRes;
use crate::transport::TransportState;

use super::components::{RecordingActive, StartRecording, StopRecording};

/// Holds the recorded data after a recording session completes.
///
/// Inserted by `recording_stop_system` on the entity that had `StopRecording`.
/// Consume and remove this component to process the recorded data.
#[derive(Component)]
pub struct RecordingResult(pub tutti::sampler::capture::Recorded);

/// Processes `StartRecording` trigger components.
///
/// Calls `sampler.recording().start_recording()` with the current transport beat,
/// replaces the trigger with `RecordingActive`.
pub fn recording_start_system(
    mut commands: Commands,
    sampler: Option<Res<SamplerRes>>,
    transport: Res<TransportState>,
    query: Query<(Entity, &StartRecording), Added<StartRecording>>,
) {
    let Some(sampler) = sampler else { return };

    for (entity, start) in query.iter() {
        match sampler.0.recording().start_recording(
            start.channel_index,
            start.source,
            start.mode,
            transport.beat,
        ) {
            Ok(()) => {
                commands
                    .entity(entity)
                    .remove::<StartRecording>()
                    .insert(RecordingActive {
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
                commands.entity(entity).remove::<StartRecording>();
            }
        }
    }
}

/// Processes `StopRecording` trigger components.
///
/// Calls `sampler.recording().stop_recording()`, removes `RecordingActive`,
/// and logs the result. The `RecordedData` is available in the log;
/// for programmatic access, use the direct sampler API.
pub fn recording_stop_system(
    mut commands: Commands,
    sampler: Option<Res<SamplerRes>>,
    query: Query<(Entity, &StopRecording), Added<StopRecording>>,
    active_query: Query<(Entity, &RecordingActive)>,
) {
    let Some(sampler) = sampler else { return };

    for (entity, stop) in query.iter() {
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
                commands
                    .entity(entity)
                    .remove::<StopRecording>()
                    .insert(RecordingResult(data));
            }
            Err(e) => {
                bevy_log::error!(
                    "Failed to stop recording on channel {}: {}",
                    stop.channel_index,
                    e
                );
                commands.entity(entity).remove::<StopRecording>();
            }
        }
    }
}
