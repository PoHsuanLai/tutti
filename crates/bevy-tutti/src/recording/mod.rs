//! Sampler recording: `StartRecording` / `StopRecording` triggers.

use bevy_app::{App, Plugin, Update};

mod components;
mod systems;

pub use components::{RecordingActive, StartRecording, StopRecording};
pub use systems::{recording_start_system, recording_stop_system, RecordingResult};

/// Bevy plugin: sampler recording control.
pub struct TuttiRecordingPlugin;

impl Plugin for TuttiRecordingPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, (recording_start_system, recording_stop_system));
    }
}
