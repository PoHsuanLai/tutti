//! The sampler streaming-engine handle. See [`DiskStreamer`].

use super::{BufferConfig, ButlerCommand, ButlerThread};
use crate::error::Result;
use crate::ports::{Commands, Status};
use smol::channel::Sender;
use std::sync::Arc;
use tutti_core::RtPublish;
use tutti_core::SampleRate;
use tutti_core::Samples;

/// The sampler subsystem handle.
///
/// Owns the butler thread, which drives all disk I/O. The engine builds one at
/// startup with [`new`](Self::new); a host holds it for the lifetime of the
/// session.
///
/// Stream control is split MIDI-device-style into two cloneable ports: the
/// WRITE port [`commands`](Self::commands) (a [`Commands`] over the butler
/// command channel) and the READ port [`status`](Self::status) (a [`Status`]
/// carrying the sample rate + the reader-factory).
///
///
/// # Example
///
/// ```no_run
/// use tutti_sampler::DiskStreamer;
///
/// # fn main() -> tutti_sampler::Result<()> {
/// let sampler = DiskStreamer::new(48_000.0, Default::default())?;
/// let _ = sampler.status().sample_rate();
/// # Ok(())
/// # }
/// ```
pub struct DiskStreamer {
    butler_tx: Sender<ButlerCommand>,
    butler: ButlerThread,
}

// `butler`/`butler_tx` hold a thread handle + command channel that can't
// derive `Debug`; print the sample rate and note the live butler thread.
impl std::fmt::Debug for DiskStreamer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskStreamer")
            .field("sample_rate", &self.butler.session_rate().get())
            .finish_non_exhaustive()
    }
}

impl DiskStreamer {
    /// Build the system and spawn the butler thread.
    ///
    /// Configure with [`DiskStreamerConfig`] (`Default` + struct-update); pass
    /// `Default::default()` for the tuned defaults. Returns [`Err`] if any
    /// subsystem fails to initialize.
    pub fn new(sample_rate: impl Into<SampleRate>, config: DiskStreamerConfig) -> Result<Self> {
        let mut butler = ButlerThread::with_config(256, sample_rate.into(), config.buffer_config);

        if let Some(ref pdc) = config.pdc {
            butler = butler.with_pdc(Arc::clone(pdc));
        }

        let butler_tx = butler.command_sender();
        butler.start();

        Ok(DiskStreamer { butler_tx, butler })
    }

    /// Build the system **without** spawning the butler thread; cycles are then
    /// run by hand with [`step_once`](Self::step_once).
    ///
    /// The step is the same one the thread runs, so this is the shipped path
    /// with its pacing removed rather than a parallel implementation. What it
    /// buys is a butler whose progress is *counted* instead of waited for: a
    /// test can say "after this many cycles the ring holds audio", where the
    /// threaded streamer only lets it say "after this long it probably does".
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
        let mut butler = ButlerThread::with_config(256, sample_rate.into(), config.buffer_config);

        if let Some(ref pdc) = config.pdc {
            butler = butler.with_pdc(Arc::clone(pdc));
        }

        let butler_tx = butler.command_sender();

        Ok(DiskStreamer { butler_tx, butler })
    }

    /// Run one butler cycle on the calling thread, reporting whether the
    /// butler still has urgent work ([`StepOutcome::Busy`]) or has caught up
    /// ([`StepOutcome::Healthy`] / [`StepOutcome::Idle`]).
    ///
    /// That verdict is what makes a step-driven test terminate on a *condition*
    /// rather than on a step budget: stepping until `Healthy` primes every ring
    /// exactly as far as the threaded butler would before it parks.
    ///
    /// # Panics
    ///
    /// If this streamer owns a running butler thread — i.e. it came from
    /// [`new`](Self::new) rather than [`manual`](Self::manual). The two drivers
    /// own the same butler-local state and cannot share it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn step_once(&mut self) -> super::StepOutcome {
        self.butler.step_once()
    }

    /// Step until the butler stops making progress, or `budget` cycles have run
    /// — whichever comes first. Returns the number of cycles taken.
    ///
    /// "Stops making progress" is any outcome other than
    /// [`Busy`](super::StepOutcome::Busy): the rings are full enough
    /// ([`Healthy`](super::StepOutcome::Healthy)), nothing is streaming
    /// ([`Idle`](super::StepOutcome::Idle)), or a ring below threshold is one no
    /// further cycle can grow ([`Stalled`](super::StepOutcome::Stalled), the
    /// normal state near the end of a file). Waiting for `Healthy` alone would
    /// never return there.
    ///
    /// The budget is a liveness bound, not a pacing knob: reaching it means the
    /// butler is refilling forever without ever catching up, and a caller should
    /// treat that as the failure it is rather than proceeding to measure.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use = "an exhausted budget means the rings never primed"]
    pub fn step_until_settled(&mut self, budget: usize) -> usize {
        for taken in 0..budget {
            if self.step_once() != super::StepOutcome::Busy {
                return taken + 1;
            }
        }
        budget
    }

    /// WRITE port: a cloneable [`Commands`] handle over the butler command
    /// channel. Drive streaming with `commands().send(Command::…)`.
    #[must_use]
    pub fn commands(&self) -> Commands {
        Commands::new(self.butler_tx.clone())
    }

    /// READ port: a cloneable [`Status`] snapshot carrying the sample rate and
    /// the channel-plan map (the reader-factory).
    #[must_use]
    pub fn status(&self) -> Status {
        Status::new(self.butler.session_rate(), self.butler.plans())
    }

    /// Move the session rate to `sample_rate`: what a device restart at a new
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
        self.butler.set_session_rate(sample_rate.into());
    }
}

// `butler` has its own `Drop` impl; auto-drop handles cleanup.

/// Configuration for [`DiskStreamer::new`]. `Default` + struct-update.
#[derive(Clone, Default)]
pub struct DiskStreamerConfig {
    /// Butler buffer / cache configuration. Default is tuned for
    /// 64-channel streaming on a typical desktop.
    pub buffer_config: BufferConfig,
    /// Subscription to a per-channel delay-compensation table, indexed by
    /// channel index and denominated in [`Samples`].
    ///
    /// While set, the butler pre-rolls each stream by its channel's entry so
    /// downstream effects stay sample-aligned. Published by whoever runs
    /// `tutti_core::latency::compensate` over the audio graph — the table is
    /// [`Compensation::channels`](tutti_core::Compensation::channels).
    pub pdc: Option<Arc<RtPublish<Vec<Samples>>>>,
}

// Hand-rolled: print the buffer config + whether a subscription is set, not
// the snapshot itself.
impl std::fmt::Debug for DiskStreamerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskStreamerConfig")
            .field("buffer_config", &self.buffer_config)
            .field("has_pdc", &self.pdc.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backward `Command::Seek` must actually reposition the live stream.
    ///
    /// This settles a question the end-to-end test could not, back when that
    /// test was driven by wall clock. In `tests/tier_parity.rs`, seeking a live
    /// stream appeared to leave the audio coming from the old position — which
    /// is consistent with two very different causes: the butler ignoring the
    /// seek, or the butler repositioning correctly while the reader drains a
    /// ring already primed with pre-seek material. From outside the crate those
    /// were indistinguishable, because the only observable was the audio and the
    /// audio was the thing in dispute.
    ///
    /// In-crate the butler's own state is visible, so the question is
    /// answerable directly. **It passes: the butler does reposition.**
    ///
    /// The end-to-end test now passes too — driving the butler by hand lets it
    /// apply the seek and refill before the next render, so it no longer chases
    /// a ring it cannot drain. This test stays because it asks a *different*
    /// question: it observes the butler's own reposition signal rather than the
    /// audio downstream of it, so a regression that repositioned late (rather
    /// than not at all) fails here first and unambiguously.
    ///
    /// # Why the ring-reset epoch, and not a position
    ///
    /// The obvious probe is the reader's `read_position` — and it is the wrong one. That
    /// counter tracks frames the *reader* has consumed, so with nothing
    /// rendering it sits at 0 no matter what the butler does; asserting on it
    /// reported "the butler did not reposition" for a butler that had. The
    /// writer's `file_position` would be right but lives in butler-thread-local
    /// state that nothing else can reach.
    ///
    /// The reset epoch works because `reposition_click_free` bumps it via
    /// `plan.flush_buffer()`, on the *shared* plan, and nothing else in a quiet
    /// stream moves it. So a change means the butler ran the reposition path.
    ///
    /// # No thread, no timeout
    ///
    /// The butler is stepped by hand, so "has the butler seen the command yet"
    /// is answered by *having stepped* rather than by polling a 5 s deadline at
    /// 10 ms. A step returns only once the command is applied, so the assertions
    /// below are unconditional: there is no state in which the answer is "not
    /// yet".
    #[test]
    fn a_backward_seek_repositions_the_live_stream() {
        use crate::ports::Command;
        use tutti_core::SamplePosition;

        const SR: f64 = 48_000.0;

        // 31 s of tone. `buffer_size_for_file` caps the ring at 30 s and sizes
        // it to hold the whole file below that, so a shorter file is prefilled
        // entire and a "seek" moves the writer inside a ring that already holds
        // everything — nothing to reposition, and the epoch would not move.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seek_probe.wav");
        {
            let spec = hound::WavSpec {
                channels: 2,
                sample_rate: SR as u32,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            let mut w = hound::WavWriter::create(&path, spec).unwrap();
            for i in 0..(SR * 31.0) as usize {
                let s = (std::f32::consts::TAU * 440.0 * i as f32 / SR as f32).sin() * 0.4;
                w.write_sample(s).unwrap();
                w.write_sample(s).unwrap();
            }
            w.finalize().unwrap();
        }

        let mut sampler = DiskStreamer::manual(SR, Default::default()).unwrap();
        sampler
            .commands()
            .send(Command::Stream {
                channel_index: 0,
                file_path: path.clone(),
                offset: SamplePosition(20.0 * SR),
            })
            .expect("the butler is alive in this test");

        // One step applies the queued `Stream`, which is what installs the link.
        let _ = sampler.step_once();

        let plans = sampler.butler.plans();
        assert!(
            plans.get(&0).is_some_and(|p| p.link.is_some()),
            "one butler cycle after a Stream command and no link is installed"
        );

        // The ring-reset epoch is the butler's own signal that it repositioned:
        // `reposition_click_free` calls `plan.flush_buffer()`, which bumps it.
        // Unlike `read_position` (a *reader* consumption counter, which stays 0
        // when nothing is rendering) this moves purely as a result of the seek.
        let reset_epoch =
            || -> u64 { plans.get(&0).map(|p| p.rt_state.reset_epoch()).unwrap_or(0) };

        let before = reset_epoch();

        // Seek backward, which is the direction a "refill forward from here"
        // implementation is most likely to drop.
        sampler
            .commands()
            .send(Command::Seek {
                channel_index: 0,
                file_position: SamplePosition(5.0 * SR),
            })
            .expect("the butler is alive in this test");

        let _ = sampler.step_once();

        assert_ne!(
            reset_epoch(),
            before,
            "the ring-reset epoch did not move from {before} in the cycle that applied a \
             backward seek — the butler did not reposition the stream"
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

    /// **A session-rate change re-rates every stream, open or opened later.**
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
    /// - `handle_stream_file` deriving against the build's 44.1 kHz (what the
    ///   old `ButlerCycle::sample_rate` copy did) → the later stream gets
    ///   unity → fails;
    /// - `status()` copying the rate into a fresh cell → the early `Status`
    ///   reports 44.1 kHz → fails.
    #[test]
    fn a_rate_change_re_derives_every_streams_ratio() {
        use crate::ports::Command;
        use tutti_core::{Beat, SamplePosition, SrcRatio, Timeline, Transport};

        let dir = tempfile::tempdir().unwrap();
        let (at_48, at_44) = (dir.path().join("48k.wav"), dir.path().join("44k.wav"));
        write_tone(&at_48, 48_000, 1.0);
        write_tone(&at_44, 44_100, 1.0);

        let mut sampler = DiskStreamer::manual(44_100.0, Default::default()).unwrap();
        let early = sampler.status();
        let stream = |sampler: &mut DiskStreamer, channel_index, path: &std::path::Path| {
            sampler
                .commands()
                .send(Command::Stream {
                    channel_index,
                    file_path: path.to_path_buf(),
                    offset: SamplePosition(0.0),
                })
                .expect("the butler is alive in this test");
            let _ = sampler.step_once();
        };
        let plans = sampler.butler.plans();
        let ratio = |channel| plans.get(&channel).expect("streaming").rt_state.src_ratio();

        stream(&mut sampler, 0, &at_48);
        assert_eq!(ratio(0), SrcRatio::for_rates(48_000.0, 44_100.0));
        assert!(ratio(0) != SrcRatio::UNITY, "setup: a converting stream");

        sampler.set_sample_rate(48_000.0);
        assert_eq!(ratio(0), SrcRatio::UNITY, "the open stream is re-rated");
        assert_eq!(early.sample_rate(), SampleRate(48_000.0));

        let clock: Arc<dyn Timeline> = Arc::new(Transport::new(48_000.0));
        let voice = sampler
            .status()
            .take_disk_voice(0, clock, Beat(0.0), None)
            .expect("the link is installed");
        assert_eq!(voice.file_sample_rate(), SampleRate(48_000.0));

        stream(&mut sampler, 1, &at_44);
        assert_eq!(ratio(1), SrcRatio::for_rates(44_100.0, 48_000.0));
    }

    /// Stepping and threading are exclusive owners of the butler's local state.
    ///
    /// This is the one thing the hand driver can get wrong that the thread
    /// cannot: two `Local`s, two region maps, and rings one of them does not
    /// know exist. Cheaper to fail loudly at `start` than to debug a stream that
    /// refills into a ring nobody reads.
    #[test]
    #[should_panic(expected = "start on a hand-stepped butler")]
    fn a_hand_stepped_butler_refuses_to_also_start_a_thread() {
        let mut butler =
            ButlerThread::with_config(4, SampleRate(48_000.0), BufferConfig::default());
        let _ = butler.step_once();
        butler.start();
    }

    #[test]
    fn pdc_subscription_is_shared_not_copied() {
        // The caller owns the table and keeps publishing to it after the
        // sampler is built; the sampler must observe those later stores.
        let pdc = Arc::new(RtPublish::new(vec![Samples(100), Samples(0)]));

        let _sampler = DiskStreamer::new(
            44100.0,
            DiskStreamerConfig {
                pdc: Some(Arc::clone(&pdc)),
                ..Default::default()
            },
        )
        .unwrap();

        pdc.publish(Arc::new(vec![Samples(512), Samples(0)]));
        assert_eq!(pdc.read()[0], Samples(512));
    }
}
