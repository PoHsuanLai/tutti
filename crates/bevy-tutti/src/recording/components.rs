use bevy_ecs::message::Message;
use bevy_ecs::prelude::*;

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
    pub source: crate::sampler::capture::Source,
    pub mode: crate::sampler::capture::Mode,
}

impl StartRecording {
    pub fn new(channel_index: usize, source: crate::sampler::capture::Source) -> Self {
        Self {
            channel_index,
            source,
            mode: crate::sampler::capture::Mode::Replace,
        }
    }

    pub fn mode(mut self, mode: crate::sampler::capture::Mode) -> Self {
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
    pub source: crate::sampler::capture::Source,
    pub mode: crate::sampler::capture::Mode,
}
