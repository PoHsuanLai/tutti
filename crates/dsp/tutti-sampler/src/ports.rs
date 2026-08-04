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

use crate::butler::{ButlerCommand, ButlerGone, ChannelPlan};
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
    ///
    /// `Ok` means the command was *queued* — the butler applies it on its own
    /// thread, and that outcome is not available synchronously.
    ///
    /// `Err(`[`ButlerGone`]`)` means the butler thread has exited and the
    /// command was **discarded**. It is permanent: every later command fails the
    /// same way, so a caller should stop rather than retry.
    ///
    /// This is the crate's documented WRITE port — the primary control surface —
    /// and it previously returned `()`, with every arm ending in `let _ =`. A
    /// dead butler therefore accepted `Stream`/`Seek`/`Loop`/`Stop` indefinitely
    /// and did nothing, which is indistinguishable from working playback.
    #[must_use = "a discarded command is a stream that never starts, seeks, or stops"]
    pub fn send(&self, cmd: Command) -> Result<(), ButlerGone> {
        match cmd {
            Command::Stream {
                channel_index,
                file_path,
                offset,
            } => crate::butler::control::stream(
                &self.tx,
                channel_index,
                file_path,
                offset.get().max(0.0) as usize,
            ),
            Command::Seek {
                channel_index,
                file_position,
            } => self
                .tx
                .send_blocking(ButlerCommand::SeekStream {
                    channel_index,
                    file_position: file_position.get().max(0.0) as u64,
                })
                .map_err(|_| ButlerGone),
            Command::SetSpeed {
                channel_index,
                speed,
                direction,
            } => crate::butler::control::set_varispeed(&self.tx, channel_index, speed, direction),
            Command::Loop {
                channel_index,
                setting,
            } => match setting {
                LoopSetting::On {
                    start,
                    end,
                    crossfade_frames,
                } => self
                    .tx
                    .send_blocking(ButlerCommand::SetStreamLoop {
                        channel_index,
                        range: (start.get().max(0.0) as u64, end.get().max(0.0) as u64),
                        crossfade_frames,
                    })
                    .map_err(|_| ButlerGone),
                LoopSetting::Off => self
                    .tx
                    .send_blocking(ButlerCommand::ClearStreamLoop { channel_index })
                    .map_err(|_| ButlerGone),
            },
            Command::Stop { channel_index } => {
                crate::butler::control::stop_stream(&self.tx, channel_index)
            }
        }
    }

    /// Dispatch a batch of commands, in order.
    ///
    /// Returns how many were queued. `< cmds.len()` means the butler was gone
    /// and the rest were **discarded** — dispatch stops at the first failure,
    /// since the condition is permanent and every later command would fail too.
    ///
    /// A count rather than a bare `Result` because the batch is *partially*
    /// applied: commands queued before the failure still stand, so a caller
    /// needs to know where the stream of intent actually stopped. Previously
    /// this returned `()`, so a whole batch could vanish with no indication how
    /// many had landed.
    #[must_use = "a short count means the rest of the batch was discarded"]
    pub fn send_all(&self, cmds: impl IntoIterator<Item = Command>) -> usize {
        let mut queued = 0;
        for cmd in cmds {
            if self.send(cmd).is_err() {
                break;
            }
            queued += 1;
        }
        queued
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
        let file_sample_rate =
            SampleRate(self.sample_rate.get() * rt_state.src_ratio().get() as f64);

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Commands` whose butler receiver is already gone — the state a real
    /// handle reaches once `ButlerThread::stop` has run.
    fn dead_butler() -> Commands {
        let (tx, rx) = smol::channel::unbounded();
        drop(rx);
        Commands::new(tx)
    }

    /// Every command variant reports a dead butler.
    ///
    /// This is the crate's documented WRITE port. It used to return `()` with
    /// every arm ending in `let _ =`, so a dead butler accepted `Stream`,
    /// `Seek`, `Loop` and `Stop` indefinitely and did nothing — indistinguishable
    /// from working playback. Covering every variant matters because the arms
    /// discard independently: fixing one and missing another would leave exactly
    /// the same silent hole for the command that was missed.
    #[test]
    fn every_command_reports_a_dead_butler() {
        let cmds = dead_butler();

        assert_eq!(
            cmds.send(Command::Stream {
                channel_index: 0,
                file_path: "/nonexistent.wav".into(),
                offset: SamplePosition(0.0),
            }),
            Err(ButlerGone)
        );
        assert_eq!(
            cmds.send(Command::Seek {
                channel_index: 0,
                file_position: SamplePosition(0.0),
            }),
            Err(ButlerGone)
        );
        assert_eq!(
            cmds.send(Command::SetSpeed {
                channel_index: 0,
                speed: PlaybackRate::new(1.0),
                direction: Direction::Forward,
            }),
            Err(ButlerGone)
        );
        assert_eq!(
            cmds.send(Command::Loop {
                channel_index: 0,
                setting: LoopSetting::Off,
            }),
            Err(ButlerGone),
            "the Loop/Off arm sends through its own `send_blocking` call"
        );
        assert_eq!(
            cmds.send(Command::Loop {
                channel_index: 0,
                setting: LoopSetting::On {
                    start: SamplePosition(0.0),
                    end: SamplePosition(100.0),
                    crossfade_frames: 0,
                },
            }),
            Err(ButlerGone),
            "the Loop/On arm is a separate send from Loop/Off"
        );
        assert_eq!(
            cmds.send(Command::Stop { channel_index: 0 }),
            Err(ButlerGone)
        );
    }

    /// A batch reports how many landed rather than vanishing wholesale.
    #[test]
    fn send_all_reports_nothing_queued_to_a_dead_butler() {
        let cmds = dead_butler();
        let queued = cmds.send_all([
            Command::Stop { channel_index: 0 },
            Command::Stop { channel_index: 1 },
            Command::Stop { channel_index: 2 },
        ]);
        assert_eq!(
            queued, 0,
            "a whole batch vanished; the count is what says so"
        );
    }

    /// With a live receiver everything queues, and the batch count matches.
    #[test]
    fn send_all_counts_what_it_queued() {
        let (tx, rx) = smol::channel::unbounded();
        let cmds = Commands::new(tx);

        let queued = cmds.send_all([
            Command::Stop { channel_index: 0 },
            Command::Stop { channel_index: 1 },
        ]);

        assert_eq!(queued, 2);
        assert_eq!(rx.len(), 2, "both reached the channel");
    }
}
