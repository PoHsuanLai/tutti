# tutti-polysynth

Polyphonic subtractive and wavetable synthesis for the Tutti audio engine.

## What this is

One type does the work: `PolySynth`, an `AudioUnit` built from a `SynthConfig`
and driven by MIDI. It takes no audio input — notes arrive through its own
lock-free MIDI inbox — and renders stereo. Around it sit the voice engine's
parts: voice allocation (`AllocationStrategy`, `VoiceMode`), unison, portamento
and tuning, all configured through `SynthConfig`.

**Notes arrive one way: the MIDI inbox.** `midi_sender()` hands back a producer a
control thread can push to while the audio thread renders; `midi_port()` is the
same endpoint as a whole borrow (routing address, mailbox, and the source-install
slot), for a host that needs all three through one downcast. There is no
`note_on` scalar entry point on this type.

## What it does not own

- **Not a SoundFont player.** `.sf2` playback is
  [`tutti-soundfont`](../tutti-soundfont)'s, a **peer** crate rather than a
  feature of this one. A sample player shares no voice engine, envelope model or
  filter with a subtractive synth, so the two have nothing in common beyond the
  `AudioUnit` trait and a MIDI inbox — and those come from `tutti-core` and
  `tutti-midi-runtime`, not from each other. The old `soundfont` feature flag was
  a dependency edge wearing a feature's clothes; depend on that crate directly.
- **Not a clip player.** Playing a recorded `Wave` on a timeline is
  [`tutti-sampler`](../tutti-sampler)'s, which correspondingly has no `note_on`.
- **No effects.** Filters live per-voice inside the synth; a send, a delay or a
  reverb is a `tutti-nodes` node after it.
- **No MIDI I/O and no file parsing.** The wire vocabulary is
  `tutti-midi-types`', the mailbox is `tutti-midi-runtime`'s, ports are
  `tutti-midi-hardware`'s.

The other removed feature flag is worth knowing about for the same reason. `midi`
never compiled with it off — a voice is *addressed* by per-note identity, so the
allocator, MPE state and `PolySynth` itself are all built on it. A synth you
cannot send a note to is not a smaller synth.

## Quick start

```rust
use tutti_polysynth::{
    EnvelopeConfig, FilterType, OscillatorType, PolySynth, SynthConfig,
};
use tutti_core::dsp::{AudioUnit, Net};
use tutti_core::{Amplitude, Hz, Resonance, Seconds};
use tutti_midi_types::translation::scaling::midi1_velocity_to_midi2;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};

// `Moog` takes `Resonance`; the `Svf` variant takes `Q` instead. The two
// filter families are deliberately not interchangeable.
let mut synth = PolySynth::new(SynthConfig {
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

// Notes arrive through the lock-free inbox, so a control thread may queue
// them while the audio thread renders.
synth.midi_sender().queue(&[MidiEvent::note_on(
    MidiGroup::FIRST,
    MidiChannel::FIRST,
    69, // A4
    midi1_velocity_to_midi2(100),
)]);

// Into the graph: no audio input, stereo out.
let mut net = Net::new(0, 2);
let voice = net.push(Box::new(synth));
net.pipe_output(voice);
net.check();

let mut out = [0.0f32; 2];
net.tick(&[], &mut out);
# Ok::<(), tutti_polysynth::Error>(())
```

## Constraint: what is fixed at construction, and what is not

The DSP chain each voice runs is assembled once from the oscillator, filter and
envelope, so **those three need a new synth to change** — there is no setter for
them, and swapping one means building a `PolySynth` and replacing the node.

What does have a live setter: unison detune, stereo spread and sub-voice count,
master volume, MPE enablement, and the MIDI source.

`max_voices` is validated at construction to `1..=16` and refused outside it. The
ceiling is not arbitrary — it is the inline capacity of the per-block
finished-voice list, which is what keeps the audio callback from allocating.

## Examples

`examples/render_synth_cases.rs` renders a set of configurations to disk;
`examples/verify_synth.py` checks the output. See `examples/README.md`.

## Where it sits

Depends on `tutti-core` (with `midi`), `tutti-mod` (for `ModParams`, so it
carries the same control-rate modulation trait every other node does),
`tutti-midi-types` and `tutti-midi-runtime`. Only `bevy-tutti` depends on it.

## Features

`default = []`. There is also a `std` flag, which nothing in the crate currently
reads — it gates no code today. See above for the two flags that were removed and
why.

## License

MIT OR Apache-2.0
