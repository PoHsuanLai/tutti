use crate::grid::{Cell, CellGrid};
use crate::map::{Cursor, TempoMap};
use crate::point::Transition;
use tutti_types::{Beat, Bpm, Frame, SampleRate, Seconds};

/// A [`TempoMap`] read at one sample rate, with the lookup hint a reader that
/// walks the map in order wants.
///
/// It is a borrow of the map, the rate and a lookup hint it owns:
/// three words, made by [`TempoMap::view`], allocating nothing, so a render
/// makes one per block. Audio asks for nearly monotone positions, so the
/// segment found last time is nearly always the right one, and a stale hint
/// costs one binary search and never changes an answer: every method returns
/// what the same query on a fresh view would.
///
/// **A view does not outlive the map it borrows**, and the map a block reads is
/// lent for that block (an `ArcLend` loan, which must not be parked), so a view
/// lives for one block at most. The hint it loses between blocks is one binary
/// search on the first query.
///
/// The queries take `&mut self` because they move the hint. A frame is counted
/// from beat 0 at the view's rate.
#[derive(Clone, Debug)]
pub struct TempoView<'a> {
    map: &'a TempoMap,
    rate: SampleRate,
    hint: Cursor,
}

impl<'a> TempoView<'a> {
    pub(crate) fn new(map: &'a TempoMap, rate: SampleRate) -> Self {
        Self {
            map,
            rate,
            hint: Cursor::default(),
        }
    }

    /// The map this reads.
    #[must_use]
    pub fn map(&self) -> &'a TempoMap {
        self.map
    }

    /// The rate frames are counted at.
    #[must_use]
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// Time from beat 0 to `beat` ([`TempoMap::seconds_at`]).
    #[must_use]
    #[inline]
    pub fn seconds_at(&mut self, beat: Beat) -> Seconds {
        self.map.seconds_at_from(beat, &mut self.hint)
    }

    /// The beat `seconds` from beat 0 ([`TempoMap::beat_at_seconds`]).
    #[must_use]
    #[inline]
    pub fn beat_at_seconds(&mut self, seconds: Seconds) -> Beat {
        self.map.beat_at_seconds_from(seconds, &mut self.hint)
    }

    /// The tempo at `beat` ([`TempoMap::tempo_at`]).
    #[must_use]
    #[inline]
    pub fn tempo_at(&mut self, beat: Beat) -> Bpm {
        self.map.tempo_at_from(beat, &mut self.hint)
    }

    /// The tempo `seconds` from beat 0 ([`TempoMap::tempo_at_seconds`]).
    #[must_use]
    #[inline]
    pub fn tempo_at_seconds(&mut self, seconds: Seconds) -> Bpm {
        self.map.tempo_at_seconds_from(seconds, &mut self.hint)
    }

    /// How fast the tempo moves from `from` to `to`, in BPM per second: the
    /// difference of the two tempos over their distance, **held inside the
    /// segment `from` is in**. The span stops where that segment ends, so a
    /// point between the two never adds its jump (a step's change of tempo is
    /// a transport event of its own, not a slope), and a ramp that ends in the
    /// span is read to its end. Exact for a [`Transition::RampTime`] ramp,
    /// whose tempo is linear in time; a beat ramp's tempo is exponential in
    /// time, and this is its chord over the span. `0.0` for a step, an empty
    /// span, or a span that ends before it begins.
    ///
    /// A bare `f64` (BPM per second): no unit covers a tempo's slope, and its
    /// reader divides it into CLAP's per-sample `tempo_inc` at the C ABI.
    #[must_use]
    #[inline]
    pub fn tempo_slope(&mut self, from: Seconds, to: Seconds) -> f64 {
        self.map.tempo_slope_from(from, to, &mut self.hint)
    }

    /// The tempo on `frame`: the curve's instantaneous tempo there, not a
    /// cell's chord. The slope a reader wants over a stretch of frames is the
    /// difference of two of these over their distance.
    #[must_use]
    #[inline]
    pub fn tempo_at_frame(&mut self, frame: Frame) -> Bpm {
        let seconds = Seconds(frame.get() as f64 / self.rate.get());
        self.tempo_at_seconds(seconds)
    }

    /// The beat on `frame`.
    #[must_use]
    #[inline]
    pub fn beat_at_frame(&mut self, frame: Frame) -> Beat {
        let seconds = Seconds(frame.get() as f64 / self.rate.get());
        self.beat_at_seconds(seconds)
    }

    /// The first frame at or after `beat`, by the engine's one placement rule
    /// ([`first_frame_at_or_after`](tutti_types::first_frame_at_or_after)): a
    /// beat within `FRAME_TOLERANCE` of a frame is on it. A beat before frame 0
    /// gives frame 0.
    ///
    /// The tolerance is absolute (1e-6 frame) and an `f64` frame count carries
    /// less than that only below 2^32 frames: 24.8 hours at 48 kHz, 6.2 hours
    /// at 192 kHz. Past that a frame to beat to frame round trip can land a frame
    /// off. The same limit is documented on `FRAME_TOLERANCE`; the map does not
    /// scale it, because the rule is shared with every reader that places a beat.
    #[must_use]
    #[inline]
    pub fn frame_at_or_after(&mut self, beat: Beat) -> Frame {
        self.map
            .frame_at_or_after_from(beat, self.rate, &mut self.hint)
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
    #[inline]
    pub fn next_change(&mut self, frame: Frame) -> Option<Frame> {
        self.map.next_change_from(frame, self.rate, &mut self.hint)
    }

    /// The cell holding `frame`.
    ///
    /// Cell boundaries are the multiples of `grid` and the frames where the
    /// map's points start ([`next_change`](Self::next_change)); a cell never
    /// crosses either. After a point the cells go back to the same absolute
    /// multiples: a cell that starts on a point ends on the next multiple, not
    /// `grid` frames on.
    ///
    /// A pure function of the map, the rate, `frame` and `grid`: where a
    /// caller's blocks begin and end cannot change it (nor can the hint), so
    /// the same map gives identical cells at identical absolute frames at any
    /// buffer size. A render that has to stop sooner (a transport command)
    /// stops inside the cell and carries on with the same cell.
    ///
    /// A cell starts on the map's beat. A ramp cell ends on the map's beat too,
    /// so the chord is the map's average tempo over the cell (a ramp segment
    /// shorter than a frame gives a tempo between its ends' and the next
    /// segment's). A step cell keeps its step's tempo to the bit, so one that
    /// ends where the next segment starts ends on its own line, which is less
    /// than one frame's beat from the map's beat at that frame, measured at the
    /// larger of the two tempos; the next cell starts on the map's.
    ///
    /// Inside a ramp the chord differs from the curve by a fraction of a frame
    /// that shrinks with the square of `grid` (about 0.04 frames for 60 to 240
    /// BPM over 8 beats at 128).
    #[must_use]
    pub fn chord_cell(&mut self, frame: Frame, grid: CellGrid) -> Cell {
        let rate = self.rate;
        let (seg, cut, next_cut) = self.map.by_frame(frame, rate, &mut self.hint);
        let on_grid = frame.get() / grid.get() * grid.get();
        let start = on_grid.max(cut);
        let end = on_grid
            .saturating_add(grid.get())
            .min(next_cut.unwrap_or(u64::MAX));
        let (transition, bpm0) = (seg.transition, seg.bpm0);
        let b0 = self.beat_at_frame(Frame::new(start)).get();
        let tempo = match transition {
            Transition::Step => bpm0,
            // The map's beat at both ends, not this segment's closed form: a
            // segment shorter than a frame ends inside its last cell, and its
            // form past that point is not the map's.
            Transition::RampTime | Transition::RampBeats => {
                let chord = (self.beat_at_frame(Frame::new(end)).get() - b0) * 60.0 * rate.get()
                    / (end - start).max(1) as f64;
                Bpm::new(chord).unwrap_or(bpm0)
            }
        };
        Cell {
            start: Frame::new(start),
            end: Frame::new(end),
            beat: Beat(b0),
            tempo,
        }
    }
}
