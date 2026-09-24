# tutti-core

Real-time audio engine core — DSP graph, transport, metering, latency.

## What this is

The engine's runtime kernel and its vocabulary: the DSP graph, playback
transport, level metering, and delay compensation. Sibling crates
(`tutti-plugin`, `tutti-sampler`, `tutti-nodes`, …) build on these types and
re-export them, so a consumer usually meets them through whichever subsystem it
already depends on.

- `dsp::Net` — the DSP graph itself. It is **fundsp's** `Net`, re-exported;
  tutti adds no wrapper around it.
- `Transport` — playback control, split into `settings` (anyone may store into)
  and `motion` (a state machine that may defer or reject). `TransportClock` puts
  the beat on graph ports, so beat-driven sources read musical time as a signal
  rather than consulting the transport.
- `MasterMeter` / `AudioTap` — level monitoring and the analysis tap.
- `latency` — delay compensation: explicit, opt-in, over any graph.
- `topology` — `compile(&Valid, &dyn Catalog, rate) -> Compiled`, turning
  `tutti_types::graph::Topology` (the graph as a *value*) into a `Net`. The value
  is the graph; `Net` is the interpreter, and nothing reads a topology back out
  of one. Only tests call it today: `bevy-tutti` builds the same value each
  rebuild but applies it incrementally to its running `Net`, which owns state
  no value can rebuild.
- `Engine` — the graph render the RT callback runs.

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

## Example — a graph, a backend, and a transport

The engine's centre in one block: build a `Net`, wire it, take the audio-thread
`backend`, then edit the graph and `commit` the edit across to it. `tutti-cpal`
does exactly this around a real device; here the backend is pulled by hand, so
the whole thing runs headless.

```rust
use tutti_core::dsp::Net;
use tutti_core::{AudioUnit, Beat, Bpm, MotionEvent, PdcDelay, Samples, Timeline, Transport};
use tutti_core::TransportClock;

let sample_rate = 48_000.0;
let transport = Transport::new(sample_rate);

// The clock is a node: beat-driven sources read musical time off their
// input ports rather than consulting the transport, so an offline render
// behaves identically to a live one.
let mut net = Net::new(0, 2);
let clock = net.push(Box::new(TransportClock::new(
    transport.clock_links(),
    sample_rate,
)));

// Any node with an input shows the wiring; this one is the compensation delay
// this crate owns. Tone generators and filters are `tutti-nodes`', a crate
// above this one.
let delay = net.push(Box::new(PdcDelay::<1>::new(Samples(64))));
net.connect(clock, 0, delay, 0);
// Fans the delay's one output across both device channels; without this
// every output edge stays `Port::Zero` and the graph renders silence.
net.pipe_output(delay);
net.check();

// The backend is the audio thread's half. There is exactly one, and after
// it exists every frontend edit needs a `commit` to reach it.
let mut backend = net.backend();
let mut frame = [0.0f32; 2];
backend.tick(&[], &mut frame);
assert_eq!(frame[0], frame[1]);

net.connect(clock, 0, delay, 0);
net.commit();

// Transport is a `motion`/`settings` split rather than a `play()` method:
// settings anyone may store into, motion a state machine that may defer or
// reject. Queued events apply on `drain`, which the audio callback runs.
transport.settings.set_tempo(Bpm(90.0));
transport.settings.set_beat(Beat(8.0));
transport
    .motion
    .try_send(MotionEvent::Play)
    .expect("the motion queue has room at startup");
transport.motion.drain();

assert!(transport.is_rolling());
assert_eq!(transport.beat(), Beat(8.0));
```

## The one thing `bevy` adds

tutti-core is a std crate whose DSP graph runtime is Bevy-agnostic. The optional
`bevy` feature is **off by default** and adds exactly one thing: a `Component`
derive on `AudioNode`, so an entity can *be* a node in the graph. Everything
that reconciles against it — the set hierarchy, the graph resources, the param
components, the declarative wiring — lives in `bevy_tutti::graph`. A non-Bevy
host wires nodes through `Net`'s `set_source` / `connect` API directly.

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
