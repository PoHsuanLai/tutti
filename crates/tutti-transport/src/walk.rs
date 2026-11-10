//! The tempo walk (doc 020, section 2.2): a [`FrameClock`] that follows a
//! [`TempoMap`] that changes tempo, as one pure function of the map, the
//! grid, the rate and the frame.
//!
//! The live [`TransportClock`](super::clock::TransportClock) and the
//! [`OfflineTimeline`](super::OfflineTimeline) both call it, so a live render
//! and an offline one of the same map cannot disagree. A constant map never
//! reaches it: the clocks run their own closed form, untouched, and a
//! constant-tempo render is what it always was, bit for bit.
//!
//! The walk keeps one number, `frame`: the map frame the playhead is on. It is
//! set at a seat (`frame_at_or_after` of the exact seat beat) and moves with
//! the clock, and it never feeds the beat; the beat is the clock's, derived in
//! closed form from the segment the walk starts. At each cell boundary
//! ([`CELL_GRID`] frames, and where a point of the map starts) it asks
//! `chord_cell` for the cell's tempo and, when that differs from the clock's,
//! starts a segment on the map's beat. A step cell carries its segment's tempo
//! to the bit, so a step interior never restarts; there is no hysteresis here
//! (a map is an immutable value, and a tempo that moved by an ulp moved on
//! purpose).

use tutti_graph::{Offset, Transport, TransportChanges};
use tutti_tempo::{CellGrid, Cursor, TempoMap, chord_cell_with};
use tutti_types::{Beat, Frame, FrameClock, LoopRange, Samples, first_frame_at_or_after};

/// The one cell grid: a cell is at most this many frames, whatever the block
/// length (doc 020, section 10: one engine constant, never per block).
pub(crate) const CELL_GRID: CellGrid = CellGrid::of(128);

/// The tempo changes inside a block, found ahead of the render
/// ([`TransportAudio::tempo_ahead`](super::TransportAudio::tempo_ahead)).
#[derive(Clone, Copy, Debug)]
pub struct TempoAhead {
    /// The changes that fit, in time order (none at offset 0).
    pub changes: TransportChanges,
    /// Where the block must end because the next change does not fit: the
    /// frame it would start on. The change is the next block's first.
    pub limit: Option<usize>,
}

impl TempoAhead {
    /// No change, and the block runs whole.
    pub const NONE: Self = Self {
        changes: TransportChanges::NONE,
        limit: None,
    };
}

/// Where a clock is on a map's timeline. `Copy`, so a look ahead walks a copy.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TempoWalk {
    /// The map frame the playhead is on. Negative before beat 0 (a count-in),
    /// where the first point's tempo holds.
    frame: i64,
    hint: Cursor,
}

impl TempoWalk {
    /// Seat `clock` at the exact `beat` and take the map's tempo there.
    pub fn seat(&mut self, map: &TempoMap, clock: &mut FrameClock, beat: Beat) {
        clock.seat(beat);
        self.place(map, clock);
    }

    /// Find `clock`'s current beat on `map`'s timeline and take the tempo in
    /// force there, leaving the clock's beat as it is: after a seat, a loop
    /// wrap, a rate change or a map swap. `true` when the tempo moved.
    pub fn place(&mut self, map: &TempoMap, clock: &mut FrameClock) -> bool {
        let rate = clock.sample_rate();
        let beat = clock.beat();
        self.frame = if beat.get() < 0.0 {
            first_frame_at_or_after(map.seconds_at_with(beat, &mut self.hint).get() * rate.get())
        } else {
            map.frame_at_or_after_with(beat, rate, &mut self.hint).get() as i64
        };
        self.sync(map, clock, true)
    }

    /// Take the tempo of the cell the playhead is on. `exact`: the clock's beat
    /// was just placed and stays; otherwise the playhead is on a cell's first
    /// frame and the segment starts on the map's beat there.
    fn sync(&mut self, map: &TempoMap, clock: &mut FrameClock, exact: bool) -> bool {
        let (tempo, beat) = match u64::try_from(self.frame) {
            Ok(f) => {
                let cell = chord_cell_with(
                    map,
                    Frame(f),
                    CELL_GRID,
                    clock.sample_rate(),
                    &mut self.hint,
                );
                (cell.tempo, Some(cell.beat))
            }
            Err(_) => (map.points()[0].tempo, None),
        };
        if tempo == clock.tempo() {
            return false;
        }
        if let (false, Some(beat)) = (exact, beat) {
            clock.seat(beat);
        }
        clock.set_tempo(tempo);
        true
    }

    /// Frames from the playhead to the end of its cell (at least 1).
    fn frames_to_edge(&mut self, map: &TempoMap, rate: tutti_types::SampleRate) -> usize {
        match u64::try_from(self.frame) {
            Ok(f) => {
                let cell = chord_cell_with(map, Frame(f), CELL_GRID, rate, &mut self.hint);
                (cell.end.get() - f).max(1) as usize
            }
            Err(_) => self.frame.unsigned_abs() as usize,
        }
    }

    /// Roll `clock` `frames` forward, wrapping at `looping`, and following the
    /// map's tempo cell by cell. With `stop`, return right after the first
    /// frame the tempo changed on (the clock standing on it); otherwise roll
    /// all `frames`. Returns the frames rolled and whether the tempo changed.
    pub fn roll(
        &mut self,
        map: &TempoMap,
        clock: &mut FrameClock,
        frames: usize,
        looping: Option<LoopRange>,
        stop: bool,
    ) -> (usize, bool) {
        let rate = clock.sample_rate();
        let (mut rolled, mut changed) = (0, false);
        while rolled < frames {
            let edge = self.frames_to_edge(map, rate);
            let chunk = (frames - rolled).min(edge);
            match clock.advance_to_wrap(Samples(chunk), looping) {
                Some(wrapped) => {
                    let w = wrapped.get();
                    rolled += w;
                    self.frame += w as i64;
                    // A wrap is a seek to the loop start: place it exactly.
                    changed |= self.place(map, clock);
                }
                None => {
                    rolled += chunk;
                    self.frame += chunk as i64;
                    if chunk == edge {
                        changed |= self.sync(map, clock, false);
                    }
                }
            }
            if changed && stop {
                break;
            }
        }
        (rolled, changed)
    }

    /// The tempo changes inside the next `frames` of a rolling `clock`, at most
    /// `cap` of them. `transport` builds the transport from the clock after a
    /// change. Walks a copy, with the code [`roll`](Self::roll) runs for real, so
    /// a render that rolls to a change's offset finds the same change there.
    pub fn ahead(
        &self,
        map: &TempoMap,
        clock: &FrameClock,
        frames: usize,
        looping: Option<LoopRange>,
        cap: usize,
        transport: impl Fn(&FrameClock) -> Transport,
    ) -> TempoAhead {
        let (mut walk, mut clock) = (*self, *clock);
        let mut out = TempoAhead::NONE;
        let mut at = 0;
        while at < frames {
            let (rolled, changed) = walk.roll(map, &mut clock, frames - at, looping, true);
            at += rolled;
            if !changed || at >= frames {
                break;
            }
            if out.changes.len() >= cap {
                out.limit = Some(at);
                break;
            }
            let offset = Offset::new(at, Samples(frames)).expect("inside the block");
            let _ = out.changes.push(offset, transport(&clock));
        }
        out
    }
}
