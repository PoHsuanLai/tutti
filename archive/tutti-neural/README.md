# Tutti Neural

Neural audio inference for Tutti.

## What this is

Framework-agnostic orchestration around an off-thread inference engine. Pair
[`Engine`](crate::Engine) with any backend constructor that returns a
[`Backend`](crate::Backend) — e.g. [`tutti-burn`](../tutti-burn) for native Burn
models or [`tutti-ort`](../tutti-ort) for runtime ONNX loading.

## Quick start

```rust,ignore
use tutti_neural::{engine, Config};

let engine = engine(Config::default(), Box::new(tutti_burn::backend))?;

// A closure-style model.
let (effect, _id) = engine.from_closure(|audio| audio.to_vec())?;

// A native backend model.
let id = engine.register_model(tutti_burn::model(cpu_factory, gpu_factory))?;
let effect = engine.effect(id, 2, 512);
```

## How it works

- **Engine thread.** One OS thread owns the [`Backend`]. Commands and requests
  flow in over a single bounded channel; per-tick the engine drains and batches
  them in one [`Backend::forward`] call.
- **Audio-thread nodes.** [`effect_node`](crate::effect_node) and
  [`synth_node`](crate::synth_node) compose three named leaves
  (`Accumulator`, `Submitter`, `Reader`) — each a small struct with a narrow API.
- **Lock-free IPC.** A per-node [`Slot`](crate::Slot) moves processed audio back
  to the audio thread via [`arc_swap`].
- **Bounded-channel backpressure.** `try_send` drops the newest request when the
  engine's 256-slot channel is full.

## License

MIT OR Apache-2.0
