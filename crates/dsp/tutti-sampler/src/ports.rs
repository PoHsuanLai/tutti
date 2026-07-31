//! Public ports onto the sampler streaming engine, split MIDI-device-style into
//! a WRITE port ([`Commands`]) and a READ port ([`Status`]).
//!
//! The [`DiskStreamer`](crate::DiskStreamer) handle owns the butler thread; these two
//! cloneable handles are the differentiated surfaces onto it:
//!
//! - [`Commands`] wraps the butler command channel and exposes a single
//!   [`send`](Commands::send) over a public [`Command`] enum — mirroring the
//!   audio-thread [`VoicePoolHandle`](crate::VoicePoolHandle).
//! - [`Status`] wraps the sample rate + channel-plan map and exposes reads
//!   ([`sample_rate`](Status::sample_rate)) plus the reader-factory
//!   [`take_disk_voice`](Status::take_disk_voice).

use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use smol::channel::Sender;

use crate::butler::{ButlerCommand, ChannelPlan};
use crate::voice::{Direction, DiskVoice, DiskVoiceConfig, LoopSetting, VoiceWindow};
use tutti_core::{Beat, BeatDuration, PlaybackRate, SamplePosition, SampleRate, Timeline, Wave};

/// The caller's stated choice of playback tier for a voice: whole-file in memory
/// (`Memory`) or incremental disk streaming (`Disk`). Plain data — the sampler
/// never decides the tier on its own; it plays whichever variant it is handed.
/// The caller owns the tier decision (e.g. dawai-model's `TieringPolicy`).
#[derive(Clone)]
pub enum Source {
    /// Whole file decoded into memory, played by a `MemorySource`.
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
#[non_exhaustive]
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
        speed: PlaybackRate,
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
/// Mirrors [`VoicePoolHandle`](crate::VoicePoolHandle) — a thin,
/// `Clone` handle over a `Sender` with a single [`send`](Self::send) method.
#[derive(Clone, Debug)]
pub struct Commands {
    tx: Sender<ButlerCommand>,
}

impl Commands {
    pub(crate) fn new(tx: Sender<ButlerCommand>) -> Self {
        Self { tx }
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
                crate::butler::control::set_varispeed(&self.tx, channel_index, speed, direction);
            }
            Command::Loop {
                channel_index,
                setting,
            } => match setting {
                LoopSetting::On {
                    start,
                    end,
                    crossfade_frames,
                } => {
                    let _ = self.tx.send_blocking(ButlerCommand::SetStreamLoop {
                        channel_index,
                        range: (start.get().max(0.0) as u64, end.get().max(0.0) as u64),
                        crossfade_frames,
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

/// READ port onto the streaming engine: a cloneable snapshot of the sample rate
/// and the channel-plan map, exposing state reads plus the reader-factory.
#[derive(Clone)]
pub struct Status {
    sample_rate: SampleRate,
    plans: Arc<DashMap<usize, ChannelPlan>>,
}

// `ChannelPlan` isn't `Debug` (it holds butler-internal cache/link state), so
// hand-roll a summary rather than deriving — never touch the map's contents to
// avoid contending with the butler thread.
impl std::fmt::Debug for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Status")
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

impl Status {
    pub(crate) fn new(sample_rate: SampleRate, plans: Arc<DashMap<usize, ChannelPlan>>) -> Self {
        Self { sample_rate, plans }
    }

    /// Sample rate the system was built with.
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Build a [`DiskVoice`] for a channel whose butler stream is
    /// ready, binding it to the timeline placement gate.
    ///
    /// Pulls the ring consumer + shared `RtState` out of the channel's
    /// [`ChannelPlan`] link and wraps them in a placement-gated reader. Returns
    /// `None` while the butler hasn't installed the link yet (the caller retries
    /// next frame).
    pub fn take_disk_voice(
        &self,
        channel_index: usize,
        transport: Arc<dyn Timeline>,
        start_beat: Beat,
        duration: Option<BeatDuration>,
    ) -> Option<DiskVoice> {
        let (inner, rt_state) =
            crate::butler::control::take_streaming_unit(&self.plans, channel_index)?;

        // file_sr / session_sr is the src_ratio the butler set on the plan; the
        // reader's placement gate converts transport seconds → file samples with
        // the file's own rate, so recover it from that ratio.
        let file_sample_rate = self.sample_rate.get() * rt_state.src_ratio().get() as f64;

        Some(DiskVoice::new(
            inner,
            rt_state,
            DiskVoiceConfig {
                timeline: transport,
                window: VoiceWindow {
                    start: start_beat,
                    duration,
                },
                file_sample_rate,
            },
        ))
    }
}
