//! Portamento: one pitch glide, stepped per sample.
//!
//! Each voice has its own, so a note's glide moves that voice and no other (M1
//! p.16). A glide is a factor on the voice's pitch
//! ([`Portamento::ratio`]), not a pitch of its own, so it composes with a
//! per-note pitch and the channel's bend instead of replacing them.
//!
//! Interpolation is logarithmic, so a glide is even in musical interval rather
//! than in [`Hz`]; the [`Law`] of [`PortamentoConfig::curve`] reshapes only how far along
//! that interval the glide has travelled.

use tutti_curve::Law;
use tutti_types::{Hz, SampleRate, Seconds, Semitones};

/// When a new note glides from the previous pitch rather than jumping to its
/// own.
#[derive(Debug, Clone, Copy, PartialEq, Hash, Eq, Default)]
#[non_exhaustive]
pub enum PortamentoMode {
    /// Glide into every note, whether or not the previous one is still held.
    Always,
    /// Glide only when the new note overlaps a sounding one. Pairs with
    /// [`VoiceMode::Legato`](crate::VoiceMode), which is what makes a note
    /// overlap reach the same voice.
    LegatoOnly,
    /// No glide: every note starts at its own pitch. The default, and the value
    /// that makes `set_target` snap rather than ramp.
    #[default]
    Off,
}

/// How long a glide takes: finite and above zero.
///
/// A glide of no time is [`PortamentoMode::Off`], not a time, so zero is not a
/// value this type holds. `new` refuses and never clamps; `of` is for a literal
/// or a constant.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct GlideTime(f64);

impl GlideTime {
    /// `seconds` as a glide time, or `None` when it is NaN, infinite, zero or
    /// negative.
    #[must_use]
    pub const fn new(seconds: f64) -> Option<Self> {
        if seconds > 0.0 && seconds <= f64::MAX {
            Some(Self(seconds))
        } else {
            None
        }
    }

    /// `seconds` as a glide time, for a literal or a constant.
    ///
    /// # Panics
    ///
    /// If `seconds` is NaN, infinite, zero or negative.
    #[must_use]
    #[track_caller]
    pub const fn of(seconds: f64) -> Self {
        match Self::new(seconds) {
            Some(time) => time,
            None => panic!("a glide time is finite and above zero"),
        }
    }

    /// The time in seconds.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }

    /// The time as [`Seconds`].
    #[must_use]
    pub const fn seconds(self) -> Seconds {
        Seconds(self.0)
    }
}

/// The refused raw of [`GlideTime`]'s `TryFrom<f64>`.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[error("a glide time is finite and above zero, but was {0}")]
pub struct GlideTimeError(pub f64);

impl TryFrom<f64> for GlideTime {
    type Error = GlideTimeError;

    fn try_from(seconds: f64) -> Result<Self, GlideTimeError> {
        Self::new(seconds).ok_or(GlideTimeError(seconds))
    }
}

impl From<GlideTime> for f64 {
    fn from(time: GlideTime) -> f64 {
        time.0
    }
}

/// How pitch glide behaves between notes.
///
/// A [`PolySynth`](crate::PolySynth) built with `None` here has no glide state
/// at all and cannot gain one later.
#[derive(Debug, Clone, PartialEq)]
pub struct PortamentoConfig {
    /// When to glide. [`Off`](PortamentoMode::Off) by default, which makes the
    /// rest of these fields inert.
    pub mode: PortamentoMode,
    /// The shape of the glide: the [`Law`] of its progress over its duration.
    ///
    /// The pitch itself is interpolated logarithmically (the glide is even in
    /// musical interval, not in [`Hz`]) and every law ends exactly on the
    /// target; the law only reshapes how the progress fraction advances.
    /// [`Linear`](Law::Linear) is the constant rate of interval change;
    /// `PowerIn(2.0)` lingers near the old note and arrives abruptly;
    /// `PowerIn(0.5)` leaves the old note at once and eases into the new one.
    pub curve: Law,
    /// Glide duration, or the *base* duration when
    /// [`constant_time`](Self::constant_time) is false.
    pub time: GlideTime,
    /// Whether every glide takes the same time regardless of how far it travels.
    ///
    /// `true` (the default) is constant-*time*: an octave and a semitone both
    /// take [`time`](Self::time). `false` is constant-*rate*: the duration
    /// scales with the interval in octaves, floored at 10% of `time` so a tiny
    /// interval still glides audibly rather than snapping.
    pub constant_time: bool,
}

impl Default for PortamentoConfig {
    fn default() -> Self {
        Self {
            mode: PortamentoMode::Off,
            curve: Law::Linear,
            time: GlideTime::of(0.1),
            constant_time: true,
        }
    }
}

/// The shortest constant-rate glide, as a fraction of the base time: the
/// interval in octaves is floored here.
const MIN_INTERVAL_OCTAVES: f32 = 0.1;

/// Where a voice's pitch is: at rest, or on its way to a new note.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Glide {
    /// At rest. `current` is the pitch the last step computed, which an
    /// arrival leaves within rounding of `target`, the pitch last glided to.
    Idle { current: Hz, target: Hz },
    /// Between `from` and `to`, `progress` (below 1) of the way, advancing by
    /// `rate` per frame. `current` is the pitch at `progress`.
    Gliding {
        from: Hz,
        to: Hz,
        current: Hz,
        progress: f32,
        rate: f32,
    },
}

#[derive(Debug, Clone)]
pub struct Portamento {
    config: PortamentoConfig,
    glide: Glide,
    sample_rate: SampleRate,
}

impl Portamento {
    /// A glider at rest on `at`.
    pub fn new(config: PortamentoConfig, sample_rate: impl Into<SampleRate>, at: Hz) -> Self {
        Self {
            config,
            glide: Glide::Idle {
                current: at,
                target: at,
            },
            sample_rate: sample_rate.into(),
        }
    }

    pub fn set_target(&mut self, freq: impl Into<Hz>, is_legato: bool) {
        let freq = freq.into();
        let should_glide = match self.config.mode {
            PortamentoMode::Off => false,
            PortamentoMode::Always => true,
            PortamentoMode::LegatoOnly => is_legato,
        };

        if should_glide {
            let from = self.current();

            let glide_time = if self.config.constant_time {
                self.config.time.get()
            } else {
                // The interval in octaves; `Semitones::from_pitch_ratio`
                // guards the non-positive ratio a bare `log2` would turn into
                // a NaN glide time.
                let interval =
                    (Semitones::from_pitch_ratio(freq.get() / from.get()).get() / 12.0).abs();
                self.config.time.get() * f64::from(interval.max(MIN_INTERVAL_OCTAVES))
            };

            // A fractional glide length feeding a reciprocal, not a frame
            // count: multiply in f64 and narrow once. Narrowing the rate first
            // computes the step at f32 precision and drifts over a long glide.
            let glide_samples = (glide_time * self.sample_rate.get()) as f32;
            let rate = if glide_samples > 0.0 {
                1.0 / glide_samples
            } else {
                1.0
            };
            self.glide = Glide::Gliding {
                from,
                to: freq,
                current: from,
                progress: 0.0,
                rate,
            };
        } else {
            self.glide = Glide::Idle {
                current: freq,
                target: freq,
            };
        }
    }

    /// Starts a glide from `from` to `to` over `time`, whatever the config's mode:
    /// a glide the MIDI stream asked for (Portamento On/Off and Portamento
    /// Control, which `tutti-voicing` resolves to a source key and a time). A
    /// glide of no time, or between equal pitches, is a jump.
    pub fn begin(&mut self, from: Hz, to: Hz, time: Seconds) {
        let glide_samples = (time.get() * self.sample_rate.get()) as f32;
        self.glide = if from == to || glide_samples <= 0.0 {
            Glide::Idle {
                current: to,
                target: to,
            }
        } else {
            Glide::Gliding {
                from,
                to,
                current: from,
                progress: 0.0,
                rate: 1.0 / glide_samples,
            }
        };
    }

    /// The glide's pitch over the pitch it is gliding to: what a voice multiplies
    /// its own pitch (a per-note tuning, the bend) by. 1 once it has arrived.
    #[inline]
    pub fn ratio(&self) -> f32 {
        match self.glide {
            Glide::Idle { .. } => 1.0,
            Glide::Gliding { to, current, .. } => current.get() / to.get(),
        }
    }

    /// One frame of the glide: the reference [`advance`](Self::advance) is
    /// pinned against (`advance_is_that_many_ticks`).
    #[cfg(test)]
    pub fn tick(&mut self) -> Hz {
        match self.glide {
            Glide::Idle { target, .. } => target,
            Glide::Gliding {
                from,
                to,
                progress,
                rate,
                ..
            } => {
                let progress = (progress + rate).min(1.0);
                let current = glide_at(self.config.curve, from, to, progress);
                self.glide = Self::after_step(from, to, current, progress, rate);
                current
            }
        }
    }

    /// Advance the glide `frames` frames and return the pitch it reaches, as
    /// `frames` one-frame steps would (`tick`, the tests' reference).
    ///
    /// Steps the progress per frame exactly as `tick` does, but interpolates
    /// the pitch once, at the progress the last computing tick reached: the
    /// two logarithms and the exponential a tick evaluates are kept only by
    /// the tick after them, so the intermediate ones are dead work.
    pub fn advance(&mut self, frames: usize) -> Hz {
        let Glide::Gliding {
            from,
            to,
            mut current,
            mut progress,
            rate,
        } = self.glide
        else {
            return self.target();
        };
        let mut stepped = false;
        let mut arrived_early = false;
        for _ in 0..frames {
            if progress >= 1.0 {
                // `tick` returns the target here, and every later tick too.
                arrived_early = true;
                break;
            }
            progress = (progress + rate).min(1.0);
            stepped = true;
        }
        if stepped {
            current = glide_at(self.config.curve, from, to, progress);
        }
        self.glide = Self::after_step(from, to, current, progress, rate);
        if arrived_early { to } else { current }
    }

    /// The state after a step that left the glide `progress` of the way.
    fn after_step(from: Hz, to: Hz, current: Hz, progress: f32, rate: f32) -> Glide {
        if progress >= 1.0 {
            Glide::Idle {
                current,
                target: to,
            }
        } else {
            Glide::Gliding {
                from,
                to,
                current,
                progress,
                rate,
            }
        }
    }

    /// The pitch the glide is at.
    #[inline]
    pub fn current(&self) -> Hz {
        match self.glide {
            Glide::Idle { current, .. } | Glide::Gliding { current, .. } => current,
        }
    }

    /// The pitch the glide is on its way to, or last arrived at.
    #[inline]
    fn target(&self) -> Hz {
        match self.glide {
            Glide::Idle { target, .. } => target,
            Glide::Gliding { to, .. } => to,
        }
    }

    #[inline]
    pub fn is_gliding(&self) -> bool {
        matches!(self.glide, Glide::Gliding { .. })
    }

    pub fn reset(&mut self, freq: impl Into<Hz>) {
        let freq = freq.into();
        self.glide = Glide::Idle {
            current: freq,
            target: freq,
        };
    }

    pub fn set_sample_rate(&mut self, sample_rate: impl Into<SampleRate>) {
        self.sample_rate = sample_rate.into();
    }
}

/// The pitch at `progress` along the glide from `from` to `to`, through
/// `curve`.
#[inline]
fn glide_at(curve: Law, from: Hz, to: Hz, progress: f32) -> Hz {
    let t = curve.eval(progress);
    let log_from = from.get().ln();
    let log_to = to.get().ln();
    Hz((log_from + (log_to - log_from) * t).exp())
}

#[cfg(test)]
mod tests;
