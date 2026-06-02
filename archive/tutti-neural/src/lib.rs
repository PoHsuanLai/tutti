//! Neural audio inference for Tutti.
//!
//! `tutti-neural` runs neural models on a dedicated engine thread and exposes
//! two audio-thread units ([`Effect`] and `Synth`, the latter behind the
//! `midi` feature) that bridge the audio graph to that thread without
//! blocking it. The crate is framework-agnostic:
//! pair it with any backend that returns a [`Backend`] — e.g. `tutti-burn`
//! (Burn + wgpu), `tutti-ort` (ONNX Runtime), or your own.
//!
//! # Architecture
//!
//! Three stages, each expressed as an on-disk module:
//!
//! ```text
//! Stage 1 — Public API (lib, engine, asset)
//!     │ engine(cfg, factory) -> Engine
//!     │ Engine::{load_model, unload, effect, synth,
//!     │          event_sender, meter, is_healthy}
//!     │ NeuralModel / probe_model_file (disk assets)
//!     ▼
//! Stage 2 — Inference engine (engine::run, backend, metering)
//!     │ Folds Event (Cmd | Req) into LoopState.
//!     │ Drains the channel per tick, calls one Backend::forward, dispatches
//!     │ results via Response.
//!     ▼
//! Stage 3 — Audio-thread units + IPC (node, effect_node, synth_node, ipc)
//!     │ Each audio unit is an `InferenceNode<T: Trigger, S: Sink>`:
//!     │   Trigger ingests one tick of input and yields a buffer when
//!     │   inference is warranted; Sink receives the engine's response and
//!     │   renders the audio-thread output.
//!     │ Effect = InferenceNode<AudioBlock, SlotSink>.
//!     │ Synth  = InferenceNode<MidiBlock,  ParamSink>.
//!     │ Lock-free IPC via Slot (arc-swap) + bounded crossbeam channel.
//! ```
//!
//! # Quick start
//!
//! ```no_run
//! use std::path::Path;
//! use std::sync::Arc;
//! use tutti_neural::{engine, Config};
//!
//! # fn fake_backend(_cfg: tutti_neural::Config)
//! #     -> Result<tutti_neural::Backend, tutti_neural::BackendError>
//! # { unimplemented!() }
//! let engine = Arc::new(engine(Config::default(), Box::new(fake_backend))?);
//!
//! // Load a model from disk. The engine picks the backend by extension
//! // and runs a probe to measure forward latency and shape compatibility.
//! let loaded = engine.load_model(Path::new("my_model.onnx"))?;
//! let samples = loaded.report.latency_samples(engine.sample_rate());
//! println!("model latency: {:?} ({} samples)", loaded.report.latency, samples);
//!
//! // Build an audio node from the loaded model id. The probe's latency
//! // flows into PDC so the graph aligns around the model's delay.
//! let latency_samples = loaded.report.latency_samples(engine.sample_rate());
//! let _effect = engine.effect(loaded.id, 2, 512, latency_samples);
//! # Ok::<(), tutti_neural::Error>(())
//! ```
//!
//! # Backend contract
//!
//! A [`Backend`] is a plain struct of three `FnMut` closures — [`load`](Backend::load),
//! [`forward`](Backend::forward), [`unload`](Backend::unload) — plus the
//! static [`supported_extensions`](Backend::supported_extensions) list
//! and [`capabilities`](Backend::capabilities). The closures are not `Send`
//! — `Backend` lives entirely on the engine thread; only the
//! [`BackendFactory`] that produces it crosses a thread boundary.
//!
//! The engine routes [`Engine::load_model`] by extension: it picks the
//! first backend whose `supported_extensions` list contains the path's
//! extension and hands the path to that backend's `load` closure. Backends
//! that can't load the path return [`BackendError::UnsupportedFormat`].
//!
//! # Backpressure and batching
//!
//! The engine's input channel is a 256-slot bounded crossbeam channel.
//! `ipc::submit` uses `try_send`; when the channel is full the newest request
//! is dropped. There is no additional pending-queue, no
//! expected-nodes barrier, no timeout flush — each tick drains the channel
//! into a local `Vec<Request>`, calls `backend.forward` once over the whole
//! slice, and dispatches results. Backends that support batching (Burn, ORT)
//! collapse consecutive same-model requests internally.
//!
//! # Feature flags
//!
//! - `midi` — enables the `Synth` audio unit and its `MidiState`
//!   accumulator. Adds dependencies on `tutti-midi` and `tutti-midi-runtime`.
//! - `bevy_asset` — `impl TuttiStreamingAsset for NeuralModel` for Bevy asset
//!   pipelines.

pub mod asset;
pub mod backend;
mod builder;
pub mod effect_node;
pub mod engine;
pub mod error;
pub mod ipc;
pub mod metering;
pub mod model_id;
pub mod node;

#[cfg(feature = "midi")]
pub mod synth_node;

pub use asset::{NeuralModel, NeuralModelFormat, NeuralModelProbeError};
pub use backend::{
    batched_forward, Backend, BackendError, BackendFactory, Capabilities, Compatibility, Config,
    Forward, Model, ProbeReport,
};
pub use builder::{neural, onnx, NeuralBuilder};
pub use effect_node::{effect_node, AudioBlock, Effect, SlotSink};
pub use engine::{engine, Engine};
pub use error::{Error, Result};
pub use ipc::{
    ask, slot, submit, tensor_to_params, ArcPool, Ask, Command, ControlParams, Event, LoadedModel,
    Reply, Request, Response, Shape, Slot, SlotReader, SlotWriter,
};
pub use metering::{BatchSnapshot, Meter, Metrics, Micros, TimingSnapshot};
pub use model_id::ModelId;
pub use node::{InferenceNode, Sink, Trigger};

#[cfg(feature = "midi")]
pub use synth_node::{synth_node, MidiBlock, MidiState, ParamSink, Synth, MIDI_FEATURE_COUNT};
