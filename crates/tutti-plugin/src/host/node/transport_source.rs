//! The transport a hosted plugin is handed each block: a pure function of the
//! block's [`Env`](tutti_graph::Env) and the project meter.
//!
//! The plugin node does not poll a shared timeline. The executor hands it the
//! block's [`Env`](tutti_graph::Env), whose
//! transport is the engine's own playhead at the block's first frame, with
//! every start, stop, seek, tempo or loop edit inside the block in
//! [`Env::changes`](tutti_graph::Env::changes). [`from_cx`] reads it at a frame with
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
//! ([`ProjectTime`], which rolls with playback, holds while stopped and
//! re-seats only at a jump)
//! and the host's monotonic clock ([`HostClock`], read once per block; none
//! for an offline render).

use tutti_graph::{Continuity, Cx, Offset, Transport};
use tutti_tempo::{Cursor, TempoMap};
use tutti_types::Samples;
use tutti_types::{Beat, Bpm, SampleRate, SegmentOrigin};
use tutti_types::{Meter, MeterMap};

use tutti_plugin_types::TransportInfo;

/// The transport facts a plugin snapshot is made of, at one frame.
///
/// Engine types throughout: [`transport_info`] is where they meet the ABI's
/// `f64`s.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Snapshot {
    pub(crate) playing: bool,
    pub(crate) recording: bool,
    pub(crate) tempo: Bpm,
    /// How the tempo moves across the chunk this snapshot opens, in BPM per
    /// sample ([`follow`](Snapshot::follow)): `0.0` until a map says
    /// otherwise. A raw `f64`: it is CLAP's `tempo_inc`, and the unit types
    /// stop at the C ABI.
    pub(crate) tempo_inc: f64,
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
/// Its own count, not [`Env::frame`](tutti_graph::Env::frame): the executor's frame is the graph's
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
/// It rolls by the frames the transport played between two reads, holds
/// while it is stopped, and is re-seated from the beat at the tempo
/// (`beat · 60 / tempo · rate`) only at a jump: the first read, a seek
/// (stopped or playing), a loop wrap, or a rate change. A tempo change is
/// not a jump, and neither is a stop, a start where the transport stood, or
/// a loop edit: project time keeps counting through them instead of jumping
/// to the beat's time at the tempo in force. The engine has no tempo map,
/// so that time is only right from a re-seat until the next tempo change; a
/// start where playback stopped continues the time the plugin was told
/// there, as a DAW's tempo map would.
///
/// Reads come a chunk apart, so several of those can land between two of
/// them. The engine's transport says which: its position is a frame count
/// on a segment ([`Transport::origin`]), and the host's clock starts a new
/// segment at a seek, a tempo change, a rate change and a loop wrap, never
/// at a stop or a start (a stopped clock only stops counting) or a loop
/// edit. So ([`played_between`]):
///
/// - **The same segment** (origin beat and tempo): the transport only
///   played, stopped or started, and the frame counts' difference is the
///   frames it played, exactly. A wrap or a seek that lands back on the
///   segment's first beat starts a segment with the same origin; counting
///   it reads what a re-seat would, since project time has been the beat's
///   time from that segment's own wrap or seek on. Only a segment begun at
///   a tempo change, sought back to within one chunk, is counted as
///   played.
/// - **A new segment at another tempo**: a tempo change. The frames from
///   the earlier read up to the new segment's first beat, at the earlier
///   tempo, plus the new segment's frames. Exact for one tempo change
///   between the reads; with more (a ramp's steps inside one chunk), the
///   frames at the tempos between are counted at the earlier one. A ramp
///   that speeds up plays those beats in fewer frames than that count, so
///   the count is capped at the frames between the reads: if those beats
///   fit the gap at the faster of the two tempos, it is a ramp, not a seek.
///   The cost is a wider seek-plus-tempo ambiguity (below): a seek forward
///   and a tempo change inside one chunk are counted as played whenever the
///   beats moved fit the gap at the faster tempo.
///   The faster of the two is the fastest only for a ramp that moves one
///   way: a tempo that rises above both reads' tempos and falls again
///   inside one chunk can play the beats in fewer frames than that count,
///   and re-seats. A non-finite beat re-seats.
/// - **A new segment at the same tempo**: a seek or a wrap, a re-seat. So is
///   a tempo that changes and comes back inside one chunk.
///
/// Either count must be no more than the frames between the reads, or it
/// re-seats. A seek and a tempo change inside one chunk cannot be told
/// from a tempo change alone: a seek forward by less than the chunk plays
/// at the faster tempo is counted as played, as the gap.
///
/// A transport built by hand ([`Transport::new`]: no frame count, which no
/// engine block carries, and none when walked on to a frame) is read by the
/// playhead's rule instead ([`continued_without_origin`]).
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
        if !usable(hz) {
            self.last = None;
            return None;
        }
        let continued = self.last.and_then(|p| {
            let gap = usize::try_from(steady.checked_sub(p.steady)?).ok()?;
            if p.rate != rate {
                return None;
            }
            let played = match (p.transport.origin(), t.origin()) {
                (Some(from), Some(to)) => played_between((&p.transport, from), (t, to), gap, rate),
                _ => continued_without_origin(&p.transport, gap, rate, t),
            };
            played.map(|n| p.frames + n as f64)
        });
        let frames = continued.unwrap_or_else(|| t.beat().get() * 60.0 / tempo * hz);
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

/// The frames the transport played from a read of `from` to one of `to`
/// `gap` frames later, from their segments (see [`ProjectTime`]); `None` at
/// a jump.
fn played_between(
    (from, a): (&Transport, SegmentOrigin),
    (to, b): (&Transport, SegmentOrigin),
    gap: usize,
    rate: SampleRate,
) -> Option<usize> {
    // A rate change is a new segment too, but `ProjectTime::at` has
    // re-seated at it already: both reads are at `rate`.
    let played = if a.beat == b.beat && from.tempo == to.tempo {
        b.frame.get().checked_sub(a.frame.get())?
    } else if from.tempo != to.tempo {
        // `to`'s segment began at the tempo change, on the beat `from`'s
        // segment had reached there: the beats between, as frames at
        // `from`'s tempo. Steps of a ramp between the reads played some of
        // them at tempos between the two, so they took no fewer frames than
        // at the faster of the two; a speeding-up ramp's count at `from`'s
        // tempo can pass the room the gap leaves, and is capped there.
        let beats = b.beat.get() - from.beat().get();
        // `min` and `max` below drop a NaN, so a non-finite count must
        // leave here.
        if !beats.is_finite() {
            return None;
        }
        let frames_at = |tempo: Bpm| (beats * 60.0 / tempo.get() * rate.get()).round();
        let least = frames_at(from.tempo.max(to.tempo));
        let room = gap.saturating_sub(usize::try_from(b.frame.get()).ok()?) as f64;
        // More than the fewest is not capped, so the check below re-seats it.
        let before = frames_at(from.tempo).min(least.max(room));
        if before < 0.0 {
            return None;
        }
        (before as u64).checked_add(b.frame.get())?
    } else {
        return None;
    };
    usize::try_from(played).ok().filter(|&n| n <= gap)
}

/// [`ProjectTime`]'s reading when a transport has no segment (one built by
/// hand): the playhead's rule ([`Transport::continuity`]) judged against
/// the earlier read's loop, so a loop edit moves nothing by itself; a stop
/// or a start between the reads is [`played_across`]. A move out past an
/// unchanged loop's end without wrapping re-seats either way.
fn continued_without_origin(
    from: &Transport,
    gap: usize,
    rate: SampleRate,
    to: &Transport,
) -> Option<usize> {
    if from.looping == to.looping
        && let Some(l) = from.looping
    {
        let (start, end) = (from.beat().get(), to.beat().get());
        if start < l.end.get() && end >= l.end.get() {
            return None;
        }
    }
    let mut seen = *to;
    seen.looping = from.looping;
    match from.continuity(Samples(gap), rate, &seen) {
        Continuity::Rolled => Some(gap),
        Continuity::Held => Some(0),
        Continuity::Wrapped => None,
        Continuity::Jumped => played_across(from, gap, rate, &seen),
    }
}

/// The frames played between a read of `from` and one of `to` `gap` frames
/// later, when the transport stopped or started (or both) between them and
/// moved only by playing: the beats moved forward, as frames at a tempo the
/// transport played at, no more than the gap. The tempo is the playing
/// side's when one read is stopped; with both playing (a stop and a start
/// between them) or both stopped (a start and a stop), `from`'s, else
/// `to`'s. Counting at one tempo frames played at another (a tempo change
/// between the reads) is off by `n · |1 − r|` frames, `r` the tempo they
/// played at over the tempo counted, up to the whole gap at a 2 : 1 change.
/// `None` for anything else: a move back, or further than the gap plays.
fn played_across(from: &Transport, gap: usize, rate: SampleRate, to: &Transport) -> Option<usize> {
    let tempos = match (from.playing, to.playing) {
        (true, false) => [from.tempo, from.tempo],
        (false, true) => [to.tempo, to.tempo],
        _ => [from.tempo, to.tempo],
    };
    let moved = to.beat().get() - from.beat().get();
    tempos.into_iter().find_map(|tempo| {
        let played = (moved * 60.0 / tempo.get() * rate.get()).round();
        (0.0..=gap as f64)
            .contains(&played)
            .then_some(played as usize)
    })
}

/// How the tempo moves over a stretch of `frames` frames that starts at
/// `beat`, in BPM per sample (CLAP's `tempo_inc`: "tempo increment for each
/// sample and until the next time info event"), for a plugin told the stretch
/// by one transport event (doc 020 stage 5b: a stretch runs to the next event
/// or the chunk's end).
///
/// The map's slope, held inside the segment the stretch starts in
/// ([`TempoMap::tempo_slope_with`]): exact for a
/// [`RampTime`](tutti_tempo::Shape::RampTime) ramp, whose tempo is linear in
/// time, and a step inside the stretch adds no jump to it (a step is an event
/// of its own). `0.0` when the transport is stopped, when the stretch reaches
/// the loop's end (its last frame's beat is past `loop_end`: the beat jumps
/// back there, so no slope describes the stretch), or when a figure is not
/// usable.
pub(crate) fn slope(
    map: &TempoMap,
    cursor: &mut Cursor,
    beat: Beat,
    playing: bool,
    loop_end: Option<Beat>,
    rate: SampleRate,
    frames: usize,
) -> f64 {
    let rate = rate.get();
    if !playing || frames == 0 || !beat.get().is_finite() || !rate.is_finite() || rate <= 0.0 {
        return 0.0;
    }
    let t0 = map.seconds_at_with(beat, cursor).get();
    if let Some(end) = loop_end {
        let last = tutti_types::Seconds(t0 + (frames - 1) as f64 / rate);
        if map.beat_at_seconds_with(last, cursor).get() > end.get() {
            return 0.0;
        }
    }
    let t1 = tutti_types::Seconds(t0 + frames as f64 / rate);
    // `.get()`: the slope is a raw `f64` BPM per sample, for the C ABI.
    map.tempo_slope_with(tutti_types::Seconds(t0), t1, cursor) / rate
}

impl Snapshot {
    /// Replaces the transport's chord tempo with the project's tempo map's at
    /// the snapshot's beat (which already accounts for latency, loops and
    /// seeks), and clears the slope ([`slope`] sets it once a stretch's length
    /// is known). Returns the beat the tempo was read at: the snapshot's, or
    /// the beat of a map point it sits within a frame's tolerance before (an
    /// engine's beat on a point's frame can fall an ulp short of the point's
    /// own, which would read the tempo before the point).
    ///
    /// - **`tempo`** is the map's instantaneous tempo there: a step's own
    ///   tempo (not a chord across it), a ramp's curve at that beat.
    ///
    /// A beat that is not finite keeps the transport's tempo.
    pub(crate) fn settle(&mut self, map: &TempoMap, cursor: &mut Cursor) -> Beat {
        self.tempo_inc = 0.0;
        let mut beat = self.beat;
        if !beat.get().is_finite() {
            return beat;
        }
        let rate = self.sample_rate.get();
        if rate.is_finite() && rate > 0.0 {
            let points = map.points();
            let from = map.seconds_at_with(beat, cursor).get();
            let mut next = points.partition_point(|p| p.beat.get() <= beat.get());
            while let Some(p) = points.get(next) {
                let ahead = (map.seconds_at_with(p.beat, cursor).get() - from) * rate;
                if tutti_types::first_frame_at_or_after(ahead) > 0 {
                    break;
                }
                beat = p.beat;
                next += 1;
            }
        }
        self.tempo = map.tempo_at_with(beat, cursor);
        beat
    }

    /// [`settle`](Self::settle), and the slope over a stretch of `chunk`
    /// frames from there ([`slope`]). What a reader that sends one transport
    /// per chunk (the in-process VST2 node) is told; the plugin node walks the
    /// chunk's stretches itself (`transport_walk`).
    #[cfg_attr(
        not(any(feature = "vst2", test)),
        allow(
            dead_code,
            reason = "only the in-process VST2 node reads one transport per chunk"
        )
    )]
    pub(crate) fn follow(&mut self, map: &TempoMap, cursor: &mut Cursor, chunk: usize) {
        let beat = self.settle(map, cursor);
        self.tempo_inc = slope(
            map,
            cursor,
            beat,
            self.playing,
            self.looping.map(|(_, end)| end),
            self.sample_rate,
            chunk,
        );
    }

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
            tempo_inc: 0.0,
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

/// What a block reads of the project besides its transport: the meter (the
/// signature and bar) and the tempo map (the tempo and its slope), each lent
/// once for the block, with the node's cursor into the map. Built per block
/// from the node's two readers; nothing here allocates.
#[cfg_attr(
    not(any(feature = "vst2", test)),
    allow(
        dead_code,
        reason = "only the in-process VST2 node reads one transport per chunk"
    )
)]
pub(crate) struct ProjectMaps<'a> {
    pub(crate) meter: &'a MeterMap,
    pub(crate) tempo: &'a TempoMap,
    pub(crate) cursor: &'a mut Cursor,
}

/// [`Snapshot::at`] for a chunk of `chunk` frames, as the plugin ABIs carry
/// it: the tempo and slope from `maps`' tempo map ([`Snapshot::follow`]) and
/// the signature and bar from its meter at the playhead.
#[cfg_attr(
    not(any(feature = "vst2", test)),
    allow(
        dead_code,
        reason = "only the in-process VST2 node reads one transport per chunk"
    )
)]
pub(crate) fn from_cx(
    cx: &Cx<'_>,
    offset: Offset,
    chunk: usize,
    continuous: i64,
    project: &mut ProjectTime,
    block_ns: Option<i64>,
    maps: &mut ProjectMaps<'_>,
) -> TransportInfo {
    let mut s = Snapshot::at(cx, offset, continuous, project, block_ns);
    s.follow(maps.tempo, maps.cursor, chunk);
    transport_info(&s, maps.meter)
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
        .with_tempo_inc(s.tempo_inc)
        .with_playing(s.playing)
        .with_recording(s.recording)
        .with_time_signature(position.signature)
        .with_bar(position.bar_start.get(), position.bar)
        .with_sample_rate(s.sample_rate.get());

    // CLAP's beats position, and its seconds: project time
    // (`ProjectTime`), or zero without one.
    let beats = s.beat.get();
    let seconds = tutti_types::Seconds(
        s.project_frames
            .map_or(0.0, |frames| frames / s.sample_rate.get()),
    );
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
    use tutti_graph::{Env, LoopRange, Transport, TransportChanges, TransportHistory};
    use tutti_types::Latency;
    use tutti_types::{BarNumber, BeatsPerBar, MeterChange, NoteValue, TimeSignature};
    use tutti_types::{Frame, FrameClock, Samples};

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

    /// `from_cx` for a chunk of one frame over `map`.
    fn info_in(
        map: &TempoMap,
        cx: &Cx<'_>,
        offset: Offset,
        continuous: i64,
        project: &mut ProjectTime,
        block_ns: Option<i64>,
        meter: &MeterMap,
    ) -> TransportInfo {
        from_cx(
            cx,
            offset,
            1,
            continuous,
            project,
            block_ns,
            &mut ProjectMaps {
                meter,
                tempo: map,
                cursor: &mut Cursor::default(),
            },
        )
    }

    /// A constant tempo map.
    fn constant(bpm: f64) -> TempoMap {
        TempoMap::constant(Bpm::of(bpm)).expect("a tempo")
    }

    /// `from_cx` over a constant 120 BPM map, the tempo these tests' transports
    /// carry, so what the plugin is told is the transport's own.
    fn info_at(
        cx: &Cx<'_>,
        offset: Offset,
        continuous: i64,
        project: &mut ProjectTime,
        block_ns: Option<i64>,
        meter: &MeterMap,
    ) -> TransportInfo {
        info_in(
            &constant(120.0),
            cx,
            offset,
            continuous,
            project,
            block_ns,
            meter,
        )
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
        let t = Transport::new(true, Bpm::of(120.0), Beat(2.0), None).with_recording(true);
        let e = env(1_000, 64, t, TransportChanges::NONE);
        let meter = MeterMap::default();

        let first = info_at(
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
        let later = info_at(
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
            Bpm::of(120.0),
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
                Transport::new(true, Bpm::of(90.0), Beat(16.0), None),
            )
            .expect("a change inside the block");
        let e = env(0, 64, t, changes);
        let meter = MeterMap::default();
        // The project's map is 120 BPM until beat 16, then 90: what the
        // transport's own tempo change (the seek lands on beat 16) agrees with.
        let map = TempoMap::new([
            tutti_tempo::TempoPoint {
                beat: Beat(0.0),
                tempo: Bpm::of(120.0),
                shape: tutti_tempo::Shape::Step,
            },
            tutti_tempo::TempoPoint {
                beat: Beat(16.0),
                tempo: Bpm::of(90.0),
                shape: tutti_tempo::Shape::Step,
            },
        ])
        .expect("a map");

        for i in 0..64 {
            let got = info_in(
                &map,
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
            info_in(
                &map,
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
        let seeked = info_in(
            &map,
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
            Transport::new(true, Bpm::of(120.0), Beat(2.0), None),
            TransportChanges::NONE,
        );
        history.observe(&first);
        // A seek at the next block's start.
        let second = env(
            64,
            64,
            Transport::new(true, Bpm::of(120.0), Beat(32.0), None),
            TransportChanges::NONE,
        );
        history.observe(&second);
        let cx = Cx {
            env: &second,
            arrival: Latency::new(Samples(100)),
            history: &history,
        };
        // Offset 50 of the second block hears frame 14 of the first.
        let got = info_at(
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
        let seven_eight = TimeSignature::new(BeatsPerBar::of(7), NoteValue::EIGHTH);
        let meter = MeterMap::new([MeterChange::new(Beat(0.0), seven_eight)]);
        // Bar 2 of 7/8 starts at 3.5 quarter notes, not 7.
        let e = env(
            0,
            64,
            Transport::new(true, Bpm::of(120.0), Beat(3.5), None),
            TransportChanges::NONE,
        );
        let out = info_at(
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

    fn point(beat: f64, bpm: f64, shape: tutti_tempo::Shape) -> tutti_tempo::TempoPoint {
        tutti_tempo::TempoPoint {
            beat: Beat(beat),
            tempo: Bpm::of(bpm),
            shape,
        }
    }

    /// What a plugin is told for a chunk of `chunk` frames starting at `t`'s
    /// beat, over `map`: the transport's own tempo is 120 in every test
    /// below, so any other figure came from the map.
    fn told(map: &TempoMap, t: Transport, chunk: usize) -> TransportInfo {
        let e = env(0, 64, t, TransportChanges::NONE);
        from_cx(
            &cx(&e),
            at(0, 64),
            chunk,
            0,
            &mut ProjectTime::default(),
            None,
            &mut ProjectMaps {
                meter: &MeterMap::default(),
                tempo: map,
                cursor: &mut Cursor::default(),
            },
        )
    }

    fn rolling(beat: f64) -> Transport {
        Transport::new(true, Bpm::of(120.0), Beat(beat), None)
    }

    /// 120 BPM, then 90 from beat 4 (a step: the new tempo from the point's
    /// own beat, no chord across it).
    fn step_map() -> TempoMap {
        TempoMap::new([
            point(0.0, 120.0, tutti_tempo::Shape::Step),
            point(4.0, 90.0, tutti_tempo::Shape::Step),
        ])
        .expect("a map")
    }

    /// 120 to 180 BPM over four beats, tempo linear in time (`RampTime`): it
    /// lasts 1.6 s (mean 150 BPM), 76 800 frames at 48 kHz, so the tempo
    /// climbs 60 / 76 800 BPM per frame, and is 150 at 0.8 s, beat 1.8
    /// ((120 t + 18.75 t^2) / 60 at t = 0.8).
    fn ramp_map() -> TempoMap {
        TempoMap::new([
            point(0.0, 120.0, tutti_tempo::Shape::RampTime),
            point(4.0, 180.0, tutti_tempo::Shape::Step),
        ])
        .expect("a map")
    }

    const RAMP_SLOPE: f64 = 60.0 / 76_800.0;

    /// A chunk that starts on a step reports the step's own tempo: the map's
    /// instantaneous tempo at the chunk's beat, not the transport's (the
    /// engine's chord, 120 here), and no slope across a step.
    ///
    /// Mutation: keep `t.tempo` in `Snapshot::follow` (drop the
    /// `tempo_at_with` line) -> the chunk at beat 4 reads 120 -> fails.
    #[test]
    fn a_chunk_on_a_step_is_told_the_steps_own_tempo() {
        let map = step_map();
        let on = told(&map, rolling(4.0), 64);
        assert_eq!(on.timing.tempo, 90.0);
        assert_eq!(on.timing.tempo_inc, 0.0);
        let before = told(&map, rolling(3.5), 64);
        assert_eq!(before.timing.tempo, 120.0);
        assert_eq!(before.timing.tempo_inc, 0.0);
    }

    /// Inside a `RampTime` ramp the chunk is told the curve's instantaneous
    /// tempo at its start beat and the analytic slope, whatever the chunk's
    /// length (tempo is linear in time).
    ///
    /// Mutation: leave `tempo_inc` at 0.0 in `follow` -> fails. Mutation:
    /// divide by `chunk + 1` -> the slope is off by 1/65 -> fails. Mutation:
    /// `frames / (rate * 2.0)` in `t1` (the chunk's end at half its time) ->
    /// the tempo gained is halved -> fails.
    #[test]
    fn a_chunk_in_a_ramp_is_told_the_curve_and_its_slope() {
        let map = ramp_map();
        for chunk in [64, 1_000] {
            let got = told(&map, rolling(1.8), chunk);
            assert!(
                (got.timing.tempo - 150.0).abs() < 1e-6,
                "tempo {}",
                got.timing.tempo
            );
            assert!(
                (got.timing.tempo_inc - RAMP_SLOPE).abs() < 1e-9,
                "slope {} for {chunk} frames",
                got.timing.tempo_inc
            );
        }
    }

    /// A stretch is told the slope of the segment it starts in, and a map
    /// point inside it does not zero it (stage 5a zeroed it; stage 5b splits
    /// the chunk at the point, `transport_walk`, so the stretch before it has
    /// a slope that is right). The slope is held to the segment's end: it is
    /// not the change of tempo from the stretch's start to the tempo one
    /// stretch later, which past a point reads the next segment. A chunk that
    /// *starts* on a point has the point's segment's slope.
    ///
    /// Mutation: in `slope`, take `(tempo_at_seconds(t1) - tempo) / frames`
    /// for the stretch (the 5a rule without its zero) -> the stretch reaching
    /// beat 4 from 3.99 reads the next segment's falling tempo -> fails.
    /// Mutation: return `0.0` when a point lies in the stretch (the 5a zero)
    /// -> the stretch reaching beat 4 is told none -> fails.
    #[test]
    fn a_stretch_with_a_point_inside_is_told_its_segments_slope() {
        let map = TempoMap::new([
            point(0.0, 120.0, tutti_tempo::Shape::RampTime),
            point(4.0, 180.0, tutti_tempo::Shape::RampTime),
            point(8.0, 60.0, tutti_tempo::Shape::Step),
        ])
        .expect("a map");
        // 1000 frames at 180 BPM is 0.0625 beat: past beat 4 from 3.99. The
        // first segment is the 120 -> 180 ramp (RAMP_SLOPE a frame), held to
        // its end at beat 4; the chunk's tempo is that ramp's, just under 180.
        let across = told(&map, rolling(3.99), 1_000);
        assert!((across.timing.tempo - 180.0).abs() < 0.5);
        assert!(
            (across.timing.tempo_inc - RAMP_SLOPE).abs() < 1e-9,
            "slope {}",
            across.timing.tempo_inc
        );
        // On the point: 180 to 60 over four beats, 2 s (mean 120 BPM), so
        // -120 BPM over 96 000 frames.
        let on = told(&map, rolling(4.0), 64);
        assert!((on.timing.tempo - 180.0).abs() < 1e-9);
        assert!(
            (on.timing.tempo_inc - (-120.0 / 96_000.0)).abs() < 1e-9,
            "slope {}",
            on.timing.tempo_inc
        );
    }

    /// A stopped transport has no slope, though the map has one at its
    /// playhead; the tempo is still the map's.
    ///
    /// Mutation: drop `!self.playing` from the early return -> the stopped
    /// chunk is told the ramp's slope -> fails.
    #[test]
    fn a_stopped_transport_is_told_no_slope() {
        let stopped = Transport::new(false, Bpm::of(120.0), Beat(1.8), None);
        let got = told(&ramp_map(), stopped, 64);
        assert!((got.timing.tempo - 150.0).abs() < 1e-6);
        assert_eq!(got.timing.tempo_inc, 0.0);
    }

    /// A chunk that crosses the loop's end is told no slope: the beat jumps
    /// back, so the ramp ahead is not what the plugin plays. One that stays
    /// inside the loop keeps it.
    ///
    /// Mutation: drop the `self.looping` check -> the crossing chunk is told
    /// the slope -> fails.
    #[test]
    fn a_chunk_crossing_the_loop_end_is_told_no_slope() {
        let looped = |end: f64| {
            Transport::new(
                true,
                Bpm::of(120.0),
                Beat(1.8),
                Some(LoopRange {
                    start: Beat(1.0),
                    end: Beat(end),
                }),
            )
        };
        // 64 frames at 150 BPM is 0.0033 beat.
        let crossing = told(&ramp_map(), looped(1.801), 64);
        assert_eq!(crossing.timing.tempo_inc, 0.0);
        let inside = told(&ramp_map(), looped(3.0), 64);
        assert!((inside.timing.tempo_inc - RAMP_SLOPE).abs() < 1e-9);
    }

    /// A constant map is its tempo and no slope.
    ///
    /// Mutation: add `+ 1.0` to the slope in `follow` -> fails.
    #[test]
    fn a_constant_map_is_its_tempo_and_no_slope() {
        let got = told(&constant(97.0), rolling(2.0), 64);
        assert_eq!(got.timing.tempo, 97.0);
        assert_eq!(got.timing.tempo_inc, 0.0);
    }

    /// A command the engine's transport takes, as [`Host::block`] lands it.
    #[derive(Clone, Copy)]
    enum Cmd {
        Play,
        Stop,
        Seek(f64),
        Tempo(f64),
        Loop(Option<(f64, f64)>),
        /// Only at a block's start: a rate is the whole block's.
        Rate(f64),
    }

    /// The transport as the engine keeps it (tutti-transport's `TransportClock`):
    /// a [`FrameClock`] that a seek seats, a tempo or rate change starts a
    /// new segment on, a loop wraps, and a stop only stops counting. Each
    /// block's transport and changes are counted ones, as the engine's
    /// `begin` reports them, so project time sees what it sees live.
    #[derive(Clone)]
    struct Host {
        clock: FrameClock,
        playing: bool,
        looping: Option<LoopRange>,
        frame: u64,
    }

    impl Host {
        fn new(playing: bool, bpm: f64, beat: f64) -> Self {
            Self {
                clock: FrameClock::new(Beat(beat), Bpm::of(bpm), SR),
                playing,
                looping: None,
                frame: 0,
            }
        }

        fn transport(&self) -> Transport {
            Transport::counted(
                self.playing,
                self.clock.tempo(),
                self.clock.origin(),
                self.looping,
            )
        }

        fn beat(&self) -> f64 {
            self.clock.beat().get()
        }

        /// The beat `frames` frames on at the tempo, unwrapped.
        fn ahead(&self, frames: f64) -> f64 {
            self.beat() + frames * self.clock.tempo().get() / 60.0 / self.clock.sample_rate().get()
        }

        fn apply(&mut self, cmd: Cmd) {
            match cmd {
                Cmd::Play => self.playing = true,
                Cmd::Stop => self.playing = false,
                Cmd::Seek(beat) => self.clock.seat(Beat(beat)),
                Cmd::Tempo(bpm) => self.clock.set_tempo(Bpm::of(bpm)),
                Cmd::Loop(l) => {
                    self.looping = l.map(|(start, end)| LoopRange {
                        start: Beat(start),
                        end: Beat(end),
                    });
                }
                Cmd::Rate(hz) => self.clock.set_sample_rate(SampleRate(hz)),
            }
        }

        fn roll(&mut self, frames: usize) {
            if self.playing {
                let region = self
                    .looping
                    .and_then(|l| tutti_types::LoopRange::new(l.start, l.end));
                self.clock.advance(Samples(frames), region);
            }
        }

        /// A block of `len` frames with each command at its offset, in
        /// order; one at offset 0 lands before the block's transport is
        /// taken.
        fn block(&mut self, len: usize, cmds: &[(usize, Cmd)]) -> Env {
            for &(_, cmd) in cmds.iter().filter(|(i, _)| *i == 0) {
                self.apply(cmd);
            }
            let start = self.transport();
            let mut changes = TransportChanges::NONE;
            let mut done = 0;
            for &(i, cmd) in cmds.iter().filter(|(i, _)| *i > 0) {
                self.roll(i - done);
                done = i;
                self.apply(cmd);
                changes
                    .push(at(i, len), self.transport())
                    .expect("a change inside the block");
            }
            self.roll(len - done);
            let e = Env {
                frame: Frame(self.frame),
                sample_rate: self.clock.sample_rate(),
                block_len: Samples(len),
                transport: start,
                changes,
                play: tutti_graph::PlayClock::ZERO,
            };
            self.frame += len as u64;
            e
        }
    }

    /// Project time read at each of `offsets` of each block, in order, the
    /// steady counter at the frame: `(frame, samples)`.
    fn read(
        project: &mut ProjectTime,
        blocks: &[Env],
        offsets: &[usize],
    ) -> Vec<(i64, Option<i64>)> {
        let meter = MeterMap::default();
        let mut got = Vec::new();
        for e in blocks {
            let len = e.block_len.get();
            for &i in offsets.iter().filter(|&&i| i < len) {
                let frame = e.frame.get() as i64 + i as i64;
                let info = info_at(&cx(e), at(i, len), frame, project, None, &meter);
                got.push((frame, info.position.samples));
            }
        }
        got
    }

    /// Project time read at each block's first frame.
    fn read_firsts(blocks: &[Env]) -> Vec<Option<i64>> {
        read(&mut ProjectTime::default(), blocks, &[0])
            .into_iter()
            .map(|(_, s)| s)
            .collect()
    }

    /// The beat's time at `bpm` and 48 kHz, as a re-seat reads it.
    fn reseat(beat: f64, bpm: f64) -> Option<i64> {
        Some((beat * 60.0 / bpm * 48_000.0).round() as i64)
    }

    /// Beat 100 at 120 BPM changing to 121 at frame 32 (project time
    /// 2 400 000 + n at frame n from here on, where the beat's time at 121
    /// is 2 380 197 + n), and the host after that first block.
    fn after_a_tempo_change() -> (Env, Host) {
        let mut h = Host::new(true, 120.0, 100.0);
        let first = h.block(64, &[(32, Cmd::Tempo(121.0))]);
        (first, h)
    }

    /// **Project time rolls through a tempo change**: the reviewer's case,
    /// beat 100 at 120 BPM changing to 121 BPM at frame 32 with no seek, and
    /// the next block on. Read at every frame in order, as a node does, it
    /// rises by exactly the frames rendered, and CLAP's seconds are the
    /// same time. Re-derived from the beat at the new tempo, frame 32 would
    /// jump back to 2 380 197.
    ///
    /// Mutation: always re-seat (`continued` ignored in `ProjectTime::at`)
    /// → the reads leave the frames rendered (frame 32 reads 2 380 197) →
    /// fails. Mutation (run): a new segment at another tempo re-seats (the
    /// tempo change's branch of `played_between` returns `None`) → frame 32
    /// reads 2 380 197 → fails. Mutation: seconds from `beats · 60 / tempo`
    /// again → frame 32's seconds are 49.587 → fails.
    #[test]
    fn project_time_rolls_through_a_tempo_change() {
        let meter = MeterMap::default();
        let (first, mut h) = after_a_tempo_change();
        let second = h.block(64, &[]);
        let mut project = ProjectTime::default();
        let mut steady = SteadyTime::default();
        for e in [first, second] {
            for i in 0..64 {
                let got = info_at(&cx(&e), at(i, 64), steady.at(i), &mut project, None, &meter);
                let want = 2_400_000 + steady.at(i);
                assert_eq!(got.position.samples, Some(want), "frame {}", steady.at(i));
                assert_eq!(got.position.seconds.get(), want as f64 / 48_000.0);
            }
            steady.advance(64);
        }
    }

    /// **A seek, a loop wrap and a start elsewhere re-seat project time** at
    /// the beat's time at the tempo; between them it rolls, and a start where
    /// the transport stood continues it. At 120 BPM and 48 kHz a beat is
    /// 24 000 frames; at 90 BPM, 32 000. Without a usable tempo there is
    /// none.
    ///
    /// Mutation (run): continue through a new segment at the same tempo
    /// (`played_between`'s last branch returning the new segment's frames)
    /// → the seek to beat 3 while stopped holds 512 023 → fails. Mutation
    /// (run): the same segment counts the whole gap → the wrap reads 96 032
    /// → fails. Mutation: drop `.with_position_samples` in
    /// `transport_info` → `None` → fails. Mutation (run): re-seat a start
    /// where it stood (the tempo change's branch returning `None`) → the
    /// start at beat 3 reads 144 000 → fails.
    #[test]
    fn a_seek_a_loop_wrap_and_a_start_elsewhere_re_seat_project_time() {
        let mut h = Host::new(true, 120.0, 4.0);
        h.apply(Cmd::Loop(Some((4.0, 4.0 + 32.0 / 24_000.0))));
        // The loop wraps at frame 32; at frame 40 the loop is switched off
        // and the transport seeks to beat 16 at 90 BPM.
        let first = h.block(
            64,
            &[
                (40, Cmd::Loop(None)),
                (40, Cmd::Tempo(90.0)),
                (40, Cmd::Seek(16.0)),
            ],
        );
        let mut project = ProjectTime::default();
        let all: Vec<usize> = (0..64).collect();
        let samples: Vec<_> = read(&mut project, &[first], &all)
            .into_iter()
            .map(|(_, s)| s)
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

        // Stopped a block later at beat 3 (a seek, so it re-seats, and
        // holds).
        let stopped = h.block(64, &[(0, Cmd::Stop), (0, Cmd::Seek(3.0))]);
        assert_eq!(read(&mut project, &[stopped], &[0]), [(64, Some(96_000))]);
        // Started where it stood (beat 3, now at 60 BPM): it continues the
        // held 96 000, not the beat's 144 000 at the new tempo.
        let (mut there, mut held) = (h.clone(), project);
        let resumed = there.block(64, &[(0, Cmd::Tempo(60.0)), (0, Cmd::Play)]);
        assert_eq!(read(&mut held, &[resumed], &[0]), [(128, Some(96_000))]);
        // Started at beat 5 at 60 BPM instead (somewhere other than where it
        // stood, so it re-seats): 5 * 48 000.
        let started = h.block(
            64,
            &[(0, Cmd::Seek(5.0)), (0, Cmd::Tempo(60.0)), (0, Cmd::Play)],
        );
        assert_eq!(read(&mut project, &[started], &[0]), [(128, Some(240_000))]);

        // A tempo cannot be zero (`Bpm` refuses it); a rate of zero still
        // leaves no project time.
        let playing = Transport::new(true, Bpm::of(120.0), Beat(2.0), None);
        assert_eq!(project.at(&playing, SampleRate(0.0), 192), None);
    }

    /// **Stopping holds project time, and a start where it stood continues
    /// it**; a seek while stopped re-seats. After 120 → 121 BPM project time
    /// is 2 400 000 + n at frame n, where a re-seat would read 2 380 197 + n.
    /// Reads come every 16 frames, a chunk's, so the stop (frame 72) and the
    /// start (frame 264) land between two reads. Before the start the tempo
    /// changes to 90 BPM while stopped (frame 260), so a re-seat there would
    /// read 50 · 32 000 = 1 600 000.
    ///
    /// Mutation (run): the same segment re-seats (its branch of
    /// `played_between` returns `None`) → frame 80 reads 2 380 237 → fails.
    /// Mutation (run): the same segment counts the whole gap instead of the
    /// frame counts' difference → frame 80 reads 2 400 080 → fails.
    /// Mutation (run): the tempo change's branch leaves out the new
    /// segment's frames → frame 272 reads the seat, not seat + 8 → fails.
    #[test]
    fn a_stop_holds_project_time_and_a_seek_while_stopped_re_seats_it() {
        let (first, mut h) = after_a_tempo_change();
        let blocks = [
            first,
            h.block(64, &[(8, Cmd::Stop)]),
            h.block(64, &[]),
            // A seek while stopped.
            h.block(64, &[(0, Cmd::Seek(50.0))]),
            // The tempo set to 90 while stopped 4 frames in, and started
            // where it stood 8 frames in.
            h.block(64, &[(4, Cmd::Tempo(90.0)), (8, Cmd::Play)]),
        ];
        let got = read(&mut ProjectTime::default(), &blocks, &[0, 16, 32, 48]);
        let seat = 50.0 * 60.0 / 121.0 * 48_000.0; // 1 190 082.64
        let want: Vec<(i64, Option<i64>)> = [
            // Playing through the tempo change: rolls.
            (0, 2_400_000.0),
            (16, 2_400_016.0),
            (32, 2_400_032.0),
            (48, 2_400_048.0),
            (64, 2_400_064.0),
            // Stopped at frame 72: 72 frames played, then held.
            (80, 2_400_072.0),
            (96, 2_400_072.0),
            (112, 2_400_072.0),
            (128, 2_400_072.0),
            (144, 2_400_072.0),
            (160, 2_400_072.0),
            (176, 2_400_072.0),
            // The seek to beat 50 while stopped re-seats there, and holds.
            (192, seat),
            (208, seat),
            (224, seat),
            (240, seat),
            (256, seat),
            // Started at frame 264 where it stood: 8 frames played by 272.
            (272, seat + 8.0),
            (288, seat + 24.0),
            (304, seat + 40.0),
        ]
        .into_iter()
        .map(|(f, s): (i64, f64)| (f, Some(s.round() as i64)))
        .collect();
        assert_eq!(got, want);
    }

    /// **A stop and a start inside one chunk keep project time**, either
    /// way round. After 120 → 121 BPM project time is 2 400 000 + n at frame
    /// n (the beat's time at 121 is 2 380 197 + n). Read once a block: a
    /// stop at frame 72 and a restart there at frame 104 (both reads
    /// playing) count the 8 + 24 frames played, 2 400 096 at frame 128; a
    /// start at frame 200 and a stop at frame 232 (both reads stopped)
    /// count the 32 played between them.
    ///
    /// Mutation (run): the same segment re-seats → frame 128 re-seats to
    /// 2 380 261, and every later read re-seats → fails.
    #[test]
    fn a_stop_and_a_start_inside_one_chunk_keep_project_time() {
        let (first, mut h) = after_a_tempo_change();
        let blocks = [
            first,
            h.block(64, &[(8, Cmd::Stop), (40, Cmd::Play)]),
            h.block(64, &[(1, Cmd::Stop)]),
            h.block(64, &[(8, Cmd::Play), (40, Cmd::Stop)]),
            h.block(64, &[]),
        ];
        assert_eq!(
            read_firsts(&blocks),
            [2_400_000, 2_400_064, 2_400_096, 2_400_097, 2_400_129].map(Some)
        );
    }

    /// **A tempo change between two reads is counted exactly**, with a stop
    /// and a start around it: after 120 → 121 BPM, a block stops at frame
    /// 72 (8 frames at 121), halves the tempo to 60.5 while stopped and
    /// starts again at frame 104, so the read at frame 128 is 2 400 064 +
    /// 8 + 24. A block that plays 16 frames at 60.5, doubles the tempo to
    /// 121 and plays 48 more counts 64.
    ///
    /// Mutation (run): count the frames before the new segment at its tempo
    /// instead of the earlier read's → frame 128 reads 2 400 104, not
    /// 2 400 096 → fails.
    #[test]
    fn a_tempo_change_between_two_reads_is_counted_exactly() {
        let (first, mut h) = after_a_tempo_change();
        let blocks = [
            first,
            h.block(
                64,
                &[(8, Cmd::Stop), (16, Cmd::Tempo(60.5)), (40, Cmd::Play)],
            ),
            h.block(64, &[(16, Cmd::Tempo(121.0))]),
            h.block(64, &[]),
        ];
        assert_eq!(
            read_firsts(&blocks),
            [2_400_000, 2_400_064, 2_400_096, 2_400_160].map(Some)
        );
    }

    /// **A tempo ramp that speeds up inside one chunk keeps project time**:
    /// beat 100 at 120 BPM, 130 BPM at frame 16 and 140 BPM at frame 32,
    /// read once a block. The 16 frames at 130 counted at 120 are 17, one
    /// more than the gap leaves, so the count is capped there: 64 frames
    /// played, where a re-seat would jump about 7 s back (2 057 203 at the
    /// second read).
    ///
    /// Mutation (run): drop the cap (the count before the new segment at
    /// `from`'s tempo only) → the second read is 65 frames past a gap of 64
    /// and re-seats to 2 057 203 → fails.
    #[test]
    fn a_tempo_ramp_that_speeds_up_inside_one_chunk_keeps_project_time() {
        let mut h = Host::new(true, 120.0, 100.0);
        let blocks = [
            h.block(64, &[(16, Cmd::Tempo(130.0)), (32, Cmd::Tempo(140.0))]),
            h.block(64, &[]),
            h.block(64, &[]),
        ];
        assert_eq!(
            read_firsts(&blocks),
            [2_400_000, 2_400_064, 2_400_128].map(Some)
        );
    }

    /// **A seek forward by beats and a tempo change inside one chunk
    /// re-seat**: after 120 → 121 BPM, a block seeks 4 beats on and changes
    /// to 130 BPM at frame 8. The beats moved take far more frames than
    /// the gap at either tempo, so the next read is the beat's time at
    /// 130 BPM.
    ///
    /// Mutation (run): drop the cap on the count (`filter(|&n| n <= gap)`
    /// in `played_between`) → the seek is counted, 2 488 743 → fails (and
    /// so does the seek at 90 BPM in
    /// `a_seek_a_loop_wrap_and_a_start_elsewhere_re_seat_project_time`).
    ///
    /// Mutation (run): cap the count before the new segment at the room the
    /// gap leaves whatever the fewest frames those beats take
    /// (`frames_at(from.tempo).min(room)`) → the seek is counted as the
    /// 64-frame gap, 2 400 128 → fails.
    #[test]
    fn a_seek_forward_with_a_tempo_change_inside_one_chunk_re_seats() {
        let (first, mut h) = after_a_tempo_change();
        let target = h.ahead(8.0) + 4.0;
        let seek = h.block(64, &[(8, Cmd::Seek(target)), (8, Cmd::Tempo(130.0))]);
        let end = h.beat();
        assert_eq!(
            read_firsts(&[first, seek, h.block(64, &[])]),
            [Some(2_400_000), Some(2_400_064), reseat(end, 130.0)]
        );
    }

    /// **A NaN beat at a tempo change re-seats**: 120 BPM at beat 100, then
    /// 130 BPM on a segment whose beat is NaN, 64 frames on. No count of
    /// frames played comes from a NaN beat, and its own time is NaN, so
    /// the read is `None`.
    ///
    /// Mutation (run): drop the `!beats.is_finite()` return in
    /// `played_between` → `min` and `max` drop the NaN and the count is the
    /// room the gap leaves, `Some(2_400_064.0)` → fails.
    #[test]
    fn a_nan_beat_at_a_tempo_change_re_seats() {
        let counted = |bpm: f64, beat: f64, frame: u64| {
            let origin = SegmentOrigin {
                beat: Beat(beat),
                frame: Frame(frame),
                sample_rate: SR,
            };
            Transport::counted(true, Bpm::of(bpm), origin, None)
        };
        let mut p = ProjectTime::default();
        assert_eq!(p.at(&counted(120.0, 100.0, 0), SR, 0), Some(2_400_000.0));
        assert_eq!(p.at(&counted(130.0, f64::NAN, 16), SR, 64), None);
    }

    /// **A seek inside one chunk re-seats, however short.** After 120 →
    /// 121 BPM, read every 1 024 frames: a seek 500 frames back while
    /// playing, one 800 frames forward while stopped, both shorter than the
    /// chunk, and one back to the beat the segment began on with a change
    /// to 90 BPM, re-seat at the beat's time at 121 BPM, where counting
    /// them as frames played would carry the 19 803 frames the tempo change
    /// put between project time and the beat's time (at 90 BPM, the
    /// beat's time there).
    ///
    /// Mutation (run): ignore the segments (every read through
    /// `continued_without_origin`) → the backward seek reads 2 400 588 → fails.
    /// Mutation (run): a new segment at the same tempo counted like a tempo
    /// change (its frames from the earlier read's beat) → the backward seek
    /// reads 2 400 588 too → fails. Mutation (run): drop the same tempo
    /// from the same segment's test → the seek back to the segment's first
    /// beat at 90 BPM reads 2 401 048, not 3 201 059 → fails.
    #[test]
    fn a_seek_inside_one_chunk_re_seats() {
        let (first, h) = after_a_tempo_change();
        let mut back = h.clone();
        let target = back.ahead(512.0 - 500.0);
        let seek = back.block(1024, &[(512, Cmd::Seek(target))]);
        let end = back.beat();
        assert_eq!(
            read_firsts(&[first, seek, back.block(64, &[])]),
            [Some(2_400_000), Some(2_400_064), reseat(end, 121.0)]
        );

        let mut fore = h.clone();
        let target = fore.ahead(8.0 + 800.0);
        let seek = fore.block(1024, &[(8, Cmd::Stop), (100, Cmd::Seek(target))]);
        assert_eq!(
            read_firsts(&[first, seek, fore.block(64, &[])]),
            [Some(2_400_000), Some(2_400_064), reseat(target, 121.0)]
        );

        // Back to the beat the 121 BPM segment began on, at 90 BPM: the
        // same origin beat, at another tempo.
        let mut again = h;
        let origin = again.transport().origin().expect("counted").beat.get();
        let seek = again.block(1024, &[(8, Cmd::Seek(origin)), (8, Cmd::Tempo(90.0))]);
        let end = again.beat();
        assert_eq!(
            read_firsts(&[first, seek, again.block(64, &[])]),
            [Some(2_400_000), Some(2_400_064), reseat(end, 90.0)]
        );
    }

    /// **A loop edit keeps project time**, playing or stopped: after 120 →
    /// 121 BPM, a loop ending at frame 80 is switched off at frame 72, so
    /// the playhead plays on through the old end (2 400 128 at frame 128);
    /// so does one extended there, and one switched off with a stop at frame
    /// 104 (2 400 104).
    ///
    /// Mutation (run): judge a loop edit a jump (the same segment's branch
    /// also requiring an equal loop) → frame 128 re-seats to 2 380 293 →
    /// fails.
    #[test]
    fn a_loop_edit_keeps_project_time() {
        let (first, mut h) = after_a_tempo_change();
        let end = h.ahead(16.0);
        h.apply(Cmd::Loop(Some((96.0, end))));
        let cases: [(&[(usize, Cmd)], i64); 3] = [
            (&[(8, Cmd::Loop(None))], 2_400_128),
            (&[(8, Cmd::Loop(Some((96.0, 200.0))))], 2_400_128),
            (&[(8, Cmd::Loop(None)), (40, Cmd::Stop)], 2_400_104),
        ];
        for (cmds, want) in cases {
            let mut h = h.clone();
            let blocks = [first, h.block(64, cmds), h.block(64, &[])];
            assert_eq!(read_firsts(&blocks), [2_400_000, 2_400_064, want].map(Some));
        }
    }

    /// **A rate change re-seats project time** at the beat's time at the new
    /// rate: playing on after 120 → 121 BPM, the rate goes from 48 to 96 kHz
    /// and the next read is the beat's time at 121 BPM and 96 kHz,
    /// 4 760 586.
    ///
    /// Two rules re-seat it: the rate check in `ProjectTime::at`, and the
    /// rate change's new segment at the same tempo. Either mutation alone
    /// survives (run). Mutation (run): drop the rate check and continue
    /// through a new segment at the same tempo (its new frames, none here)
    /// → 2 400 064 → fails.
    #[test]
    fn a_rate_change_re_seats_project_time() {
        let (first, mut h) = after_a_tempo_change();
        let second = h.block(64, &[]);
        let beat = h.beat();
        let blocks = [first, second, h.block(64, &[(0, Cmd::Rate(96_000.0))])];
        let reseat = (beat * 60.0 / 121.0 * 96_000.0).round() as i64;
        assert_eq!(
            read_firsts(&blocks),
            [2_400_000, 2_400_064, reseat].map(Some)
        );
    }

    /// **A transport built by hand (no frame count) is read by the
    /// playhead's rule.** Beat 10 at 120 BPM, then 64 frames on at 121
    /// BPM: 240 064, where the beat's time at 121 is about 238 080. The
    /// next read, 64 frames on, is a transport built by hand:
    ///
    /// - playing on at 121 BPM, it rolls (240 128);
    /// - past the loop's end (unchanged), by a stop 20 frames on or at the
    ///   whole 64, re-seats: playback would have wrapped;
    /// - past it with the loop switched off, it continues, whether the
    ///   playhead's rule reads it as rolled (64) or a stop and a start (20);
    /// - with the loop behind the playhead it continues (20);
    /// - stopped 66 frames on, past the gap and its frame of slack, it
    ///   re-seats;
    /// - stopped both times, moved 40 frames at 242 BPM, it counts 40 at the
    ///   later read's tempo (80 at the earlier one's is past the gap);
    /// - at another rate it re-seats, even unmoved.
    ///
    /// Every read goes through `Cx::transport_at`, which walks the block's
    /// transport to the frame: the walk of a bare beat has no origin, so
    /// these reads take the playhead's rule, not `played_between`.
    ///
    /// Mutation (run): the walk of a bare beat counted as the host's
    /// (`Transport::walked_to` keeping `Position::Counted` for every
    /// position) → each block looks like a new segment at the same tempo,
    /// and the read playing on re-seats to 238 144 → fails.
    /// Mutation (run): drop the loop-end check → the stop 20 frames past the
    /// unchanged loop's end continues to 240 084 → fails. Mutation (run):
    /// apply it with the loop changed too (drop `from.looping ==
    /// to.looping`) → the loop switched off re-seats to 238 100 → fails.
    /// Mutation (run): drop the check's `start < end` clause → the loop
    /// behind re-seats to 238 100 → fails. Mutation (run): cap at twice the
    /// gap → 66 continues to 240 130 → fails.
    /// Mutation (run): `from`'s tempo only when both are stopped → the 40
    /// at 242 re-seats to 119 080 → fails. Mutation (run): drop
    /// `p.rate != rate` → the other rate holds 240 064, not 476 160 →
    /// fails.
    #[test]
    fn a_transport_built_by_hand_is_read_by_the_playheads_rule() {
        let per = |n: f64, bpm: f64| n * bpm / 60.0 / 48_000.0;
        let b1 = 10.0 + per(64.0, 120.0);
        let lp = |start: f64, end: f64| {
            Some(LoopRange {
                start: Beat(start),
                end: Beat(end),
            })
        };
        let crossing = lp(9.0, b1 + per(16.0, 121.0));
        let behind = lp(5.0, 9.0);
        let read_after = |looping, playing, to: Transport, rate| {
            let first = env(
                0,
                64,
                Transport::new(true, Bpm::of(120.0), Beat(10.0), None),
                TransportChanges::NONE,
            );
            let on = Transport::new(playing, Bpm::of(121.0), Beat(b1), looping);
            let second = env(64, 64, on, TransportChanges::NONE);
            let mut third = env(128, 64, to, TransportChanges::NONE);
            third.sample_rate = rate;
            read_firsts(&[first, second, third])
        };
        let stopped = |moved: f64, looping| {
            Transport::new(false, Bpm::of(121.0), Beat(b1 + per(moved, 121.0)), looping)
        };
        let past = |n: f64| reseat(b1 + per(n, 121.0), 121.0);
        let faster = Transport::new(false, Bpm::of(242.0), Beat(b1 + per(40.0, 242.0)), None);
        let rolling = Transport::new(true, Bpm::of(121.0), Beat(b1 + per(64.0, 121.0)), None);
        let cases = [
            (None, true, rolling, SR, Some(240_128)),
            (crossing, true, stopped(20.0, crossing), SR, past(20.0)),
            (crossing, true, stopped(64.0, crossing), SR, past(64.0)),
            (crossing, true, stopped(20.0, None), SR, Some(240_084)),
            (crossing, true, stopped(64.0, None), SR, Some(240_128)),
            (behind, true, stopped(20.0, behind), SR, Some(240_084)),
            (None, true, stopped(66.0, None), SR, past(66.0)),
            (None, false, faster, SR, Some(240_104)),
            (
                None,
                true,
                stopped(0.0, None),
                SampleRate(96_000.0),
                Some((b1 * 60.0 / 121.0 * 96_000.0).round() as i64),
            ),
        ];
        for (i, (looping, playing, to, rate, want)) in cases.into_iter().enumerate() {
            let got = read_after(looping, playing, to, rate);
            assert_eq!(got[1], Some(240_064), "case {i}");
            assert_eq!(got[2], want, "case {i}");
        }
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
            Transport::new(true, Bpm::of(120.0), Beat(0.0), None),
            TransportChanges::NONE,
        );
        let block_ns = Some(5_000_000_000);
        let first = info_at(
            &cx(&e),
            at(0, 64),
            0,
            &mut ProjectTime::default(),
            block_ns,
            &meter,
        );
        assert_eq!(first.system_time_ns, Some(5_000_000_000));
        let later = info_at(
            &cx(&e),
            at(48, 64),
            0,
            &mut ProjectTime::default(),
            block_ns,
            &meter,
        );
        assert_eq!(later.system_time_ns, Some(5_001_000_000));
        let offline = info_at(
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
