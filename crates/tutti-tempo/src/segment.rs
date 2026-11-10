//! One segment's closed forms. Rates are beats per second (`bpm / 60`).
//!
//! * Step: `beats = r0 * s`.
//! * Ramp, tempo linear in time: `r(s) = r0 + a s`, `beats = r0 s + a s^2 / 2`.
//!   Over `db` beats from `r0` to `r1` it lasts `2 db / (r0 + r1)` seconds.
//!   Polynomial one way, one `sqrt` (correctly rounded) the other.
//! * Ramp, tempo linear in beats: `r(b) = r0 + g b`, `s = ln(1 + g b / r0) / g`,
//!   `beats = r0 (e^{g s} - 1) / g`.
//!
//! A negative offset is before the segment: only the first segment is ever
//! asked, and it extrapolates at its own tempo.
//!
//! Positions and spans are units (`Beat`, `BeatDuration`, `Seconds`); each
//! method takes its offset as one and reads the raw `f64` once at its edge,
//! because the closed forms mix beats, seconds and rates in one expression.
//! Two quantities stay bare `f64` because no unit covers them: a rate in
//! beats per second (`r0`, `r1`; a `Bpm` over 60, kept apart so the forms
//! read as written above) and a ramp's coefficient (`k`: beats per second
//! per second for a time ramp, per beat for a beat ramp).

use crate::point::Shape;
use tutti_types::{Beat, BeatDuration, Bpm, Seconds};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Seg {
    /// Where the segment starts.
    pub beat0: Beat,
    /// Time from beat 0 to `beat0`.
    pub sec0: Seconds,
    /// Tempo at `beat0`, as given.
    pub bpm0: Bpm,
    /// `bpm0 / 60`, in beats per second.
    pub r0: f64,
    pub shape: Shape,
    /// How long the segment lasts (`INFINITY` for the last).
    pub dsec: Seconds,
    /// `a` (time ramp) or `g` (beat ramp); 0 for a step.
    pub k: f64,
}

impl Seg {
    /// The segment from `beat0`, spanning `dbeats` to a next tempo of `r1`
    /// (`INFINITY` beats for the last segment).
    pub fn new(
        beat0: Beat,
        sec0: Seconds,
        bpm0: Bpm,
        r1: f64,
        shape: Shape,
        dbeats: BeatDuration,
    ) -> Self {
        let dbeats = dbeats.get();
        let r0 = bpm0.get() / 60.0;
        let shape = if r0 == r1 { Shape::Step } else { shape };
        let (k, dsec) = match shape {
            Shape::Step => (0.0, dbeats / r0),
            Shape::RampTime => {
                let t = 2.0 * dbeats / (r0 + r1);
                ((r1 - r0) / t, t)
            }
            Shape::RampBeats => {
                let g = (r1 - r0) / dbeats;
                (g, ln_ratio(g, dbeats, r0))
            }
        };
        Self {
            beat0,
            sec0,
            bpm0,
            r0,
            shape,
            dsec: Seconds(dsec),
            k,
        }
    }

    /// Whether the closed forms agree with themselves over the whole segment:
    /// the duration is finite and positive and the forward and inverse maps
    /// meet at the far end. An overflowing square or quotient fails it.
    pub fn is_sound(&self, dbeats: BeatDuration) -> bool {
        let back = self.sec_in(dbeats).get();
        let dsec = self.dsec.get();
        dsec.is_finite() && dsec > 0.0 && self.k.is_finite() && (back - dsec).abs() <= 1e-9 * dsec
    }

    /// Time into the segment at `db` beats in.
    pub fn sec_in(&self, db: BeatDuration) -> Seconds {
        Seconds(self.raw_sec_in(db.get()))
    }

    fn raw_sec_in(&self, db: f64) -> f64 {
        if db < 0.0 {
            return db / self.r0;
        }
        match self.shape {
            Shape::Step => db / self.r0,
            Shape::RampTime => {
                2.0 * db / (self.r0 + (self.r0 * self.r0 + 2.0 * self.k * db).sqrt())
            }
            Shape::RampBeats => ln_ratio(self.k, db, self.r0),
        }
    }

    /// Beats into the segment at `ds` in.
    pub fn beat_in(&self, ds: Seconds) -> BeatDuration {
        BeatDuration(self.raw_beat_in(ds.get()))
    }

    fn raw_beat_in(&self, ds: f64) -> f64 {
        if ds < 0.0 {
            return self.r0 * ds;
        }
        match self.shape {
            Shape::Step => self.r0 * ds,
            Shape::RampTime => ds * (self.r0 + 0.5 * self.k * ds),
            Shape::RampBeats => {
                let y = self.k * ds;
                if y == 0.0 {
                    self.r0 * ds
                } else {
                    self.r0 * ds * (libm::expm1(y) / y)
                }
            }
        }
    }

    /// Tempo `ds` in. Written as the given tempo plus a change, so offset 0 is
    /// that tempo to the bit.
    pub fn bpm_at_sec(&self, ds: Seconds) -> Bpm {
        let ds = ds.get();
        if ds < 0.0 {
            return self.bpm0;
        }
        let bpm0 = self.bpm0.get();
        self.in_range(match self.shape {
            Shape::Step => bpm0,
            Shape::RampTime => bpm0 + 60.0 * self.k * ds,
            Shape::RampBeats => bpm0 * libm::exp(self.k * ds),
        })
    }

    /// Tempo in BPM `db` beats in. As [`bpm_at_sec`](Self::bpm_at_sec); the
    /// time ramp's `sqrt(r0^2 + 2 k db) - r0` is `2 k db / (sqrt(..) + r0)`,
    /// which does not cancel.
    pub fn bpm_at_beat(&self, db: BeatDuration) -> Bpm {
        let db = db.get();
        if db < 0.0 {
            return self.bpm0;
        }
        let bpm0 = self.bpm0.get();
        self.in_range(match self.shape {
            Shape::Step => bpm0,
            Shape::RampTime => {
                let root = (self.r0 * self.r0 + 2.0 * self.k * db).sqrt();
                bpm0 + 60.0 * (2.0 * self.k * db) / (root + self.r0)
            }
            Shape::RampBeats => bpm0 + 60.0 * self.k * db,
        })
    }

    /// `raw` as a tempo; the segment's own where the closed form leaves the
    /// range (a ramp read far past its end).
    fn in_range(&self, raw: f64) -> Bpm {
        Bpm::new(raw).unwrap_or(self.bpm0)
    }
}

/// `ln(1 + g db / r0) / g`, and `db / r0` as `g -> 0`.
fn ln_ratio(g: f64, db: f64, r0: f64) -> f64 {
    let x = g * db / r0;
    if x == 0.0 {
        db / r0
    } else {
        (db / r0) * (libm::log1p(x) / x)
    }
}
