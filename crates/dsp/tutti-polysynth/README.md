# tutti-polysynth

Polyphonic subtractive synthesis for the Tutti audio engine.

Use it when you want a playable synthesizer voice in a Tutti graph: saw,
square, triangle, sine or noise oscillators through a Moog-style ladder or a
state-variable filter, with an ADSR envelope, unison, portamento, alternative
tunings and MPE / MIDI 2.0 per-note expression.

`PolySynth` is the synth: a graph node (`tutti_graph::Node`) built from a
`SynthConfig`, with no audio input, stereo output and one MIDI event input.
Wire a clip node, a keyboard's `MidiQueueNode` or the hardware input node (all
`tutti-midi-runtime`) to the event input; each event is applied on its own
frame. The rest of the crate is configuration: `SynthConfig` with its
`OscillatorType`, `FilterType`, `EnvelopeConfig` and `FilterModConfig`; voice
allocation (`VoiceMode`, `AllocationStrategy`); `UnisonConfig`;
`PortamentoConfig`; and `Tuning`.

Inserted into a graph, the synth hands back a `tutti_graph::ParamSet` over its
live params by `UnitParam`: the master `Volume` and, with a unison engine,
`Detune` and `StereoSpread`. A fork of the graph (an offline export) starts
from the values last set through it.

## Quick start

```rust
use tutti_polysynth::{
    EnvelopeConfig, FilterType, OscillatorType, PolySynth, SynthConfig,
};
use tutti_core::graph::{OutPort, Source};
use tutti_core::{Amplitude, Hz, NodeKey, Resonance, SampleRate, Samples, Seconds, UnitParam};
use tutti_graph::{Editor, Prepare};

// `Moog` takes `Resonance`; the `Svf` variant takes `Q` instead.
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

## What is fixed at construction

The voice bank every voice renders in is built once for the oscillator, filter
type and envelope, so those need a new synth to change: build a new
`PolySynth` and replace the node. The filter's cutoff (and a ladder's
resonance) are modulated live by `FilterModConfig`, CC74, CC71 and MPE slide.

Live setters exist for the master volume, unison detune, stereo spread and
sub-voice count, and MPE enablement.

`max_voices` must be at least 1 and has no upper bound. Every voice is built
up front, so it is a memory and CPU budget.

## Real-time behaviour

Rendering allocates nothing and takes no lock, at any block length. Up to 512
MIDI events per block are applied; any beyond that are dropped. Building a
synth, forking it and changing the unison voice count allocate, and belong on
the control thread.

## Scope

- **Not a SoundFont player.** `.sf2` playback is `tutti-soundfont`, a separate
  crate that shares only the graph-node shape with this one.
- **Not a clip player.** Playing recorded audio on a timeline is
  `tutti-sampler`.
- **No effects.** Filters live per voice inside the synth; a send, a delay or
  a reverb is a `tutti-nodes` node after it.
- **No MIDI I/O and no file parsing.** The message types are
  `tutti-midi-types`, the MIDI nodes are `tutti-midi-runtime`, device ports
  are `tutti-midi-hardware`.

The `tutti` crate re-exports this one as `tutti::polysynth` behind its
`synth` feature, and `bevy-tutti` uses it behind the same feature.

## Examples

`examples/render_synth_cases.rs` renders a set of configurations to disk;
`examples/verify_synth.py` checks the output against synthesis theory. See
`examples/README.md`.

## Features

`default = []`. The `std` feature is accepted but currently gates nothing.

## License

MIT OR Apache-2.0
