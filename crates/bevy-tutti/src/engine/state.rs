//! [`AudioEngineState`] — whether the audio engine is running, and if not, why.

use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

/// The engine's lifecycle state.
///
/// Inserted by [`TuttiPlugin`](crate::TuttiPlugin) during plugin build, before
/// frame 1, and never absent afterwards. This is the single source of truth for
/// "may audio systems run" — [`engine_ready`](crate::graph::engine_ready) reads
/// it rather than probing for a resource, so a host cannot end up with the two
/// disagreeing.
///
/// A failed engine used to be invisible: `build_into` logged an error and the
/// app ran on in silence with nothing in the world to show for it. A UI can now
/// query this and say so.
///
/// ```rust,ignore
/// fn audio_status_ui(state: Res<AudioEngineState>) {
///     match &*state {
///         AudioEngineState::Running => {}
///         AudioEngineState::Failed(why) => warn_banner(format!("No audio: {why}")),
///         AudioEngineState::Disabled => {}
///     }
/// }
/// ```
#[derive(Resource, Reflect, Debug, Clone, PartialEq, Eq, Default)]
#[reflect(Resource, Debug, Default)]
pub enum AudioEngineState {
    /// The device is open, the graph is built, and the callback is live.
    Running,
    /// The engine could not be built. Carries the rendered error.
    ///
    /// A message rather than the error itself: consumers here want to *show*
    /// the failure, and a `String` stays `Clone` + `Reflect` where
    /// [`Error`](crate::Error) is neither (it wraps `std::io::Error`, which
    /// cannot be cloned).
    Failed(String),
    /// No engine was requested. Not a failure — headless and CI runs take this
    /// path deliberately, and it is the default so a `World` without
    /// [`TuttiPlugin`](crate::TuttiPlugin) reports something truthful.
    #[default]
    Disabled,
}

impl AudioEngineState {
    /// Whether the callback is live.
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }

    /// Why the engine is not running, if it failed. `None` when running or
    /// deliberately disabled — "disabled" is a choice, not an error.
    pub fn failure(&self) -> Option<&str> {
        match self {
            Self::Failed(why) => Some(why),
            _ => None,
        }
    }
}
