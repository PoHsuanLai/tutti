use crate::error::TempoError;
use crate::point::{Shape, TempoPoint};
use crate::segment::Seg;
use tutti_types::{Beat, Bpm, Frame, SampleRate, first_frame_at_or_after};

/// An immutable tempo map.
///
/// Built on the control side (`new` and `with_point` allocate); every query
/// borrows it and allocates nothing.
#[derive(Clone, Debug)]
pub struct TempoMap {
    pts: Vec<TempoPoint>,
    segs: Vec<Seg>,
}

/// A lookup hint. Audio asks for nearly monotone positions, so the segment
/// found last time is nearly always right; a wrong hint costs one binary
/// search and never changes a result.
#[derive(Clone, Copy, Debug, Default)]
pub struct Cursor(usize);

impl TempoMap {
    /// One tempo from beat 0 on.
    ///
    /// # Errors
    ///
    /// [`TempoError::Unrepresentable`] when `tempo` is so small that a beat's
    /// rate underflows.
    pub fn constant(tempo: Bpm) -> Result<Self, TempoError> {
        Self::new([TempoPoint {
            beat: Beat(0.0),
            tempo,
            shape: Shape::Step,
        }])
    }

    /// Build from points sorted by beat. Allocates; control thread only.
    ///
    /// # Errors
    ///
    /// A [`TempoError`] naming what is wrong: no points, a first point off
    /// beat 0, beats that are not finite and strictly increasing, a last point that ramps, or a segment
    /// whose duration an `f64` cannot hold.
    pub fn new(points: impl IntoIterator<Item = TempoPoint>) -> Result<Self, TempoError> {
        let pts: Vec<TempoPoint> = points.into_iter().collect();
        let first = pts.first().ok_or(TempoError::Empty)?;
        if first.beat.get() != 0.0 {
            return Err(TempoError::FirstNotAtZero);
        }
        for (i, p) in pts.iter().enumerate() {
            if i > 0 && !(p.beat.get().is_finite() && p.beat > pts[i - 1].beat) {
                return Err(TempoError::BadOrder(i));
            }
        }
        if pts.last().is_some_and(|p| p.shape != Shape::Step) {
            return Err(TempoError::RampWithoutEnd);
        }
        // Anchors: a Neumaier-compensated running sum of segment durations, so
        // a map of 10^5 points keeps the error of one addition, not of 10^5.
        let mut segs = Vec::with_capacity(pts.len());
        let (mut sum, mut comp) = (0.0f64, 0.0f64);
        for (i, p) in pts.iter().enumerate() {
            let (r1, db) = match pts.get(i + 1) {
                Some(n) => (n.tempo.get() / 60.0, n.beat.get() - p.beat.get()),
                None => (p.tempo.get() / 60.0, f64::INFINITY),
            };
            let s = Seg::new(p.beat.get(), sum + comp, p.tempo, r1, p.shape, db);
            if i + 1 < pts.len() {
                if !s.is_sound(db) {
                    return Err(TempoError::Unrepresentable(i));
                }
                let t = sum + s.dsec;
                comp += if sum.abs() >= s.dsec.abs() {
                    (sum - t) + s.dsec
                } else {
                    (s.dsec - t) + sum
                };
                sum = t;
                if !(sum + comp).is_finite() {
                    return Err(TempoError::Unrepresentable(i));
                }
            } else if !(s.r0.is_finite() && s.r0 > 0.0) {
                return Err(TempoError::Unrepresentable(i));
            }
            segs.push(s);
        }
        Ok(Self { pts, segs })
    }

    /// A new map with `point` added, or replacing the point at its beat. The
    /// old map is untouched: this is what a control thread publishes.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new), for the edited list.
    pub fn with_point(&self, point: TempoPoint) -> Result<Self, TempoError> {
        // `-0.0 + 0.0` is `+0.0`: `total_cmp` would otherwise order a negative
        // zero before the point at beat 0 and insert instead of replace.
        let point = TempoPoint {
            beat: Beat(point.beat.get() + 0.0),
            ..point
        };
        let mut pts = self.pts.clone();
        match pts.binary_search_by(|p| p.beat.get().total_cmp(&point.beat.get())) {
            Ok(i) => pts[i] = point,
            Err(i) => pts.insert(i, point),
        }
        Self::new(pts)
    }

    /// The points, as built.
    #[must_use]
    pub fn points(&self) -> &[TempoPoint] {
        &self.pts
    }

    /// The segment holding `x` by `key` (`beat0` or `sec0`), starting from the
    /// hint. Segment `i` holds `key(i) <= x < key(i + 1)`; below the first it is
    /// the first.
    #[inline]
    fn locate(&self, key: impl Fn(&Seg) -> f64, x: f64, hint: &mut Cursor) -> usize {
        let holds =
            |i: usize| key(&self.segs[i]) <= x && self.segs.get(i + 1).is_none_or(|n| x < key(n));
        let h = hint.0.min(self.segs.len() - 1);
        let i = if holds(h) {
            h
        } else if h + 1 < self.segs.len() && holds(h + 1) {
            h + 1
        } else {
            self.segs.partition_point(|s| key(s) <= x).saturating_sub(1)
        };
        hint.0 = i;
        i
    }

    fn by_beat(&self, beat: f64, hint: &mut Cursor) -> &Seg {
        &self.segs[self.locate(|s| s.beat0, beat, hint)]
    }

    fn by_sec(&self, sec: f64, hint: &mut Cursor) -> &Seg {
        &self.segs[self.locate(|s| s.sec0, sec, hint)]
    }

    /// The segment that owns `frame`, with the frame where it starts and the
    /// frame where the next one does. A point's frame is the first frame at or
    /// after its time ([`first_frame_at_or_after`]); that frame and the ones
    /// after it belong to the point.
    pub(crate) fn by_frame(
        &self,
        frame: Frame,
        rate: SampleRate,
        hint: &mut Cursor,
    ) -> (&Seg, u64, Option<u64>) {
        let cut = |s: &Seg| first_frame_at_or_after(s.sec0 * rate.get()).max(0) as u64;
        let i = self.locate(|s| cut(s) as f64, frame.get() as f64, hint);
        (
            &self.segs[i],
            cut(&self.segs[i]),
            self.segs.get(i + 1).map(cut),
        )
    }

    /// Seconds from beat 0 to `beat`, in `f64` (negative before beat 0, at the
    /// first tempo).
    #[must_use]
    pub fn seconds_at(&self, beat: Beat) -> f64 {
        self.seconds_at_with(beat, &mut Cursor::default())
    }

    /// [`seconds_at`](Self::seconds_at) with a hint.
    #[must_use]
    #[inline]
    pub fn seconds_at_with(&self, beat: Beat, hint: &mut Cursor) -> f64 {
        let b = beat.get();
        let s = self.by_beat(b, hint);
        s.sec0 + s.sec_in(b - s.beat0)
    }

    /// The beat `seconds` from beat 0.
    #[must_use]
    pub fn beat_at_seconds(&self, seconds: f64) -> Beat {
        self.beat_at_seconds_with(seconds, &mut Cursor::default())
    }

    /// [`beat_at_seconds`](Self::beat_at_seconds) with a hint.
    #[must_use]
    #[inline]
    pub fn beat_at_seconds_with(&self, seconds: f64, hint: &mut Cursor) -> Beat {
        let s = self.by_sec(seconds, hint);
        Beat(s.beat0 + s.beat_in(seconds - s.sec0))
    }

    /// The tempo at `beat`. A point's own beat belongs to that point, so a step
    /// reads its new tempo there, and a ramp reads the tempo it was given.
    #[must_use]
    pub fn tempo_at(&self, beat: Beat) -> Bpm {
        self.tempo_at_with(beat, &mut Cursor::default())
    }

    /// [`tempo_at`](Self::tempo_at) with a hint.
    #[must_use]
    #[inline]
    pub fn tempo_at_with(&self, beat: Beat, hint: &mut Cursor) -> Bpm {
        let s = self.by_beat(beat.get(), hint);
        s.bpm_at_beat(beat.get() - s.beat0)
    }

    /// The tempo `seconds` from beat 0.
    #[must_use]
    pub fn tempo_at_seconds(&self, seconds: f64) -> Bpm {
        self.tempo_at_seconds_with(seconds, &mut Cursor::default())
    }

    /// [`tempo_at_seconds`](Self::tempo_at_seconds) with a hint.
    #[must_use]
    #[inline]
    pub fn tempo_at_seconds_with(&self, seconds: f64, hint: &mut Cursor) -> Bpm {
        let s = self.by_sec(seconds, hint);
        s.bpm_at_sec(seconds - s.sec0)
    }

    /// The beat on `frame`, counted from beat 0 at `rate`.
    #[must_use]
    pub fn beat_at_frame(&self, frame: Frame, rate: SampleRate) -> Beat {
        self.beat_at_frame_with(frame, rate, &mut Cursor::default())
    }

    /// [`beat_at_frame`](Self::beat_at_frame) with a hint.
    #[must_use]
    #[inline]
    pub fn beat_at_frame_with(&self, frame: Frame, rate: SampleRate, hint: &mut Cursor) -> Beat {
        self.beat_at_seconds_with(frame.get() as f64 / rate.get(), hint)
    }

    /// The first frame at or after `beat`, by the engine's one placement rule
    /// ([`first_frame_at_or_after`]): a beat within `FRAME_TOLERANCE` of a frame
    /// is on it. A beat before frame 0 gives frame 0.
    ///
    /// The tolerance is absolute (1e-6 frame) and an `f64` frame count carries
    /// less than that only below 2^32 frames: 24.8 hours at 48 kHz, 6.2 hours
    /// at 192 kHz. Past that a frame to beat to frame round trip can land a frame
    /// off. The same limit is documented on `FRAME_TOLERANCE`; the map does not
    /// scale it, because the rule is shared with every reader that places a beat.
    #[must_use]
    pub fn frame_at_or_after(&self, beat: Beat, rate: SampleRate) -> Frame {
        self.frame_at_or_after_with(beat, rate, &mut Cursor::default())
    }

    /// [`frame_at_or_after`](Self::frame_at_or_after) with a hint.
    #[must_use]
    #[inline]
    pub fn frame_at_or_after_with(&self, beat: Beat, rate: SampleRate, hint: &mut Cursor) -> Frame {
        let frames = self.seconds_at_with(beat, hint) * rate.get();
        Frame(first_frame_at_or_after(frames).max(0) as u64)
    }

    /// The first frame at or after `frame` on which a point of the map starts
    /// (a step, or the start or end of a ramp), or `None` when there is none
    /// left. The first point, at beat 0, is not a change.
    ///
    /// A renderer cuts a block here: from this frame on the tempo is another
    /// segment's, and the beat there is [`beat_at_frame`](Self::beat_at_frame)
    /// (the map's own, so a step that falls between two frames lands on the
    /// next one).
    #[must_use]
    pub fn next_change(&self, frame: Frame, rate: SampleRate) -> Option<Frame> {
        self.next_change_with(frame, rate, &mut Cursor::default())
    }

    /// [`next_change`](Self::next_change) with a hint.
    #[must_use]
    #[inline]
    pub fn next_change_with(
        &self,
        frame: Frame,
        rate: SampleRate,
        hint: &mut Cursor,
    ) -> Option<Frame> {
        let (seg, start, next) = self.by_frame(frame, rate, hint);
        // Only the first segment starts at beat 0, and that is not a change.
        if start == frame.get() && seg.beat0 != 0.0 {
            return Some(frame);
        }
        next.map(Frame)
    }
}
