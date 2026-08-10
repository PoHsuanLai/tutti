# tutti-polysynth

Polyphonic subtractive and wavetable synthesis for Tutti.

## What this is

One type does the work: `PolySynth`, an `AudioUnit` built from a `SynthConfig`
and driven by MIDI. It takes no audio input — notes arrive through its own
lock-free MIDI inbox — and renders stereo. Around it sit the voice engine's
parts: voice allocation (`AllocationStrategy`, `VoiceMode`), unison, portamento,
and tuning, all configured through `SynthConfig`.

```rust
use tutti_polysynth::{
    EnvelopeConfig, FilterType, OscillatorType, PolySynth, SynthConfig,
};
use tutti_core::{Amplitude, Hz, Resonance, Seconds};

let synth = PolySynth::new(SynthConfig {
    oscillator: OscillatorType::Saw,
    max_voices: 8,
    filter: FilterType::Moog {
        cutoff: Hz(2000.0),
        resonance: Resonance(0.7),
    },
    envelope: EnvelopeConfig {
        attack: Seconds(0.01),
        decay: Seconds(0.2),
        sustain: Amplitude(0.6),
        release: Seconds(0.3),
    },
    ..Default::default()
})?;

let sender = synth.midi_sender();
# Ok::<(), tutti_polysynth::Error>(())
```

## What is fixed at construction, and what is not

The DSP chain each voice runs is assembled once from the oscillator, filter and
envelope, so those three need a new synth to change. Unison detune, spread and
sub-voice count, master volume, and MPE enablement all have live setters.
`max_voices` is capped at 16, so the per-block finished-voice list stays inline
and the audio callback never allocates.

## Why it is its own crate — and not a SoundFont player

`.sf2` playback is [`tutti-soundfont`](../tutti-soundfont)'s, a **peer** crate
rather than a feature of this one. A sample player shares no voice engine,
envelope model or filter with a subtractive synth, so the two have nothing to
hold in common beyond the `AudioUnit` trait. The old `soundfont` feature flag
was a dependency edge wearing a feature's clothes; depend on that crate directly.

The other removed flag is worth knowing about for the same reason. `midi` never
compiled with it off — a voice is *addressed* by per-note identity, so the
allocator, MPE state and `PolySynth` itself are all built on it. A synth you
cannot send a note to is not a smaller synth.

## Where it sits

Depends on `tutti-core` (with `midi`), `tutti-mod` (for `ModParams`, so it
carries the same control-rate modulation trait every other node does),
`tutti-midi-types` and `tutti-midi-runtime`. Only `bevy-tutti` depends on it.

## Features

`default = []`, plus a `std` flag. There is nothing else to gate — see above for
the two that were removed and why.

## Examples

`examples/render_synth_cases.rs` renders a set of configurations to disk;
`examples/verify_synth.py` checks the output. See `examples/README.md`.

## License

MIT OR Apache-2.0
