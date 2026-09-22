//! [`SynthConfig`] and the value types that configure a synth (oscillator /
//! filter / envelope). The synth itself ([`PolySynth`](crate::PolySynth)) and
//! its per-voice engine live in `crate::polysynth` / `crate::synth_voice`.

use core::fmt;

use crate::{AllocationStrategy, PortamentoConfig, Tuning, UnisonConfig, VoiceMode};
use tutti_core::{Amplitude, Depth, Hz, Resonance, Seconds, Semitones, Q};

/// The waveform every sub-voice of every voice generates.
///
/// One oscillator per synth, chosen at construction: the config is read when
/// each voice's DSP chain is built, so changing it needs a new
/// [`PolySynth`](crate::PolySynth). All variants except [`Noise`](Self::Noise)
/// track the voice's pitch; `Noise` ignores it entirely.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum OscillatorType {
    /// A pure sine — no harmonics for the filter to work on.
    Sine,
    /// Band-limited sawtooth. The default, and the usual subtractive starting
    /// point: every harmonic present, so the filter has material to remove.
    #[default]
    Saw,
    /// Band-limited pulse whose duty cycle is `pulse_width`.
    Square {
        /// Duty cycle as a fraction of the period. `0.5` is a true square
        /// (odd harmonics only); values either side thin the tone toward a
        /// nasal pulse. Passed straight to fundsp's `poly_pulse`, and fixed
        /// for the life of the synth — it is baked into the DSP chain at
        /// voice-build time rather than read per sample.
        pulse_width: f32,
    },
    /// Triangle — odd harmonics rolling off steeply, a mellow alternative to
    /// [`Square`](Self::Square).
    Triangle,
    /// Pink noise. Pitch-free: the voice's pitch cell is still driven but no
    /// oscillator reads it, so note number only selects which voice sounds.
    /// `Display` renders this as "Pink Noise".
    Noise,
}

/// Which response of the state-variable filter [`FilterType::Svf`] taps.
///
/// All four share one cutoff and one `Q`; only the output tap differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SvfMode {
    /// Passes below cutoff. The default, and the conventional subtractive tap.
    #[default]
    Lowpass,
    /// Passes above cutoff.
    Highpass,
    /// Passes a band around cutoff, whose width narrows as `Q` rises.
    Bandpass,
    /// Rejects a band around cutoff, passing everything else.
    Notch,
}

/// Which filter each voice runs, and its settings.
///
/// Chosen at construction and baked into every voice's DSP chain, so the
/// *variant* cannot change on a live synth. The cutoff and resonance within it
/// can: both are held in shared cells the modulation path writes, which is what
/// [`FilterModConfig`], CC74 and MPE slide move.
///
/// # The two variants carry different resonance types deliberately
///
/// [`Resonance`] and [`Q`] both answer "how resonant", but a ladder at 1.0
/// self-oscillates while a `Q` of 1.0 is a mild bell. Swapping them is silent to
/// the compiler when both are bare floats, and sounds like a mistuned filter.
/// The nodes downstream take `Resonance` and `Q` respectively; these fields
/// carry that distinction all the way in.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum FilterType {
    /// A four-pole Moog-style ladder lowpass, 24 dB/octave.
    Moog {
        /// Corner frequency in [`Hz`]. This is the *base* cutoff — every
        /// modulation source multiplies it rather than replacing it — and the
        /// value each voice returns to on `reset`.
        cutoff: Hz,
        /// Ladder feedback. Musically 0.0 (no emphasis) to ~1.0, where the
        /// ladder self-oscillates; CC71 sweeps this up to a ceiling of 0.95.
        resonance: Resonance,
    },
    /// A state-variable filter, 12 dB/octave, tapped per [`SvfMode`].
    Svf {
        /// Corner (or center, for [`SvfMode::Bandpass`]/[`SvfMode::Notch`])
        /// frequency in [`Hz`]. Base value, modulated multiplicatively as above.
        cutoff: Hz,
        /// Filter [`Q`]. Around 0.707 is maximally flat; higher values put a
        /// resonant peak at cutoff and narrow the band of the band tabs.
        ///
        /// Unlike `cutoff`, this one is **fixed for the life of the voice** —
        /// it is passed by value into the SVF node when the chain is built, so
        /// resonance modulation (CC71) reaches only [`Moog`](Self::Moog).
        q: Q,
        /// Which response is tapped.
        mode: SvfMode,
    },
    /// No filter: the oscillator goes straight to the amplitude envelope. The
    /// default. Voices still expose a cutoff cell (parked at 20 kHz) so the
    /// modulation path has somewhere to write, but nothing reads it.
    #[default]
    None,
}

impl fmt::Display for OscillatorType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OscillatorType::Sine => write!(f, "Sine"),
            OscillatorType::Saw => write!(f, "Saw"),
            OscillatorType::Square { .. } => write!(f, "Square"),
            OscillatorType::Triangle => write!(f, "Triangle"),
            OscillatorType::Noise => write!(f, "Pink Noise"),
        }
    }
}

impl fmt::Display for SvfMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SvfMode::Lowpass => write!(f, "Lowpass"),
            SvfMode::Highpass => write!(f, "Highpass"),
            SvfMode::Bandpass => write!(f, "Bandpass"),
            SvfMode::Notch => write!(f, "Notch"),
        }
    }
}

impl fmt::Display for FilterType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FilterType::Moog { .. } => write!(f, "Moog Ladder"),
            FilterType::Svf { mode, .. } => write!(f, "SVF ({})", mode),
            FilterType::None => write!(f, "None"),
        }
    }
}

/// The ADSR amplitude envelope every voice's gate drives.
///
/// One envelope per synth, shared in shape by all voices; each voice runs its
/// own instance, retriggered by note-on and released by note-off. The values
/// are read once when a voice's DSP chain is built, so changing them needs a
/// new [`PolySynth`](crate::PolySynth).
///
/// [`release`](Self::release) is the one field with a consequence outside the
/// sound: [`PolySynth`](crate::PolySynth)'s `tail()` reports it as the node's
/// ring-out, so an offline bounce that ignored it would chop the release off
/// every note in the project.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnvelopeConfig {
    /// Time in [`Seconds`] from note-on to full level. `0.0` is an instant
    /// click; a few milliseconds is a percussive edge; a second or more is a
    /// pad swell.
    pub attack: Seconds,
    /// Time in [`Seconds`] from full level down to [`sustain`](Self::sustain).
    /// Irrelevant when `sustain` is already unity.
    pub decay: Seconds,
    /// The held level — an [`Amplitude`], not a time, which is why it is the
    /// one `Amplitude` among three `Seconds`. `1.0` holds at full (an organ),
    /// `0.0` makes the note die away after decay regardless of how long the key
    /// is held (a pluck).
    pub sustain: Amplitude,
    /// Time in [`Seconds`] from note-off to silence. Also the value the synth
    /// reports as its `tail()`, so it decides how far past the last note-off an
    /// offline render must run.
    pub release: Seconds,
}

impl Default for EnvelopeConfig {
    fn default() -> Self {
        Self {
            attack: Seconds(0.01),
            decay: Seconds(0.1),
            sustain: Amplitude(0.7),
            release: Seconds(0.2),
        }
    }
}

impl EnvelopeConfig {
    /// Builds an envelope from attack, decay, sustain and release.
    ///
    /// The three times are [`Seconds`] and `sustain` is an [`Amplitude`]; the
    /// odd one out sits third, so the unit types are what keep a positional
    /// call from silently transposing a level and a time.
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

    /// A plucked envelope: 1 ms attack, 300 ms decay to **zero** sustain, 100 ms
    /// release. Zero sustain is what makes the note die on its own — holding the
    /// key longer than the decay changes nothing.
    pub fn pluck() -> Self {
        Self::new(Seconds(0.001), Seconds(0.3), Amplitude(0.0), Seconds(0.1))
    }

    /// A pad envelope: 500 ms swell, 200 ms decay to 0.8, 1 s release. The long
    /// release means voices keep ringing well past note-off, so a pad patch
    /// exhausts `max_voices` far sooner than its note count suggests.
    pub fn pad() -> Self {
        Self::new(Seconds(0.5), Seconds(0.2), Amplitude(0.8), Seconds(1.0))
    }
}

/// Per-voice modulation routed onto the filter cutoff.
///
/// All three sources **multiply** the [`FilterType`] base cutoff rather than
/// replacing it, and they compound: with every depth at zero the whole block is
/// skipped, which is the default and costs nothing per sample. Each voice runs
/// its own LFO phase, so the sweep is not synchronised across a chord.
///
/// Evaluated once per sample, on the audio thread, inside the voice tick.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FilterModConfig {
    /// How far the mod wheel (CC1) opens the filter, as a [`Depth`] in 0.0..1.0.
    /// At 1.0 a fully-open wheel doubles the cutoff frequency; at 0.0 (the
    /// default) the wheel is unrouted.
    pub mod_wheel_depth: Depth,
    /// How far note velocity opens the filter, as a [`Depth`] in 0.0..1.0.
    /// At 1.0 the softest note halves the cutoff and the hardest leaves it at
    /// the base value — velocity only ever darkens. Per-note, latched at
    /// note-on from that voice's velocity.
    pub velocity_depth: Depth,
    /// LFO rate in [`Hz`]. `0.0` (the default) disables the LFO outright — the
    /// sweep block is gated on `lfo_rate > 0`, so a nonzero
    /// [`lfo_depth`](Self::lfo_depth) alone does nothing.
    ///
    /// The one [`Hz`] among three [`Depth`]s, sitting between two depths that
    /// read nothing like a frequency — as bare floats any permutation of the
    /// four compiles.
    pub lfo_rate: Hz,
    /// How far the LFO sweeps the cutoff, as a [`Depth`] in 0.0..1.0. At 1.0
    /// the sine sweeps cutoff ±50% around its base. Needs a nonzero
    /// [`lfo_rate`](Self::lfo_rate) to do anything.
    pub lfo_depth: Depth,
}

/// Everything a [`PolySynth`](crate::PolySynth) is built from.
///
/// Fill it the idiomatic way — `Default` plus struct-update — and hand it to
/// [`PolySynth::new`](crate::PolySynth::new).
///
/// # Most of this is construction-only
///
/// The synth keeps the config and reads parts of it per block, but the DSP
/// chain each voice runs is assembled once from
/// [`oscillator`](Self::oscillator), [`filter`](Self::filter) and
/// [`envelope`](Self::envelope). Changing those three needs a new synth. The
/// fields that *are* live afterwards have setters on `PolySynth`: unison detune
/// / spread / voice count, master volume, and
/// [`mpe_enabled`](Self::mpe_enabled). [`tuning`](Self::tuning),
/// [`pitch_bend_range`](Self::pitch_bend_range) and
/// [`filter_mod`](Self::filter_mod) are consulted per note or per sample but
/// have no setter.
#[derive(Debug, Clone)]
pub struct SynthConfig {
    /// Rate the voices are built at. `PolySynth` re-derives everything
    /// rate-dependent on `AudioUnit::set_sample_rate`, so a host that runs at a
    /// different rate does not need this to be right — only the LFO and
    /// portamento step sizes are computed from it.
    pub sample_rate: tutti_core::SampleRate,
    /// How many notes may sound at once. Must be at least 1; there is no
    /// upper bound. Every voice is fully constructed up front, so this is a
    /// memory and CPU budget rather than just a ceiling — 64 voices of saw
    /// through a ladder filter is real work every block.
    ///
    /// It was capped at 16 until the per-block finished-voice list stopped
    /// being a `SmallVec<[usize; 16]>`; that type's inline capacity had to
    /// bound this, because a spill would have allocated in the audio
    /// callback. A `Vec` sized here at construction gives the same guarantee
    /// with no bound.
    pub max_voices: usize,
    /// Poly, mono or legato. Under mono and legato only slot 0 is ever used,
    /// whatever `max_voices` says.
    pub voice_mode: VoiceMode,
    /// The waveform every voice generates. Construction-only.
    pub oscillator: OscillatorType,
    /// The filter every voice runs. The variant is construction-only; its
    /// cutoff moves at runtime under [`filter_mod`](Self::filter_mod), CC74 and
    /// MPE slide.
    pub filter: FilterType,
    /// The amplitude envelope every voice's gate drives. Construction-only, and
    /// its release sets the synth's reported `tail()`.
    pub envelope: EnvelopeConfig,
    /// Pitch glide between notes, or `None` for no glide. A synth built without
    /// one cannot gain it later — the glide state is only allocated when this is
    /// `Some`.
    pub portamento: Option<PortamentoConfig>,
    /// Detuned stacked sub-voices per note, or `None` for one oscillator per
    /// voice. `Some` allocates that many sub-voice DSP chains *per voice*, so
    /// the real oscillator count is `max_voices * voice_count`. As with
    /// portamento, a synth built with `None` has no unison engine to configure
    /// later.
    pub unison: Option<UnisonConfig>,
    /// Which voice gets taken when all `max_voices` are busy and another note
    /// arrives.
    pub allocation_strategy: AllocationStrategy,
    /// Note-number-to-frequency mapping. Consulted at every note-on and on
    /// every global pitch-bend update; defaults to 12-tone equal temperament
    /// with A4 = 440 Hz.
    pub tuning: Tuning,
    /// How far a full-scale global (channel) pitch bend sweeps, in
    /// [`Semitones`], applied symmetrically either side of the note. Default
    /// 2.0 — the MIDI convention of a whole tone. Applied as a frequency ratio,
    /// so it bends by the same interval in every [`tuning`](Self::tuning).
    pub pitch_bend_range: Semitones,
    /// Cutoff modulation sources. All depths default to zero, which skips the
    /// per-sample modulation work entirely.
    pub filter_mod: FilterModConfig,
    /// Whether per-note (MPE / MIDI 2.0) expression is applied at render time.
    /// Off by default. Per-note messages are *accepted* either way — this
    /// decides whether the accumulated per-note bend, pressure, slide and gain
    /// reach the audio. Live-settable via
    /// [`PolySynth::set_mpe_enabled`](crate::PolySynth::set_mpe_enabled).
    pub mpe_enabled: bool,
    /// How far a full-scale *per-note* pitch bend sweeps, in [`Semitones`].
    /// Default 48.0, the MPE convention. Independent of
    /// [`pitch_bend_range`](Self::pitch_bend_range), and overridden at runtime
    /// by the per-note pitch-bend sensitivity RPN.
    pub mpe_pitch_bend_range: Semitones,
}

impl Default for SynthConfig {
    fn default() -> Self {
        Self {
            sample_rate: tutti_core::SampleRate::SR_44K1,
            max_voices: 8,
            voice_mode: VoiceMode::Poly,
            oscillator: OscillatorType::default(),
            filter: FilterType::default(),
            envelope: EnvelopeConfig::default(),
            portamento: None,
            unison: None,
            allocation_strategy: AllocationStrategy::Oldest,
            tuning: Tuning::equal_temperament(),
            pitch_bend_range: Semitones(2.0),
            filter_mod: FilterModConfig::default(),
            mpe_enabled: false,
            mpe_pitch_bend_range: Semitones(48.0),
        }
    }
}
