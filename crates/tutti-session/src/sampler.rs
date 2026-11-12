//! The session's sampler (design 030, A5): a disk stream opened on the
//! session's spawner, and what lands from it in
//! [`Session::update`](crate::Session::update).
//!
//! The streamer opens a file on a thread of its own, so an open takes as long
//! as the file's header and its first reads. [`Session::open_stream`] starts
//! one and returns at once; the stream lands as
//! [`SamplerEvent::Opened`] with the call's [`SamplerOpen`], under
//! [`SessionEvent::Sampler`] (design 030,
//! convention 1), or as [`SamplerEvent::OpenFailed`] with the reason.

use std::path::PathBuf;

use tutti_sampler::{Clip, OpenError, PlaceError, Placement, ProbeError, Stream, StreamReader};

use crate::jobs::JobFailed;
use crate::{Session, SessionEvent};

/// An open [`Session::open_stream`] started, named so the event that lands it
/// can be matched to the call: [`SamplerEvent::Opened`] and
/// [`SamplerEvent::OpenFailed`] carry it. The session mints it; it names
/// nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SamplerOpen(u64);

/// Past how many bytes of decoded audio [`Residency::Auto`] streams a file
/// instead of holding it: 32 MiB of 32-bit floats, 95 seconds of 44.1 kHz
/// stereo (about 8 million frames of one channel).
///
/// A resident clip costs its decoded size for as long as the wave lives, and a
/// session holds one per file in use; a stream costs a ring and a decode on
/// the streamer's thread whatever the file's length. Samples, loops and stems
/// of a minute or so sit under the line, where a wave in memory seeks and
/// loops with no read behind it; a song or a long take sits over it, where a
/// few of them would take a gigabyte (the streamer's own cache is capped at
/// 1 GiB) for audio the transport reads once through.
pub const STREAM_ABOVE_BYTES: u64 = 32 * 1024 * 1024;

/// Which tier [`Session::load_clip`] plays a file from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Residency {
    /// By the file's size: resident up to [`STREAM_ABOVE_BYTES`] of decoded
    /// audio, streamed past it. Resident whatever its size when the file
    /// cannot be streamed, its header gives no length to decide by, or the
    /// session holds no disk streamer.
    #[default]
    Auto,
    /// Decoded whole into memory, whatever its size.
    Resident,
    /// Streamed from disk, whatever its size.
    Streamed,
}

/// A sampler event of the session, one per open or load that ended.
#[derive(Debug)]
#[non_exhaustive]
pub enum SamplerEvent {
    /// An open [`Session::open_stream`] started finished: the stream is
    /// running, and its two halves are the host's. The reader is the voice's
    /// (`DiskVoice::from_reader`); the stream is the control side, which a
    /// `VoiceNode::streaming` or a pool's `add_streaming` takes, so the voice
    /// sounds for as long as the node or the pool slot lives. Dropping the
    /// stream ends it.
    Opened {
        /// The id [`Session::open_stream`] returned.
        id: SamplerOpen,
        /// The control side of the stream.
        stream: Stream,
        /// The voice's side of the stream.
        reader: StreamReader,
    },
    /// A clip [`Session::load_clip`] started is ready to add to the graph
    /// (`Session::graph().add(clip)`), resident or streamed as the load
    /// decided ([`Clip::is_streamed`]).
    ClipLoaded {
        /// The id [`Session::load_clip`] returned.
        id: SamplerOpen,
        /// The clip, at beat 0 until placed.
        clip: Clip,
    },
    /// An open or a load did not finish, and will not.
    OpenFailed {
        /// The id [`Session::open_stream`] or [`Session::load_clip`] returned.
        id: SamplerOpen,
        /// Why.
        error: SamplerError,
    },
}

impl From<SamplerEvent> for SessionEvent {
    fn from(event: SamplerEvent) -> Self {
        Self::Sampler(event)
    }
}

/// Why a sampler call or open did not go through.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SamplerError {
    /// The session holds no disk streamer: it was built without one, or a
    /// host took it ([`Session::take_disk_streamer`]).
    #[error("the session has no disk streamer")]
    NoStreamer,
    /// The disk streamer could not be started ([`SessionBuilder`](crate::SessionBuilder)).
    #[error(transparent)]
    Streamer(#[from] tutti_sampler::Error),
    /// The streamer could not open the file: it is missing or unreadable, is
    /// not audio this build decodes, cannot be placed into a voice, or the
    /// streamer thread is gone.
    #[error(transparent)]
    Open(#[from] OpenError),
    /// A file's header could not be read to decide how to play it.
    #[error(transparent)]
    Probe(#[from] ProbeError),
    /// A file could not be decoded into memory.
    #[error(transparent)]
    Decode(#[from] tutti_codec::DecodeError),
    /// A file's channels cannot be placed into a voice.
    #[error(transparent)]
    Place(#[from] PlaceError),
    /// The open's job did not finish: it panicked, or the spawner dropped it.
    #[error(transparent)]
    Job(#[from] JobFailed),
}

impl Session {
    /// Opens `path` as a disk stream, its reader starting at `place`, on the
    /// session's spawner. Returns at once; the stream lands as
    /// [`SamplerEvent::Opened`] with the returned id in a later
    /// [`update`](Self::update), on the thread that calls `update()`. A file
    /// that cannot be opened lands as [`SamplerEvent::OpenFailed`], a job that
    /// did not finish as [`SamplerError::Job`] in it.
    ///
    /// The voice is as wide as the file (`tutti_sampler::voice_width`). Sending
    /// the open waits while the streamer's command queue is full, as every
    /// stream command does.
    ///
    /// # Errors
    ///
    /// [`SamplerError::NoStreamer`] when the session holds no disk streamer.
    #[must_use = "the id matches the event that lands the open"]
    pub fn open_stream(
        &mut self,
        path: impl Into<PathBuf>,
        place: Placement,
    ) -> Result<SamplerOpen, SamplerError> {
        let streamer = self
            .disk_streamer
            .as_ref()
            .ok_or(SamplerError::NoStreamer)?;
        let opening = streamer.open(&path.into(), place);
        self.sampler_opens += 1;
        let id = SamplerOpen(self.sampler_opens);
        self.jobs.submit(
            move || opening.wait(),
            move |_, done, events| {
                let event = match done {
                    Ok(Ok((stream, reader))) => SamplerEvent::Opened { id, stream, reader },
                    Ok(Err(error)) => SamplerEvent::OpenFailed {
                        id,
                        error: error.into(),
                    },
                    Err(failed) => SamplerEvent::OpenFailed {
                        id,
                        error: failed.into(),
                    },
                };
                events.push(event.into());
            },
        );
        Ok(id)
    }
}

/// What a clip load's first job decided and did, off the control thread.
enum Decided {
    /// The wave was read whole, and the clip is built.
    Resident(Result<Clip, SamplerError>),
    /// The file is to stream: the open waits for the control thread, which
    /// holds the streamer.
    Stream,
}

impl Session {
    /// Loads the file at `path` as a [`Clip`] on the session's spawner,
    /// resident or streamed as `residency` says ([`Residency::Auto`] decides
    /// by the file's size: [`STREAM_ABOVE_BYTES`]). Returns at once; the clip
    /// lands as [`SamplerEvent::ClipLoaded`] with the returned id in a later
    /// [`update`](Self::update), on the thread that calls `update()`, or as
    /// [`SamplerEvent::OpenFailed`].
    ///
    /// A streamed clip owns its stream: it ends when the clip's node leaves
    /// the graph.
    ///
    /// # Errors
    ///
    /// [`SamplerError::NoStreamer`] for [`Residency::Streamed`] when the
    /// session holds no disk streamer.
    #[must_use = "the id matches the event that lands the load"]
    pub fn load_clip(
        &mut self,
        path: impl Into<PathBuf>,
        residency: Residency,
    ) -> Result<SamplerOpen, SamplerError> {
        if residency == Residency::Streamed && self.disk_streamer.is_none() {
            return Err(SamplerError::NoStreamer);
        }
        self.sampler_opens += 1;
        let id = SamplerOpen(self.sampler_opens);
        let path: PathBuf = path.into();
        let at = path.clone();
        let can_stream = self.disk_streamer.is_some();
        self.jobs.submit(
            move || -> Result<Decided, SamplerError> {
                let stream = match residency {
                    Residency::Resident => false,
                    Residency::Streamed => true,
                    Residency::Auto if !can_stream => false,
                    Residency::Auto => {
                        let facts = tutti_sampler::probe(&path)?;
                        let bytes = facts.frames.map_or(0, |frames| {
                            frames.get() as u64 * u64::from(facts.layout.count()) * 4
                        });
                        facts.streamable && bytes > STREAM_ABOVE_BYTES
                    }
                };
                if stream {
                    return Ok(Decided::Stream);
                }
                let wave = tutti_codec::Wave::read(&path)?;
                Ok(Decided::Resident(
                    Clip::new(wave).map_err(SamplerError::from),
                ))
            },
            move |session, done, events| {
                let decided = match done {
                    Ok(decided) => decided,
                    Err(failed) => Err(failed.into()),
                };
                match decided {
                    Ok(Decided::Resident(Ok(clip))) => {
                        events.push(SamplerEvent::ClipLoaded { id, clip }.into());
                    }
                    Ok(Decided::Resident(Err(error))) | Err(error) => {
                        events.push(SamplerEvent::OpenFailed { id, error }.into());
                    }
                    Ok(Decided::Stream) => session.stream_clip(id, at, events),
                }
            },
        );
        Ok(id)
    }

    /// The second half of a streamed [`load_clip`](Self::load_clip): the open,
    /// started here because the streamer is the session's, and waited for on
    /// the spawner.
    fn stream_clip(&mut self, id: SamplerOpen, path: PathBuf, events: &mut Vec<SessionEvent>) {
        let Some(streamer) = &self.disk_streamer else {
            events.push(
                SamplerEvent::OpenFailed {
                    id,
                    error: SamplerError::NoStreamer,
                }
                .into(),
            );
            return;
        };
        let opening = streamer.open(&path, Placement::HEAD);
        self.jobs.submit(
            move || opening.wait(),
            move |_, done, events| {
                let event = match done {
                    Ok(Ok((stream, reader))) => SamplerEvent::ClipLoaded {
                        id,
                        clip: Clip::streaming(reader, stream),
                    },
                    Ok(Err(error)) => SamplerEvent::OpenFailed {
                        id,
                        error: error.into(),
                    },
                    Err(failed) => SamplerEvent::OpenFailed {
                        id,
                        error: failed.into(),
                    },
                };
                events.push(event.into());
            },
        );
    }
}
