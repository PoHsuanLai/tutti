# tutti-polysynth

Polyphonic subtractive and wavetable synthesis for the Tutti audio engine.

## What this is

One type does the work: `PolySynth`, built from a `SynthConfig` and driven by
MIDI. It takes no audio input and renders stereo. Around it sit the voice engine's
parts: voice allocation (`AllocationStrategy`, `VoiceMode`), unison, portamento
and tuning, all configured through `SynthConfig`.

**Notes arrive on its MIDI event input.** It is a `tutti_graph::Node` with one
MIDI event input: a clip node, a keyboard's `MidiQueueNode` or the hardware
input node (all `tutti-midi-runtime`) wires to it, each event played on its
frame. There is no `note_on` scalar entry point on this type, and no queue to
drive it by hand: a test or bench hands it events the way a graph does
(`tutti_graph::contract::{drive_in, Direct}`).

**Its controls are its params.** Inserted into a graph, it hands back a
`tutti_graph::ParamSet` over its live cells — the master volume and, with a
unison engine, the unison detune and stereo spread, by `UnitParam` — and a
fork of the graph (an export) starts from the values last set through it.

## What it does not own

- **Not a SoundFont player.** `.sf2` playback is
  [`tutti-soundfont`](../tutti-soundfont)'s, a **peer** crate rather than a
  feature of this one. A sample player shares no voice engine, envelope model or
  filter with a subtractive synth, so the two have nothing in common beyond the
  node contract and a MIDI event input — and those come from `tutti-graph`, not
  from each other. The old `soundfont` feature flag was
  a dependency edge wearing a feature's clothes; depend on that crate directly.
- **Not a clip player.** Playing a recorded `Wave` on a timeline is
  [`tutti-sampler`](../tutti-sampler)'s, which correspondingly has no `note_on`.
- **No effects.** Filters live per-voice inside the synth; a send, a delay or a
  reverb is a `tutti-nodes` node after it.
- **No MIDI I/O and no file parsing.** The wire vocabulary is
  `tutti-midi-types`', the MIDI nodes are `tutti-midi-runtime`'s, ports are
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
use tutti_core::graph::{OutPort, Source};
use tutti_core::{Amplitude, Hz, NodeKey, Resonance, SampleRate, Samples, Seconds, UnitParam};
use tutti_graph::{Editor, Prepare};

// `Moog` takes `Resonance`; the `Svf` variant takes `Q` instead. The two
// filter families are deliberately not interchangeable.
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

// Into the graph: no audio input, stereo out, one MIDI event input (wire a
// clip or a keyboard's queue node to it with `GraphSpec::connect_events`).
let (mut editor, _executor) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
let synth_key = NodeKey(1);
let params = editor.insert(synth_key, "synth", synth);
editor.spec_mut().topology.outputs = (0..2)
    .map(|port| Source::Node(OutPort { node: synth_key, port }))
    .collect();
editor.commit().expect("a one-node graph commits");

// Its controls: the live params, by address.
assert!(params.set(UnitParam::Volume, 0.8));
# Ok::<(), tutti_polysynth::Error>(())
```

## Constraint: what is fixed at construction, and what is not

The voice bank every voice renders in is built once for the oscillator, filter
and envelope, so **those three need a new synth to change** — there is no setter for
them, and swapping one means building a `PolySynth` and replacing the node.

What does have a live setter: unison detune, stereo spread and sub-voice count,
master volume and MPE enablement.

`max_voices` must be at least 1 and has no upper bound. It was capped at 16
until the per-block finished-voice list stopped being a `SmallVec<[usize; 16]>`
— that type's inline capacity had to bound it, because a spill would have
allocated in the audio callback. The list is now a `Vec` sized once at
construction and only `clear()`ed, which keeps the callback allocation-free
without a ceiling.

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
