# tutti-core

Real-time audio engine core — DSP graph, transport, metering, latency.

## What this is

The engine's runtime kernel and its vocabulary: the DSP graph, playback
transport, level metering, and delay compensation. Sibling crates
(`tutti-plugin`, `tutti-sampler`, `tutti-nodes`, …) build on these types and
re-export them, so a consumer usually meets them through whichever subsystem it
already depends on.

- `Engine` — the graph render the RT callback runs. It renders the native
  graph (`tutti-graph`'s `Executor`, whose `Editor` the control thread keeps),
  and only that since design doc 013's Phase 3 PR 15.
- `Transport` — playback control, split into `settings` (anyone may store into)
  and `motion` (a state machine that may defer or reject), with commands
  timed to a frame or a beat (`MotionFsm::schedule`). The engine drives the
  transport's clock and hands each block its transport in `Env`; `EnvClock`
  puts the beat on graph ports, so beat-driven sources read musical time as a
  signal rather than consulting the transport.
- `MasterMeter` / `AudioTap` — level monitoring and the analysis tap.
- `latency` — delay compensation: explicit, opt-in, over any graph.
- `topology` — `compile(&Valid, &dyn Catalog, rate) -> Compiled`, turning
  `tutti_types::graph::Topology` (the graph as a *value*) into fundsp's `Net`.
  Only tests call it; the native graph compiles the same value itself, and
  this seam goes with `Net` (doc 013, Phase 5).
- `dsp::Net` — **fundsp's** graph container, re-exported, which the nodes'
  own tests still wire units in. `Engine` does not render it.

A consumer that wants the whole engine behind one dependency takes `bevy-tutti`,
the umbrella. A consumer that wants audio without Bevy takes the `tutti` facade crate,
which re-exports all of them behind one dependency; it can also depend on
these crates
directly.

## What this crate does not own

- **MIDI.** The vocabulary is `tutti-midi-types`', the state machines
  `tutti-midi-runtime`'s, and the OS edge `tutti-midi-hardware`'s. `Engine` is
  MIDI-free; pre-block delivery is `MidiPreBlock`, one crate over.
- **The device.** Opening a stream and driving the real-time callback is
  `tutti-cpal`'s job. This crate only knows how to render a block.
- **The value vocabulary.** The unit newtypes, the RT primitives, the
  `AudioIn`/`AudioOut` traits, and the PDC and meter math are all `tutti-types`'.
  They are re-exported here so a consumer already on the engine root needs no
  second dependency, but their home is one layer down.
- **The ECS binding.** The reconcile pipeline, the graph resources and the
  declarative wiring live in the host adapter, `bevy_tutti::graph`. The optional
  `bevy` feature here adds exactly one thing — see below.

## Example — a graph, an engine, and a transport

The engine's centre in one block: build a graph, hand its executor to an
`Engine` over a `Transport`, keep the editor, and render a block. `tutti-cpal`
does exactly this around a real device; here the block is pulled by hand, so
the whole thing runs headless.

```rust
use tutti_core::graph::{OutPort, Source};
use tutti_core::{
    Beat, Bpm, ChannelLayout, Engine, EnvClock, InterleavedMut, MotionEvent, NodeKey,
    SampleRate, Samples, Timeline, Transport,
};
use tutti_graph::{Editor, Prepare};

let transport = Transport::new(48_000.0);

// The editor is the control thread's half of the graph; the executor goes to
// the engine. Every edit reaches it through a `commit`.
let (mut editor, executor) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(256)));

// A node reads the transport from each block's `Env`; `EnvClock` puts the beat
// on two ports (whole beats, then the fraction), for a node that wants it as
// a signal. Tone generators and filters are `tutti-nodes`', a crate above
// this one.
let clock = NodeKey(1);
editor.insert(clock, "clock", EnvClock::new());
editor.spec_mut().topology.outputs = (0..2)
    .map(|port| Source::Node(OutPort { node: clock, port }))
    .collect();
editor.commit().expect("the graph compiles");

let engine = Engine::new(&transport, &mut editor, executor).expect("within the limits");

// Transport is a `motion`/`settings` split rather than a `play()` method:
// settings anyone may store into, motion a state machine that may defer or
// reject. Queued events apply at the next block, which the audio callback
// renders.
transport.settings.set_tempo(Bpm(90.0));
transport
    .motion
    .try_send(MotionEvent::locate_and_play(Beat(8.0)))
    .expect("the motion queue has room at startup");

let mut block = vec![0.0f32; 256 * 2];
engine.process(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));

// The first frame carries beat 8 on the clock's ports; the block moved the
// playhead on by 256 frames at 90 BPM.
assert_eq!((block[0], block[1]), (8.0, 0.0));
assert!(transport.is_rolling());
assert!(transport.beat() > Beat(8.0));
```

## The one thing `bevy` adds

tutti-core is a std crate whose DSP graph runtime is Bevy-agnostic. The optional
`bevy` feature is **off by default** and adds exactly one thing: a `Component`
derive on `AudioNode`, so an entity can *be* a node in the graph. Everything
that reconciles against it — the set hierarchy, the graph resources, the param
components, the declarative wiring — lives in `bevy_tutti::graph`. A non-Bevy
host edits the native graph through `tutti_graph::Editor` (or builds one with
`GraphBuilder`) directly.

## Features

All are off by default (`default = []`), which is what keeps this crate
Bevy-free unless a consumer asks:

- `bevy` — the `Component` derive on `AudioNode` described above.
- `bevy_ecs` — a back-compat alias for `bevy`, kept because sibling crates still
  spell it that way.
- `midi` — reserved; the MIDI subsystems are separate crates.
- `serde` — `Serialize`/`Deserialize` on the shared value vocabulary (forwards
  to `tutti-types/serde`). Off by default, since the engine itself never
  serializes.

There are no codec or asset features. Decoding a file (`Wave`, `FileIn`,
`can_decode`) and the Bevy `WaveAsset` are `tutti-io`'s, behind its own
`wav` / `flac` / `mp3` / `ogg` and `bevy` features.

## License

MIT OR Apache-2.0
