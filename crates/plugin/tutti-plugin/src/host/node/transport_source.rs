//! The transport a hosted plugin is handed each block: a pure function of the
//! block's [`Env`] and the project meter.
//!
//! Doc 013 (Verdicts, `TransportSource`): the plugin node no longer polls a
//! shared timeline. The executor hands it the block's [`Env`], whose
//! transport is the engine's own playhead at the block's first frame, with
//! every start, stop, seek, tempo or loop edit inside the block in
//! [`Env::changes`]. [`from_env`] reads it at a frame with
//! [`Env::transport_at`], which walks loop wraps and changes with the host's
//! own clock, so what the plugin is told is what the engine played — live, and
//! offline, where the forked node's `Env` is the render's.
//!
//! The mapping onto the plugin ABIs' snapshot is [`transport_info`], shared
//! with the one node that still polls a timeline: the in-process VST2 client
//! is an `AudioUnit` with no `Env` until it is ported (doc 013, "Port
//! mechanically"), and builds the same [`Snapshot`] from its polled reader
//! (`PolledTransport`, behind the `vst2` feature).

use tutti_core::meter::{Meter, MeterMap};
use tutti_core::{Beat, Bpm, SampleRate};
use tutti_graph::{Env, Offset};

use crate::protocol::TransportInfo;

/// The transport facts a plugin snapshot is made of, at one frame.
///
/// Engine types throughout: [`transport_info`] is where they meet the ABI's
/// `f64`s.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Snapshot {
    pub(crate) playing: bool,
    pub(crate) recording: bool,
    pub(crate) tempo: Bpm,
    /// The playhead, in quarter notes.
    pub(crate) beat: Beat,
    /// The loop region, when looping.
    pub(crate) looping: Option<(Beat, Beat)>,
    /// The free-running sample counter: frames rendered, not musical time, so
    /// it keeps counting while the transport is stopped and never jumps on a
    /// seek or a wrap. An `i64` because that is what every ABI carries.
    pub(crate) continuous: i64,
    pub(crate) sample_rate: SampleRate,
}

impl Snapshot {
    /// The transport at frame `offset` of `env`'s block: the change in force
    /// there, its beat advanced to the frame (wrapping at the loop), and the
    /// frame itself as the continuous counter.
    pub(crate) fn at(env: &Env, offset: Offset) -> Self {
        let t = env.transport_at(offset);
        Self {
            playing: t.playing,
            recording: t.recording,
            tempo: t.tempo,
            beat: t.beat(),
            looping: t.looping.map(|l| (l.start, l.end)),
            // `Frame` is a `u64`; the ABIs carry `i64`. 2^63 frames is three
            // million years at 96 kHz, so the saturation is unreachable.
            continuous: i64::try_from(env.frame_at(offset).get()).unwrap_or(i64::MAX),
            sample_rate: env.sample_rate,
        }
    }
}

/// [`Snapshot::at`], as the plugin ABIs carry it, with `meter`'s signature
/// and bar at the playhead.
pub(crate) fn from_env(env: &Env, offset: Offset, meter: &MeterMap) -> TransportInfo {
    transport_info(&Snapshot::at(env, offset), meter)
}

/// Map `s` onto the snapshot the plugin ABIs read, with `meter`'s signature
/// and bar at the playhead.
pub(crate) fn transport_info(s: &Snapshot, meter: &MeterMap) -> TransportInfo {
    // `.get()` from here on: `TransportInfo` is handed to C ABIs, where the
    // unit types stop.
    let tempo = s.tempo.get();
    let position = meter.bar_at(s.beat);

    let mut info = TransportInfo::new()
        .with_tempo(tempo)
        .with_playing(s.playing)
        .with_recording(s.recording)
        .with_time_signature(position.signature)
        .with_bar(position.bar_start.get(), position.bar)
        .with_sample_rate(s.sample_rate.get());

    // CLAP-style beats position; seconds derived from beats + tempo.
    let beats = s.beat.get();
    let seconds = if tempo > 0.0 {
        beats * 60.0 / tempo
    } else {
        0.0
    };
    info = info.with_position_beats(beats, seconds);

    // The beat is already quarter notes, which is exactly what VST2's
    // `ppqPos` and VST3's `projectTimeMusic` want.
    //
    // `continuous` is the free-running counter; project-time samples jump on a
    // loop or seek, and deriving them from beats and tempo would be wrong the
    // moment tempo moves, so they stay `None` — each format host decides what
    // to do with the absence rather than forwarding a placeholder 0 as fact.
    info = info
        .with_position_quarters(beats)
        .with_continuous_samples(s.continuous);

    if let Some((start, end)) = s.looping {
        info = info.with_loop(true, start.get(), end.get());
    }
    info
}

// `BlockReset for TransportInfo` lives next to its definition's consumers; a
// reset is a full default snapshot (the "no transport installed" state).
impl crate::host::node::input_slot::BlockReset for TransportInfo {
    fn reset(&mut self) {
        *self = TransportInfo::default();
    }
}

/// A transport read by **polling** a live [`TransportState`], for the one
/// plugin node that has no [`Env`]: the in-process VST2 client, an
/// `AudioUnit` run through `Legacy` until it is ported (doc 013, "Port
/// mechanically"). The subprocess plugin node reads [`from_env`] instead.
///
/// [`TransportState`]: tutti_core::transport::TransportState
#[cfg(feature = "vst2")]
pub(crate) use polled::PolledTransport;

#[cfg(feature = "vst2")]
mod polled {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use atomic_float::AtomicF64;
    use tutti_core::meter::MeterMap;
    use tutti_core::transport::TransportState;
    use tutti_core::SampleRate;

    use super::{transport_info, Snapshot};
    use crate::host::node::input_slot::{BlockCtx, BlockInput};
    use crate::protocol::TransportInfo;

    /// A live [`TransportState`] and the project meter, snapshotted each
    /// block. The rate is an `Arc<AtomicF64>` the running box reads live, so
    /// a device change reaches an installed source without a re-install.
    #[derive(Clone)]
    pub(crate) struct PolledTransport {
        reader: Arc<dyn TransportState>,
        /// A separate handle from `reader`: meter is a layer over the
        /// timeline, not transport state, and an edit to it reaches the
        /// running box without a re-install.
        meter: Arc<tutti_core::RtPublish<MeterMap>>,
        sample_rate: Arc<AtomicF64>,
    }

    impl PolledTransport {
        pub(crate) fn new(
            reader: Arc<dyn TransportState>,
            meter: Arc<tutti_core::RtPublish<MeterMap>>,
            sample_rate: impl Into<SampleRate>,
        ) -> Self {
            Self {
                reader,
                meter,
                // `.get()` at the atomic, which needs a primitive.
                sample_rate: Arc::new(AtomicF64::new(sample_rate.into().get())),
            }
        }

        /// Update the stamped rate live (device / rate switch).
        pub(crate) fn set_sample_rate(&self, sample_rate: impl Into<SampleRate>) {
            self.sample_rate
                .store(sample_rate.into().get(), Ordering::Release);
        }
    }

    impl BlockInput for PolledTransport {
        type Out = TransportInfo;
        fn refill(&self, _ctx: BlockCtx, out: &mut TransportInfo) {
            let r = &self.reader;
            let snapshot = Snapshot {
                playing: r.is_rolling(),
                recording: r.is_recording(),
                tempo: r.tempo(),
                beat: r.beat(),
                looping: r.loop_range().map(|l| (l.start(), l.end())),
                continuous: r.steady_time(),
                sample_rate: SampleRate(self.sample_rate.load(Ordering::Acquire)),
            };
            // One meter read per block — this runs in `refill`, not per sample.
            *out = transport_info(&snapshot, &self.meter.read());
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::host::node::input_slot::BlockCtx;
        use tutti_core::transport::Transport;

        /// The polled source reads the live transport through the shared
        /// mapping, and its rate is live.
        ///
        /// Mutation: read `r.beat()` as `Beat(0.0)` → the seconds are 0 →
        /// fails. Mutation: a per-clone rate → the second read keeps 44.1 kHz
        /// → fails.
        #[test]
        fn a_polled_snapshot_reflects_the_transport_and_a_live_rate() {
            let t = Transport::new(44_100.0);
            t.settings.set_tempo(120.0);
            let _ = t.motion.try_send(tutti_core::MotionEvent::Play);
            t.motion.drain();
            t.clock_links()
                .expect("the only playhead writer")
                .set_playhead(2.0);
            let meter = Arc::new(tutti_core::RtPublish::new(MeterMap::default()));
            let src = PolledTransport::new(Arc::new(t.clone()), meter, 44_100.0);
            let ctx = BlockCtx { block_size: 64 };
            let mut out = TransportInfo::default();
            src.refill(ctx, &mut out);
            assert!(out.state.playing);
            // seconds = beats * 60 / tempo = 2 * 60 / 120 = 1.0
            assert!((out.position.seconds - 1.0).abs() < 1e-6);
            src.set_sample_rate(48_000.0);
            src.refill(ctx, &mut out);
            assert_eq!(out.sample_rate, 48_000.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::meter::{BarNumber, BeatsPerBar, MeterChange, NoteValue, TimeSignature};
    use tutti_core::{Frame, Samples, SegmentOrigin};
    use tutti_graph::{LoopRange, Transport, TransportChanges};

    const SR: SampleRate = SampleRate(48_000.0);

    fn env(frame: u64, len: usize, transport: Transport, changes: TransportChanges) -> Env {
        Env {
            frame: Frame(frame),
            sample_rate: SR,
            block_len: Samples(len),
            transport,
            changes,
        }
    }

    fn at(i: usize, len: usize) -> Offset {
        Offset::new(i, Samples(len)).expect("inside the block")
    }

    /// Every field the snapshot carries is `Env`'s, at the frame asked for:
    /// the block's own transport at its first frame, the beat advanced to a
    /// later frame at the tempo, and the frame as the continuous counter.
    ///
    /// Mutation: read `env.transport` instead of `env.transport_at(offset)`
    /// in `Snapshot::at` → the beat at offset 24 stays 2.0 → fails. Mutation:
    /// `continuous: env.frame` → the counter at offset 24 is 1000 → fails.
    /// Mutation: drop `.with_recording` in `transport_info` → fails.
    #[test]
    fn the_snapshot_is_env_at_the_frame() {
        let t = Transport::new(true, Bpm(120.0), Beat(2.0), None).with_recording(true);
        let e = env(1_000, 64, t, TransportChanges::NONE);
        let meter = MeterMap::default();

        let first = from_env(&e, at(0, 64), &meter);
        assert!(first.state.playing);
        assert!(first.state.recording);
        assert_eq!(first.timing.tempo, 120.0);
        assert_eq!(first.position.beats, 2.0);
        assert_eq!(first.sample_rate, 48_000.0);

        // 24 frames at 120 BPM / 48 kHz: 24 / 24 000 of a beat.
        let later = from_env(&e, at(24, 64), &meter);
        assert_eq!(
            later.position.beats,
            e.transport_at(at(24, 64)).beat().get()
        );
        assert!((later.position.beats - (2.0 + 24.0 / 24_000.0)).abs() < 1e-12);
        assert_eq!(later.position.continuous_samples, 1_024);
    }

    /// A seek and a tempo change inside the block, and a loop wrap: the
    /// snapshot follows each at its frame, exactly as `Env::transport_at`
    /// reports it.
    ///
    /// Mutation: ignore `env.changes` (use the block's transport throughout)
    /// → the post-seek snapshot reads the pre-seek beat → fails.
    #[test]
    fn a_seek_a_tempo_change_and_a_loop_wrap_inside_the_block_are_followed() {
        let looping = Some(LoopRange {
            start: Beat(4.0),
            end: Beat(4.0 + 32.0 / 24_000.0),
        });
        let t = Transport::counted(
            true,
            Bpm(120.0),
            SegmentOrigin {
                beat: Beat(4.0),
                frame: Frame(0),
                sample_rate: SR,
            },
            looping,
        );
        let mut changes = TransportChanges::NONE;
        changes
            .push(
                at(40, 64),
                Transport::new(true, Bpm(90.0), Beat(16.0), None),
            )
            .expect("a change inside the block");
        let e = env(0, 64, t, changes);
        let meter = MeterMap::default();

        for i in 0..64 {
            let got = from_env(&e, at(i, 64), &meter);
            let want = e.transport_at(at(i, 64));
            assert_eq!(got.position.beats, want.beat().get(), "frame {i}");
            assert_eq!(got.timing.tempo, want.tempo.get(), "frame {i}");
            assert_eq!(got.state.cycle_active, want.looping.is_some(), "frame {i}");
        }
        // The wrap: 32 frames into a 32-frame loop is back at its start.
        assert_eq!(from_env(&e, at(32, 64), &meter).position.beats, 4.0);
        // The seek, and its tempo.
        let seeked = from_env(&e, at(40, 64), &meter);
        assert_eq!(seeked.position.beats, 16.0);
        assert_eq!(seeked.timing.tempo, 90.0);
        assert!(!seeked.state.cycle_active);
    }

    /// The signature and bar fields come from the meter map. Left at their
    /// defaults, every hosted plugin is told the song is 4/4 at bar 0.
    ///
    /// Mutation: `with_bar(0.0, BarNumber(0))` → fails.
    #[test]
    fn the_snapshot_carries_the_meter_and_bar() {
        let seven_eight = TimeSignature::new(BeatsPerBar::new(7), NoteValue::EIGHTH);
        let meter = MeterMap::new([MeterChange::new(Beat(0.0), seven_eight)]);
        // Bar 2 of 7/8 starts at 3.5 quarter notes, not 7.
        let e = env(
            0,
            64,
            Transport::new(true, Bpm(120.0), Beat(3.5), None),
            TransportChanges::NONE,
        );
        let out = from_env(&e, at(0, 64), &meter);

        assert_eq!(out.timing.signature, seven_eight);
        assert_eq!(out.bar.number, BarNumber(2));
        assert!((out.bar.start_beats - 3.5).abs() < 1e-9);
        // VST2's `bar_start_pos` / VST3's `barPositionMusic` read this one.
        assert!((out.bar.position_quarters - 3.5).abs() < 1e-9);
        // And the VST playhead.
        assert!((out.position.quarters - 3.5).abs() < 1e-9);
    }
}
