use bevy_ecs::message::MessageReader;
use bevy_ecs::prelude::*;

use crate::resources::SamplerRes;
use crate::transport::TransportState;

use super::components::{RecordingActive, StartRecording, StopRecording};

/// Holds the recorded data after a recording session completes.
///
/// Spawned by `recording_stop_system` on its own entity. Consume and
/// despawn the entity to process the recorded data.
#[derive(Component)]
pub struct RecordingResult(pub crate::sampler::capture::Recorded);

/// Processes `StartRecording` messages.
///
/// Calls `sampler.recording().start_recording()` with the current transport
/// beat, then spawns a `RecordingActive` entity to track the session.
pub fn recording_start_system(
    mut commands: Commands,
    sampler: Res<SamplerRes>,
    transport: Res<TransportState>,
    mut events: MessageReader<StartRecording>,
) {
    for start in events.read() {
        match sampler.0.recording().start_recording(
            start.channel_index,
            start.source,
            start.mode,
            transport.beat,
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
