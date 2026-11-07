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

use crate::point::Shape;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Seg {
    pub beat0: f64,
    /// Seconds from beat 0 to `beat0`.
    pub sec0: f64,
    /// Tempo at `beat0`, as given.
    pub bpm0: f64,
    /// `bpm0 / 60`.
    pub r0: f64,
    pub shape: Shape,
    /// Seconds in the segment (`INFINITY` for the last).
    pub dsec: f64,
    /// `a` (time ramp) or `g` (beat ramp); 0 for a step.
    pub k: f64,
}

impl Seg {
    /// The segment from `beat0`, spanning `dbeats` to a next tempo of `r1`
    /// (`INFINITY` beats for the last segment).
    pub fn new(beat0: f64, sec0: f64, bpm0: f64, r1: f64, shape: Shape, dbeats: f64) -> Self {
        let r0 = bpm0 / 60.0;
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
            dsec,
            k,
        }
    }

    /// Whether the closed forms agree with themselves over the whole segment:
    /// the duration is finite and positive and the forward and inverse maps
    /// meet at the far end. An overflowing square or quotient fails it.
    pub fn is_sound(&self, dbeats: f64) -> bool {
        let back = self.sec_in(dbeats);
        self.dsec.is_finite()
            && self.dsec > 0.0
            && self.k.is_finite()
            && (back - self.dsec).abs() <= 1e-9 * self.dsec
    }

    /// Seconds into the segment at `db` beats in.
    pub fn sec_in(&self, db: f64) -> f64 {
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

    /// Beats into the segment at `ds` seconds in.
    pub fn beat_in(&self, ds: f64) -> f64 {
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

    /// The beat at `t` seconds from beat 0, by this segment's own line (it
    /// continues past the segment's end).
    pub fn beat_at(&self, t: f64) -> f64 {
        self.beat0 + self.beat_in(t - self.sec0)
    }

    /// Tempo in BPM `ds` seconds in. Written as the given tempo plus a change,
    /// so offset 0 is that tempo to the bit.
    pub fn bpm_at_sec(&self, ds: f64) -> f64 {
        if ds < 0.0 {
            return self.bpm0;
        }
        match self.shape {
            Shape::Step => self.bpm0,
            Shape::RampTime => self.bpm0 + 60.0 * self.k * ds,
            Shape::RampBeats => self.bpm0 * libm::exp(self.k * ds),
        }
    }

    /// Tempo in BPM `db` beats in. As [`bpm_at_sec`](Self::bpm_at_sec); the
    /// time ramp's `sqrt(r0^2 + 2 k db) - r0` is `2 k db / (sqrt(..) + r0)`,
    /// which does not cancel.
    pub fn bpm_at_beat(&self, db: f64) -> f64 {
        if db < 0.0 {
            return self.bpm0;
        }
        match self.shape {
            Shape::Step => self.bpm0,
            Shape::RampTime => {
                let root = (self.r0 * self.r0 + 2.0 * self.k * db).sqrt();
                self.bpm0 + 60.0 * (2.0 * self.k * db) / (root + self.r0)
            }
            Shape::RampBeats => self.bpm0 + 60.0 * self.k * db,
        }
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
