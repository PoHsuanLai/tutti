# tutti-core

Real-time audio engine core — DSP graph render, transport, metering, latency.

## What this is

The engine's runtime kernel and its vocabulary: the per-block graph render,
playback transport, level metering, and delay compensation. Use it directly
when you drive the audio callback yourself (a custom device layer, an offline
renderer, a test harness); otherwise take the `tutti` facade, which re-exports
this crate as `tutti::core`, or `bevy-tutti` for a Bevy app. Sibling crates
(`tutti-plugin`, `tutti-sampler`, `tutti-nodes`, …) build on these types, so a
consumer often meets them through whichever subsystem it already depends on.

- `Engine` — the graph render the RT callback runs: each block it renders
  `tutti-graph`'s `Executor` (whose `Editor` the control thread keeps), folds
  the output to the device width, and applies transport commands on their
  frame.
- `Transport` — playback control, split into `settings` (anyone may store into)
  and `motion` (a state machine that may defer or reject), with commands
  timed to a frame or a beat (`MotionFsm::schedule`). The engine drives the
  transport's clock and hands each block its transport in `Env`, so a
  beat-driven node reads musical time per frame (`Env::for_each_beat`,
  `Env::transport_at`) rather than consulting the transport.
- `MasterMeter` / `AudioTap` — level monitoring and the analysis tap.
- `latency` — delay compensation as a pure pass over any graph: what each
  output needs, and (`latency::delays`) which ports to delay by how much. The
  graph compiler applies it; this crate inserts nothing.

- `prelude` — what a host driving the engine names, in one import.

A consumer that wants the whole engine behind one dependency takes `tutti`
(no Bevy) or `bevy-tutti` (a Bevy plugin); either can be mixed with direct
dependencies on these crates.

## What this crate does not own

- **MIDI.** The vocabulary is `tutti-midi-types`', the state machines
  `tutti-midi-runtime`'s, and the OS edge `tutti-midi-hardware`'s. `Engine` is
  MIDI-free; MIDI travels on the graph's event ports (tutti-graph), and the
  MIDI nodes are `tutti-midi-runtime`'s.
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
    Beat, Bpm, ChannelLayout, Engine, InterleavedMut, MotionEvent, NodeKey, SampleRate, Samples,
    Tail, Timeline, Transport,
};
use tutti_graph::{Cx, Editor, ForkByClone, Io, Node, Prepare, Shape, Status};

// A node reads the transport from each block's `Env`. This one puts the beat
// on two ports (whole beats, then the fraction). Tone generators and filters
// are `tutti-nodes`', a crate above this one.
#[derive(Clone)]
struct Beats;

impl Node for Beats {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO).with_tail(Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        cx.env.for_each_beat(|i, beat| {
            io.output(0)[i] = beat.floor().get() as f32;
            io.output(1)[i] = beat.fract().get() as f32;
        });
        Status::Modified
    }
    fn reset(&mut self) {}
}

let transport = Transport::new(48_000.0);

// The editor is the control thread's half of the graph; the executor goes to
// the engine. Every edit reaches it through a `commit`.
let (mut editor, executor) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(256)));

// Every insert says whether the node forks (for an export): `Beats` reads
// only its block's `Env`, so a clone of it is a fork.
let beats = NodeKey(1);
editor.insert(beats, "beats", ForkByClone(Beats));
editor.spec_mut().topology.outputs = (0..2)
    .map(|port| Source::Node(OutPort { node: beats, port }))
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

// The first frame carries beat 8 on the node's ports; the block moved the
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
host edits the graph through `tutti_graph::Editor` (or builds one with
`GraphBuilder`) directly.

## Features

All are off by default (`default = []`), which is what keeps this crate
Bevy-free unless a consumer asks:

- `bevy` — the `Component` derive on `AudioNode` described above.
- `bevy_ecs` — an alias for `bevy`.
- `midi` — gates nothing here; the MIDI subsystems are separate crates.
- `serde` — `Serialize`/`Deserialize` on the shared value vocabulary (forwards
  to `tutti-types/serde`). Off by default, since the engine itself never
  serializes.

There are no codec or asset features. Decoding a file (`Wave`, `FileIn`,
`can_decode`) and the Bevy `WaveAsset` are `tutti-io`'s, behind its own
`wav` / `flac` / `mp3` / `ogg` and `bevy` features.

## License

MIT OR Apache-2.0
