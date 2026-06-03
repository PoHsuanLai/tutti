//! Bevy `Resource` wrappers around the flat `TuttiEngine` bundle.
//!
//! Each subsystem of the engine is surfaced as its own resource so systems
//! can take only the ones they need. The wrappers are thin newtypes; most
//! provide `Deref` to the inner handle.
//!
//! `TuttiGraphRes` skips `Deref` so `.0` access keeps the per-frame
//! commit boundary visible. `TuttiDriverRes` is a non-send resource
//! (`cpal::Stream` is not `Sync`) — accessed via `NonSend` /
//! `NonSendMut`, not `Res` / `ResMut`.

#[cfg(any(
    feature = "midi",
    feature = "midi-hardware",
    feature = "plugin",
    feature = "analysis",
    feature = "soundfont"
))]
use bevy_ecs::prelude::*;

#[cfg(feature = "soundfont")]
use std::sync::Arc;

#[cfg(feature = "midi")]
use crate::midi_runtime::MidiBus;
#[cfg(feature = "midi-hardware")]
use tutti_midi_io::MidiIo;
use crate::TuttiDriver;

// The leaf-agnostic engine resources moved into tutti-core's ECS hub; re-export
// them so existing `crate::resources::{AudioConfig, TuttiGraphRes, …}` paths
// hold. The driver + feature resources below stay here (they wrap CPAL /
// leaf-crate types).
pub use tutti_core::ecs::{AudioConfig, MeteringRes, TransportRes, TuttiGraphRes};

/// Owns the CPAL stream lifecycle (device selection, restart, enumeration).
///
/// `TuttiDriver` holds a `cpal::Stream` which is `Send` but **not** `Sync`
/// on every CPAL backend that matters (CoreAudio, ALSA — Stream wraps a
/// thread-pinned platform handle). Inserted as a non-send resource and
/// accessed via `NonSend<TuttiDriverRes>` / `NonSendMut<TuttiDriverRes>`
/// so Bevy pins the systems that touch it to the main thread — same
/// pattern Bevy uses for `Window` / `AudioOutput`. Driver operations
/// (`set_device`, `restart`, device enumeration) are user-driven and
/// infrequent, so the main-thread pin has no perf impact.
pub struct TuttiDriverRes(pub TuttiDriver);

impl TuttiDriverRes {
    pub fn new(driver: TuttiDriver) -> Self {
        Self(driver)
    }
}

/// MIDI fan-out bus — audio-thread event dispatch to per-unit inboxes.
#[cfg(feature = "midi")]
#[derive(Resource, Clone)]
pub struct MidiBusRes(pub MidiBus);

#[cfg(feature = "midi")]
impl std::ops::Deref for MidiBusRes {
    type Target = MidiBus;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Hardware MIDI I/O (OS port management + virtual ports). Only present
/// when `.midi()` was called on the builder.
#[cfg(feature = "midi-hardware")]
#[derive(Resource, Clone)]
pub struct MidiIoRes(pub MidiIo);

#[cfg(feature = "midi-hardware")]
impl std::ops::Deref for MidiIoRes {
    type Target = MidiIo;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// SoundFont system (file cache + synth instantiation).
#[cfg(feature = "soundfont")]
#[derive(Resource, Clone)]
pub struct SoundFontRes(pub Arc<crate::synth::SoundFontSystem>);

#[cfg(feature = "soundfont")]
impl std::ops::Deref for SoundFontRes {
    type Target = crate::synth::SoundFontSystem;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Analysis handle (transient / pitch / stereo analysis).
///
/// `AnalysisHandle` is not `Clone` upstream.
#[cfg(feature = "analysis")]
#[derive(Resource)]
pub struct AnalysisRes(pub tutti_analysis::AnalysisHandle);

#[cfg(feature = "analysis")]
impl std::ops::Deref for AnalysisRes {
    type Target = tutti_analysis::AnalysisHandle;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Non-Send marker resource that forces plugin editor systems to run on the
/// main thread. AppKit (macOS), Win32, and X11 window operations must happen
/// on the main thread. JUCE, VSTGUI, and other plugin GUI frameworks assume
/// this. Inserted as `insert_non_send_resource` so any system that takes
/// `NonSend<PluginEditorMainThread>` is pinned to the main thread.
#[cfg(feature = "plugin")]
pub struct PluginEditorMainThread;

/// The plugin discovery + loading catalog. Owns the on-disk DB and the
/// scan-dir config; systems reach in to `register_bundled_plugin`,
/// `unregister_bundled_plugins`, `rescan`, etc.
///
/// `Plugins` is `Send + Sync` (the `PluginCatalog` trait carries
/// `Send + Sync` supertraits, which propagate through `Box<dyn ...>`), so
/// Bevy's `ResMut<PluginsRes>` exclusivity is the only synchronization
/// needed — no extra `Mutex`.
#[cfg(feature = "plugin")]
#[derive(Resource)]
pub struct PluginsRes(pub tutti_plugin::catalog::Plugins);

#[cfg(feature = "plugin")]
impl PluginsRes {
    pub fn new(plugins: tutti_plugin::catalog::Plugins) -> Self {
        Self(plugins)
    }
}
