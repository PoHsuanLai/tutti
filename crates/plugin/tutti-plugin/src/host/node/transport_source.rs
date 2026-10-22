//! The transport a hosted plugin is handed each block: a pure function of the
//! block's [`Env`] and the project meter.
//!
//! The plugin node does not poll a shared timeline. The executor hands it the
//! block's [`Env`], whose
//! transport is the engine's own playhead at the block's first frame, with
//! every start, stop, seek, tempo or loop edit inside the block in
//! [`Env::changes`]. [`from_cx`] reads it at a frame with
//! [`Cx::transport_at`]: the transport the node's **input** was rendered at,
//! its compiled arrival latency earlier, reaching before the block through
//! the executor's transport history. Behind a latent path the plugin is told
//! the playhead of the audio it is processing, not of the graph's own frame,
//! so a tempo-synced effect stays on the beat the audio is on. The walk is
//! the host's own clock, through loop wraps and changes, so what the plugin
//! is told is what the engine played — live, and offline, where the forked
//! node's `Env` is the render's.
//!
//! The mapping onto the plugin ABIs' snapshot is [`transport_info`]. The
//! in-process VST2 node, live and forked, reads [`from_cx`] too, so every
//! hosted plugin is told its transport by this one path.
//!
//! Three clocks ride beside the transport, each the node's own: the
//! free-running sample counter ([`SteadyTime`]), project time
//! ([`ProjectTime`], which rolls with playback and re-seats only at a jump)
//! and the host's monotonic clock ([`HostClock`], read once per block; none
//! for an offline render).

use tutti_core::meter::{Meter, MeterMap};
use tutti_core::{Beat, Bpm, SampleRate};
use tutti_graph::{Continuity, Cx, Offset, Transport};
use tutti_types::Samples;

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
    /// The host's monotonic clock at the frame, in nanoseconds
    /// ([`HostClock`]); `None` for an offline render.
    pub(crate) system_time_ns: Option<i64>,
    /// Project time at the frame, in frames ([`ProjectTime`]); `None`
    /// without a usable tempo and rate.
    pub(crate) project_frames: Option<f64>,
    pub(crate) sample_rate: SampleRate,
}

/// The host's monotonic clock, as the plugin ABIs carry it (VST3
/// `systemTime`, VST2 `nanoSeconds`): the OS's monotonic clock in
/// nanoseconds, the clock a plugin compares these fields against.
///
/// - Linux and the other Unixes: `CLOCK_MONOTONIC` (a vDSO read on Linux).
/// - macOS: `CLOCK_UPTIME_RAW`, which is `mach_absolute_time` in
///   nanoseconds: Core Audio's host time (`AudioTimeStamp::mHostTime`, read
///   through `AudioConvertHostTimeToNanos`) is that clock.
/// - Windows: `QueryPerformanceCounter`, scaled to nanoseconds by its
///   frequency (fixed at boot).
///
/// The OS's epoch, not one of the host's, so plugins in every process (the
/// host's and each plugin server's) read one timebase without being handed
/// an origin. A node reads it once per block ([`now_ns`](Self::now_ns)) and
/// places each chunk on it by its frame ([`at`](Self::at)), so the time a
/// plugin is told and the frames it is handed advance together.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HostClock;

impl HostClock {
    /// The OS monotonic clock now, in nanoseconds. Audio-thread safe: a
    /// clock read with no lock, no allocation and no syscall beyond the
    /// vDSO's or QPC's. 2^63 ns is 292 years of uptime, so the saturation
    /// is unreachable.
    pub(crate) fn now_ns(self) -> i64 {
        os_monotonic_ns()
    }

    /// The clock `frames` past a reading of `block_ns`, at `rate`: where a
    /// chunk starting that far into the block sits on it.
    pub(crate) fn at(block_ns: i64, frames: usize, rate: SampleRate) -> i64 {
        let rate = rate.get();
        if !(rate.is_finite() && rate > 0.0) {
            return block_ns;
        }
        let ns = (frames as f64 * 1e9 / rate).round() as i64;
        block_ns.saturating_add(ns)
    }
}

/// The free-running sample counter a node hands its plugin: frames it has
/// rendered, whatever the transport did.
///
/// Its own count, not [`Env::frame`]: the executor's frame is the graph's
/// clock, which a re-prepare (a sample-rate change) starts over, and the plugin
/// ABIs' steady time (VST2 `samplePos`'s continuous twin, VST3
/// `continousTimeSamples`) must never jump. A node keeps one and advances it by
/// every block it renders.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SteadyTime(u64);

impl SteadyTime {
    /// Count `frames` more rendered frames.
    pub(crate) fn advance(&mut self, frames: usize) {
        self.0 = self.0.saturating_add(frames as u64);
    }

    /// The counter `at` frames past this one, as the ABIs carry it: an
    /// `i64`. 2^63 frames is three million years at 96 kHz, so the saturation
    /// is unreachable.
    pub(crate) fn at(self, at: usize) -> i64 {
        i64::try_from(self.0.saturating_add(at as u64)).unwrap_or(i64::MAX)
    }
}

/// Project time a node tells its plugin (VST2 `samplePos`, VST3
/// `projectTimeSamples`, CLAP `song_pos_seconds`, AU's sample time): frames
/// of playback since the song's start, the node's own state.
///
/// It rolls by the frames rendered while the transport plays on, and is
/// re-seated from the beat at the tempo (`beat · 60 / tempo · rate`) only at
/// a jump: the first read, a seek, a loop wrap or edit, a start, while
/// stopped, or a rate change. A tempo change is not a jump
/// ([`Transport::continuity`], the playhead's own rule), so project time
/// keeps counting through it instead of jumping to the beat's time at the
/// new tempo: the engine has no tempo map, so the beat's time at the tempo
/// in force is only right from a re-seat until the next tempo change.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ProjectTime {
    last: Option<ProjectRead>,
}

/// The read [`ProjectTime`] continues from.
#[derive(Clone, Copy, Debug)]
struct ProjectRead {
    transport: Transport,
    rate: SampleRate,
    /// The [`SteadyTime`] counter at the read.
    steady: i64,
    frames: f64,
}

impl ProjectTime {
    /// Project time, in frames, at a read of `t` at `rate` with the node's
    /// steady counter at `steady`; `None` without a usable tempo and rate.
    /// Reads come in frame order (the counter never goes back). Audio-thread
    /// safe: arithmetic only.
    pub(crate) fn at(&mut self, t: &Transport, rate: SampleRate, steady: i64) -> Option<f64> {
        let usable = |v: f64| v.is_finite() && v > 0.0;
        let (tempo, hz) = (t.tempo.get(), rate.get());
        if !(usable(tempo) && usable(hz)) {
            self.last = None;
            return None;
        }
        let rolled = self.last.and_then(|p| {
            let gap = usize::try_from(steady.checked_sub(p.steady)?).ok()?;
            let rolls = p.transport.playing && t.playing && p.rate == rate;
            let continuity = p.transport.continuity(Samples(gap), rate, t);
            (rolls && continuity == Continuity::Rolled).then(|| p.frames + gap as f64)
        });
        let frames = rolled.unwrap_or_else(|| t.beat().get() * 60.0 / tempo * hz);
        if !frames.is_finite() {
            self.last = None;
            return None;
        }
        self.last = Some(ProjectRead {
            transport: *t,
            rate,
            steady,
            frames,
        });
        Some(frames)
    }
}

impl Snapshot {
    /// The transport the node's input at frame `offset` was rendered at
    /// ([`Cx::transport_at`]: the node's arrival latency earlier, through
    /// changes, loop wraps and earlier blocks), with `continuous` as the
    /// free-running counter ([`SteadyTime`]), `project` the node's project
    /// time (advanced to this read) and `block_ns` the [`HostClock`] read
    /// at the block's first frame (`None` offline).
    pub(crate) fn at(
        cx: &Cx<'_>,
        offset: Offset,
        continuous: i64,
        project: &mut ProjectTime,
        block_ns: Option<i64>,
    ) -> Self {
        let t = cx.transport_at(offset);
        let project_frames = project.at(&t, cx.env.sample_rate, continuous);
        Self {
            playing: t.playing,
            recording: t.recording(),
            tempo: t.tempo,
            beat: t.beat(),
            looping: t.looping.map(|l| (l.start, l.end)),
            continuous,
            system_time_ns: block_ns
                .map(|ns| HostClock::at(ns, offset.index(), cx.env.sample_rate)),
            project_frames,
            sample_rate: cx.env.sample_rate,
        }
    }
}

/// [`Snapshot::at`], as the plugin ABIs carry it, with `meter`'s signature
/// and bar at the playhead.
pub(crate) fn from_cx(
    cx: &Cx<'_>,
    offset: Offset,
    continuous: i64,
    project: &mut ProjectTime,
    block_ns: Option<i64>,
    meter: &MeterMap,
) -> TransportInfo {
    transport_info(
        &Snapshot::at(cx, offset, continuous, project, block_ns),
        meter,
    )
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

    // CLAP's beats position, and its seconds: project time
    // (`ProjectTime`), or zero without one.
    let beats = s.beat.get();
    let seconds = s
        .project_frames
        .map_or(0.0, |frames| frames / s.sample_rate.get());
    info = info.with_position_beats(beats, seconds);

    // The beat is already quarter notes, which is exactly what VST2's
    // `ppqPos` and VST3's `projectTimeMusic` want. `continuous` is the
    // free-running counter; project time in samples is `ProjectTime`'s,
    // rounded (an `as` cast saturates, and an `i64` of frames is three
    // million years).
    info = info
        .with_position_quarters(beats)
        .with_continuous_samples(s.continuous);
    if let Some(frames) = s.project_frames {
        info = info.with_position_samples(frames.round() as i64);
    }
    if let Some(ns) = s.system_time_ns {
        info = info.with_system_time_ns(ns);
    }

    if let Some((start, end)) = s.looping {
        info = info.with_loop(true, start.get(), end.get());
    }
    info
}

/// `CLOCK_MONOTONIC` (`CLOCK_UPTIME_RAW` on Apple), in nanoseconds; see
/// [`HostClock`].
#[cfg(unix)]
fn os_monotonic_ns() -> i64 {
    #[cfg(target_vendor = "apple")]
    const CLOCK: libc::clockid_t = libc::CLOCK_UPTIME_RAW;
    #[cfg(not(target_vendor = "apple"))]
    const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable `timespec`, and the clock id is one
    // every Unix (or, on Apple, macOS 10.12+) provides. It cannot fail with
    // those, and a failure would leave `ts` zero, a valid reading.
    unsafe { libc::clock_gettime(CLOCK, &mut ts) };
    // The `timespec` field types are platform-sized; widen to `i64`.
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    let (secs, nanos) = (i64::from(ts.tv_sec as i64), i64::from(ts.tv_nsec as i64));
    secs.saturating_mul(1_000_000_000).saturating_add(nanos)
}

/// `QueryPerformanceCounter`, in nanoseconds; see [`HostClock`].
#[cfg(windows)]
fn os_monotonic_ns() -> i64 {
    use std::sync::atomic::{AtomicI64, Ordering};
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

    // The frequency is fixed at boot: read once, then cached (0 is unread).
    static FREQUENCY: AtomicI64 = AtomicI64::new(0);
    let mut freq = FREQUENCY.load(Ordering::Relaxed);
    if freq <= 0 {
        // SAFETY: a valid, writable `i64`. It cannot fail on Windows XP and
        // later; a failure leaves it 0, handled below.
        let _ = unsafe { QueryPerformanceFrequency(&mut freq) };
        if freq <= 0 {
            return 0;
        }
        FREQUENCY.store(freq, Ordering::Relaxed);
    }
    let mut count = 0i64;
    // SAFETY: as above.
    let _ = unsafe { QueryPerformanceCounter(&mut count) };
    // Whole seconds and the remainder apart, so the scaling cannot overflow.
    let (secs, rest) = (count / freq, count % freq);
    secs.saturating_mul(1_000_000_000)
        .saturating_add(rest * 1_000_000_000 / freq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tutti_core::meter::{BarNumber, BeatsPerBar, MeterChange, NoteValue, TimeSignature};
    use tutti_core::{Frame, Samples, SegmentOrigin};
    use tutti_graph::{Env, LoopRange, Transport, TransportChanges, TransportHistory};
    use tutti_types::Latency;

    const SR: SampleRate = SampleRate(48_000.0);

    fn env(frame: u64, len: usize, transport: Transport, changes: TransportChanges) -> Env {
        Env {
            frame: Frame(frame),
            sample_rate: SR,
            block_len: Samples(len),
            transport,
            changes,
            play: tutti_graph::PlayClock::ZERO,
        }
    }

    fn at(i: usize, len: usize) -> Offset {
        Offset::new(i, Samples(len)).expect("inside the block")
    }

    /// A node's context in `e` with no arrival latency.
    fn cx(e: &Env) -> Cx<'_> {
        Cx {
            env: e,
            arrival: Latency::ZERO,
            history: TransportHistory::none(),
        }
    }

    /// Every transport field the snapshot carries is `Env`'s, at the frame
    /// asked for: the block's own transport at its first frame, the beat
    /// advanced to a later frame at the tempo. The continuous counter is the
    /// node's own [`SteadyTime`], not `Env`'s frame (which a re-prepare starts
    /// over).
    ///
    /// Mutation: read `cx.env.transport` instead of `cx.transport_at(offset)`
    /// in `Snapshot::at` → the beat at offset 24 stays 2.0 → fails. Mutation:
    /// `continuous: env.frame_at(offset)` → the counter is 1024, not the
    /// node's 7 024 → fails. Mutation: drop `.with_recording` in
    /// `transport_info` → fails.
    #[test]
    fn the_snapshot_is_env_at_the_frame() {
        let t = Transport::new(true, Bpm(120.0), Beat(2.0), None).with_recording(true);
        let e = env(1_000, 64, t, TransportChanges::NONE);
        let meter = MeterMap::default();

        let first = from_cx(
            &cx(&e),
            at(0, 64),
            0,
            &mut ProjectTime::default(),
            None,
            &meter,
        );
        assert!(first.state.playing);
        assert!(first.state.recording);
        assert_eq!(first.timing.tempo, 120.0);
        assert_eq!(first.position.beats, 2.0);
        assert_eq!(first.sample_rate, 48_000.0);

        // 24 frames at 120 BPM / 48 kHz: 24 / 24 000 of a beat.
        // The node has rendered 7 000 frames of its own, whatever `Env`'s
        // frame says (1 000 here: as after a re-prepare restarted the graph).
        let mut steady = SteadyTime::default();
        steady.advance(7_000);
        let later = from_cx(
            &cx(&e),
            at(24, 64),
            steady.at(24),
            &mut ProjectTime::default(),
            None,
            &meter,
        );
        assert_eq!(
            later.position.beats,
            e.transport_at(at(24, 64)).beat().get()
        );
        assert!((later.position.beats - (2.0 + 24.0 / 24_000.0)).abs() < 1e-12);
        assert_eq!(later.position.continuous_samples, 7_024);
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
            let got = from_cx(
                &cx(&e),
                at(i, 64),
                0,
                &mut ProjectTime::default(),
                None,
                &meter,
            );
            let want = e.transport_at(at(i, 64));
            assert_eq!(got.position.beats, want.beat().get(), "frame {i}");
            assert_eq!(got.timing.tempo, want.tempo.get(), "frame {i}");
            assert_eq!(got.state.cycle_active, want.looping.is_some(), "frame {i}");
        }
        // The wrap: 32 frames into a 32-frame loop is back at its start.
        assert_eq!(
            from_cx(
                &cx(&e),
                at(32, 64),
                0,
                &mut ProjectTime::default(),
                None,
                &meter
            )
            .position
            .beats,
            4.0
        );
        // The seek, and its tempo.
        let seeked = from_cx(
            &cx(&e),
            at(40, 64),
            0,
            &mut ProjectTime::default(),
            None,
            &meter,
        );
        assert_eq!(seeked.position.beats, 16.0);
        assert_eq!(seeked.timing.tempo, 90.0);
        assert!(!seeked.state.cycle_active);
    }

    /// Behind a latent path the snapshot is the transport the node's input
    /// was rendered at: `arrival` frames earlier, in an earlier block, before
    /// the seek this block starts with — not the block's own playhead.
    ///
    /// Mutation: read `cx.env.transport_at(offset)` in `Snapshot::at` → the
    /// snapshot reads the post-seek beat 32 → fails.
    #[test]
    fn behind_a_latent_path_the_snapshot_is_the_inputs_transport() {
        let meter = MeterMap::default();
        let mut history = TransportHistory::new();
        let first = env(
            0,
            64,
            Transport::new(true, Bpm(120.0), Beat(2.0), None),
            TransportChanges::NONE,
        );
        history.observe(&first);
        // A seek at the next block's start.
        let second = env(
            64,
            64,
            Transport::new(true, Bpm(120.0), Beat(32.0), None),
            TransportChanges::NONE,
        );
        history.observe(&second);
        let cx = Cx {
            env: &second,
            arrival: Latency::new(Samples(100)),
            history: &history,
        };
        // Offset 50 of the second block hears frame 14 of the first.
        let got = from_cx(
            &cx,
            at(50, 64),
            0,
            &mut ProjectTime::default(),
            None,
            &meter,
        );
        assert_eq!(
            got.position.beats,
            first.transport_at(at(14, 64)).beat().get()
        );
        assert!((got.position.beats - (2.0 + 14.0 / 24_000.0)).abs() < 1e-12);
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
        let out = from_cx(
            &cx(&e),
            at(0, 64),
            0,
            &mut ProjectTime::default(),
            None,
            &meter,
        );

        assert_eq!(out.timing.signature, seven_eight);
        assert_eq!(out.bar.number, BarNumber(2));
        assert!((out.bar.start_beats - 3.5).abs() < 1e-9);
        // VST2's `bar_start_pos` / VST3's `barPositionMusic` read this one.
        assert!((out.bar.position_quarters - 3.5).abs() < 1e-9);
        // And the VST playhead.
        assert!((out.position.quarters - 3.5).abs() < 1e-9);
    }

    /// **Project time rolls through a tempo change**: the reviewer's case,
    /// beat 100 at 120 BPM changing to 121 BPM at frame 32 with no seek, and
    /// the next block on. Read at every frame in order, as a node does, it
    /// rises by exactly the frames rendered, and CLAP's seconds are the
    /// same time. Re-derived from the beat at the new tempo, frame 32 would
    /// jump back to 2 380 198.
    ///
    /// Mutation: always re-seat (`rolled` ignored in `ProjectTime::at`) →
    /// frame 32 reads 2 380 198 → fails. Mutation: seconds from
    /// `beats · 60 / tempo` again → frame 32's seconds are 49.587 → fails.
    #[test]
    fn project_time_rolls_through_a_tempo_change() {
        let meter = MeterMap::default();
        let after = Beat(100.0 + 32.0 / 24_000.0);
        let mut changes = TransportChanges::NONE;
        changes
            .push(at(32, 64), Transport::new(true, Bpm(121.0), after, None))
            .expect("a change inside the block");
        let first = env(
            0,
            64,
            Transport::new(true, Bpm(120.0), Beat(100.0), None),
            changes,
        );
        // 32 frames at 121 BPM on from the change.
        let next_beat = Beat(after.get() + 32.0 * 121.0 / 60.0 / 48_000.0);
        let second = env(
            64,
            64,
            Transport::new(true, Bpm(121.0), next_beat, None),
            TransportChanges::NONE,
        );
        let mut project = ProjectTime::default();
        let mut steady = SteadyTime::default();
        for e in [first, second] {
            for i in 0..64 {
                let got = from_cx(&cx(&e), at(i, 64), steady.at(i), &mut project, None, &meter);
                let want = 2_400_000 + steady.at(i);
                assert_eq!(got.position.samples, Some(want), "frame {}", steady.at(i));
                assert_eq!(got.position.seconds, want as f64 / 48_000.0);
            }
            steady.advance(64);
        }
    }

    /// **A seek, a loop wrap and a start re-seat project time** at the beat's
    /// time at the tempo; between them it rolls. At 120 BPM and 48 kHz a beat
    /// is 24 000 frames; at 90 BPM, 32 000. Without a usable tempo there is
    /// none.
    ///
    /// Mutation: roll on a wrap too (`continuity != Jumped` in
    /// `ProjectTime::at`) → the wrap reads 96 032 → fails. Mutation: roll
    /// through a jump (`continuity` ignored) → the seek reads 96 040 too. Mutation: roll
    /// from a stopped read (drop `p.transport.playing`) → the start reads
    /// the stopped value plus 64 → fails. Mutation: drop
    /// `.with_position_samples` in `transport_info` → `None` → fails.
    #[test]
    fn a_seek_a_loop_wrap_and_a_start_re_seat_project_time() {
        let meter = MeterMap::default();
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
        let mut project = ProjectTime::default();
        let samples: Vec<_> = (0..64)
            .map(|i| {
                from_cx(&cx(&e), at(i, 64), i as i64, &mut project, None, &meter)
                    .position
                    .samples
            })
            .collect();
        // Beat 4 at 120 BPM: 2 s.
        assert_eq!(samples[0], Some(96_000));
        assert_eq!(samples[7], Some(96_007));
        // The wrap back to the loop's start.
        assert_eq!(samples[32], Some(96_000));
        assert_eq!(samples[39], Some(96_007));
        // The seek to beat 16 at 90 BPM: 16 * 32 000.
        assert_eq!(samples[40], Some(512_000));
        assert_eq!(samples[63], Some(512_023));

        // Stopped a block later at beat 3 (a stop re-seats, and holds), then
        // started there at 60 BPM: 3 * 48 000.
        let stopped = env(
            64,
            64,
            Transport::new(false, Bpm(90.0), Beat(3.0), None),
            TransportChanges::NONE,
        );
        let held = from_cx(&cx(&stopped), at(0, 64), 64, &mut project, None, &meter);
        assert_eq!(held.position.samples, Some(96_000));
        let started = env(
            128,
            64,
            Transport::new(true, Bpm(60.0), Beat(3.0), None),
            TransportChanges::NONE,
        );
        let run = from_cx(&cx(&started), at(0, 64), 128, &mut project, None, &meter);
        assert_eq!(run.position.samples, Some(144_000));

        let stalled = Transport::new(true, Bpm(0.0), Beat(2.0), None);
        let e = env(192, 64, stalled, TransportChanges::NONE);
        let none = from_cx(&cx(&e), at(0, 64), 192, &mut project, None, &meter);
        assert_eq!(none.position.samples, None);
    }

    /// **The host clock is the block's reading advanced by the chunk's
    /// frames**, and an offline render (no reading) reports none. 48 frames at
    /// 48 kHz are 1 ms.
    ///
    /// Mutation: `HostClock::at` ignores `frames` → the chunk 48 frames in
    /// reads the block's time → fails. Mutation: `system_time_ns` not passed
    /// on in `transport_info` → `None` → fails.
    #[test]
    fn the_host_clock_is_the_blocks_reading_at_the_chunks_frame() {
        let meter = MeterMap::default();
        let e = env(
            0,
            64,
            Transport::new(true, Bpm(120.0), Beat(0.0), None),
            TransportChanges::NONE,
        );
        let block_ns = Some(5_000_000_000);
        let first = from_cx(
            &cx(&e),
            at(0, 64),
            0,
            &mut ProjectTime::default(),
            block_ns,
            &meter,
        );
        assert_eq!(first.system_time_ns, Some(5_000_000_000));
        let later = from_cx(
            &cx(&e),
            at(48, 64),
            0,
            &mut ProjectTime::default(),
            block_ns,
            &meter,
        );
        assert_eq!(later.system_time_ns, Some(5_001_000_000));
        let offline = from_cx(
            &cx(&e),
            at(48, 64),
            0,
            &mut ProjectTime::default(),
            None,
            &meter,
        );
        assert_eq!(offline.system_time_ns, None);

        // The OS's monotonic clock: `Instant` reads the same clock on
        // every platform (`CLOCK_MONOTONIC`, `CLOCK_UPTIME_RAW`, QPC), so
        // a reading sits between two of its own, with a millisecond of
        // slack for the conversions.
        let before = Instant::now();
        let x = HostClock.now_ns();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let y = HostClock.now_ns();
        let elapsed = i64::try_from(before.elapsed().as_nanos()).expect("fits");
        assert!(y >= x + 20_000_000, "a later reading is 20 ms on");
        assert!(
            y - x <= elapsed + 1_000_000,
            "and no further than Instant says"
        );
    }

    /// **The host clock is the OS's**, not one this process started: on
    /// Unix a reading sits between two direct reads of the clock it names
    /// (on Windows, QPC's scaling is pinned by the 20 ms check above).
    ///
    /// Mutation: count from a process-local `Instant` origin again → the
    /// reading is milliseconds, the OS's is the uptime → fails. Mutation:
    /// `CLOCK_REALTIME` → decades past the uptime → fails.
    #[cfg(unix)]
    #[test]
    fn the_host_clock_is_the_os_monotonic_clock() {
        #[cfg(target_vendor = "apple")]
        const CLOCK: libc::clockid_t = libc::CLOCK_UPTIME_RAW;
        #[cfg(not(target_vendor = "apple"))]
        const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;
        let direct = || {
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: a valid, writable `timespec`.
            unsafe { libc::clock_gettime(CLOCK, &mut ts) };
            #[allow(clippy::unnecessary_cast)]
            let ns = ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64;
            ns
        };
        let a = direct();
        let x = HostClock.now_ns();
        let b = direct();
        assert!(a <= x && x <= b, "{a} <= {x} <= {b}");
    }
}
