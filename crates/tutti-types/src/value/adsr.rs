//! The four stages of an ADSR envelope.

use super::{Amplitude, Seconds};

/// An ADSR's stages: a synth's amplitude envelope, and a note envelope as a
/// modulation source.
///
/// Every stage is a straight line. A note-off releases from whatever level the
/// envelope has reached, taking `release` to reach silence; a note-on that
/// retriggers a sounding voice attacks from its current level.
///
/// The sustain is an [`Amplitude`], the held level as a fraction of the peak
/// (1.0): the one `Amplitude` among three [`Seconds`], so the types keep a
/// positional call from transposing a level and a time. A consumer decides what
/// it does with a value outside `[0, 1]` (the modulation compiler refuses it).
///
/// Equality is bitwise, so it is [`Eq`].
#[derive(Clone, Copy, Debug)]
pub struct Adsr {
    /// Note-on to the peak. `0.0` is an instant click; a few milliseconds is a
    /// percussive edge; a second or more is a pad swell.
    pub attack: Seconds,
    /// The peak down to `sustain`. Irrelevant when `sustain` is unity.
    pub decay: Seconds,
    /// The held level, `0..=1` of the peak. `1.0` holds at full (an organ);
    /// `0.0` makes the note die away after decay however long the key is held
    /// (a pluck).
    pub sustain: Amplitude,
    /// Note-off to zero. A synth reports it as its `tail()`, so it decides how
    /// far past the last note-off an offline render must run.
    pub release: Seconds,
}

impl Adsr {
    /// An envelope from attack, decay, sustain and release.
    pub fn new(
        attack: impl Into<Seconds>,
        decay: impl Into<Seconds>,
        sustain: impl Into<Amplitude>,
        release: impl Into<Seconds>,
    ) -> Self {
        Self {
            attack: attack.into(),
            decay: decay.into(),
            sustain: sustain.into(),
            release: release.into(),
        }
    }

    /// A gated organ envelope: 1 ms attack, no decay, full sustain, 10 ms
    /// release. The note is at full level for exactly as long as the key is
    /// held, and the short release only takes the edge off the note-off click.
    pub fn organ() -> Self {
        Self::new(Seconds(0.001), Seconds(0.0), Amplitude(1.0), Seconds(0.01))
    }

    /// A plucked envelope: 1 ms attack, 300 ms decay to **zero** sustain,
    /// 100 ms release. Zero sustain is what makes the note die on its own.
    pub fn pluck() -> Self {
        Self::new(Seconds(0.001), Seconds(0.3), Amplitude(0.0), Seconds(0.1))
    }

    /// A pad envelope: 500 ms swell, 200 ms decay to 0.8, 1 s release. The long
    /// release means voices keep ringing well past note-off.
    pub fn pad() -> Self {
        Self::new(Seconds(0.5), Seconds(0.2), Amplitude(0.8), Seconds(1.0))
    }

    fn bits(&self) -> ([u64; 3], u32) {
        (
            [
                self.attack.get().to_bits(),
                self.decay.get().to_bits(),
                self.release.get().to_bits(),
            ],
            self.sustain.get().to_bits(),
        )
    }
}

impl Default for Adsr {
    /// 10 ms attack, 100 ms decay, 0.7 sustain, 200 ms release.
    fn default() -> Self {
        Self::new(Seconds(0.01), Seconds(0.1), Amplitude(0.7), Seconds(0.2))
    }
}

impl PartialEq for Adsr {
    fn eq(&self, other: &Self) -> bool {
        self.bits() == other.bits()
    }
}

impl Eq for Adsr {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Equality is bitwise: `-0.0` differs from `0.0`, and a NaN stage equals
    /// itself.
    ///
    /// Mutation (run): comparing the floats with `==` -> fails.
    #[test]
    fn equality_is_bitwise() {
        let a = Adsr::new(Seconds(0.0), Seconds(0.1), Amplitude(0.5), Seconds(0.2));
        let neg = Adsr::new(Seconds(-0.0), Seconds(0.1), Amplitude(0.5), Seconds(0.2));
        assert_ne!(a, neg);
        let nan = Adsr::new(
            Seconds(f64::NAN),
            Seconds(0.1),
            Amplitude(0.5),
            Seconds(0.2),
        );
        assert_eq!(nan, nan);
    }

    /// The presets and the default keep their stages.
    ///
    /// Mutation (run): `pluck` with sustain 1.0 -> fails.
    #[test]
    fn presets_hold_their_stages() {
        assert_eq!(Adsr::organ().sustain, Amplitude(1.0));
        assert_eq!(Adsr::pluck().sustain, Amplitude(0.0));
        assert_eq!(Adsr::pad().release, Seconds(1.0));
        assert_eq!(Adsr::default().release, Seconds(0.2));
    }
}
