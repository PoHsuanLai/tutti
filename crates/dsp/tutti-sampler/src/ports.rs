//! Public ports onto the sampler streaming engine, split MIDI-device-style into
//! a WRITE port ([`Commands`]) and a READ port ([`Status`]).
//!
//! The [`Sampler`](crate::Sampler) handle owns the butler thread; these two
//! cloneable handles are the differentiated surfaces onto it:
//!
//! - [`Commands`] wraps the butler command channel and exposes a single
//!   [`send`](Commands::send) over a public [`Command`] enum — mirroring the
//!   audio-thread [`TrackClipReaderHandle`](crate::TrackClipReaderHandle).
//! - [`Status`] wraps the sample rate + channel-plan map and exposes reads
//!   ([`sample_rate`](Status::sample_rate)) plus the reader-factory
//!   [`take_clip_reader`](Status::take_clip_reader).

use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use smol::channel::Sender;

use crate::butler::{ButlerCommand, ChannelPlan, RtState};
use crate::playback::{
    Direction, LoopSetting, StreamingClipConfig, StreamingClipReader, TransportPlacement,
};
use crate::StreamingSamplerUnit;
use tutti_core::{BeatDuration, BeatPosition, Ratio, SamplePosition, TransportReader, Wave};

/// The caller's stated choice of playback tier for a clip: whole-file in RAM
/// (`Memory`) or incremental disk streaming (`Disk`). Plain data — the sampler
/// never decides the tier on its own; it plays whichever variant it is handed.
/// The caller owns the tier decision (e.g. dawai-model's `TieringPolicy`).
#[derive(Clone)]
pub enum Source {
    /// Whole file decoded into RAM, played by a `SamplerUnit`.
    Memory(Arc<Wave>),
    /// File streamed incrementally from disk via the butler.
    Disk(PathBuf),
}

// `Wave` doesn't implement `Debug`, so hand-roll it (len/rate summary for
// `Memory`, path for `Disk`) rather than deriving.
impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Memory(wave) => f
                .debug_struct("Memory")
                .field("len", &wave.len())
                .field("sample_rate", &wave.sample_rate())
                .finish(),
            Source::Disk(path) => f.debug_tuple("Disk").field(path).finish(),
        }
    }
}

/// A butler stream-control command, one public variant per streaming operation.
///
/// Each variant maps to exactly one internal `ButlerCommand`; the [`Commands`]
/// port performs that mapping in [`send`](Commands::send).
#[derive(Debug, Clone)]
pub enum Command {
    /// Register a disk-streaming source for a timeline clip on `channel_index`,
    /// starting at `offset` in file samples. Maps to `StreamAudioFile`.
    Stream {
        channel_index: usize,
        file_path: PathBuf,
        offset: SamplePosition,
    },
    /// Reposition a live stream to an absolute file sample offset (timeline
    /// seek). Maps to `SeekStream`.
    Seek {
        channel_index: usize,
        file_position: SamplePosition,
    },
    /// Set varispeed (playback speed magnitude + direction). Maps to
    /// `SetVarispeed`.
    SetSpeed {
        channel_index: usize,
        speed: Ratio,
        direction: Direction,
    },
    /// Enable/replace or disable looping. `LoopSetting::On { .. }` maps to
    /// `SetStreamLoop`; `LoopSetting::Off` maps to `ClearStreamLoop`.
    Loop {
        channel_index: usize,
        setting: LoopSetting,
    },
    /// Stop the stream on a channel — drops its ring + link. Maps to
    /// `StopStreaming`.
    Stop { channel_index: usize },
}

/// WRITE port onto the butler: a cloneable wrapper over the command channel
/// that dispatches a public [`Command`] to the butler.
///
/// Mirrors [`TrackClipReaderHandle`](crate::TrackClipReaderHandle) — a thin,
/// `Clone` handle over a `Sender` with a single [`send`](Self::send) method.
#[derive(Clone)]
pub struct Commands {
    tx: Sender<ButlerCommand>,
    plans: Arc<DashMap<usize, ChannelPlan>>,
}

impl Commands {
    pub(crate) fn new(tx: Sender<ButlerCommand>, plans: Arc<DashMap<usize, ChannelPlan>>) -> Self {
        Self { tx, plans }
    }

    /// Bind an ergonomic per-channel [`ClipControl`] over this write port.
    ///
    /// The returned handle is a thin façade that fills in `channel_index` and
    /// routes every verb back through [`send`](Self::send) — one mapping.
    pub fn channel(&self, channel_index: usize) -> ClipControl {
        ClipControl {
            commands: self.clone(),
            plans: Arc::clone(&self.plans),
            channel_index,
        }
    }

    /// Dispatch a single command to the butler.
    pub fn send(&self, cmd: Command) {
        match cmd {
            Command::Stream {
                channel_index,
                file_path,
                offset,
            } => {
                crate::butler::control::stream(
                    &self.tx,
                    channel_index,
                    file_path,
                    offset.get().max(0.0) as usize,
                );
            }
            Command::Seek {
                channel_index,
                file_position,
            } => {
                let _ = self.tx.send_blocking(ButlerCommand::SeekStream {
                    channel_index,
                    file_position: file_position.get().max(0.0) as u64,
                });
            }
            Command::SetSpeed {
                channel_index,
                speed,
                direction,
            } => {
                crate::butler::control::set_varispeed(
                    &self.tx,
                    channel_index,
                    speed.get(),
                    direction.is_reverse(),
                );
            }
            Command::Loop {
                channel_index,
                setting,
            } => match setting {
                LoopSetting::On {
                    start,
                    end,
                    crossfade_samples,
                } => {
                    let _ = self.tx.send_blocking(ButlerCommand::SetStreamLoop {
                        channel_index,
                        range: (start.get().max(0.0) as u64, end.get().max(0.0) as u64),
                        crossfade_samples,
                    });
                }
                LoopSetting::Off => {
                    let _ = self
                        .tx
                        .send_blocking(ButlerCommand::ClearStreamLoop { channel_index });
                }
            },
            Command::Stop { channel_index } => {
                crate::butler::control::stop_stream(&self.tx, channel_index);
            }
        }
    }

    /// Dispatch a batch of commands, in order.
    pub fn send_all(&self, cmds: impl IntoIterator<Item = Command>) {
        for cmd in cmds {
            self.send(cmd);
        }
    }
}

/// Ergonomic per-channel handle over the [`Commands`] write port.
///
/// A thin façade bound to one `channel_index`: every verb assembles the
/// matching [`Command`] and dispatches it through the *same*
/// [`Commands::send`] mapping — there is no second mapping. Hand-written
/// callers (e.g. the preview [`Auditioner`](crate::Auditioner)) prefer this to
/// building `Command`s and repeating `channel_index` by hand; machine-generated
/// call sites stay on explicit [`Command`]/[`send`](Commands::send).
#[derive(Clone)]
pub struct ClipControl {
    commands: Commands,
    plans: Arc<DashMap<usize, ChannelPlan>>,
    channel_index: usize,
}

impl ClipControl {
    /// Start streaming `file` from the beginning.
    pub fn stream(&self, file: impl Into<PathBuf>) {
        self.stream_from(file, SamplePosition::new(0.0));
    }

    /// Start streaming `file` from file-sample offset `at`.
    pub fn stream_from(&self, file: impl Into<PathBuf>, at: impl Into<SamplePosition>) {
        self.commands.send(Command::Stream {
            channel_index: self.channel_index,
            file_path: file.into(),
            offset: at.into(),
        });
    }

    /// Reposition the live stream to absolute file-sample offset `to`.
    pub fn seek(&self, to: impl Into<SamplePosition>) {
        self.commands.send(Command::Seek {
            channel_index: self.channel_index,
            file_position: to.into(),
        });
    }

    /// Set forward playback speed.
    pub fn speed(&self, speed: impl Into<Ratio>) {
        self.commands.send(Command::SetSpeed {
            channel_index: self.channel_index,
            speed: speed.into(),
            direction: Direction::Forward,
        });
    }

    /// Set reverse playback speed (magnitude).
    pub fn reverse(&self, speed: impl Into<Ratio>) {
        self.commands.send(Command::SetSpeed {
            channel_index: self.channel_index,
            speed: speed.into(),
            direction: Direction::Reverse,
        });
    }

    /// Loop the stream over `range`, using the default 256-sample crossfade.
    pub fn looping(&self, range: core::ops::Range<SamplePosition>) {
        self.looping_xfade(range, 256);
    }

    /// Loop the stream over `range` with an explicit crossfade length.
    pub fn looping_xfade(
        &self,
        range: core::ops::Range<SamplePosition>,
        crossfade_samples: usize,
    ) {
        self.commands.send(Command::Loop {
            channel_index: self.channel_index,
            setting: LoopSetting::On {
                start: range.start,
                end: range.end,
                crossfade_samples,
            },
        });
    }

    /// Disable looping (one-shot playback).
    pub fn one_shot(&self) {
        self.commands.send(Command::Loop {
            channel_index: self.channel_index,
            setting: LoopSetting::Off,
        });
    }

    /// Stop the stream on this channel.
    pub fn stop(&self) {
        self.commands.send(Command::Stop {
            channel_index: self.channel_index,
        });
    }

    /// Take the un-gated streaming consumer for this channel (free-running
    /// handoff the preview path uses). `None` while the butler hasn't installed
    /// the link yet.
    pub fn take_streaming_unit(&self) -> Option<(StreamingSamplerUnit, Arc<RtState>)> {
        crate::butler::control::take_streaming_unit(&self.plans, self.channel_index)
    }
}

/// READ port onto the streaming engine: a cloneable snapshot of the sample rate
/// and the channel-plan map, exposing state reads plus the reader-factory.
#[derive(Clone)]
pub struct Status {
    sample_rate: f64,
    plans: Arc<DashMap<usize, ChannelPlan>>,
}

impl Status {
    pub(crate) fn new(sample_rate: f64, plans: Arc<DashMap<usize, ChannelPlan>>) -> Self {
        Self { sample_rate, plans }
    }

    /// Sample rate the system was built with.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Build a [`StreamingClipReader`] for a channel whose butler stream is
    /// ready, binding it to the timeline placement gate.
    ///
    /// Pulls the ring consumer + shared `RtState` out of the channel's
    /// [`ChannelPlan`] link and wraps them in a placement-gated reader. Returns
    /// `None` while the butler hasn't installed the link yet (the caller retries
    /// next frame).
    pub fn take_clip_reader(
        &self,
        channel_index: usize,
        transport: Arc<dyn TransportReader>,
        start_beat: BeatPosition,
        duration: Option<BeatDuration>,
    ) -> Option<StreamingClipReader> {
        let (inner, rt_state) =
            crate::butler::control::take_streaming_unit(&self.plans, channel_index)?;

        // file_sr / session_sr is the src_ratio the butler set on the plan; the
        // reader's placement gate converts transport seconds → file samples with
        // the file's own rate, so recover it from that ratio.
        let file_sample_rate = self.sample_rate * rt_state.src_ratio().get() as f64;

        Some(StreamingClipReader::new(
            inner,
            rt_state,
            StreamingClipConfig {
                placement: TransportPlacement {
                    transport,
                    start_beat,
                    duration_beats: duration,
                },
                file_sample_rate,
            },
        ))
    }
}
