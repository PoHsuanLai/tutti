use crate::builder::TempoMapBuilder;
use crate::error::TempoError;
use crate::point::{TempoPoint, Transition};
use crate::segment::Seg;
use crate::view::TempoView;
use tutti_types::{Beat, BeatDuration, Bpm, Frame, SampleRate, Seconds, first_frame_at_or_after};

/// An immutable tempo map.
///
/// Built on the control side (`new` and `with_point` allocate); every query
/// borrows it and allocates nothing.
#[derive(Clone, Debug, PartialEq)]
pub struct TempoMap {
    pts: Vec<TempoPoint>,
    segs: Vec<Seg>,
}

/// A lookup hint, owned by a [`TempoView`]. Audio asks for
/// nearly monotone positions, so the segment found last time is nearly always
/// right; a wrong hint costs one binary search and never changes a result.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Cursor(usize);

impl TempoMap {
    /// One tempo from beat 0 on.
    ///
    /// # Errors
    ///
    /// [`TempoError::Unrepresentable`] when `tempo` is so small that a beat's
    /// rate underflows.
    pub fn constant(tempo: Bpm) -> Result<Self, TempoError> {
        Self::new([TempoPoint::step(Beat(0.0), tempo)])
    }

    /// A [`TempoMapBuilder`] that starts at `first` from beat 0, for a map
    /// written point by point with no way to end on a ramp.
    pub fn builder(first: Bpm) -> TempoMapBuilder {
        TempoMapBuilder::new(first)
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
        if pts.last().is_some_and(|p| p.transition != Transition::Step) {
            return Err(TempoError::RampWithoutEnd);
        }
        // Anchors: a Neumaier-compensated running sum of segment durations, so
        // a map of 10^5 points keeps the error of one addition, not of 10^5.
        let mut segs = Vec::with_capacity(pts.len());
        let (mut sum, mut comp) = (0.0f64, 0.0f64);
        for (i, p) in pts.iter().enumerate() {
            let (r1, db) = match pts.get(i + 1) {
                Some(n) => (n.tempo.get() / 60.0, n.beat - p.beat),
                None => (p.tempo.get() / 60.0, BeatDuration(f64::INFINITY)),
            };
            let s = Seg::new(p.beat, Seconds(sum + comp), p.tempo, r1, p.transition, db);
            if i + 1 < pts.len() {
                if !s.is_sound(db) {
                    return Err(TempoError::Unrepresentable(i));
                }
                // The compensated sum's own arithmetic is on the raw seconds.
                let dsec = s.dsec.get();
                let t = sum + dsec;
                comp += if sum.abs() >= dsec.abs() {
                    (sum - t) + dsec
                } else {
                    (dsec - t) + sum
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

    /// A [`TempoView`] over this map at `rate`: the frame queries, with a
    /// lookup hint, for a reader that walks the map in order.
    #[must_use]
    pub fn view(&self, rate: SampleRate) -> TempoView<'_> {
        TempoView::new(self, rate)
    }

    /// The points, as built.
    #[must_use]
    pub fn points(&self) -> &[TempoPoint] {
        &self.pts
    }

    /// The tempo, when the whole map is one point (what
    /// [`constant`](Self::constant) builds): `Some` of that point's tempo to
    /// the bit, `None` for a map that changes. A reader that can follow only
    /// one tempo asks this and refuses the rest.
    #[must_use]
    pub fn constant_tempo(&self) -> Option<Bpm> {
        match self.pts.as_slice() {
            [only] => Some(only.tempo),
            _ => None,
        }
    }

    /// The segment holding `x` by `key` (`beat0` or `sec0`), starting from the
    /// hint. Segment `i` holds `key(i) <= x < key(i + 1)`; below the first it is
    /// the first.
    #[inline]
    fn locate<T: PartialOrd + Copy>(
        &self,
        key: impl Fn(&Seg) -> T,
        x: T,
        hint: &mut Cursor,
    ) -> usize {
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

    fn by_beat(&self, beat: Beat, hint: &mut Cursor) -> &Seg {
        &self.segs[self.locate(|s| s.beat0, beat, hint)]
    }

    fn by_sec(&self, sec: Seconds, hint: &mut Cursor) -> &Seg {
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
        let cut = |s: &Seg| first_frame_at_or_after(s.sec0.get() * rate.get()).max(0) as u64;
        let i = self.locate(cut, frame.get(), hint);
        (
            &self.segs[i],
            cut(&self.segs[i]),
            self.segs.get(i + 1).map(cut),
        )
    }

    /// Time from beat 0 to `beat` (negative before beat 0, at the first
    /// tempo).
    ///
    /// One query from a cold start. A caller that asks many nearby positions
    /// takes a [`view`](Self::view) instead.
    #[must_use]
    pub fn seconds_at(&self, beat: Beat) -> Seconds {
        self.seconds_at_from(beat, &mut Cursor::default())
    }

    #[inline]
    pub(crate) fn seconds_at_from(&self, beat: Beat, hint: &mut Cursor) -> Seconds {
        let s = self.by_beat(beat, hint);
        s.sec0 + s.sec_in(beat - s.beat0)
    }

    /// The beat `seconds` from beat 0.
    #[must_use]
    pub fn beat_at_seconds(&self, seconds: Seconds) -> Beat {
        self.beat_at_seconds_from(seconds, &mut Cursor::default())
    }

    #[inline]
    pub(crate) fn beat_at_seconds_from(&self, seconds: Seconds, hint: &mut Cursor) -> Beat {
        let s = self.by_sec(seconds, hint);
        s.beat0 + s.beat_in(seconds - s.sec0)
    }

    /// The tempo at `beat`. A point's own beat belongs to that point, so a step
    /// reads its new tempo there, and a ramp reads the tempo it was given.
    #[must_use]
    pub fn tempo_at(&self, beat: Beat) -> Bpm {
        self.tempo_at_from(beat, &mut Cursor::default())
    }

    #[inline]
    pub(crate) fn tempo_at_from(&self, beat: Beat, hint: &mut Cursor) -> Bpm {
        let s = self.by_beat(beat, hint);
        s.bpm_at_beat(beat - s.beat0)
    }

    /// The tempo `seconds` from beat 0.
    #[must_use]
    pub fn tempo_at_seconds(&self, seconds: Seconds) -> Bpm {
        self.tempo_at_seconds_from(seconds, &mut Cursor::default())
    }

    #[inline]
    pub(crate) fn tempo_at_seconds_from(&self, seconds: Seconds, hint: &mut Cursor) -> Bpm {
        let s = self.by_sec(seconds, hint);
        s.bpm_at_sec(seconds - s.sec0)
    }

    #[inline]
    pub(crate) fn tempo_slope_from(&self, from: Seconds, to: Seconds, hint: &mut Cursor) -> f64 {
        let s = self.by_sec(from, hint);
        let ds0 = from - s.sec0;
        let ds1 = (to - s.sec0).min(s.dsec);
        let span = (ds1 - ds0).get();
        // `is_nan` too: a NaN span is no span.
        if span.is_nan() || span <= 0.0 {
            return 0.0;
        }
        (s.bpm_at_sec(ds1).get() - s.bpm_at_sec(ds0).get()) / span
    }

    #[inline]
    pub(crate) fn frame_at_or_after_from(
        &self,
        beat: Beat,
        rate: SampleRate,
        hint: &mut Cursor,
    ) -> Frame {
        let frames = self.seconds_at_from(beat, hint).get() * rate.get();
        Frame::new(first_frame_at_or_after(frames).max(0) as u64)
    }

    #[inline]
    pub(crate) fn next_change_from(
        &self,
        frame: Frame,
        rate: SampleRate,
        hint: &mut Cursor,
    ) -> Option<Frame> {
        let (seg, start, next) = self.by_frame(frame, rate, hint);
        // Only the first segment starts at beat 0, and that is not a change.
        if start == frame.get() && seg.beat0 != Beat(0.0) {
            return Some(frame);
        }
        next.map(Frame::new)
    }
}
