//! MIDI files: **read as assets, written off the main thread**.
//!
//! **Write** — the request is an entity, and the result arrives on it. The bytes
//! are the caller's: nothing here encodes, for the reason the last section
//! gives.
//!
//! ```rust
//! use std::sync::{Arc, Mutex};
//!
//! use bevy_app::prelude::*;
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::midi::{MidiFilePlugin, MidiFileWrite, MidiFileWritten};
//! use tutti_core::Beat;
//! use tutti_midi_file::{encode_midi_file, MidiWriteConfig, SmfMessage, SmfTimedEvent};
//!
//! let bytes = encode_midi_file(
//!     &[vec![SmfTimedEvent {
//!         time_beats: Beat(0.0),
//!         channel: 0,
//!         msg: SmfMessage::NoteOn { key: 60.into(), vel: 100.into() },
//!     }]],
//!     &MidiWriteConfig::default(),
//! )
//! .expect("one track encodes");
//!
//! let dir = tempfile::tempdir().expect("a temp dir");
//! let landed: Arc<Mutex<Option<bool>>> = Arc::default();
//! let seen = landed.clone();
//!
//! let mut app = App::new();
//! app.add_plugins((
//!     bevy_app::TaskPoolPlugin::default(),
//!     // The read half registers an asset loader, so an `AssetServer` must exist.
//!     bevy_asset::AssetPlugin::default(),
//!     MidiFilePlugin,
//! ));
//! app.world_mut()
//!     .spawn(MidiFileWrite::new(dir.path().join("out.mid"), bytes.clone()))
//!     .observe(move |done: On<MidiFileWritten>| {
//!         *seen.lock().unwrap() = Some(done.result.is_ok());
//!     });
//!
//! // The IO runs on the task pool, so the app keeps ticking until it reports.
//! for _ in 0..2000 {
//!     app.update();
//!     if landed.lock().unwrap().is_some() {
//!         break;
//!     }
//!     std::thread::sleep(std::time::Duration::from_millis(2));
//! }
//! assert_eq!(*landed.lock().unwrap(), Some(true));
//!
//! // **Read** — a host takes `asset_server.load("song.mid")` and matches on the
//! // loaded asset's `contents`. The loader is this call with the file's bytes,
//! // which is the half worth showing without a file on disk:
//! use bevy_tutti::midi::{MidiFileAsset, MidiFileContents};
//!
//! let asset = MidiFileAsset::from_bytes(&bytes).expect("the bytes we just encoded");
//! match &asset.contents {
//!     // Which arm you land in is decided by the leading magic bytes, not the
//!     // extension — both containers wear `.mid`.
//!     MidiFileContents::Smf(tracks) => assert_eq!(tracks.len(), 1),
//!     MidiFileContents::Clip(_) => panic!("that was an SMF"),
//! }
//! ```
//!
//! # Why reads are assets and writes are not
//!
//! The two halves look symmetric and are not, so they use different mechanisms.
//!
//! **A read is an asset load**, and this crate already says so twice:
//! [`SoundFontAssetLoader`](crate::soundfont::SoundFontAssetLoader) and
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

// Straight from `tutti-midi-file`, not through `tutti-midi-hardware`'s re-export.
// This module decodes bytes an asset loader already read; it opens no MIDI port,
// so it must not depend on the crate that does — `midi-hardware` gates that one,
// and reaching through it makes *reading a `.mid` file* require an OS MIDI layer.
use tutti_midi_file::clip::MidiFileKind;
use tutti_midi_file::smf::{tracks, SmfTrack};
use tutti_midi_file::ParsedClipFile;
// The byte-slice clip decoder lives one crate further down, in
// `tutti-midi-types`; `tutti-midi-file` wraps it for the path-taking form.
use tutti_midi_types::read_clip_file;

/// What a decoded MIDI file carries.
///
/// Two variants because the formats genuinely differ in what they hold — an SMF
/// is a list of named tracks with 7-bit velocities, a Clip File is one stream
/// that keeps 16 bits. Collapsing them here would discard whichever half the
/// other lacks, in a layer whose job is only to move bytes.
#[derive(Debug)]
pub enum MidiFileContents {
    /// A Standard MIDI File: named tracks, 7-bit velocities.
    Smf(Vec<SmfTrack>),
    /// A MIDI 2.0 Clip File: one stream, keeping 16-bit velocities.
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
    /// The decoded file, in whichever of the two containers it turned out to be.
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

/// Why a [`MidiFileAsset`] load failed: unreadable, unrecognised, or malformed.
#[derive(Debug, thiserror::Error)]
pub enum MidiFileLoaderError {
    /// The bytes could not be read at all.
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
    /// The leading magic matched neither container, so no decoder was run.
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
    /// Where to write. Created or truncated; no directories are made.
    pub path: PathBuf,
    /// The encoded file, whole — this is written verbatim.
    pub bytes: Vec<u8>,
}

impl MidiFileWrite {
    /// A request to write `bytes` to `path`. Spawn it on an entity to start the
    /// write.
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
    /// The request entity, still alive — the caller despawns it.
    pub entity: Entity,
    /// Whether the write landed. An `Err` here is the filesystem's, never an
    /// encoding failure: nothing in this module inspects the bytes.
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
