//! MIDI files: **read as assets, written off the main thread**.
//!
//! ```ignore
//! // Read — a handle, like any other asset.
//! let handle: Handle<MidiFileAsset> = asset_server.load("song.mid");
//! // ...later, once loaded:
//! match &assets.get(&handle).unwrap().contents {
//!     MidiFileContents::Smf(tracks) => { /* ... */ }
//!     MidiFileContents::Clip(clip) => { /* ... */ }
//! }
//!
//! // Write — bytes some caller already encoded.
//! commands
//!     .spawn(MidiFileWrite::new("out.mid", bytes))
//!     .observe(|done: On<MidiFileWritten>| { /* ... */ });
//! ```
//!
//! # Why reads are assets and writes are not
//!
//! The two halves look symmetric and are not, so they use different mechanisms.
//!
//! **A read is an asset load**, and this crate already says so twice:
//! [`SoundFontAssetLoader`](crate::synth::SoundFontAssetLoader) and
//! `WaveAssetLoader` both take this route. Bevy itself asset-loads *shaders* —
//! small text files — which is the tell that size was never the criterion.
//! Being a file the app reads by path is. Going through `AssetLoader` buys the
//! things a hand-rolled reader has to reinvent badly: handle-based dedup so two
//! clips naming one file parse once, hot-reload, the `Handle` + `Assets<T>`
//! lifecycle every other loadable thing here already uses, and one convention
//! for a reader to learn instead of two.
//!
//! **A write is not**, because `AssetLoader` is read-only — there is no
//! asset-system path for "encode these bytes to that path". So the write half
//! keeps the request-entity + [`AsyncComputeTaskPool`] shape, which is the
//! argument [`crate::export`] already makes for `tutti-export`: a host wanting
//! IO off the main thread owns a task pool better at it than a raw
//! `std::thread`.
//!
//! An earlier version of this module put *both* halves on the task pool. That
//! was a mistake of omission rather than of judgement — the reasoning weighed
//! blocking-vs-async and never asked what the crate's existing convention for
//! reading a file was.
//!
//! # The codecs stay pure; this is the only part that touches a path
//!
//! Encoding and decoding are separately callable on byte slices, and callers
//! that already have bytes (a document exporter, a drag-and-drop payload)
//! should keep using those. [`MidiFileWrite`] takes `Vec<u8>` rather than
//! events for the same reason: choosing what to encode belongs to the caller,
//! and re-deciding it here would put format policy in the IO layer.

use std::path::PathBuf;

use bevy_app::{App, Plugin, Update};
use bevy_asset::{io::Reader, AssetApp, AssetLoader, LoadContext};
use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;
use bevy_tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};

use tutti_midi_io::clip::MidiFileKind;
use tutti_midi_io::smf::{tracks, SmfTrack};
use tutti_midi_io::ParsedClipFile;
// The byte-slice clip decoder lives in `tutti-midi-types`; `tutti-midi-io`
// re-imports it privately for its own path-taking wrapper.
use tutti_midi_types::read_clip_file;

/// What a decoded MIDI file carries.
///
/// Two variants because the formats genuinely differ in what they hold — an SMF
/// is a list of named tracks with 7-bit velocities, a Clip File is one stream
/// that keeps 16 bits. Collapsing them here would discard whichever half the
/// other lacks, in a layer whose job is only to move bytes.
#[derive(Debug)]
pub enum MidiFileContents {
    Smf(Vec<SmfTrack>),
    Clip(Box<ParsedClipFile>),
}

/// A decoded MIDI file, loadable as a Bevy asset.
///
/// Covers both containers, because they share an extension: `.mid` is worn by a
/// Standard MIDI File and a MIDI 2.0 Clip File alike, so which decoder to run
/// is decided by the leading magic bytes rather than by the file name. That is
/// also why this is one asset type and not two — an `AssetLoader` is selected
/// by extension, so two loaders both claiming `.mid` could not be told apart.
#[derive(Debug, bevy_asset::Asset, TypePath)]
pub struct MidiFileAsset {
    pub contents: MidiFileContents,
}

impl MidiFileAsset {
    /// Extensions the loader claims.
    ///
    /// `.midi` and `.mid` are the Standard MIDI File's; `.midi2` and `.mid2`
    /// are in use for Clip Files. All four are handled by the same loader,
    /// which sniffs rather than trusting any of them.
    pub const EXTENSIONS: &'static [&'static str] = &["mid", "midi", "mid2", "midi2"];

    /// Decode a complete MIDI file from an in-memory byte slice.
    ///
    /// Sniffs the container from its magic — `MThd` for an SMF, `SMF2CLIP` for
    /// a Clip File — because the extension cannot discriminate the two.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MidiFileLoaderError> {
        let contents = match MidiFileKind::sniff(bytes) {
            Some(MidiFileKind::StandardMidiFile) => MidiFileContents::Smf(tracks(bytes)?),
            Some(MidiFileKind::ClipFile) => {
                MidiFileContents::Clip(Box::new(read_clip_file(bytes)?))
            }
            // Distinct from a parse failure, and worth its own error: "this is
            // not a MIDI file" is a different thing for a user to fix than
            // "this MIDI file is malformed".
            None => return Err(MidiFileLoaderError::UnknownFormat),
        };
        Ok(Self { contents })
    }
}

/// Loader for [`MidiFileAsset`]. Reads the whole payload, then delegates to
/// [`MidiFileAsset::from_bytes`].
#[derive(Default, TypePath)]
pub struct MidiFileAssetLoader;

#[derive(Debug, thiserror::Error)]
pub enum MidiFileLoaderError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// An SMF that did not parse.
    ///
    /// The *file* error, not the I/O crate's: this is a decode failure, and
    /// nothing here opens a MIDI port.
    #[error(transparent)]
    Smf(#[from] tutti_midi_file::Error),
    /// A Clip File that did not parse. Kept distinct from [`Self::Smf`] because
    /// the two decoders have unrelated error vocabularies, and flattening them
    /// would lose which container was actually being read.
    #[error(transparent)]
    Clip(#[from] tutti_midi_types::ClipFileError),
    #[error("not a Standard MIDI File or a MIDI 2.0 Clip File")]
    UnknownFormat,
}

impl AssetLoader for MidiFileAssetLoader {
    type Asset = MidiFileAsset;
    type Settings = ();
    type Error = MidiFileLoaderError;

    async fn load(
        &self,
        reader: &mut dyn Reader,
        _settings: &Self::Settings,
        _load_context: &mut LoadContext<'_>,
    ) -> Result<Self::Asset, Self::Error> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        MidiFileAsset::from_bytes(&bytes)
    }

    fn extensions(&self) -> &[&str] {
        MidiFileAsset::EXTENSIONS
    }
}

/// Spawn an entity with this to write bytes to a file off the main thread.
///
/// Takes **encoded bytes**, not events — see the module docs for why the choice
/// of what to encode stays with the caller.
///
/// The request is consumed when the write starts: it is replaced by
/// [`MidiFileWriteInFlight`], and on completion [`MidiFileWritten`] is triggered
/// on the same entity. The entity is left in place for the caller to despawn.
#[derive(Component, Debug, Clone)]
pub struct MidiFileWrite {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
}

impl MidiFileWrite {
    pub fn new(path: impl Into<PathBuf>, bytes: Vec<u8>) -> Self {
        Self {
            path: path.into(),
            bytes,
        }
    }
}

/// A write whose IO is running on the task pool.
///
/// Replaces the request component so [`start_midi_file_writes`]'s query does not
/// see it again — the same one-shot shape `PlaySoundFont` → `PendingSoundFontUnit`
/// uses, and for the same reason: a trigger left in place re-fires every frame.
#[derive(Component)]
pub struct MidiFileWriteInFlight {
    task: Task<std::io::Result<()>>,
}

impl MidiFileWriteInFlight {
    fn poll(&mut self) -> Option<std::io::Result<()>> {
        block_on(future::poll_once(&mut self.task))
    }
}

/// Triggered on the request entity when its write finishes, successfully or not.
///
/// An entity event rather than a result component, mirroring
/// [`ExportDone`](crate::export::ExportDone): a result is handled **once**, and
/// polling a `Query<&Output>` every frame while removing the component to avoid
/// re-handling it is a hand-rolled one-shot. Observing at the spawn site also
/// keeps the surrounding context in scope.
#[derive(EntityEvent, Debug)]
pub struct MidiFileWritten {
    pub entity: Entity,
    pub result: std::io::Result<()>,
}

/// Move every new write request onto the task pool.
///
/// No cap on in-flight writes, unlike [`ExportInFlight`](crate::export::ExportInFlight)
/// which admits one render at a time because each deep-clones the live net.
/// Nothing here touches the graph: the main-thread cost of a request is moving a
/// `PathBuf` and a `Vec<u8>` onto the pool, so a queue would add latency and
/// prevent nothing.
pub fn start_midi_file_writes(
    mut commands: Commands,
    requests: Query<(Entity, &MidiFileWrite), Without<MidiFileWriteInFlight>>,
) {
    let pool = AsyncComputeTaskPool::get();
    for (entity, request) in requests.iter() {
        let path = request.path.clone();
        // Cloned rather than moved out of the component, because the component
        // is removed by a *deferred* command below — it is still borrowed here.
        // A MIDI file is kilobytes, so this is cheap; a caller writing
        // something genuinely large should hand over a path, not a buffer.
        let bytes = request.bytes.clone();
        let task = pool.spawn(async move { std::fs::write(&path, &bytes) });
        commands
            .entity(entity)
            .remove::<MidiFileWrite>()
            .insert(MidiFileWriteInFlight { task });
    }
}

/// Drive in-flight writes; trigger [`MidiFileWritten`] on the ones that finished.
pub fn poll_midi_file_writes(
    mut commands: Commands,
    mut in_flight: Query<(Entity, &mut MidiFileWriteInFlight)>,
) {
    for (entity, mut request) in in_flight.iter_mut() {
        let Some(result) = request.poll() else {
            continue; // still running — a normal steady state
        };
        commands
            .entity(entity)
            .remove::<MidiFileWriteInFlight>()
            .trigger(move |entity: Entity| MidiFileWritten { entity, result });
    }
}

/// Registers the [`MidiFileAsset`] loader and the write systems.
pub struct MidiFilePlugin;

impl Plugin for MidiFilePlugin {
    fn build(&self, app: &mut App) {
        app.init_asset::<MidiFileAsset>()
            .register_asset_loader(MidiFileAssetLoader);
        app.add_systems(
            Update,
            (start_midi_file_writes, poll_midi_file_writes).chain(),
        );
    }
}

#[cfg(test)]
mod tests;
