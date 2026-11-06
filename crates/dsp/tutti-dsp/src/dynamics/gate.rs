//! The gate's per-sample gain: the open/closed decision, the
//! attack/hold/release follower, and the range ramp.

use super::curves::{
    amplitude_to_db, compute_gate_gain, db_to_amplitude, gate_gain_at, ramp_db, ramp_is_at_end,
};
use super::envelope::GateEnvelopeFollower;
use tutti_types::{Db, SampleRate, Seconds};

/// The controls one block of a gate runs on, read once at the top of the
/// block.
///
/// The threshold only decides open/closed, and the attack/hold/release
/// follower already turns a changed decision into a ramp, so it is held for
/// the block and converted to a linear level once. The range multiplies the
/// output directly (the closed floor), so a step would click; it ramps
/// linearly from the previous block's value instead.
#[derive(Clone, Copy, Debug)]
pub struct GateBlock {
    /// The block's threshold as a linear level: the detector is compared in
    /// the linear domain, so no `log10` runs per sample.
    threshold_linear: f32,
    /// Whether a silent detector (pinned at [`Db::FLOOR`]) is at or above the
    /// threshold. Only a threshold at or under the floor opens on silence.
    silence_opens: bool,
    range_from: Db,
    range_to: Db,
    /// `range_to` as a linear gain, converted once for the block.
    range_to_linear: f32,
}

impl GateBlock {
    /// A block at `threshold`, with the range ramping from `range_from` to
    /// `range_to` (both at most 0 dB: the closed gain).
    #[must_use]
    pub fn new(threshold: Db, range_from: Db, range_to: Db) -> Self {
        Self {
            threshold_linear: db_to_amplitude(threshold).get(),
            silence_opens: Db::FLOOR >= threshold,
            range_from,
            range_to,
            range_to_linear: db_to_amplitude(range_to).get(),
        }
    }
}

/// A gate's state: the sidechain level last seen and the follower with its
/// hold. Every control arrives per call as plain values.
#[derive(Clone, Debug)]
pub struct GateKernel {
    envelope: f32,
    follower: GateEnvelopeFollower,
}

impl GateKernel {
    /// A kernel with `attack`, `hold` and `release` times at `sample_rate`.
    #[must_use]
    pub fn new(
        attack: impl Into<Seconds>,
        hold: impl Into<Seconds>,
        release: impl Into<Seconds>,
        sample_rate: impl Into<SampleRate>,
    ) -> Self {
        Self {
            envelope: 0.0,
            follower: GateEnvelopeFollower::new(attack, hold, release, sample_rate),
        }
    }

    /// Whether the gate is more than half open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.follower.value() > 0.5
    }

    /// The gate's open fraction, `0` closed to `1` open.
    #[must_use]
    pub fn gate_level(&self) -> f32 {
        self.follower.value()
    }

    /// Clears the envelope and the follower.
    pub fn reset(&mut self) {
        self.envelope = 0.0;
        self.follower.reset();
    }

    /// Retunes the follower for `sample_rate` at the given times.
    pub fn set_sample_rate(
        &mut self,
        sample_rate: impl Into<SampleRate>,
        attack: Seconds,
        hold: Seconds,
        release: Seconds,
    ) {
        self.follower
            .set_sample_rate(sample_rate, attack, hold, release);
    }

    /// Re-derives the follower's coefficients and hold if the times moved.
    #[inline]
    pub fn update_coefficients(&mut self, attack: Seconds, hold: Seconds, release: Seconds) {
        self.follower.update_coefficients(attack, hold, release);
    }

    /// Gate gain (linear) for frame `i` of an `n`-frame block, given the
    /// sidechain level and an optional per-sample threshold override (dB).
    /// `None` uses the block's threshold (the fast path); `Some(db)` overrides
    /// it, the audio-rate modulation path. A non-finite override falls back to
    /// the block's threshold.
    #[inline]
    pub fn gain(
        &mut self,
        block: &GateBlock,
        i: usize,
        n: usize,
        sc_level: f32,
        threshold_override: Option<Db>,
    ) -> f32 {
        // The block's threshold compares in the linear domain. A per-sample
        // override changes every frame, so converting it costs what the
        // detector's `log10` does; it stays in dB.
        let open = match threshold_override.filter(|t| t.get().is_finite()) {
            Some(t) => amplitude_to_db(sc_level) >= t,
            // Exact equality is unreachable: `10^(t/20)` rounds, so no level a
            // test can name sits on the threshold, and `>=` versus `>` cannot
            // be pinned by a test.
            None => sc_level >= block.threshold_linear || (sc_level <= 0.0 && block.silence_opens),
        };
        self.envelope = sc_level;
        self.follower.step(open);
        // At the end of the ramp (a held range is always there) the range is
        // the block's, converted once.
        if ramp_is_at_end(block.range_from, block.range_to, i, n) {
            gate_gain_at(self.follower.value(), block.range_to_linear).get()
        } else {
            let range = ramp_db(block.range_from, block.range_to, i, n);
            compute_gate_gain(self.follower.value(), range).get()
        }
    }
}
