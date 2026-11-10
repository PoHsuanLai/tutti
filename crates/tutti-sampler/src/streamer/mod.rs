//! Asynchronous disk I/O for audio file streaming.
//!
//! A background "streamer" thread decodes from disk into one SPSC ring per
//! channel; the audio thread only ever pops. Nothing here blocks or allocates on
//! the audio thread — the split is what makes the streaming tier real-time safe.

mod cache;
mod command;
mod config;
pub(crate) mod control;
mod handlers;
mod io;
mod loop_body;
mod loops;
mod metrics;
mod open;
mod plan;
mod prefetch;
mod region_map;
mod rt_state;
mod step;
mod thread;

pub use command::StreamId;
pub(crate) use command::StreamerCommand;
pub(crate) use config::BufferConfig;
pub(crate) use handlers::SessionRate;
#[cfg(test)]
pub(crate) use loops::GUARD_FRAMES;
pub(crate) use loops::{Arrangement, RingMap};
pub(crate) use prefetch::SharedReader;
pub(crate) use rt_state::RtState;
pub(crate) use thread::StreamerThread;

/// The sending end of the streamer's command queue: a bounded
/// `crossbeam_channel`, sent on by control threads only. A send waits while
/// the queue is full and fails once the streamer has gone (every receiver
/// dropped: the thread's on its exit, the controller's on `stop`).
pub(crate) type StreamerSender = crossbeam_channel::Sender<StreamerCommand>;

// The streamer's public face: the handle a host holds to drive disk streaming,
// and the one failure every stream-control command can report.
pub use control::StreamerGone;
pub use open::{Open, OpenError, Opening, Placement, RefusedPlacement, StreamReader};

// The hand-driven cycle's verdict. Public only alongside the driver that
// produces it — the threaded streamer consumes its own outcomes.
#[cfg(any(test, feature = "test-support"))]
pub use step::StepOutcome;

// Test-only re-exports for unit tests outside the streamer module tree (e.g.
// `units::disk_voice`) that build readers directly. Gated so they
// do not count as dead code in normal builds.
#[cfg(test)]
pub(crate) use prefetch::RegionBuffer;

use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::Result;
use crate::ports::{Commands, Status};
use tutti_types::SampleRate;

/// The disk-streaming engine: owns the streamer thread that reads files into
/// per-stream rings for [`DiskVoice`](crate::DiskVoice)s to play.
///
/// A host builds one at startup with [`new`](Self::new) and holds it for the
/// lifetime of the audio engine; dropping it stops the streamer thread. Stream
/// control is split into two cloneable ports: the WRITE port
/// [`commands`](Self::commands) (a [`Commands`] over the streamer's command
/// channel) and the READ port [`status`](Self::status) (a [`Status`] carrying
/// the sample rate). A stream is opened with [`open`](Self::open), whose reply
/// is the [`StreamReader`] a [`DiskVoice`](crate::DiskVoice) is built from.
///
/// All of this is control-thread API. The audio thread only ever touches the
/// `DiskVoice` built from the reader, which reads its ring without locking or
/// allocating.
///
/// # Examples
///
/// Open a file and build the voice that plays it:
///
/// ```no_run
/// use tutti_types::Beat;
/// use tutti_sampler::{DiskStreamer, DiskVoice, Open, Placement};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let streamer = DiskStreamer::new(48_000.0, Default::default())?;
///
/// // The streamer opens the file on its own thread; the reply is polled, and
/// // a poll that is not ready hands the `Opening` back.
/// let mut opening = streamer.open("take_1.wav".as_ref(), Placement::HEAD);
/// let reader = loop {
///     match opening.poll() {
///         Open::Ready(reader) => break reader,
///         Open::Pending(again) => opening = again,
///         Open::Failed(error) => return Err(error.into()),
///     }
///     std::thread::sleep(std::time::Duration::from_millis(1));
/// };
///
/// // The reader is the voice's, by value: place it at beat 0 for the file's
/// // full length. `voice` is a graph node: insert it into the graph that
/// // renders the track.
/// let voice = DiskVoice::from_reader(reader, Beat(0.0), None);
/// # drop(voice);
/// # Ok(())
/// # }
/// ```
pub struct DiskStreamer {
    streamer_tx: StreamerSender,
    streamer: StreamerThread,
    /// The next stream's identity, minted here on the control side.
    next_stream: AtomicU64,
}

// `streamer`/`streamer_tx` hold a thread handle + command channel that can't
// derive `Debug`; print the sample rate and note the live streamer thread.
impl std::fmt::Debug for DiskStreamer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskStreamer")
            .field("sample_rate", &self.streamer.session_rate().get())
            .finish_non_exhaustive()
    }
}

impl DiskStreamer {
    /// Creates the streaming engine at `sample_rate` (the engine's rate) and
    /// spawns the streamer thread.
    ///
    /// Configure with [`DiskStreamerConfig`] (`Default` + struct-update); pass
    /// `Default::default()` for the tuned defaults. Allocates and spawns a
    /// thread: control thread only.
    ///
    /// # Errors
    ///
    /// None at present: every path returns `Ok`. The `Result` is the crate's
    /// [`Error`](crate::Error) type, for initialization that touches the
    /// filesystem.
    ///
    /// # Panics
    ///
    /// If the OS refuses to spawn the streamer thread.
    pub fn new(sample_rate: impl Into<SampleRate>, config: DiskStreamerConfig) -> Result<Self> {
        let mut streamer =
            StreamerThread::with_config(256, sample_rate.into(), config.buffer_config);

        let streamer_tx = streamer.command_sender();
        streamer.start();

        Ok(DiskStreamer {
            streamer_tx,
            streamer,
            next_stream: AtomicU64::new(0),
        })
    }

    /// Creates the streaming engine without spawning the streamer thread;
    /// cycles are then run by hand with [`step_once`](Self::step_once).
    /// Requires the `test-support` feature.
    ///
    /// The step is the same one the thread runs, so this is the shipped path
    /// with its pacing removed rather than a parallel implementation. What it
    /// buys is a streamer whose progress is *counted* instead of waited for: a
    /// test can say "after this many cycles the ring holds audio", where the
    /// threaded streamer only lets it say "after this long it probably does".
    ///
    /// # Errors
    ///
    /// None at present, as for [`new`](Self::new).
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_sampler::{DiskStreamer, DiskStreamerConfig};
    ///
    /// # fn main() -> tutti_sampler::Result<()> {
    /// let mut sampler = DiskStreamer::manual(48_000.0, DiskStreamerConfig::default())?;
    /// sampler.step_once();   // nothing queued: an idle cycle
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(any(test, feature = "test-support"))]
    pub fn manual(sample_rate: impl Into<SampleRate>, config: DiskStreamerConfig) -> Result<Self> {
        let streamer = StreamerThread::with_config(256, sample_rate.into(), config.buffer_config);

        let streamer_tx = streamer.command_sender();

        Ok(DiskStreamer {
            streamer_tx,
            streamer,
            next_stream: AtomicU64::new(0),
        })
    }

    /// Runs one streamer cycle on the calling thread, reporting whether the
    /// streamer still has urgent work ([`Busy`](crate::StepOutcome::Busy)) or has
    /// caught up ([`Healthy`](crate::StepOutcome::Healthy) /
    /// [`Idle`](crate::StepOutcome::Idle)).
    ///
    /// That verdict is what makes a step-driven test terminate on a *condition*
    /// rather than on a step budget: stepping until `Healthy` primes every ring
    /// exactly as far as the threaded streamer would before it parks.
    ///
    /// # Panics
    ///
    /// If this streamer owns a running streamer thread — i.e. it came from
    /// [`new`](Self::new) rather than [`manual`](Self::manual). The two drivers
    /// own the same streamer-local state and cannot share it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn step_once(&mut self) -> StepOutcome {
        self.streamer.step_once()
    }

    /// Steps until the streamer stops making progress, or `budget` cycles have run
    /// — whichever comes first. Returns the number of cycles taken.
    ///
    /// "Stops making progress" is any outcome other than
    /// [`Busy`](StepOutcome::Busy): the rings are full enough
    /// ([`Healthy`](StepOutcome::Healthy)), nothing is streaming
    /// ([`Idle`](StepOutcome::Idle)), or a ring below threshold is one no
    /// further cycle can grow ([`Stalled`](StepOutcome::Stalled), the
    /// normal state near the end of a file). Waiting for `Healthy` alone would
    /// never return there.
    ///
    /// The budget is a liveness bound, not a pacing knob: reaching it means the
    /// streamer is refilling forever without ever catching up, and a caller should
    /// treat that as the failure it is rather than proceeding to measure.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use = "an exhausted budget means the rings never primed"]
    pub fn step_until_settled(&mut self, budget: usize) -> usize {
        for taken in 0..budget {
            if self.step_once() != StepOutcome::Busy {
                return taken + 1;
            }
        }
        budget
    }

    /// Opens `path` as a new stream, its reader starting at `place`, and
    /// answers with an [`Opening`]: poll it for the stream's one
    /// [`StreamReader`]. The voice is as wide as the file
    /// ([`voice_width`](crate::voice_width)); [`open_into`](Self::open_into)
    /// asks for another width.
    ///
    /// Every open is a stream of its own, with its own ring: two opens of one
    /// file are two streams, and a reader belongs to the open it came from.
    ///
    /// Control thread. Like [`Commands::send`] it waits while the streamer's
    /// queue is full. A streamer that is gone answers
    /// [`OpenError::StreamerGone`] at the first poll.
    pub fn open(&self, path: &std::path::Path, place: Placement) -> Opening {
        self.open_as(path, place, None)
    }

    /// [`open`](Self::open) for a voice `width` channels wide: the file's
    /// channels are placed into it by their
    /// [`ChannelTopology`](tutti_types::ChannelTopology), as
    /// [`memory_voice`](crate::memory_voice) places a wave. A file in another
    /// order than the engine's (a WAV 7.1 lists the rears before the sides) is
    /// permuted to it, one wider than `width` with an order is folded into it,
    /// and a mono one takes the centre of a wider frame. A placement that
    /// cannot be made (a channel no speaker names, or a narrowing with an
    /// unordered end: a 5.1 file into 5 channels) is
    /// [`OpenError::Placement`], and nothing is opened.
    pub fn open_into(
        &self,
        path: &std::path::Path,
        place: Placement,
        width: tutti_types::ChannelLayout,
    ) -> Opening {
        self.open_as(path, place, Some(width))
    }

    fn open_as(
        &self,
        path: &std::path::Path,
        place: Placement,
        width: Option<tutti_types::ChannelLayout>,
    ) -> Opening {
        let id = StreamId(self.next_stream.fetch_add(1, Ordering::Relaxed) + 1);
        let (reply, answer) = crossbeam_channel::bounded(1);
        // A refused send drops the command and with it `reply`, which is how
        // a gone streamer reaches the caller: the answer is disconnected.
        let _ = self.streamer_tx.send(StreamerCommand::Open {
            id,
            file_path: path.to_path_buf(),
            start: place.frame(),
            width,
            reply,
        });
        Opening::new(id, answer)
    }

    /// WRITE port: a cloneable [`Commands`] handle over the streamer command
    /// channel. Drive streaming with `commands().send(Command::…)`.
    #[must_use]
    pub fn commands(&self) -> Commands {
        Commands::new(self.streamer_tx.clone())
    }

    /// READ port: a cloneable [`Status`] snapshot carrying the session rate.
    #[must_use]
    pub fn status(&self) -> Status {
        Status::new(self.streamer.session_rate())
    }

    /// The loop the stream `stream` runs, as the streamer last set it; `None`
    /// when it is not open (tests).
    #[cfg(test)]
    pub(crate) fn stream_loop(&self, stream: StreamId) -> Option<crate::voice::LoopSetting> {
        control::stream_loop(&self.streamer.plans(), stream)
    }

    /// Moves the session rate to `sample_rate`: what a device restart at a new
    /// rate does, between the stream's stop and its start.
    ///
    /// Every open stream keeps its file and its position (in file frames, which
    /// the rate does not touch) and has its conversion ratio re-derived against
    /// the new rate, so it plays on at the right pitch and speed from the next
    /// block; a stream opened later derives its ratio against the new rate too,
    /// and every [`Status`] — clones taken earlier included — reports it.
    /// `&self`: the rate is one shared cell and the ratios are atomics, so a
    /// host holding the streamer behind a shared reference can call it.
    ///
    /// Control thread only. Nothing changes for a disk voice already in the
    /// graph beyond its ratio: its placement gate converts beats to file frames
    /// at the file's own rate, which is not the session's.
    pub fn set_sample_rate(&self, sample_rate: impl Into<SampleRate>) {
        self.streamer.set_session_rate(sample_rate.into());
    }
}

// `streamer` has its own `Drop` impl; auto-drop handles cleanup.

/// Configuration for [`DiskStreamer::new`]. `Default` + struct-update.
#[derive(Clone, Debug, Default)]
pub struct DiskStreamerConfig {
    /// Streamer buffer / cache configuration. Default is tuned for
    /// 64-channel streaming on a typical desktop.
    pub buffer_config: BufferConfig,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A loop change on a streamed file reads only the frames it needs:
    /// its fade's lead-in and a short loop's body, through the stream's own
    /// decoder — never the whole file, which blocked every other stream while
    /// it decoded and held the plan map. Observed through
    /// the cache the whole-file read filled: a hard loop, a crossfaded one and
    /// a cleared one leave it empty, and the loop still plays (the ring holds
    /// the loop's frames past its end).
    ///
    /// Mutation (run): the handler reading the file whole for the lead-in
    /// (`load_wave`) → the cache holds the file → fails.
    #[test]
    fn a_loop_change_reads_only_the_frames_it_needs() {
        use crate::ports::Command;
        use crate::voice::LoopSetting;
        use tutti_types::SamplePosition;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loop.wav");
        write_tone(&path, 48_000, 2.0);
        let mut sampler = DiskStreamer::manual(48_000.0, Default::default()).unwrap();
        let opening = sampler.open(&path, Placement::HEAD);
        let reader = crate::testing::settle(&mut sampler, opening).expect("the file opens");
        for fade in [0usize, 500] {
            sampler
                .commands()
                .send(Command::Loop {
                    stream: reader.id(),
                    setting: LoopSetting::On {
                        start: SamplePosition(10_000.0),
                        end: SamplePosition(20_000.0),
                        crossfade_frames: fade,
                    },
                })
                .expect("the streamer is alive");
            let _ = sampler.step_until_settled(1_000);
        }
        let window = reader.ring.window();
        assert!(
            window.1 > 40_000,
            "the loop plays on past its end: {window:?}"
        );
        assert!(
            sampler.streamer.cache().get(&path).is_none(),
            "a loop change decoded the whole file"
        );
    }

    /// Write `seconds` of a stereo 440 Hz tone at `rate` to `path`.
    fn write_tone(path: &std::path::Path, rate: u32, seconds: f64) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..(f64::from(rate) * seconds) as usize {
            let s = (std::f32::consts::TAU * 440.0 * i as f32 / rate as f32).sin() * 0.4;
            w.write_sample(s).unwrap();
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
    }

    /// A session-rate change re-rates every stream, open or opened later.
    /// A streamer built at 44.1 kHz streams a 48 kHz file (ratio 48/44.1);
    /// then the device restarts at 48 kHz (`set_sample_rate`):
    ///
    /// - the open stream's ratio is re-derived, to unity — it plays on at the
    ///   right pitch and speed rather than 8.8% sharp;
    /// - a `Status` taken *before* the restart reports the new rate;
    /// - a disk voice taken after it still converts beats to file frames at
    ///   the file's 48 kHz (its seek targets stay where they were);
    /// - a 44.1 kHz file streamed after it gets 44.1/48, not unity.
    ///
    /// Mutations (run):
    /// - `SessionRate::set` not re-deriving the open streams → the first
    ///   ratio stays 48/44.1 → fails;
    /// - `SessionRate::set` not storing the rate → the early `Status` reports
    ///   44.1 kHz → fails;
    /// - `open_stream_region` deriving against the build's 44.1 kHz (what the
    ///   old `StreamerCycle::sample_rate` copy did) → the later stream gets
    ///   unity → fails;
    /// - `status()` copying the rate into a fresh cell → the early `Status`
    ///   reports 44.1 kHz → fails.
    #[test]
    fn a_rate_change_re_derives_every_streams_ratio() {
        use tutti_types::{Beat, SrcRatio};

        let dir = tempfile::tempdir().unwrap();
        let (at_48, at_44) = (dir.path().join("48k.wav"), dir.path().join("44k.wav"));
        write_tone(&at_48, 48_000, 1.0);
        write_tone(&at_44, 44_100, 1.0);

        let mut sampler = DiskStreamer::manual(44_100.0, Default::default()).unwrap();
        let early = sampler.status();
        let stream = |sampler: &mut DiskStreamer, path: &std::path::Path| {
            let opening = sampler.open(path, Placement::HEAD);
            crate::testing::settle(sampler, opening).expect("the file opens")
        };
        let plans = sampler.streamer.plans();
        let ratio = |id: StreamId| plans.get(&id).expect("streaming").rt_state.src_ratio();

        let first = stream(&mut sampler, &at_48);
        let first_id = first.id();
        assert_eq!(ratio(first_id), SrcRatio::for_rates(48_000.0, 44_100.0));
        assert!(
            ratio(first_id) != SrcRatio::UNITY,
            "setup: a converting stream"
        );

        sampler.set_sample_rate(48_000.0);
        assert_eq!(
            ratio(first_id),
            SrcRatio::UNITY,
            "the open stream is re-rated"
        );
        assert_eq!(early.sample_rate(), SampleRate(48_000.0));

        let voice = crate::DiskVoice::from_reader(first, Beat(0.0), None);
        assert_eq!(voice.file_sample_rate(), SampleRate(48_000.0));

        let second = stream(&mut sampler, &at_44);
        assert_eq!(ratio(second.id()), SrcRatio::for_rates(44_100.0, 48_000.0));
    }

    /// A stopped stream is freed: its ring (and with it the writer's decoder
    /// and any resident wave) goes with its plan, in the step that applies the
    /// `Stop`. The reader is dropped first, so the streamer's writer is the
    /// ring's last holder.
    ///
    /// Mutation (run): `close_stream` not removing the stream's region from
    /// `local.regions` -> the ring is still held by its writer -> fails.
    #[test]
    fn a_stopped_stream_is_freed() {
        use crate::ports::Command;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        write_tone(&path, 48_000, 1.0);

        let mut sampler = DiskStreamer::manual(48_000.0, Default::default()).unwrap();
        let opening = sampler.open(&path, Placement::HEAD);
        let reader = crate::testing::settle(&mut sampler, opening).expect("the file opens");
        let (id, ring) = (reader.id(), Arc::downgrade(&reader.ring));
        drop(reader);
        assert!(ring.upgrade().is_some(), "setup: the writer holds the ring");

        sampler
            .commands()
            .send(Command::Stop { stream: id })
            .expect("the streamer is alive in this test");
        let _ = sampler.step_once();
        assert!(
            ring.upgrade().is_none(),
            "the stopped stream's ring is still alive: its region leaked"
        );
    }

    /// Stepping and threading are exclusive owners of the streamer's local state.
    ///
    /// This is the one thing the hand driver can get wrong that the thread
    /// cannot: two `Local`s, two region maps, and rings one of them does not
    /// know exist. Cheaper to fail loudly at `start` than to debug a stream that
    /// refills into a ring nobody reads.
    #[test]
    #[should_panic(expected = "start on a hand-stepped streamer")]
    fn a_hand_stepped_streamer_refuses_to_also_start_a_thread() {
        let mut streamer =
            StreamerThread::with_config(4, SampleRate(48_000.0), BufferConfig::default());
        let _ = streamer.step_once();
        streamer.start();
    }
}
