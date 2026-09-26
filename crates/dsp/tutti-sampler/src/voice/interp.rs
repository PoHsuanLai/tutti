//! Shared, zero-alloc interpolation kernel for the sampler playback units.
//!
//! Both the in-memory [`MemorySource`](super::memory_source::MemorySource) and the
//! disk-streaming [`DiskVoice`](super::disk_voice::DiskVoice)
//! read fractional sample positions, so both must interpolate the same way or
//! the same source sounds different on the two tiers. Shared here: one
//! `cubic_hermite` kernel and one transport-placement gate, used by both.
//!
//! `read_frame` is the in-memory reader only — the streaming tier pulls from the
//! butler ring rather than an indexable `Wave`, so it feeds the same kernel from
//! its own 4-tap history. Same interpolation, different fetch.
//!
//! `read_frame` also owns the crate's **channel policy** (see its docs); the
//! butler's planar unpack in `wave_io` follows the same rule. Two
//! implementations of a channel policy is how the two tiers drift apart — the
//! same failure mode this module exists to prevent for interpolation.
//!
//! Everything here is pure per-sample arithmetic — no allocation, no locks —
//! so it is safe to call from `process`/`tick` hot paths.

use std::ops::Range;
use std::sync::Arc;
use tutti_core::{
    fold_frame, snap_to_whole_frame, Beat, BeatDuration, Bpm, Frame, ReadRate, SamplePosition,
    SampleRate, Samples, TimelineSegment,
};
use tutti_io::Wave;

use super::clock::{BlockClock, Run};
use super::loop_span::{blend, LoopSpan};
use super::memory_source::VoiceWindow;
use crate::MAX_SAMPLER_CHANNELS;

/// Where the playhead sits in source samples, or `None` when it is outside the
/// voice's window.
///
/// The single source of truth for the transport-placement gate, shared by the
/// in-memory [`MemorySource`](super::memory_source::MemorySource) and the
/// disk-streaming [`DiskVoice`](super::disk_voice::DiskVoice), read at the
/// transport a block's `Env` gives (`tutti_graph::Env::transport_at`).
///
/// # A placed voice has no position of its own
///
/// Position is **derived** from the playhead, never accumulated here — the same
/// model `tutti_core`'s transport uses, where `TransportClock` is the one node
/// that advances time (it counts frames and derives the beat from them) and everything
/// downstream reads the result. A voice that also carried a read cursor would be
/// a second, competing clock, and the two would drift apart the moment the
/// transport looped, seeked, or changed tempo.
///
/// `rate` scales the derived offset rather than stepping a cursor: at 0.5 the
/// voice is half as far into its material for a given playhead position, which is
/// what "half speed" means for something the timeline owns. That is why varispeed
/// belongs *here*, in the beat→sample mapping, and not as a per-unit `+= speed`
/// accumulator.
///
/// # Why both rate arguments are typed
///
/// The two tiers split the sample-rate-conversion factor between these two
/// arguments *differently*: memory passes `(wave.sample_rate(), speed)` while
/// disk passes `(session_rate × src_ratio, speed)`. Those are algebraically
/// equal — both are `seconds × file_rate × speed` — so both are correct, but a
/// caller cannot read which split it is using off the values alone. A third
/// caller combining them the obvious way applies `src_ratio` twice, and the cost
/// is concrete: on a 48 kHz file in a 44.1 kHz session the gate outruns the ring
/// by ~4.2k source samples per second, tripping a seek about once a second,
/// forever.
///
/// [`ReadRate`] is the product's own type, so `rate` can only be built through
/// [`PlaybackRate::read_rate`](tutti_core::PlaybackRate::read_rate) — the one
/// place the two factors compose. A caller holding a bare varispeed cannot pass
/// it here by accident.
///
/// Returns `None` when the transport is stopped, the playhead is before
/// `start_beat`, past `duration`, or the tempo is non-positive.
///
/// # Reached by frame, not by comparing beats
///
/// "Has the playhead reached `start_beat`?" is the engine's one beat→frame
/// rule ([`TimelineSegment::reached_by`], doc 013 §6): the voice enters on
/// the first frame at or after its start, within a millionth of a frame. A
/// bare `beat < start` puts a clip whose start is exactly on the playhead's
/// frame, but whose `f64` came out an ulp later than the clock's, a frame
/// late. The window's end is the same rule, so a voice leaves on the frame its
/// successor enters. The frames are the source's own at unit speed
/// (`source_rate`): the gate sees no session rate, and a millionth of either
/// kind of frame is far below anything audible.
///
/// Pure arithmetic: no allocation, no locks — safe on the audio thread.
#[inline]
pub fn window_position(
    transport: &tutti_graph::Transport,
    start_beat: Beat,
    duration: Option<BeatDuration>,
    source_rate: SampleRate,
    rate: ReadRate,
) -> Option<SamplePosition> {
    if !transport.playing {
        return None;
    }
    let tempo = transport.tempo;
    if tempo.get() <= 0.0 {
        return None;
    }
    let now = transport.beat();
    let gate = Gate {
        window: VoiceWindow {
            start: start_beat,
            duration,
        },
        source_rate,
        rate,
    };
    if !gate.reached(now, tempo, start_beat) {
        return None;
    }
    if let Some(dur) = duration {
        if gate.reached(now, tempo, start_beat + dur) {
            return None;
        }
    }
    Some(gate.origin(now, tempo))
}

/// A placed read's gate: its window, the rate the window is measured in
/// (the source's frames at unit speed), and the rate elapsed time maps onto
/// the source at (varispeed and a stretcher's rate; see [`window_position`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Gate {
    pub(crate) window: VoiceWindow,
    pub(crate) source_rate: SampleRate,
    pub(crate) rate: ReadRate,
}

impl Gate {
    /// Whether a playhead at `now` has reached `target`, at `tempo`: the
    /// engine's one rule ([`TimelineSegment::reached_by`]), in the source's
    /// frames.
    #[inline]
    fn reached(&self, now: Beat, tempo: Bpm, target: Beat) -> bool {
        TimelineSegment::new(Frame::ZERO, now, tempo, self.source_rate)
            .reached_by(Frame::ZERO, target)
    }

    /// The source position of a playhead at `now` (inside the window): its
    /// elapsed time since the window's start, in the source's frames at the
    /// gate's rate.
    #[inline]
    fn origin(&self, now: Beat, tempo: Bpm) -> SamplePosition {
        // Reached within the tolerance may be a hair before the start: that
        // is the start itself.
        let beat_offset = (now - self.window.start).get().max(0.0);
        let seconds_offset = beat_offset * 60.0 / tempo.get();
        let position = seconds_offset * self.source_rate.get() * self.rate.get();
        // The clock's beat is its frame count in closed form; back through
        // seconds it lands a hair off the whole frame it is (frame 128 of a
        // clip at 120 BPM, 48 kHz came out 127.99999999999). Landed on the
        // frame by the engine's one tolerance, a clip on a beat plays its own
        // samples exactly.
        SamplePosition(snap_to_whole_frame(position))
    }

    /// The first block frame in `from..to` of `run` whose beat has reached
    /// `target`, or `to` when none has. The beat never falls inside a run,
    /// so the answer is found from the closed-form estimate by stepping.
    fn first_reaching(&self, run: &Run, from: usize, to: usize, target: Beat) -> usize {
        let tempo = run.tempo();
        let at = |j: usize| self.reached(run.beat_at(j), tempo, target);
        if from >= to || at(from) {
            return from;
        }
        if to - from == 1 || !at(to - 1) {
            return to;
        }
        let fpb = run.frames_per_beat().unwrap_or(1.0);
        let est = ((target - run.beat_at(from)).get() * fpb).ceil();
        let mut j = if est.is_finite() && est >= 1.0 {
            (from + est as usize).min(to - 1)
        } else {
            from + 1
        };
        while j > from + 1 && at(j - 1) {
            j -= 1;
        }
        while !at(j) {
            j += 1;
        }
        j
    }
}

/// The positions a placed read plays over block frames `range`, one per
/// frame into `out` (`out.len() == range.len()`): `None` outside the
/// window, or while the transport stands.
///
/// **Frame-exact at the window's edges.** Each run of the block
/// ([`BlockClock::runs`]) is gated frame by frame by the one beat→frame
/// rule: the read enters on the first frame whose beat reaches the window's
/// start and leaves on the first that reaches its end, wherever in the block
/// they fall — a clip placed mid-block starts on its frame, not at the next
/// block.
///
/// **Seated, then stepped.** At the first frame of the range a run sounds
/// on (or its entry frame), the read seats where the gate puts the
/// playhead ([`window_position`]'s origin) and steps from there by `step`
/// per frame: `origin + step × k`. The owner renders a block in pieces of at
/// most `LANE_FRAMES` (64), so the read re-seats on the clock at each piece,
/// as it did when the graph called it once per 64-frame chunk — the same
/// positions, bit for bit, wherever the window is open throughout. The step
/// is the read's own rate (with the conversion to the render rate), the
/// origin the gate's (in the source's frames); they agree to rounding.
pub(crate) fn place(
    clock: &BlockClock<'_>,
    range: Range<usize>,
    gate: Gate,
    step: ReadRate,
    out: &mut [Option<SamplePosition>],
) {
    debug_assert_eq!(out.len(), range.len(), "one position per frame");
    let base = range.start;
    for run in clock.runs_in(range) {
        let (a, b) = (run.start, run.end);
        let slots = &mut out[a - base..b - base];
        if !run.rolling() || run.frames_per_beat().is_none() {
            slots.fill(None);
            continue;
        }
        let window = gate.window;
        let e = gate.first_reaching(&run, a, b, window.start);
        let x = match window.duration {
            Some(d) => gate.first_reaching(&run, e, b, window.start + d),
            None => b,
        };
        slots[..e - a].fill(None);
        slots[x - a..].fill(None);
        if e < x {
            let origin = gate.origin(run.beat_at(e), run.tempo());
            for (k, p) in slots[e - a..x - a].iter_mut().enumerate() {
                *p = Some(origin + step.advance(Samples(k)));
            }
        }
    }
}

/// Whether the playhead has left `window` for good by the last frame of
/// `range` (rolling, past its end): a reader holding a file for it may let
/// it go.
pub(crate) fn past_window(clock: &BlockClock<'_>, range: Range<usize>, gate: Gate) -> bool {
    let Some(duration) = gate.window.duration else {
        return false;
    };
    let last = range.end.saturating_sub(1);
    clock.runs_in(range).last().is_some_and(|run| {
        run.rolling() && gate.reached(run.beat_at(last), run.tempo(), gate.window.start + duration)
    })
}

/// Catmull-Rom cubic Hermite interpolation across four consecutive taps.
///
/// `y1` is the sample at the integer position, `y0`/`y2`/`y3` its neighbours
/// (`y0` one behind, `y2`/`y3` ahead); `t` is the fractional offset in
/// `[0, 1)` between `y1` and `y2`.
#[inline]
pub fn cubic_hermite(y0: f32, y1: f32, y2: f32, y3: f32, t: f32) -> f32 {
    let c0 = y1;
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    ((c3 * t + c2) * t + c1) * t + c0
}

/// [`cubic_hermite`] along time: `out[i]` is the cubic through `y[0][i]` ..
/// `y[3][i]` at `t[i]`, for every `i` of `out`.
///
/// The same expression per element as [`cubic_hermite`] (inlined into the
/// loop, so the compiler vectorises it across frames; Rust never contracts a
/// multiply and an add into an FMA on its own), so each output is bit for bit
/// the scalar kernel's. The slices are cut to `out`'s length first, which
/// keeps the bounds checks out of the loop.
#[inline]
pub(crate) fn hermite_lanes(out: &mut [f32], y: [&[f32]; 4], t: &[f32]) {
    let n = out.len();
    let (y0, y1, y2, y3, t) = (&y[0][..n], &y[1][..n], &y[2][..n], &y[3][..n], &t[..n]);
    for i in 0..n {
        out[i] = cubic_hermite(y0[i], y1[i], y2[i], y3[i], t[i]);
    }
}

/// Read one `out.len()`-wide frame from `wave` at fractional position
/// `position` using 4-tap cubic Hermite interpolation per channel.
///
/// The four taps are `idx-1, idx, idx+1, idx+2` (where `idx = floor(position)`),
/// each clamped to the wave bounds so edges reuse the nearest valid sample.
/// Pure arithmetic: no allocation, so this is safe on the RT thread.
///
/// **Writes every element of `out`**, including the silent cases — a caller
/// never has to pre-zero, and a partial write can never leave a stale channel
/// from the previous block in a trailing slot.
///
/// # Channel policy
///
/// One rule: **a mono source has no channel identity and fans to every channel;
/// anything else has one and folds through [`fold_frame`].**
///
/// - **mono wave → N-wide frame**: fans to all N. A mono sample is a point
///   source whose speaker placement is the panner's job, not the reader's, so
///   dropping it into channel 0 alone would be wrong. This is also what the
///   stereo kernel always did (mono duplicated to both sides), so the behaviour
///   at width 2 is unchanged.
/// - **wider wave → narrower frame**: [`fold_frame`], i.e. the ITU-R BS.775
///   matrix at width 2. Front-pair passthrough would silently drop the centre
///   (dialogue) and the surrounds (ambience). The engine already owns these
///   coefficients in exactly one place, and a sampler that folded differently
///   would make the same file sound different depending on the node's width.
/// - **narrower (but not mono) wave → wider frame**: [`fold_frame`], which
///   copies straight through and zero-fills the extra channels. Deliberately
///   asymmetric with the mono case: stereo carries a real L/R image, and fanning
///   it into the centre and surrounds would smear that image and invent phantom
///   centre content.
#[inline]
pub fn read_frame(wave: &Arc<Wave>, position: f64, out: &mut [f32]) {
    if out.is_empty() {
        return;
    }
    let len = wave.len();
    let src_ch = wave.channels();
    if len == 0 || src_ch == 0 {
        out.fill(0.0);
        return;
    }
    let (taps, frac) = tap_indices(len, position);
    // Every channel index `interpolate_taps` asks for is `< src_ch`, which is
    // what keeps `Wave::at` — an unchecked `self.vec[channel][index]` — from
    // panicking; `tap_indices` keeps the frame index in bounds.
    interpolate_taps(src_ch, frac, out, |c, t| wave.at(c, taps[t]));
}

/// The four frames [`read_frame`] interpolates `position` from, in a source
/// `len` frames long, and the fractional offset between the second and third.
///
/// `idx-1, idx, idx+1, idx+2` (where `idx = floor(position)`), each clamped to
/// the source so its edges reuse the nearest valid frame. Split from
/// `read_frame` so a source that is not a resident [`Wave`] (the offline disk
/// reader, which pages the file in) reads the same four frames and runs them
/// through the same kernel: the two tiers cannot then disagree about which
/// frames a position means.
///
/// `len` must be non-zero.
#[inline]
pub(crate) fn tap_indices(len: usize, position: f64) -> ([usize; 4], f32) {
    let (idx, frac) = split_position(position);
    let last = len - 1;
    // All four taps clamp to `last`, `im1` included: `saturating_sub` guards
    // only the LOW end, so a `position` past `len` leaves `im1` past the end
    // too, and `Wave::at` is an unchecked index — that is a panic, not a bad
    // sample. The in-tree caller gates on `position >= len` first, but
    // `read_frame` is a `pub` function and must not depend on that.
    let im1 = idx.saturating_sub(1).min(last);
    let i0 = idx.min(last);
    let i1 = (idx + 1).min(last);
    let i2 = (idx + 2).min(last);
    ([im1, i0, i1, i2], frac)
}

/// A position's whole frame and the fraction past it, as every tap layout
/// ([`tap_indices`], and a loop's, `LoopSpan::taps`) takes them.
#[inline]
pub(crate) fn split_position(position: f64) -> (usize, f32) {
    let mut idx = position.floor() as usize;
    let mut frac = position.fract() as f32;
    // A fraction a hair under 1 rounds to 1.0 in `f32`: "frame n, all the
    // way to n + 1", which the cubic returns as frame n + 1 only to within
    // an ulp. It *is* frame n + 1 at `t` = 0, where the kernel returns the
    // tap exactly. (A position n - e inside a block, which no snap on the
    // block's origin reaches, landed here.)
    if frac >= 1.0 {
        idx += 1;
        frac = 0.0;
    }
    (idx, frac)
}

/// Read one frame of a looped `wave` at `pos` (placed on the loop, `looped`
/// once it has been round; see [`LoopSpan::taps`]) into `out`, writing every
/// element: the four frames of the looped sequence, each blended toward its
/// lead-in inside the crossfade, through the one kernel and channel policy
/// [`read_frame`] uses.
///
/// Silent for an empty wave, as `read_frame` is.
#[inline]
pub(crate) fn read_looped_frame(
    wave: &Arc<Wave>,
    span: &LoopSpan,
    pos: f64,
    looped: bool,
    out: &mut [f32],
) {
    if out.is_empty() {
        return;
    }
    let len = wave.len();
    let src_ch = wave.channels();
    if len == 0 || src_ch == 0 {
        out.fill(0.0);
        return;
    }
    let (taps, frac) = span.taps(len, pos, looped);
    interpolate_taps(src_ch, frac, out, |c, t| {
        let tap = taps[t];
        let tail = wave.at(c, tap.frame);
        match tap.fade {
            Some((lead, w)) => blend(tail, wave.at(c, lead), w),
            None => tail,
        }
    });
}

/// Interpolate one frame into `out` from four taps of a `src_ch`-wide source
/// (`sample(c, t)` is channel `c` of tap `t`, `t` in `0..4` as
/// [`tap_indices`] orders them), applying the crate's channel policy (see
/// [`read_frame`]).
///
/// **Writes every element of `out`.** `src_ch` must be non-zero.
#[inline]
pub(crate) fn interpolate_taps(
    src_ch: usize,
    frac: f32,
    out: &mut [f32],
    sample: impl Fn(usize, usize) -> f32,
) {
    // One interpolated sample from channel `c`. Every caller below derives `c`
    // from a bound that is `<= src_ch`.
    let tap =
        |c: usize| cubic_hermite(sample(c, 0), sample(c, 1), sample(c, 2), sample(c, 3), frac);

    // Mono fans out. Bound: `c` is unused, only channel 0 is read.
    if src_ch == 1 {
        out.fill(tap(0));
        return;
    }

    // Matched width — the common case, including plain stereo. Bound: `src_ch
    // == out.len()`, so `0..out.len()` is in range. Kept as a distinct path
    // rather than routed through `fold_frame` so the hot case stays a straight
    // per-channel read with no matrix dispatch.
    if src_ch == out.len() {
        for (c, o) in out.iter_mut().enumerate() {
            *o = tap(c);
        }
        return;
    }

    // Mismatched width: interpolate at the source width into a bounded stack
    // frame, then let the engine's one matrix decide the mapping. Bound:
    // `0..n` where `n <= src_ch`.
    let n = src_ch.min(MAX_SAMPLER_CHANNELS);
    debug_assert!(
        src_ch <= MAX_SAMPLER_CHANNELS,
        "wave has {src_ch} channels, past MAX_SAMPLER_CHANNELS ({MAX_SAMPLER_CHANNELS}); \
         channels {MAX_SAMPLER_CHANNELS}.. are dropped by the fold"
    );
    let mut src = [0.0f32; MAX_SAMPLER_CHANNELS];
    for (c, s) in src.iter_mut().enumerate().take(n) {
        *s = tap(c);
    }
    fold_frame(&src[..n], out);
}

/// Stereo shim over [`read_frame`]: returns the interpolated `(left, right)`
/// pair for a fixed 2-channel read. Same channel policy — a mono wave fans to
/// both sides, a wider one folds through the ITU-R BS.775 matrix.
#[inline]
pub fn read_stereo_frame(wave: &Arc<Wave>, position: f64) -> (f32, f32) {
    let mut out = [0.0f32; 2];
    read_frame(wave, position, &mut out);
    (out[0], out[1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::{PlaybackRate, SrcRatio};
    use tutti_graph::{Env, Offset, Transport, TransportChanges};

    /// **A position a hair under a whole frame reads that frame exactly.**
    /// Its fraction rounds to 1.0 in `f32`; the kernel at `t` = 1.0 returns
    /// the next tap only to within an ulp, so the read lands on the next frame
    /// at `t` = 0 instead, where the tap is returned as it is.
    ///
    /// Mutation (run): the `frac >= 1.0` carry removed from `tap_indices` →
    /// taps `[3, 4, 5, 6]` at 1.0 → fails.
    #[test]
    fn a_position_a_hair_under_a_frame_reads_that_frame() {
        let below = 5.0 - 1e-12;
        assert_eq!(tap_indices(100, below), ([4, 5, 6, 7], 0.0));
        let mut wave = Wave::new(1, 48_000.0);
        for i in 0..16 {
            wave.push_frame(&[(i as f32 + 1.0) * 0.1234567]);
        }
        let wave = Arc::new(wave);
        let mut out = [0.0f32; 1];
        read_frame(&wave, below, &mut out);
        assert_eq!(out[0], wave.at(0, 5));
    }

    /// **Gate parity.** Both tiers must derive the SAME source position from the
    /// same transport reading — asserted on the value, not on liveness.
    ///
    /// Both tiers reach the gate with a rate argument that is already the *file*
    /// rate — memory from `wave.sample_rate()`, disk from `session_rate ×
    /// src_ratio` — so the beat→sample conversion is complete and the rate
    /// argument must carry varispeed ALONE. Passing a full `read_rate` here
    /// applies `src_ratio` a second time and lands 8.8% deep on a 48 kHz file in
    /// a 44.1 kHz session.
    ///
    /// A doubled factor is invisible at matched rates, where `src_ratio` is
    /// `UNITY` and the extra multiply is exactly 1.0. Hence the table below: the
    /// mismatched rows are the whole point, and the matched row is there to show
    /// it is not what distinguishes them.
    ///
    /// Driven at the kernel rather than through the two units, because that is
    /// where the splits meet — the units differ in how they *fetch*, which is a
    /// separate question from where they think the playhead is.
    #[test]
    fn both_tier_splits_agree_on_the_same_position() {
        let session = 44_100.0;
        for (file_rate, speed) in [
            (44_100.0f64, 1.0f32),
            (48_000.0, 1.0),
            (48_000.0, 0.5),
            (22_050.0, 2.0),
            (96_000.0, 0.25),
        ] {
            let src = SrcRatio::for_rates(file_rate, session);
            let rate = PlaybackRate::new(speed);
            let transport = Transport::new(true, Bpm(120.0), Beat(4.0), None);

            // Memory's split: the wave's own rate, times varispeed alone.
            let memory = window_position(
                &transport,
                Beat::new(0.0),
                None,
                SampleRate::new(file_rate),
                rate.read_rate(SrcRatio::UNITY),
            )
            .expect("inside the window");

            // Disk's split: a rate that already carries the conversion, times
            // varispeed alone.
            let disk = window_position(
                &transport,
                Beat::new(0.0),
                None,
                SampleRate::new(session * src.get() as f64),
                rate.read_rate(SrcRatio::UNITY),
            )
            .expect("inside the window");

            // Beat 4 at 120 BPM is 2 seconds.
            let expected = 2.0 * file_rate * speed as f64;
            assert!(
                (memory.get() - expected).abs() < 1.0,
                "memory split at {file_rate} Hz x{speed}: want ~{expected}, got {}",
                memory.get()
            );
            assert!(
                (memory.get() - disk.get()).abs() < 1.0,
                "the two tiers disagree at {file_rate} Hz x{speed}: \
                 memory {} vs disk {}",
                memory.get(),
                disk.get()
            );
        }
    }

    /// The gate is a *gate*: outside the window it reports nothing, so a caller
    /// cannot read stale material by ignoring a `None`.
    #[test]
    fn window_position_is_none_outside_the_window() {
        let stopped = Transport::new(false, Bpm(120.0), Beat(4.0), None);
        assert!(
            window_position(
                &stopped,
                Beat::new(0.0),
                None,
                SampleRate::new(44_100.0),
                ReadRate::UNITY
            )
            .is_none(),
            "a stopped transport has no position"
        );

        let rolling = Transport::new(true, Bpm(120.0), Beat(2.0), None);
        assert!(
            window_position(
                &rolling,
                Beat::new(8.0),
                None,
                SampleRate::new(44_100.0),
                ReadRate::UNITY
            )
            .is_none(),
            "before the window start"
        );
        assert!(
            window_position(
                &rolling,
                Beat::new(0.0),
                Some(BeatDuration::new(1.0)),
                SampleRate::new(44_100.0),
                ReadRate::UNITY
            )
            .is_none(),
            "past the window duration"
        );
        // The boundary is half-open: beat 2 with duration 2 is already outside.
        assert!(
            window_position(
                &rolling,
                Beat::new(0.0),
                Some(BeatDuration::new(2.0)),
                SampleRate::new(44_100.0),
                ReadRate::UNITY
            )
            .is_none(),
            "the window end is exclusive"
        );
    }

    /// ch0 = (i+1)*0.01, ch1 = -(i+1)*0.01 — every sample distinct and the two
    /// channels distinguishable by sign, so a channel swap is visible.
    fn stereo_ramp() -> Arc<Wave> {
        let mut w = Wave::zero(2, 44_100.0, 64.0 / 44_100.0);
        for i in 0..w.len() {
            w.set(0, i, (i as f32 + 1.0) * 0.01);
            w.set(1, i, -((i as f32 + 1.0) * 0.01));
        }
        Arc::new(w)
    }

    fn mono_ramp() -> Arc<Wave> {
        let mut w = Wave::zero(1, 44_100.0, 64.0 / 44_100.0);
        for i in 0..w.len() {
            w.set(0, i, (i as f32 + 1.0) * 0.01);
        }
        Arc::new(w)
    }

    /// `channel c` carries the constant `c + 1`, so a wrong-channel read yields a
    /// wrong *value* rather than a plausible one.
    fn indexed_wave(channels: usize) -> Arc<Wave> {
        let mut w = Wave::zero(channels, 44_100.0, 32.0 / 44_100.0);
        for i in 0..w.len() {
            for c in 0..channels {
                w.set(c, i, (c + 1) as f32);
            }
        }
        Arc::new(w)
    }

    /// Golden-vector gate: width-2 output must be **bit-identical** to what the
    /// pre-width-generic stereo kernel produced.
    ///
    /// `assert_eq` on `f32::to_bits`, not an epsilon compare: a reassociated
    /// `cubic_hermite` or a fold that multiplies by 1.0 changes the last mantissa
    /// bit, and that is a real (if inaudible) divergence worth knowing about.
    ///
    /// These constants were captured by running the stereo kernel on `main`
    /// before any part of the width change existed. **Regenerating them is not
    /// the fix if this fails** — it would erase the only evidence that stereo
    /// still behaves as it did, which is the load-bearing claim for every
    /// unchanged app-side call site.
    #[test]
    fn stereo_output_is_bit_identical_to_the_pre_width_kernel() {
        const EXPECTED: [(u32, u32); 16] = [
            (1011413796, 3158897444),
            (1025222116, 3172705764),
            (1031865893, 3179349541),
            (1035221336, 3182704984),
            (1038576779, 3186060427),
            (1041059808, 3188543456),
            (1042737529, 3190221177),
            (1044415251, 3191898899),
            (1046092972, 3193576620),
            (1047770693, 3195254341),
            (1049012207, 3196495855),
            (1049851069, 3197334717),
            (1050689929, 3198173577),
            (1051528791, 3199012439),
            (1052367651, 3199851299),
            (1053206511, 3200690159),
        ];
        let w = stereo_ramp();
        let got: Vec<(u32, u32)> = (0..16)
            .map(|k| {
                let (l, r) = read_stereo_frame(&w, k as f64 * 2.5 + 0.3);
                (l.to_bits(), r.to_bits())
            })
            .collect();
        assert_eq!(
            got.as_slice(),
            &EXPECTED[..],
            "stereo playback diverged from the pre-width-generic kernel"
        );
    }

    /// The mono fan-out is likewise pinned bit-for-bit — it is the one policy
    /// arm that deliberately does *not* defer to `fold_frame`'s zero-fill.
    #[test]
    fn mono_fan_out_is_bit_identical_to_the_pre_width_kernel() {
        const EXPECTED: [(u32, u32); 8] = [
            (1015590651, 1015590651),
            (1028309124, 1028309124),
            (1034416029, 1034416029),
            (1038778106, 1038778106),
            (1041663787, 1041663787),
            (1043844824, 1043844824),
            (1046025863, 1046025863),
            (1048206901, 1048206901),
        ];
        let w = mono_ramp();
        let got: Vec<(u32, u32)> = (0..8)
            .map(|k| {
                let (l, r) = read_stereo_frame(&w, k as f64 * 3.25 + 0.7);
                (l.to_bits(), r.to_bits())
            })
            .collect();
        assert_eq!(got.as_slice(), &EXPECTED[..]);
    }

    /// A 6-channel wave must deliver **all six** channels, each to its own slot —
    /// not channel 0 fanned, not the front pair with four zeros. A width match
    /// arm collapsing to `(at(0), at(1))` truncates exactly that way, and every
    /// stereo fixture in the crate passes over it.
    #[test]
    fn six_channel_wave_reaches_all_six_outputs() {
        let w = indexed_wave(6);
        let mut out = [0.0f32; 6];
        read_frame(&w, 4.0, &mut out);
        for (c, &got) in out.iter().enumerate() {
            assert!(
                (got - (c + 1) as f32).abs() < 1e-4,
                "channel {c} should carry {}, got {got} — full frame {out:?}",
                c + 1
            );
        }
        assert!(
            out[2..].iter().all(|&s| s.abs() > 0.5),
            "channels 2..6 were dropped: {out:?}"
        );
    }

    /// Mono has no channel identity, so it fans to every channel of a wide frame.
    #[test]
    fn mono_wave_fans_to_every_channel_of_a_six_wide_frame() {
        let w = mono_ramp();
        let mut out = [0.0f32; 6];
        read_frame(&w, 8.0, &mut out);
        assert!(out[0].abs() > 0.0, "mono read produced silence");
        for c in 1..6 {
            assert_eq!(out[c], out[0], "channel {c} did not receive the mono fan");
        }
    }

    /// Stereo *does* have a channel identity, so it occupies the front pair and
    /// leaves the rest silent. Pinned because the asymmetry with mono is
    /// deliberate: fanning L into the centre and surrounds would smear the image
    /// and invent phantom centre content.
    #[test]
    fn stereo_wave_on_a_six_wide_frame_zero_fills_the_surrounds() {
        let w = indexed_wave(2);
        let mut out = [9.0f32; 6];
        read_frame(&w, 4.0, &mut out);
        assert!((out[0] - 1.0).abs() < 1e-4);
        assert!((out[1] - 2.0).abs() < 1e-4);
        for (c, &s) in out.iter().enumerate().skip(2) {
            assert_eq!(s, 0.0, "channel {c} must be exactly silent, got {s}");
        }
    }

    /// Narrowing goes through the engine's ITU-R BS.775 matrix, not a front-pair
    /// truncation: a centre-only 5.1 source must reach **both** stereo outputs at
    /// −3 dB. Cross-checked against `fold_frame` directly so the assertion is
    /// "the engine's matrix was used", not a hand-picked constant.
    #[test]
    fn six_channel_wave_on_a_stereo_frame_folds_through_the_itu_matrix() {
        // 5.1 order: L R C LFE Ls Rs — centre only.
        let mut w = Wave::zero(6, 44_100.0, 32.0 / 44_100.0);
        for i in 0..w.len() {
            w.set(2, i, 1.0);
        }
        let w = Arc::new(w);

        let mut out = [0.0f32; 2];
        read_frame(&w, 4.0, &mut out);

        let mut expected = [0.0f32; 2];
        fold_frame(&[0.0, 0.0, 1.0, 0.0, 0.0, 0.0], &mut expected);
        assert!(
            (out[0] - expected[0]).abs() < 1e-6 && (out[1] - expected[1]).abs() < 1e-6,
            "expected the engine fold {expected:?}, got {out:?}"
        );
        assert!(
            out[0] > 0.5 && out[1] > 0.5,
            "centre was dropped — this is front-pair truncation, not a fold: {out:?}"
        );
    }

    /// A wave narrower than the frame must not read past its own channel count.
    /// `Wave::at` is an unchecked index, so getting this wrong panics rather than
    /// producing bad audio.
    #[test]
    fn narrow_wave_on_a_wide_frame_does_not_read_out_of_bounds() {
        let w = indexed_wave(2);
        let mut out = [0.0f32; MAX_SAMPLER_CHANNELS];
        read_frame(&w, 4.0, &mut out); // must not panic
    }

    #[test]
    fn empty_wave_and_zero_width_frame_are_silent() {
        let w = Arc::new(Wave::zero(2, 44_100.0, 0.0));
        let mut out = [7.0f32; 2];
        read_frame(&w, 0.0, &mut out);
        assert_eq!(out, [0.0, 0.0], "empty wave must produce silence");

        let w = stereo_ramp();
        read_frame(&w, 1.0, &mut []); // zero-width: must not panic
    }

    /// `read_frame` is `pub`, so it must survive a position past the end of the
    /// wave rather than relying on its caller to gate first.
    ///
    /// `Wave::at` is an unchecked `self.vec[c][i]`, so an unclamped tap panics
    /// instead of returning a wrong sample. The trap is `im1`: `saturating_sub(1)`
    /// guards only the LOW end, so it needs `.min(last)` exactly as the other
    /// three taps do.
    #[test]
    fn position_past_the_end_does_not_panic() {
        let w = stereo_ramp();
        let len = w.len() as f64;
        let mut out = [0.0f32; 2];
        for pos in [len - 0.5, len, len + 1.0, len * 4.0, 1e9] {
            read_frame(&w, pos, &mut out); // must not panic
        }
    }

    /// A block of `len` frames at 48 kHz under `transport`, with `changes`.
    fn env(len: usize, transport: Transport, changes: &[(usize, Transport)]) -> Env {
        let mut c = TransportChanges::NONE;
        for &(at, to) in changes {
            c.push(Offset::new(at, Samples(len)).unwrap(), to).unwrap();
        }
        Env {
            frame: Frame(0),
            sample_rate: SampleRate(48_000.0),
            block_len: Samples(len),
            transport,
            changes: c,
        }
    }

    /// 120 BPM at 48 kHz: 24 000 frames a beat.
    const FPB: f64 = 24_000.0;

    fn rolling(beat: f64) -> Transport {
        Transport::new(true, Bpm(120.0), Beat(beat), None)
    }

    /// A gate over a 48 kHz source at unit rate, for `window`.
    fn gate(window: VoiceWindow) -> Gate {
        Gate {
            window,
            source_rate: SampleRate(48_000.0),
            rate: ReadRate::UNITY,
        }
    }

    fn placed(env: &Env, gate: Gate) -> Vec<Option<f64>> {
        let block = super::super::clock::Clock::new().observe(env);
        let mut out = vec![None; env.block_len.get()];
        place(&block, 0..out.len(), gate, ReadRate::UNITY, &mut out);
        out.into_iter().map(|p| p.map(|p| p.get())).collect()
    }

    /// **A window that opens and closes inside a block does so on its
    /// frames** (doc 013 §6): a clip placed 10 frames into a 64-frame block
    /// reads its frame 0 there, and one lasting 30 frames is silent from
    /// frame 40 on. Under the 64-frame chunked read the gate was asked once
    /// per chunk, and this clip entered at the next chunk.
    ///
    /// Mutation (run): `place` seating only if the range's first frame is
    /// inside the window (the chunked gate) → frames 10..40 are `None` →
    /// fails. Mutation (run): the exit never found inside the range
    /// (`x = b`) → frame 40 sounds → fails.
    #[test]
    fn a_window_opens_and_closes_on_its_frames() {
        let start = 1.0;
        let env = env(64, rolling(start - 10.0 / FPB), &[]);
        let got = placed(
            &env,
            gate(VoiceWindow::span(Beat(start), BeatDuration(30.0 / FPB))),
        );
        for (i, p) in got.iter().enumerate() {
            let want = (10..40).contains(&i).then(|| (i - 10) as f64);
            assert_eq!(*p, want, "frame {i}");
        }
    }

    /// **A seek inside a block moves the read on its frame**, re-seated at
    /// the gate there: a clip playing from beat 0 is at source frame
    /// `24 000 + i` at frame `i` of a block at beat 1, and after a seek to
    /// beat 2 on frame 32, at `48 000 + (i - 32)`. A stop on frame 48 silences
    /// it there.
    ///
    /// Mutation (run): `place` reading the block's transport for every frame
    /// (ignoring `Env::changes`) → frame 32 reads 24 032 → fails.
    #[test]
    fn a_seek_and_a_stop_inside_a_block_land_on_their_frames() {
        let stopped = Transport::new(false, Bpm(120.0), Beat(2.5), None);
        let env = env(64, rolling(1.0), &[(32, rolling(2.0)), (48, stopped)]);
        let got = placed(&env, gate(VoiceWindow::default()));
        for (i, p) in got.iter().enumerate() {
            let want = match i {
                0..32 => Some(24_000.0 + i as f64),
                32..48 => Some(48_000.0 + (i - 32) as f64),
                _ => None,
            };
            assert_eq!(*p, want, "frame {i}");
        }
    }

    /// **A loop wrap inside a block re-seats the read at the loop's start on
    /// the wrap's frame**, and the clock reports it as a jump (a new
    /// generation): the buffered state behind a read — a stretch filter, the
    /// live disk reader — is flushed or crossfaded there.
    ///
    /// Mutation (run): `Runs` not cutting a run at a wrap (`wrap = None`) →
    /// frames past the wrap read on past the loop's end → fails.
    #[test]
    fn a_loop_wrap_inside_a_block_reseats_at_the_loop_start() {
        let looping = Some(tutti_graph::LoopRange {
            start: Beat(0.0),
            end: Beat(1.0),
        });
        let t = Transport::new(true, Bpm(120.0), Beat(1.0 - 20.0 / FPB), looping);
        let env = env(64, t, &[]);
        let got = placed(&env, gate(VoiceWindow::default()));
        for (i, p) in got.iter().enumerate() {
            let want = if i < 20 {
                FPB - 20.0 + i as f64
            } else {
                (i - 20) as f64
            };
            assert_eq!(*p, Some(want), "frame {i}");
        }
        let mut clock = super::super::clock::Clock::new();
        let block = clock.observe(&env);
        let runs: Vec<_> = block.runs().collect();
        assert_eq!(runs.len(), 2);
        assert_eq!((runs[1].start, runs[1].jump), (20, true));
        assert_eq!(runs[1].generation, runs[0].generation + 1);
    }
}
