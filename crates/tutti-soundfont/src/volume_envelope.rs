// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use crate::envelope_stage::EnvelopeStage;
use crate::render::Render;
use crate::soundfont_math::SoundFontMath;

#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) struct VolumeEnvelope {
    sample_rate: i32,

    attack_slope: f64,
    decay_slope: f64,
    release_slope: f64,

    attack_start_time: f64,
    hold_start_time: f64,
    decay_start_time: f64,
    release_start_time: f64,

    sustain_level: f32,
    release_level: f32,

    processed_sample_count: usize,
    stage: EnvelopeStage,
    value: f32,
}

impl VolumeEnvelope {
    pub(crate) fn new(settings: &Render) -> Self {
        Self {
            sample_rate: settings.hz(),
            attack_slope: 0_f64,
            decay_slope: 0_f64,
            release_slope: 0_f64,
            attack_start_time: 0_f64,
            hold_start_time: 0_f64,
            decay_start_time: 0_f64,
            release_start_time: 0_f64,
            sustain_level: 0_f32,
            release_level: 0_f32,
            processed_sample_count: 0,
            stage: EnvelopeStage::Delay,
            value: 0_f32,
        }
    }

    pub(crate) fn start(
        &mut self,
        delay: f32,
        attack: f32,
        hold: f32,
        decay: f32,
        sustain: f32,
        release: f32,
    ) {
        self.attack_slope = 1_f64 / attack as f64;
        self.decay_slope = -SoundFontMath::HUNDRED_DB_NEPERS / decay as f64;
        self.release_slope = -SoundFontMath::HUNDRED_DB_NEPERS / release as f64;

        self.attack_start_time = delay as f64;
        self.hold_start_time = self.attack_start_time + attack as f64;
        self.decay_start_time = self.hold_start_time + hold as f64;
        self.release_start_time = 0_f64;

        self.sustain_level = SoundFontMath::clamp(sustain, 0_f32, 1_f32);
        self.release_level = 0_f32;

        self.processed_sample_count = 0;
        self.stage = EnvelopeStage::Delay;
        self.value = 0_f32;

        self.process(0);
    }

    pub(crate) fn release(&mut self) {
        self.stage = EnvelopeStage::Release;
        self.release_start_time = self.processed_sample_count as f64 / self.sample_rate as f64;
        self.release_level = self.value;
    }

    /// Releases within `seconds` at the longest: the release already running,
    /// or the one to come, is made at least as steep as one that falls 100 dB
    /// in that time (the definition of a release time, generator 38).
    /// Called on a voice that is releasing, it continues from the level it
    /// has and never slows the fall.
    pub(crate) fn release_within(&mut self, seconds: f32) {
        self.release_slope = self
            .release_slope
            .min(-SoundFontMath::HUNDRED_DB_NEPERS / seconds as f64);
        if self.stage == EnvelopeStage::Release {
            self.release_start_time = self.processed_sample_count as f64 / self.sample_rate as f64;
            self.release_level = self.value;
        }
    }

    pub(crate) fn process(&mut self, sample_count: usize) -> bool {
        self.processed_sample_count += sample_count;

        let current_time = self.processed_sample_count as f64 / self.sample_rate as f64;

        while self.stage <= EnvelopeStage::Hold {
            let end_time = match self.stage {
                EnvelopeStage::Delay => self.attack_start_time,
                EnvelopeStage::Attack => self.hold_start_time,
                EnvelopeStage::Hold => self.decay_start_time,
                _ => unreachable!(),
            };

            if current_time < end_time {
                break;
            }
            self.stage = self.stage.next();
        }

        match self.stage {
            EnvelopeStage::Delay => {
                self.value = 0_f32;
                true
            }
            EnvelopeStage::Attack => {
                self.value = (self.attack_slope * (current_time - self.attack_start_time)) as f32;
                true
            }
            EnvelopeStage::Hold => {
                self.value = 1_f32;
                true
            }
            EnvelopeStage::Decay => {
                self.value = SoundFontMath::max(
                    SoundFontMath::exp_cutoff(
                        self.decay_slope * (current_time - self.decay_start_time),
                    ) as f32,
                    self.sustain_level,
                );
                self.value > SoundFontMath::NON_AUDIBLE
            }
            EnvelopeStage::Release => {
                self.value = (self.release_level as f64
                    * SoundFontMath::exp_cutoff(
                        self.release_slope * (current_time - self.release_start_time),
                    )) as f32;
                self.value > SoundFontMath::NON_AUDIBLE
            }
        }
    }

    pub(crate) fn value(&self) -> f32 {
        self.value
    }
}

/// SF2 2.04 section 8.1.2: the volume envelope's decay (generator 36) and
/// release (generator 38) times are the time to fall 100 dB, at a constant dB
/// per time unit. So half the time on, the level is 50 dB down from where the
/// phase began, whatever that level was.
#[cfg(test)]
mod tests {
    use tutti_types::SampleRate;

    use super::VolumeEnvelope;
    use crate::render::Render;

    const RATE: f64 = 48_000.0;

    fn envelope(decay: f32, sustain: f32, release: f32) -> VolumeEnvelope {
        let mut envelope = VolumeEnvelope::new(&Render::new(SampleRate(RATE)));
        // No delay, a 1 ms attack, no hold.
        envelope.start(0.0, 0.001, 0.0, decay, sustain, release);
        envelope
    }

    fn db(level: f32) -> f32 {
        20.0 * level.log10()
    }

    /// Release: from the level at key-off, `fraction` of the release time on is
    /// `fraction` of 100 dB down.
    #[test]
    fn a_release_falls_100_db_in_its_time() {
        // Key-off from full level, and from the sustain at -6 dB: the fall is
        // the same 100 dB a release time, from wherever it starts.
        for (sustain_db, fractions) in [(0.0_f32, [0.1, 0.3, 0.5]), (6.0, [0.1, 0.3, 0.5])] {
            let sustain = 10f32.powf(-sustain_db / 20.0);
            let mut envelope = envelope(1.0, sustain, 2.0);
            envelope.process(RATE as usize / 2);
            let before = db(envelope.value());
            envelope.release();
            let mut elapsed = 0.0_f32;
            for fraction in fractions {
                let to = fraction * 2.0;
                envelope.process(((to - elapsed) * RATE as f32) as usize);
                elapsed = to;
                let fell = before - db(envelope.value());
                assert!(
                    (fell - 100.0 * fraction).abs() < 0.2,
                    "{fraction} of the release is {fell} dB down, not {}",
                    100.0 * fraction
                );
            }
        }
    }

    /// Decay: the same figure, to a sustain level of 0 (a 100 dB sustain is
    /// below the voice's end).
    #[test]
    fn a_decay_falls_100_db_in_its_time() {
        let mut envelope = envelope(2.0, 0.0, 1.0);
        let attack = (0.001 * RATE) as usize;
        envelope.process(attack);
        for fraction in [0.1_f32, 0.3, 0.5] {
            let mut again = envelope.clone();
            again.process((fraction * 2.0 * RATE as f32) as usize);
            let fell = -db(again.value());
            assert!(
                (fell - 100.0 * fraction).abs() < 0.2,
                "{fraction} of the decay is {fell} dB down"
            );
        }
    }

    /// The quick release of an exclusive class falls 100 dB in its time too, so
    /// it is -50 dB after half of it.
    #[test]
    fn a_quick_release_falls_100_db_in_its_time() {
        let mut envelope = envelope(10.0, 1.0, 5.0);
        envelope.process(RATE as usize / 100);
        envelope.release_within(0.01);
        envelope.release();
        envelope.process(RATE as usize / 200);
        assert!((-db(envelope.value()) - 50.0).abs() < 0.2);
    }
}
