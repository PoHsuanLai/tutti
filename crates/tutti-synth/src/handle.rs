//! Fluent handle for creating MIDI-responsive polyphonic synthesizers.

use tutti_core::{Cents, Hz, Linear, Ratio, SampleRate, Seconds, Semitones};

use crate::{
    AllocationStrategy, EnvelopeConfig, FilterType, OscillatorType, PolySynth, PortamentoConfig,
    PortamentoCurve, PortamentoMode, SvfMode, SynthBuilder, Tuning, UnisonConfig,
};

/// Generate fluent setter methods that delegate to `self.builder`.
///
/// Each arm takes the form:
///   `[doc_string] method_name(params) => builder_method(args);`
///
/// The macro generates `pub fn method_name(mut self, params) -> Self`
/// which calls `self.builder = self.builder.builder_method(args)`.
macro_rules! synth_setter {
    ($(
        $(#[doc = $doc:expr])*
        $method:ident( $($param:ident : $ty:ty),* ) => $builder_method:ident( $($arg:tt)* );
    )*) => {
        $(
            $(#[doc = $doc])*
            pub fn $method(mut self, $($param: $ty),*) -> Self {
                self.builder = self.builder.$builder_method($($arg)*);
                self
            }
        )*
    };
}

/// Fluent builder for creating MIDI-responsive polyphonic synthesizers.
///
/// The built synth always owns its own MIDI inbox; grab the producer
/// handle via [`PolySynth::midi_sender`] after [`Self::build`].
pub struct SynthHandle {
    builder: SynthBuilder,
}

impl SynthHandle {
    pub fn new(sample_rate: impl Into<SampleRate>) -> Self {
        Self {
            builder: SynthBuilder::new(sample_rate.into().get()),
        }
    }

    synth_setter! {
        sine() => oscillator(OscillatorType::Sine);
        saw() => oscillator(OscillatorType::Saw);
        /// Pulse width ranges from 0.0 to 1.0.
        square(pulse_width: f32) => oscillator(OscillatorType::Square { pulse_width });
        triangle() => oscillator(OscillatorType::Triangle);
        noise() => oscillator(OscillatorType::Noise);
    }

    synth_setter! {
        poly(voices: usize) => poly(voices);
        /// Single voice, retriggers on each note.
        mono() => mono();
        /// Single voice, glides between overlapping notes.
        legato() => legato();
    }

    /// Moog ladder filter. Cutoff in Hz, resonance 0.0-1.0.
    pub fn filter_moog(mut self, cutoff: impl Into<Hz>, resonance: impl Into<Linear>) -> Self {
        self.builder = self.builder.filter(FilterType::Moog {
            cutoff: cutoff.into().get(),
            resonance: resonance.into().get(),
        });
        self
    }

    /// SVF lowpass filter. Cutoff in Hz, Q as a dimensionless ratio.
    pub fn filter_lowpass(mut self, cutoff: impl Into<Hz>, q: impl Into<Ratio>) -> Self {
        self.builder = self.builder.filter(FilterType::Svf {
            cutoff: cutoff.into().get(),
            q: q.into().get(),
            mode: SvfMode::Lowpass,
        });
        self
    }

    /// SVF highpass filter. Cutoff in Hz, Q as a dimensionless ratio.
    pub fn filter_highpass(mut self, cutoff: impl Into<Hz>, q: impl Into<Ratio>) -> Self {
        self.builder = self.builder.filter(FilterType::Svf {
            cutoff: cutoff.into().get(),
            q: q.into().get(),
            mode: SvfMode::Highpass,
        });
        self
    }

    /// SVF bandpass filter. Cutoff in Hz, Q as a dimensionless ratio.
    pub fn filter_bandpass(mut self, cutoff: impl Into<Hz>, q: impl Into<Ratio>) -> Self {
        self.builder = self.builder.filter(FilterType::Svf {
            cutoff: cutoff.into().get(),
            q: q.into().get(),
            mode: SvfMode::Bandpass,
        });
        self
    }

    /// SVF notch filter. Cutoff in Hz, Q as a dimensionless ratio.
    pub fn filter_notch(mut self, cutoff: impl Into<Hz>, q: impl Into<Ratio>) -> Self {
        self.builder = self.builder.filter(FilterType::Svf {
            cutoff: cutoff.into().get(),
            q: q.into().get(),
            mode: SvfMode::Notch,
        });
        self
    }

    synth_setter! {
        no_filter() => filter(FilterType::None);
    }

    /// Attack/decay/release in seconds, sustain as a 0.0-1.0 level.
    pub fn adsr(
        mut self,
        attack: impl Into<Seconds>,
        decay: impl Into<Seconds>,
        sustain: impl Into<Linear>,
        release: impl Into<Seconds>,
    ) -> Self {
        self.builder = self.builder.envelope(
            attack.into().get(),
            decay.into().get(),
            sustain.into().get(),
            release.into().get(),
        );
        self
    }

    synth_setter! {
        envelope_organ() => envelope_config(EnvelopeConfig::organ());
        envelope_pluck() => envelope_config(EnvelopeConfig::pluck());
        envelope_pad() => envelope_config(EnvelopeConfig::pad());
    }

    synth_setter! {
        /// When all voices are in use, steal the voice that started earliest (default).
        voice_steal_oldest() => voice_stealing(AllocationStrategy::Oldest);
        /// When all voices are in use, steal the voice with the lowest envelope level.
        voice_steal_quietest() => voice_stealing(AllocationStrategy::Quietest);
        /// Steal the highest pitched voice. Good for bass-heavy sounds.
        voice_steal_highest() => voice_stealing(AllocationStrategy::HighestNote);
        /// Steal the lowest pitched voice. Good for lead sounds.
        voice_steal_lowest() => voice_stealing(AllocationStrategy::LowestNote);
        /// Steal the most recently triggered voice.
        voice_steal_newest() => voice_stealing(AllocationStrategy::Newest);
        /// When all voices are in use, new notes are ignored.
        no_voice_steal() => voice_stealing(AllocationStrategy::NoSteal);
    }

    synth_setter! {
        tuning_equal() => tuning(Tuning::equal_temperament());
        /// Pure intervals based on simple frequency ratios.
        /// Sounds more "in tune" for single keys but doesn't transpose well.
        tuning_just() => tuning(Tuning::just_intonation());
        /// Based on pure perfect fifths. Common in medieval music.
        tuning_pythagorean() => tuning(Tuning::pythagorean());
        /// Compromise tuning common in Renaissance/Baroque music.
        tuning_meantone() => tuning(Tuning::meantone());
    }

    /// Each element is a cent offset from the root. Scale repeats every octave (1200 cents).
    pub fn tuning_from_cents<I>(mut self, cents: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<Cents>,
    {
        let raw: Vec<f32> = cents.into_iter().map(|c| c.into().get()).collect();
        self.builder = self.builder.tuning(Tuning::from_cents(&raw));
        self
    }

    /// Each ratio is relative to the root.
    pub fn tuning_from_ratios<I>(mut self, ratios: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<Ratio>,
    {
        let raw: Vec<f32> = ratios.into_iter().map(|r| r.into().get()).collect();
        self.builder = self.builder.tuning(Tuning::from_ratios(&raw));
        self
    }

    synth_setter! {
        /// Enable MPE mode: per-voice pitch bend, pressure, and slide routing.
        mpe(enabled: bool) => mpe(enabled);
    }

    /// MPE per-note pitch bend range (default: 48 semitones).
    pub fn mpe_pitch_bend_range(mut self, semitones: impl Into<Semitones>) -> Self {
        self.builder = self.builder.mpe_pitch_bend_range(semitones.into().get());
        self
    }

    /// Global pitch bend range (default: 2 semitones).
    pub fn pitch_bend_range(mut self, semitones: impl Into<Semitones>) -> Self {
        self.builder = self.builder.pitch_bend_range(semitones.into().get());
        self
    }

    /// At depth 1.0, mod wheel fully up doubles the cutoff frequency.
    pub fn mod_wheel_to_filter(mut self, depth: impl Into<Linear>) -> Self {
        self.builder = self.builder.mod_wheel_to_filter(depth.into().get());
        self
    }

    /// At depth 1.0, velocity 0 halves cutoff and velocity 127 leaves it unchanged.
    pub fn velocity_to_filter(mut self, depth: impl Into<Linear>) -> Self {
        self.builder = self.builder.velocity_to_filter(depth.into().get());
        self
    }

    /// Rate in Hz, depth 0.0-1.0 (at 1.0 sweeps cutoff +/-50%).
    pub fn lfo_to_filter(mut self, rate: impl Into<Hz>, depth: impl Into<Linear>) -> Self {
        self.builder = self
            .builder
            .lfo_to_filter(rate.into().get(), depth.into().get());
        self
    }

    /// Voices 1-16, detune_cents is total spread (e.g., 15.0 for +/-7.5 cents).
    /// Stereo spread: 0.0 = mono, 1.0 = full stereo.
    pub fn unison(
        mut self,
        voices: u8,
        detune_cents: impl Into<Cents>,
        stereo_spread: impl Into<Linear>,
    ) -> Self {
        self.builder = self.builder.unison(UnisonConfig {
            voice_count: voices,
            detune_cents: detune_cents.into(),
            stereo_spread: stereo_spread.into().get(),
            phase_randomize: true,
        });
        self
    }

    fn with_portamento(
        mut self,
        time: Seconds,
        mode: PortamentoMode,
        curve: PortamentoCurve,
    ) -> Self {
        self.builder = self.builder.portamento(PortamentoConfig {
            mode,
            curve,
            time,
            constant_time: true,
        });
        self
    }

    /// Glide time in seconds, applies to all notes.
    pub fn portamento(self, time: impl Into<Seconds>) -> Self {
        self.with_portamento(time.into(), PortamentoMode::Always, PortamentoCurve::Linear)
    }
    /// Only glides when notes overlap (legato playing).
    pub fn portamento_legato(self, time: impl Into<Seconds>) -> Self {
        self.with_portamento(
            time.into(),
            PortamentoMode::LegatoOnly,
            PortamentoCurve::Linear,
        )
    }
    /// Starts slow, speeds up toward target pitch.
    pub fn portamento_exp(self, time: impl Into<Seconds>) -> Self {
        self.with_portamento(
            time.into(),
            PortamentoMode::Always,
            PortamentoCurve::Exponential,
        )
    }
    /// Starts fast, slows down toward target pitch.
    pub fn portamento_log(self, time: impl Into<Seconds>) -> Self {
        self.with_portamento(
            time.into(),
            PortamentoMode::Always,
            PortamentoCurve::Logarithmic,
        )
    }

    pub fn build(self) -> crate::Result<PolySynth> {
        self.builder.build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::convert::{
        midi1_cc_to_midi2, midi1_pitch_bend_to_midi2, midi1_velocity_to_midi2,
    };
    use tutti_midi_types::ump::MidiEvent;

    fn ev_note_on(channel: u8, note: u8, vel: u8) -> MidiEvent {
        MidiEvent::note_on(0, channel, note, midi1_velocity_to_midi2(vel))
    }
    fn ev_note_off(channel: u8, note: u8) -> MidiEvent {
        MidiEvent::note_off(0, channel, note, 0)
    }
    fn ev_cc(channel: u8, cc_num: u8, value: u8) -> MidiEvent {
        MidiEvent::cc(0, channel, cc_num, midi1_cc_to_midi2(value))
    }
    fn ev_bend(channel: u8, bend14: u16) -> MidiEvent {
        MidiEvent::pitch_bend(0, channel, midi1_pitch_bend_to_midi2(bend14))
    }
    use tutti_core::AudioUnit;

    /// Helper to calculate RMS of stereo samples
    fn rms_stereo(samples: &[(f32, f32)]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_sq: f32 = samples.iter().map(|(l, r)| l * l + r * r).sum();
        (sum_sq / (samples.len() * 2) as f32).sqrt()
    }

    /// Render N samples from a synth, returning stereo pairs
    fn render_samples(synth: &mut PolySynth, count: usize) -> Vec<(f32, f32)> {
        let mut samples = Vec::with_capacity(count);
        for _ in 0..count {
            let mut output = [0.0f32; 2];
            synth.tick(&[], &mut output);
            samples.push((output[0], output[1]));
        }
        samples
    }

    #[test]
    fn test_synth_owns_midi_inbox() {
        let mut synth = SynthHandle::new(44100.0).saw().poly(4).build().unwrap();

        // Queue a MIDI note through the synth's own sender
        let note_on = ev_note_on(0, 60, 100);
        synth.midi_sender().queue(&[note_on]);

        // Process to trigger the note
        let mut output = [0.0f32; 2];
        synth.tick(&[], &mut output);

        // Synth should have an active voice now
        assert_eq!(synth.active_voice_count(), 1);
    }

    #[test]
    fn test_synth_produces_audio_on_note() {
        let mut synth = SynthHandle::new(44100.0)
            .saw()
            .poly(4)
            .adsr(0.001, 0.1, 0.8, 0.1) // Fast attack
            .build()
            .unwrap();

        // Queue note on
        let note_on = ev_note_on(0, 60, 100);
        synth.midi_sender().queue(&[note_on]);

        // Render samples - should produce audio
        let samples = render_samples(&mut synth, 1000);
        let rms = rms_stereo(&samples);

        assert!(
            rms > 0.01,
            "Synth should produce audio on note, RMS={}",
            rms
        );
    }

    #[test]
    fn test_note_off_silences_synth() {
        let mut synth = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .adsr(0.001, 0.05, 0.8, 0.05) // Fast release
            .build()
            .unwrap();

        // Note on
        let note_on = ev_note_on(0, 60, 100);
        synth.midi_sender().queue(&[note_on]);
        let samples_playing = render_samples(&mut synth, 500);
        let rms_playing = rms_stereo(&samples_playing);
        assert!(rms_playing > 0.01, "Should produce audio while note is on");

        // Note off
        let note_off = ev_note_off(0, 60);
        synth.midi_sender().queue(&[note_off]);

        // Wait for release to complete
        let _ = render_samples(&mut synth, 5000);
        let samples_after = render_samples(&mut synth, 500);
        let rms_after = rms_stereo(&samples_after);

        assert!(
            rms_after < rms_playing * 0.1,
            "After note off, RMS={} should be much less than playing RMS={}",
            rms_after,
            rms_playing
        );
    }

    #[test]
    fn test_velocity_affects_volume() {
        // Low velocity note
        let mut synth_soft = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .adsr(0.001, 0.1, 0.8, 0.1)
            .build()
            .unwrap();
        let note_soft = ev_note_on(0, 60, 30); // velocity 30
        synth_soft.midi_sender().queue(&[note_soft]);
        let samples_soft = render_samples(&mut synth_soft, 1000);
        let rms_soft = rms_stereo(&samples_soft);

        // High velocity note
        let mut synth_loud = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .adsr(0.001, 0.1, 0.8, 0.1)
            .build()
            .unwrap();
        let note_loud = ev_note_on(0, 60, 127); // velocity 127
        synth_loud.midi_sender().queue(&[note_loud]);
        let samples_loud = render_samples(&mut synth_loud, 1000);
        let rms_loud = rms_stereo(&samples_loud);

        // Loud note should be louder than soft note
        assert!(
            rms_loud > rms_soft * 2.0,
            "High velocity (127) RMS={} should be significantly louder than low velocity (30) RMS={}",
            rms_loud,
            rms_soft
        );
    }

    #[test]
    fn test_unison_increases_stereo_width() {
        // Without unison - should be centered (equal L/R)
        let mut synth_mono = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .adsr(0.001, 0.1, 0.8, 0.1)
            .build()
            .unwrap();
        let note = ev_note_on(0, 60, 100);
        synth_mono.midi_sender().queue(&[note]);
        let samples_mono = render_samples(&mut synth_mono, 500);

        // With unison and full stereo spread
        let mut synth_unison = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .adsr(0.001, 0.1, 0.8, 0.1)
            .unison(3, 15.0, 1.0) // Full stereo spread
            .build()
            .unwrap();
        synth_unison.midi_sender().queue(&[note]);
        let samples_unison = render_samples(&mut synth_unison, 500);

        // Calculate L/R difference (stereo width indicator)
        let diff_mono: f32 = samples_mono.iter().map(|(l, r)| (l - r).abs()).sum::<f32>()
            / samples_mono.len() as f32;
        let diff_unison: f32 = samples_unison
            .iter()
            .map(|(l, r)| (l - r).abs())
            .sum::<f32>()
            / samples_unison.len() as f32;

        assert!(
            diff_unison > diff_mono,
            "Unison with stereo spread should have more L/R difference ({}) than mono ({})",
            diff_unison,
            diff_mono
        );
    }

    #[test]
    fn test_tuning_affects_pitch() {
        // Equal temperament A4 = 440 Hz
        let mut synth_equal = SynthHandle::new(44100.0)
            .sine()
            .poly(1)
            .adsr(0.001, 0.1, 1.0, 0.1)
            .tuning_equal()
            .build()
            .unwrap();

        // Just intonation - should produce different frequencies for some notes
        let mut synth_just = SynthHandle::new(44100.0)
            .sine()
            .poly(1)
            .adsr(0.001, 0.1, 1.0, 0.1)
            .tuning_just()
            .build()
            .unwrap();

        // Play E4 (major third above C4)
        // In equal temperament: 329.63 Hz
        // In just intonation: should be slightly different (5/4 ratio from C)
        let note = ev_note_on(0, 64, 100); // E4
        synth_equal.midi_sender().queue(&[note]);
        synth_just.midi_sender().queue(&[note]);

        let samples_equal = render_samples(&mut synth_equal, 1000);
        let samples_just = render_samples(&mut synth_just, 1000);

        // Both should produce audio
        assert!(rms_stereo(&samples_equal) > 0.01);
        assert!(rms_stereo(&samples_just) > 0.01);

        // The samples should be different (different tuning = different phase accumulation)
        let diff: f32 = samples_equal
            .iter()
            .zip(samples_just.iter())
            .map(|((l1, _), (l2, _))| (l1 - l2).abs())
            .sum::<f32>()
            / samples_equal.len() as f32;

        assert!(
            diff > 0.001,
            "Equal and just intonation should produce different waveforms, diff={}",
            diff
        );
    }

    // =========================================================================
    // Frequency estimation helpers
    // =========================================================================

    /// Count zero crossings in the left channel of stereo pairs.
    fn zero_crossings_left(samples: &[(f32, f32)]) -> usize {
        samples
            .windows(2)
            .filter(|w| (w[0].0 >= 0.0) != (w[1].0 >= 0.0))
            .count()
    }

    /// Estimate frequency from zero crossings (left channel).
    fn estimate_frequency(samples: &[(f32, f32)], sample_rate: impl Into<SampleRate>) -> Hz {
        let sample_rate = sample_rate.into().get();
        let crossings = zero_crossings_left(samples);
        let duration = samples.len() as f64 / sample_rate;
        Hz(((crossings as f64 / 2.0) / duration) as f32)
    }

    // =========================================================================
    // Pitch Bend Tests
    // =========================================================================

    #[test]
    fn test_pitch_bend_changes_frequency() {
        let mut synth = SynthHandle::new(44100.0)
            .sine()
            .poly(1)
            .adsr(0.001, 0.1, 1.0, 0.1)
            .build()
            .unwrap();

        // Play A4
        let note_on = ev_note_on(0, 69, 100);
        synth.midi_sender().queue(&[note_on]);

        // Render baseline (unbent)
        let samples_base = render_samples(&mut synth, 4000);
        let freq_base = estimate_frequency(&samples_base, 44100.0).get();

        // Bend full up (16383 = max)
        let bend_up = ev_bend(0, 16383);
        synth.midi_sender().queue(&[bend_up]);

        // Render bent
        let samples_bent = render_samples(&mut synth, 4000);
        let freq_bent = estimate_frequency(&samples_bent, 44100.0).get();

        // Default bend range is 2 semitones: 440 * 2^(2/12) ~= 493.88 Hz
        let expected_bent = 440.0_f32 * 2.0_f32.powf(2.0 / 12.0);

        assert!(
            freq_bent > freq_base * 1.08,
            "Bent frequency ({:.1} Hz) should be >8% higher than base ({:.1} Hz)",
            freq_bent,
            freq_base
        );
        assert!(
            (freq_bent - expected_bent).abs() < expected_bent * 0.10,
            "Bent frequency ({:.1} Hz) should be within 10% of expected ({:.1} Hz)",
            freq_bent,
            expected_bent
        );
    }

    #[test]
    fn test_pitch_bend_range_affects_shift() {
        // Synth A: 2 semitone range (default)
        let mut synth_2st = SynthHandle::new(44100.0)
            .sine()
            .poly(1)
            .adsr(0.001, 0.1, 1.0, 0.1)
            .pitch_bend_range(2.0)
            .build()
            .unwrap();

        // Synth B: 12 semitone range (one octave)
        let mut synth_12st = SynthHandle::new(44100.0)
            .sine()
            .poly(1)
            .adsr(0.001, 0.1, 1.0, 0.1)
            .pitch_bend_range(12.0)
            .build()
            .unwrap();

        // Both play A4 + full bend up
        let note_on = ev_note_on(0, 69, 100);
        let bend_up = ev_bend(0, 16383);

        synth_2st.midi_sender().queue(&[note_on, bend_up]);
        synth_12st.midi_sender().queue(&[note_on, bend_up]);

        let samples_2st = render_samples(&mut synth_2st, 4000);
        let samples_12st = render_samples(&mut synth_12st, 4000);

        let freq_2st = estimate_frequency(&samples_2st, 44100.0).get();
        let freq_12st = estimate_frequency(&samples_12st, 44100.0).get();

        // 12st range should produce much higher frequency than 2st
        // 2st: 440 * 2^(2/12) ~= 494 Hz
        // 12st: 440 * 2^(12/12) = 880 Hz
        assert!(
            freq_12st > freq_2st * 1.5,
            "12-semitone range ({:.1} Hz) should be >1.5x higher than 2-semitone range ({:.1} Hz)",
            freq_12st,
            freq_2st
        );
    }

    #[test]
    fn test_pitch_bend_down() {
        let mut synth = SynthHandle::new(44100.0)
            .sine()
            .poly(1)
            .adsr(0.001, 0.1, 1.0, 0.1)
            .build()
            .unwrap();

        // Play A4 + bend full down
        let note_on = ev_note_on(0, 69, 100);
        let bend_down = ev_bend(0, 0);
        synth.midi_sender().queue(&[note_on, bend_down]);

        let samples = render_samples(&mut synth, 4000);
        let freq = estimate_frequency(&samples, 44100.0).get();

        // Full bend down with 2-semitone range: 440 * 2^(-2/12) ~= 392 Hz
        let expected = 440.0_f32 * 2.0_f32.powf(-2.0 / 12.0);

        assert!(
            freq < 440.0 * 0.95,
            "Bent-down frequency ({:.1} Hz) should be lower than 440 Hz",
            freq
        );
        assert!(
            (freq - expected).abs() < expected * 0.10,
            "Bent-down frequency ({:.1} Hz) should be within 10% of expected ({:.1} Hz)",
            freq,
            expected
        );
    }

    #[test]
    fn test_pitch_bend_center_no_change() {
        let mut synth = SynthHandle::new(44100.0)
            .sine()
            .poly(1)
            .adsr(0.001, 0.1, 1.0, 0.1)
            .build()
            .unwrap();

        // Play A4 + center bend (no change)
        let note_on = ev_note_on(0, 69, 100);
        let bend_center = ev_bend(0, 8192);
        synth.midi_sender().queue(&[note_on, bend_center]);

        let samples = render_samples(&mut synth, 4000);
        let freq = estimate_frequency(&samples, 44100.0).get();

        assert!(
            (freq - 440.0).abs() < 22.0,
            "Center bend frequency ({:.1} Hz) should be ~440 Hz",
            freq
        );
    }

    // =========================================================================
    // Filter Modulation Tests
    // =========================================================================

    #[test]
    fn test_mod_wheel_to_filter_changes_brightness() {
        // Use two separate synths to avoid temporal correlation:
        // one with mod wheel at 0, one with mod wheel at max.
        let mut synth_closed = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .filter_moog(200.0, 0.8)
            .adsr(0.001, 0.0, 1.0, 0.1)
            .mod_wheel_to_filter(1.0)
            .build()
            .unwrap();

        let mut synth_open = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .filter_moog(200.0, 0.8)
            .adsr(0.001, 0.0, 1.0, 0.1)
            .mod_wheel_to_filter(1.0)
            .build()
            .unwrap();

        // Set mod wheel to max on the open synth before note-on
        let cc1_max = ev_cc(0, 1, 127);
        synth_open.midi_sender().queue(&[cc1_max]);
        render_samples(&mut synth_open, 1);

        // Play same note on both
        let note_closed = ev_note_on(0, 60, 100);
        let note_open = ev_note_on(0, 60, 100);
        synth_closed.midi_sender().queue(&[note_closed]);
        synth_open.midi_sender().queue(&[note_open]);

        // Skip attack transient
        render_samples(&mut synth_closed, 200);
        render_samples(&mut synth_open, 200);

        let samples_closed = render_samples(&mut synth_closed, 2000);
        let samples_open = render_samples(&mut synth_open, 2000);

        let rms_closed = rms_stereo(&samples_closed);
        let rms_open = rms_stereo(&samples_open);

        assert!(
            rms_closed > 0.001,
            "Should produce audio with filter closed"
        );
        assert!(rms_open > 0.001, "Should produce audio with filter open");
        assert!(
            rms_open > rms_closed * 1.05,
            "Opening filter should increase RMS: open={:.4}, closed={:.4}",
            rms_open,
            rms_closed
        );
    }

    #[test]
    fn test_velocity_to_filter_brightness() {
        // Verify that velocity_to_filter actually changes the filter cutoff
        // by comparing two synths with vel_to_filter: one at low vel, one at high vel.
        // The difference in RMS should be greater than what velocity amplitude
        // scaling alone would produce.
        let mut synth_no_mod_soft = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .filter_moog(500.0, 0.3)
            .adsr(0.001, 0.0, 1.0, 0.1)
            .build()
            .unwrap();

        let mut synth_no_mod_loud = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .filter_moog(500.0, 0.3)
            .adsr(0.001, 0.0, 1.0, 0.1)
            .build()
            .unwrap();

        let mut synth_mod_soft = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .filter_moog(500.0, 0.3)
            .adsr(0.001, 0.0, 1.0, 0.1)
            .velocity_to_filter(1.0)
            .build()
            .unwrap();

        let mut synth_mod_loud = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .filter_moog(500.0, 0.3)
            .adsr(0.001, 0.0, 1.0, 0.1)
            .velocity_to_filter(1.0)
            .build()
            .unwrap();

        // Play soft (vel=30) and loud (vel=127) on both configs
        synth_no_mod_soft
            .midi_sender()
            .queue(&[ev_note_on(0, 60, 30)]);
        synth_no_mod_loud
            .midi_sender()
            .queue(&[ev_note_on(0, 60, 127)]);
        synth_mod_soft.midi_sender().queue(&[ev_note_on(0, 60, 30)]);
        synth_mod_loud
            .midi_sender()
            .queue(&[ev_note_on(0, 60, 127)]);

        // Skip attack transient
        render_samples(&mut synth_no_mod_soft, 200);
        render_samples(&mut synth_no_mod_loud, 200);
        render_samples(&mut synth_mod_soft, 200);
        render_samples(&mut synth_mod_loud, 200);

        let rms_no_soft = rms_stereo(&render_samples(&mut synth_no_mod_soft, 2000));
        let rms_no_loud = rms_stereo(&render_samples(&mut synth_no_mod_loud, 2000));
        let rms_mod_soft = rms_stereo(&render_samples(&mut synth_mod_soft, 2000));
        let rms_mod_loud = rms_stereo(&render_samples(&mut synth_mod_loud, 2000));

        // The ratio between loud and soft should be bigger with velocity_to_filter
        let ratio_no_mod = rms_no_loud / rms_no_soft;
        let ratio_with_mod = rms_mod_loud / rms_mod_soft;

        assert!(rms_no_soft > 0.001, "Should produce audio");
        assert!(rms_mod_soft > 0.001, "Should produce audio");
        assert!(
            (ratio_with_mod - ratio_no_mod).abs() > 0.01,
            "Velocity-to-filter should change the loud/soft ratio: without_mod={:.3}, with_mod={:.3}",
            ratio_no_mod, ratio_with_mod
        );
    }

    #[test]
    fn test_lfo_to_filter_varies_over_time() {
        let mut synth = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .filter_moog(200.0, 0.8)
            .adsr(0.001, 0.0, 1.0, 0.1)
            .lfo_to_filter(5.0, 1.0)
            .build()
            .unwrap();

        // Play note and skip attack transient
        let note_on = ev_note_on(0, 48, 100);
        synth.midi_sender().queue(&[note_on]);
        render_samples(&mut synth, 200);

        // 5 Hz LFO = 8820 samples per cycle at 44100 Hz
        // Render 8 quarter-cycle blocks (2 full LFO cycles) for better coverage
        let block_size = 2205;
        let mut block_rms = Vec::new();
        for _ in 0..8 {
            let samples = render_samples(&mut synth, block_size);
            block_rms.push(rms_stereo(&samples));
        }

        let max_rms = block_rms.iter().copied().fold(0.0f32, f32::max);
        let min_rms = block_rms.iter().copied().fold(f32::MAX, f32::min);

        assert!(
            min_rms > 0.001,
            "All blocks should have audio, min RMS={}",
            min_rms
        );
        assert!(
            max_rms / min_rms > 1.03,
            "LFO should cause RMS variation: max={:.4}, min={:.4}, ratio={:.2}",
            max_rms,
            min_rms,
            max_rms / min_rms
        );
    }

    #[test]
    fn test_mod_wheel_no_effect_without_depth() {
        let mut synth = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .filter_moog(500.0, 0.5)
            .adsr(0.001, 0.1, 1.0, 0.1)
            // No mod_wheel_to_filter() call - depth is 0
            .build()
            .unwrap();

        // Play note, measure RMS
        let note_on = ev_note_on(0, 60, 100);
        synth.midi_sender().queue(&[note_on]);
        let samples_before = render_samples(&mut synth, 2000);
        let rms_before = rms_stereo(&samples_before);

        // Push mod wheel to max - should have no effect
        let cc1_max = ev_cc(0, 1, 127);
        synth.midi_sender().queue(&[cc1_max]);
        let samples_after = render_samples(&mut synth, 2000);
        let rms_after = rms_stereo(&samples_after);

        assert!(rms_before > 0.001, "Should produce audio");
        let diff_pct = (rms_after - rms_before).abs() / rms_before;
        assert!(
            diff_pct < 0.10,
            "Without mod depth, CC1 should not change RMS: before={:.4}, after={:.4}, diff={:.1}%",
            rms_before,
            rms_after,
            diff_pct * 100.0
        );
    }

    // =========================================================================
    // Oscillator Type Tests
    // =========================================================================

    #[test]
    fn test_oscillators_produce_different_waveforms() {
        let osc_builders: Vec<Box<dyn Fn(SynthHandle) -> SynthHandle>> = vec![
            Box::new(|s| s.sine()),
            Box::new(|s| s.saw()),
            Box::new(|s| s.square(0.5)),
            Box::new(|s| s.triangle()),
        ];

        let mut all_samples: Vec<Vec<(f32, f32)>> = Vec::new();

        for builder_fn in &osc_builders {
            let mut synth = builder_fn(SynthHandle::new(44100.0))
                .poly(1)
                .adsr(0.001, 0.1, 0.8, 0.1)
                .build()
                .unwrap();

            let note_on = ev_note_on(0, 60, 100);
            synth.midi_sender().queue(&[note_on]);
            all_samples.push(render_samples(&mut synth, 1000));
        }

        // Each pair should differ
        let names = ["sine", "saw", "square", "triangle"];
        let mut pairs_different = 0;
        for i in 0..4 {
            for j in (i + 1)..4 {
                let diff: f32 = all_samples[i]
                    .iter()
                    .zip(all_samples[j].iter())
                    .map(|((l1, _), (l2, _))| (l1 - l2).abs())
                    .sum::<f32>()
                    / all_samples[i].len() as f32;
                if diff > 0.001 {
                    pairs_different += 1;
                } else {
                    eprintln!(
                        "Warning: {} and {} are very similar (diff={})",
                        names[i], names[j], diff
                    );
                }
            }
        }

        assert!(
            pairs_different >= 4,
            "At least 4 of 6 oscillator pairs should differ, got {}",
            pairs_different
        );
    }

    // =========================================================================
    // Filter Type Tests
    // =========================================================================

    #[test]
    fn test_moog_filter_reduces_rms() {
        // Unfiltered
        let mut synth_no_filter = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .no_filter()
            .adsr(0.001, 0.1, 1.0, 0.1)
            .build()
            .unwrap();

        // With Moog lowpass at 500 Hz
        let mut synth_filtered = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .filter_moog(500.0, 0.5)
            .adsr(0.001, 0.1, 1.0, 0.1)
            .build()
            .unwrap();

        let note_on = ev_note_on(0, 60, 100);
        synth_no_filter.midi_sender().queue(&[note_on]);
        synth_filtered.midi_sender().queue(&[note_on]);

        let samples_no_filter = render_samples(&mut synth_no_filter, 2000);
        let samples_filtered = render_samples(&mut synth_filtered, 2000);

        let rms_no_filter = rms_stereo(&samples_no_filter);
        let rms_filtered = rms_stereo(&samples_filtered);

        assert!(
            rms_filtered < rms_no_filter * 0.8,
            "Moog filter should reduce RMS: filtered={:.4}, unfiltered={:.4}",
            rms_filtered,
            rms_no_filter
        );
    }

    #[test]
    fn test_svf_lowpass_produces_audio() {
        let mut synth = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .filter_lowpass(1000.0, 1.0)
            .adsr(0.001, 0.1, 0.8, 0.1)
            .build()
            .unwrap();

        let note_on = ev_note_on(0, 60, 100);
        synth.midi_sender().queue(&[note_on]);
        let samples = render_samples(&mut synth, 1000);
        assert!(
            rms_stereo(&samples) > 0.01,
            "SVF lowpass should produce audio"
        );
    }

    // =========================================================================
    // SVF Filter Differentiation Tests
    // =========================================================================

    #[test]
    fn test_svf_lowpass_reduces_brightness() {
        let mut synth_unfiltered = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .no_filter()
            .adsr(0.001, 0.0, 1.0, 0.1)
            .build()
            .unwrap();

        // Use a low cutoff (200 Hz) so the filter cuts more of the saw's harmonics
        let mut synth_lowpass = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .filter_lowpass(200.0, 1.0)
            .adsr(0.001, 0.0, 1.0, 0.1)
            .build()
            .unwrap();

        let note = ev_note_on(0, 60, 100);
        synth_unfiltered.midi_sender().queue(&[note]);
        synth_lowpass.midi_sender().queue(&[note]);

        // Skip attack transient
        render_samples(&mut synth_unfiltered, 200);
        render_samples(&mut synth_lowpass, 200);

        let rms_unfiltered = rms_stereo(&render_samples(&mut synth_unfiltered, 2000));
        let rms_lowpass = rms_stereo(&render_samples(&mut synth_lowpass, 2000));

        assert!(
            rms_lowpass < rms_unfiltered * 0.95,
            "SVF lowpass should reduce RMS: lowpass={:.4}, unfiltered={:.4}",
            rms_lowpass,
            rms_unfiltered
        );
    }

    #[test]
    fn test_svf_highpass_differs_from_lowpass() {
        let mut synth_lp = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .filter_lowpass(500.0, 1.0)
            .adsr(0.001, 0.0, 1.0, 0.1)
            .build()
            .unwrap();

        let mut synth_hp = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .filter_highpass(500.0, 1.0)
            .adsr(0.001, 0.0, 1.0, 0.1)
            .build()
            .unwrap();

        let note = ev_note_on(0, 60, 100);
        synth_lp.midi_sender().queue(&[note]);
        synth_hp.midi_sender().queue(&[note]);

        // Skip attack transient
        render_samples(&mut synth_lp, 200);
        render_samples(&mut synth_hp, 200);

        let samples_lp = render_samples(&mut synth_lp, 2000);
        let samples_hp = render_samples(&mut synth_hp, 2000);

        let diff: f32 = samples_lp
            .iter()
            .zip(samples_hp.iter())
            .map(|((l1, _), (l2, _))| (l1 - l2).abs())
            .sum::<f32>()
            / samples_lp.len() as f32;

        assert!(
            diff > 0.01,
            "Lowpass and highpass should produce different waveforms, avg diff={:.4}",
            diff
        );
    }

    // =========================================================================
    // Pulse Width Test
    // =========================================================================

    #[test]
    fn test_square_pulse_width_affects_waveform() {
        let widths = [0.1, 0.5, 0.9];
        let mut all_samples: Vec<Vec<(f32, f32)>> = Vec::new();

        for &pw in &widths {
            let mut synth = SynthHandle::new(44100.0)
                .square(pw)
                .poly(1)
                .adsr(0.001, 0.0, 1.0, 0.1)
                .build()
                .unwrap();

            let note = ev_note_on(0, 60, 100);
            synth.midi_sender().queue(&[note]);

            // Skip attack transient
            render_samples(&mut synth, 200);
            all_samples.push(render_samples(&mut synth, 2000));
        }

        // Each pair of different pulse widths should produce different waveforms
        let mut pairs_different = 0;
        for i in 0..3 {
            for j in (i + 1)..3 {
                let diff: f32 = all_samples[i]
                    .iter()
                    .zip(all_samples[j].iter())
                    .map(|((l1, _), (l2, _))| (l1 - l2).abs())
                    .sum::<f32>()
                    / all_samples[i].len() as f32;
                if diff > 0.001 {
                    pairs_different += 1;
                }
            }
        }

        assert!(
            pairs_different >= 2,
            "At least 2 of 3 pulse width pairs should differ, got {}",
            pairs_different
        );
    }

    // =========================================================================
    // SynthHandle Builder Method Tests
    // =========================================================================

    #[test]
    fn test_envelope_pluck_preset() {
        let mut synth = SynthHandle::new(44100.0)
            .saw()
            .poly(1)
            .envelope_pluck()
            .build()
            .unwrap();

        // Pluck = fast attack, fast decay, no sustain
        let note = ev_note_on(0, 60, 100);
        synth.midi_sender().queue(&[note]);

        // Early samples should have audio
        let early = render_samples(&mut synth, 500);
        assert!(
            rms_stereo(&early) > 0.001,
            "Pluck should produce audio initially"
        );

        // After decay, should be much quieter (sustain = 0)
        let late = render_samples(&mut synth, 20000);
        let late_rms = rms_stereo(&late);
        let early_rms = rms_stereo(&early);
        assert!(
            late_rms < early_rms,
            "Pluck should decay: early={:.4}, late={:.4}",
            early_rms,
            late_rms
        );
    }

    #[test]
    fn test_portamento_builds_and_glides() {
        let mut synth = SynthHandle::new(44100.0)
            .sine()
            .poly(2)
            .adsr(0.001, 0.0, 1.0, 0.1)
            .portamento(0.05) // 50ms glide
            .build()
            .unwrap();

        // Play two notes — should glide
        let note1 = ev_note_on(0, 60, 100);
        synth.midi_sender().queue(&[note1]);
        let _ = render_samples(&mut synth, 4000); // Let first note settle

        let note2 = ev_note_on(0, 72, 100);
        synth.midi_sender().queue(&[note2]);
        let samples = render_samples(&mut synth, 2000);
        assert!(
            rms_stereo(&samples) > 0.01,
            "Portamento synth should produce audio"
        );
    }
}
