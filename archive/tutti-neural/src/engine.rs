//! The inference engine thread.
//!
//! A single bounded channel carries every [`Event`]: control-plane
//! [`Command`]s (register, unload, probe, …) and data-plane `Request`s
//! (actual inference). Each tick the engine drains the channel, folds
//! commands into an internal loop state, and batches whatever requests
//! arrived into one [`Backend::forward`] call.
//!
//! # Lifecycle
//!
//! 1. [`engine()`] spawns the engine thread. Inside it, the [`BackendFactory`]
//!    closure runs exactly once to produce the [`Backend`]. That `Backend`
//!    stays on the thread for the engine's lifetime.
//! 2. [`Engine`] methods ([`load_model`](Engine::load_model),
//!    [`unload`](Engine::unload)) send one [`Command`] and block on the
//!    paired [`Ask`](crate::ipc::Ask) for a reply.
//! 3. [`Engine::event_sender`] hands audio nodes a `Sender<Event>` they use
//!    to submit requests lock-free via [`submit`](crate::ipc::submit).
//! 4. [`Engine`] drops: the thread receives `Command::Shutdown` and joins.
//!
//! # Batching, ordering, backpressure
//!
//! Request-ordering relative to commands is FIFO (single channel). That
//! preserves the invariant that `Command::Unload` cannot race past a
//! still-pending `Request` for the same model.
//!
//! Within a tick all collected requests go to `backend.forward` in the order
//! they arrived. Backends that implement same-model run detection (Burn,
//! ORT) collapse consecutive same-id requests into one tensor call.
//!
//! Backpressure is the bounded channel — 256 slots, `try_send` drops the
//! newest request when full. No extra queue, no shedding heuristic.
//!
//! [`Event`]: crate::ipc::Event
//! [`Command`]: crate::ipc::Command

use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::backend::{Backend, BackendError, BackendFactory, Compatibility, Config, ProbeReport};
use crate::error::{Error, Result};
use crate::ipc::{ask, tensor_to_params, Command, Event, LoadedModel, Request, Response};
use crate::metering::Meter;
use crate::model_id::ModelId;

/// Number of forward passes averaged when computing [`ProbeReport::latency`].
const PROBE_SAMPLES: usize = 3;

/// Event-channel capacity. Every [`Event`](crate::ipc::Event) — command or
/// request — flows through the same 256-slot bounded channel. Overflow at
/// the audio thread is handled by dropping the newest request in
/// [`submit`](crate::ipc::submit).
const CHANNEL_CAPACITY: usize = 256;

/// How long the engine sleeps between ticks when idle. Short enough that
/// request → result latency is dominated by the backend, not by the loop.
const IDLE_SLEEP: Duration = Duration::from_micros(100);

/// Handle to the running engine thread.
///
/// Cheaply cloned via `Arc<Engine>` in callers. Dropping the last handle
/// sends [`Command::Shutdown`] and joins the thread.
pub struct Engine {
    event_tx: Sender<Event>,
    thread: Option<JoinHandle<()>>,
    meter: Arc<Meter>,
    health_timeout: Duration,
    buffer_size: usize,
    sample_rate: f64,
}

/// Engine-thread state folded by [`step_command`]. `model_count` is a
/// display metric, not load-bearing.
struct LoopState {
    model_count: u32,
}

impl LoopState {
    fn new() -> Self {
        Self { model_count: 0 }
    }
}

/// Spawn the engine thread and return a handle.
///
/// The factory closure runs *on the engine thread* to produce the [`Backend`].
/// `Backend`'s internal closures are not `Send` — they never leave the thread.
/// Only the factory itself crosses the thread boundary.
///
/// # Errors
///
/// Fails with [`Error::Inference`] if the engine thread can't be spawned.
/// Backend-init errors are logged and the thread exits without serving
/// requests — caller-facing method calls then fail with
/// [`Error::InferenceThreadSend`] / [`Error::InferenceThreadRecv`].
pub fn engine(cfg: Config, factory: BackendFactory) -> Result<Engine> {
    let (event_tx, event_rx) = bounded::<Event>(CHANNEL_CAPACITY);
    let meter = Arc::new(Meter::new());
    let health_timeout = cfg.health_timeout;
    let buffer_size = cfg.buffer_size;
    let sample_rate = cfg.sample_rate;
    let meter_clone = Arc::clone(&meter);
    let thread = std::thread::Builder::new()
        .name("neural-engine".into())
        .spawn(move || {
            #[cfg(target_os = "macos")]
            set_realtime_priority();
            run(cfg, factory, event_rx, meter_clone);
        })
        .map_err(|e| Error::Inference(format!("Failed to spawn engine thread: {}", e)))?;

    Ok(Engine {
        event_tx,
        thread: Some(thread),
        meter,
        health_timeout,
        buffer_size,
        sample_rate,
    })
}

impl Engine {
    /// Load a model from disk, probe it for shape + latency, and return a
    /// typed descriptor.
    ///
    /// The engine thread picks the backend whose
    /// [`Backend::supported_extensions`] matches the path's extension,
    /// calls [`Backend::load`], then runs a short probe at the engine's
    /// configured `buffer_size` to measure forward latency and classify
    /// shape compatibility. Returns [`BackendError::UnsupportedFormat`]
    /// surfaced as [`Error::Inference`] when no backend claims the extension.
    ///
    /// The returned [`ProbeReport`] carries the measured latency. Convert
    /// to samples via [`ProbeReport::latency_samples`] and feed it to the
    /// graph's PDC system so downstream nodes align around the model's
    /// constant processing delay.
    ///
    /// ```no_run
    /// # use std::path::Path;
    /// # use tutti_neural::{Engine, Result};
    /// # fn go(engine: &Engine) -> Result<()> {
    /// let loaded = engine.load_model(Path::new("my_model.onnx"))?;
    /// let samples = loaded.report.latency_samples(engine.sample_rate());
    /// println!("model latency: {:?} ({} samples)", loaded.report.latency, samples);
    /// # Ok(()) }
    /// ```
    pub fn load_model(&self, path: &Path) -> Result<LoadedModel> {
        let (ask_resp, reply) = ask::<std::result::Result<LoadedModel, BackendError>>();
        self.send_cmd(Command::Load {
            path: path.to_path_buf(),
            reply,
        })?;
        ask_resp
            .recv()
            .map_err(|_| Error::InferenceThreadRecv)?
            .map_err(|e| Error::Inference(e.to_string()))
    }

    /// Unload a registered model, freeing its backend resources.
    ///
    /// Returns [`BackendError::NotFound`] surfaced as [`Error::Inference`]
    /// if the id is unknown.
    pub fn unload(&self, id: ModelId) -> Result<()> {
        let (ask_resp, reply) = ask::<std::result::Result<(), BackendError>>();
        self.send_cmd(Command::Unload { id, reply })?;
        ask_resp
            .recv()
            .map_err(|_| Error::InferenceThreadRecv)?
            .map_err(|e| Error::Inference(e.to_string()))
    }

    /// Build an [`Effect`](crate::effect_node::Effect) audio unit driven by the given model.
    ///
    /// `latency_samples` is the constant processing delay the node will
    /// report to the graph's PDC — typically
    /// [`loaded.report.latency_samples(engine.sample_rate())`](crate::ProbeReport::latency_samples).
    pub fn effect(
        self: &Arc<Self>,
        id: ModelId,
        channels: usize,
        buffer_size: usize,
        latency_samples: usize,
    ) -> crate::effect_node::Effect {
        crate::effect_node::effect_node(
            id,
            channels,
            buffer_size,
            latency_samples,
            self.event_sender(),
        )
    }

    /// Build a [`Synth`](crate::synth_node::Synth) audio unit driven by the
    /// given model. Requires the `midi` feature.
    ///
    /// `latency_samples` is the constant processing delay the node will
    /// report to the graph's PDC — typically
    /// [`loaded.report.latency_samples(engine.sample_rate())`](crate::ProbeReport::latency_samples).
    #[cfg(feature = "midi")]
    pub fn synth(
        self: &Arc<Self>,
        id: ModelId,
        sample_rate: f32,
        buffer_size: usize,
        latency_samples: usize,
    ) -> crate::synth_node::Synth {
        crate::synth_node::synth_node(
            id,
            sample_rate,
            buffer_size,
            latency_samples,
            self.event_sender(),
        )
    }

    /// Clone of the event channel sender. Audio nodes submit requests
    /// through this using [`submit`](crate::ipc::submit).
    pub fn event_sender(&self) -> Sender<Event> {
        self.event_tx.clone()
    }

    /// Access to the meter for metrics + health tracking.
    pub fn meter(&self) -> &Arc<Meter> {
        &self.meter
    }

    /// Engine's configured audio buffer size in frames.
    pub fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    /// Engine's configured sample rate in Hz.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// `true` if the engine thread has heartbeated within
    /// `config.health_timeout`. A stalled backend or a panicked engine
    /// thread will return `false`.
    pub fn is_healthy(&self) -> bool {
        self.meter.is_alive(self.health_timeout)
    }

    fn send_cmd(&self, cmd: Command) -> Result<()> {
        self.event_tx
            .send(Event::Cmd(cmd))
            .map_err(|_| Error::InferenceThreadSend)
    }

    /// Signal the engine thread to exit and wait for it. Idempotent — called
    /// automatically by [`Drop`].
    pub fn shutdown(&mut self) {
        let _ = self.event_tx.send(Event::Cmd(Command::Shutdown));
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(cfg: Config, factory: BackendFactory, rx: Receiver<Event>, meter: Arc<Meter>) {
    let probe_features = cfg.buffer_size;
    let mut backend = match factory(cfg) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("backend init failed: {}", e);
            return;
        }
    };

    tracing::info!(
        "neural engine started (backend: {})",
        backend.capabilities.name
    );

    let mut state = LoopState::new();
    let mut pending: Vec<Request> = Vec::with_capacity(CHANNEL_CAPACITY);

    loop {
        meter.heartbeat();

        let mut shutdown = false;

        // Drain all available events this tick.
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::Cmd(cmd) => {
                    if step_command(&mut state, cmd, &mut backend, &meter, probe_features)
                        .is_break()
                    {
                        shutdown = true;
                    }
                }
                Event::Req(req) => pending.push(req),
            }
        }

        if !pending.is_empty() {
            forward_batch(&mut backend, &mut pending, &meter);
        }

        if shutdown {
            tracing::info!("neural engine shutting down");
            return;
        }

        if pending.is_empty() {
            std::thread::sleep(IDLE_SLEEP);
        }
    }
}

/// Handle one [`Command`]. Returns [`ControlFlow::Break`] on `Shutdown`.
fn step_command(
    state: &mut LoopState,
    cmd: Command,
    backend: &mut Backend,
    meter: &Meter,
    probe_features: usize,
) -> ControlFlow<()> {
    match cmd {
        Command::Load { path, reply } => {
            let result = load_and_probe(backend, &path, probe_features);
            if result.is_ok() {
                state.model_count += 1;
                meter.set_model_count(state.model_count);
            }
            reply.send(result);
        }
        Command::Unload { id, reply } => {
            let result = (backend.unload)(id);
            if result.is_ok() {
                state.model_count = state.model_count.saturating_sub(1);
                meter.set_model_count(state.model_count);
            }
            reply.send(result);
        }
        Command::Shutdown => return ControlFlow::Break(()),
    }
    ControlFlow::Continue(())
}

/// Load + probe: ask the backend to load `path`, then run `PROBE_SAMPLES`
/// zero-filled forwards at `probe_features` to measure forward latency and
/// shape compatibility. Unloads the model on probe failure so the engine
/// doesn't accumulate dead ids.
fn load_and_probe(
    backend: &mut Backend,
    path: &Path,
    probe_features: usize,
) -> std::result::Result<LoadedModel, BackendError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_string();
    if !ext.is_empty() && !backend.handles_extension(&ext) {
        return Err(BackendError::UnsupportedFormat(ext));
    }
    let id = (backend.load)(path)?;

    let input = vec![0.0f32; probe_features];
    let mut samples = Vec::with_capacity(PROBE_SAMPLES);
    let mut last_output_len = 0usize;
    for _ in 0..PROBE_SAMPLES {
        let start = Instant::now();
        let results = match (backend.forward)(&[(id, input.clone(), probe_features)]) {
            Ok(r) => r,
            Err(e) => {
                let _ = (backend.unload)(id);
                return Err(e);
            }
        };
        samples.push(start.elapsed());
        last_output_len = results.into_iter().next().unwrap_or_default().len();
    }
    samples.sort();
    let latency = samples[samples.len() / 2];

    let compatibility = if last_output_len == probe_features {
        Compatibility::Ok
    } else {
        Compatibility::ShapeMismatch {
            input_len: probe_features,
            output_len: last_output_len,
        }
    };

    Ok(LoadedModel {
        id,
        report: ProbeReport {
            input_shape: vec![1, probe_features as i64],
            output_shape: vec![1, last_output_len as i64],
            latency,
            compatibility,
        },
    })
}

/// Run one batched forward pass over every pending request. On error, the
/// input is passed through so the audio thread stays glitch-free.
fn forward_batch(backend: &mut Backend, pending: &mut Vec<Request>, meter: &Meter) {
    let requests: Vec<Request> = std::mem::take(pending);
    let batch_size = requests.len();

    let forward_reqs: Vec<(ModelId, Vec<f32>, usize)> = requests
        .iter()
        .map(|r| (r.id, r.input.to_vec(), r.shape.features))
        .collect();

    let start = Instant::now();
    let outcome = (backend.forward)(&forward_reqs);

    meter.record_inference(start.elapsed());
    meter.record_batch(batch_size);

    match outcome {
        Ok(results) => {
            for (req, out) in requests.into_iter().zip(results) {
                dispatch(req, out);
            }
        }
        Err(e) => {
            tracing::error!("batched forward failed: {}", e);
            for req in requests {
                let passthrough = req.input.to_vec();
                dispatch(req, passthrough);
            }
        }
    }
}

fn dispatch(req: Request, result: Vec<f32>) {
    match req.resp {
        Response::Audio(mut w) => w.write(&result),
        Response::Params { tx, buffer_size } => {
            let params = tensor_to_params(&result, buffer_size);
            let _ = tx.try_send(params);
        }
    }
}

/// Elevate the engine thread to `SCHED_RR` priority 47.
///
/// Chosen above background threads but well below audio-callback threads so
/// the engine won't starve real-time audio even on a pegged CPU.
#[cfg(target_os = "macos")]
fn set_realtime_priority() {
    unsafe {
        let thread = libc::pthread_self();
        let mut policy = 0i32;
        let mut param: libc::sched_param = std::mem::zeroed();
        if libc::pthread_getschedparam(thread, &mut policy, &mut param) == 0 {
            param.sched_priority = 47;
            let _ = libc::pthread_setschedparam(thread, libc::SCHED_RR, &param);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Capabilities;
    use crate::ipc::{slot, submit, ControlParams, Response, Shape};
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;

    /// Stub backend: every `load` returns the same passthrough model;
    /// `forward` echoes input. `supported_extensions` accepts `.stub`.
    fn stub_factory() -> BackendFactory {
        Box::new(|_cfg| {
            let loaded: Rc<RefCell<Vec<ModelId>>> = Rc::new(RefCell::new(Vec::new()));
            let l1 = loaded.clone();
            let l2 = loaded.clone();
            let l3 = loaded;
            Ok(Backend {
                load: Box::new(move |_path| {
                    let id = ModelId::new();
                    l1.borrow_mut().push(id);
                    Ok(id)
                }),
                register_model: Box::new(|_m| Err(BackendError::UnknownModel)),
                forward: Box::new(move |reqs| {
                    let map = l2.borrow();
                    let mut out = Vec::with_capacity(reqs.len());
                    for (id, data, _) in reqs {
                        if map.contains(id) {
                            out.push(data.clone());
                        } else {
                            return Err(BackendError::NotFound(*id));
                        }
                    }
                    Ok(out)
                }),
                unload: Box::new(move |id| {
                    let mut v = l3.borrow_mut();
                    if let Some(pos) = v.iter().position(|x| *x == id) {
                        v.remove(pos);
                        Ok(())
                    } else {
                        Err(BackendError::NotFound(id))
                    }
                }),
                supported_extensions: &["stub"],
                capabilities: Capabilities {
                    name: "stub",
                    has_gpu: false,
                },
            })
        })
    }

    fn start_engine() -> Engine {
        engine(Config::default(), stub_factory()).unwrap()
    }

    fn stub_path() -> PathBuf {
        PathBuf::from("dummy.stub")
    }

    #[test]
    fn test_engine_start_shutdown() {
        let mut e = start_engine();
        e.shutdown();
    }

    #[test]
    fn test_load_model_returns_loaded() {
        let e = start_engine();
        let loaded = e.load_model(&stub_path()).unwrap();
        assert_ne!(loaded.id.as_u64(), 0);
        assert!(matches!(loaded.report.compatibility, Compatibility::Ok));
        // Stub forward is instant — latency is a small duration, not a gate.
        assert!(loaded.report.latency < Duration::from_millis(10));
    }

    #[test]
    fn test_load_model_unsupported_extension_fails() {
        let e = start_engine();
        let err = e.load_model(&PathBuf::from("foo.bin")).unwrap_err();
        let Error::Inference(msg) = err else {
            panic!("expected Error::Inference, got {err:?}");
        };
        assert!(msg.contains("bin"), "got: {msg}");
    }

    #[test]
    fn test_unload_model() {
        let e = start_engine();
        let loaded = e.load_model(&stub_path()).unwrap();
        assert!(e.unload(loaded.id).is_ok());
        assert!(e.unload(loaded.id).is_err());
    }

    #[test]
    fn test_error_passthrough_on_forward_error() {
        // Factory whose forward always fails.
        let factory: BackendFactory = Box::new(|_cfg| {
            Ok(Backend {
                load: Box::new(|_p| Ok(ModelId::new())),
                register_model: Box::new(|_m| Err(BackendError::UnknownModel)),
                forward: Box::new(|_r| Err(BackendError::Forward("boom".into()))),
                unload: Box::new(|_id| Ok(())),
                supported_extensions: &["stub"],
                capabilities: Capabilities {
                    name: "failing",
                    has_gpu: false,
                },
            })
        });
        let e = engine(Config::default(), factory).unwrap();
        // Load will attempt probe which will fail and unload.
        assert!(e.load_model(&stub_path()).is_err());
    }

    #[test]
    fn test_rt_latency_roundtrip() {
        let e = start_engine();
        let loaded = e.load_model(&stub_path()).unwrap();

        let (reader, writer) = slot(2, 512);
        let input: Vec<f32> = (0..1024).map(|i| i as f32 / 1024.0).collect();
        let req = Request {
            id: loaded.id,
            input: Arc::from(input.as_slice()),
            shape: Shape::new(1, 1024),
            resp: Response::Audio(writer),
        };
        let submit_time = Instant::now();
        submit(&e.event_sender(), req);

        loop {
            if reader.has_output() {
                break;
            }
            if submit_time.elapsed() > Duration::from_millis(100) {
                panic!("timeout: output never appeared");
            }
            std::thread::sleep(Duration::from_micros(50));
        }
        let latency_us = submit_time.elapsed().as_micros();
        assert!(
            latency_us < 10_000,
            "latency {latency_us}us should be < 10ms"
        );
    }

    #[test]
    fn test_health() {
        let e = start_engine();
        std::thread::sleep(Duration::from_millis(5));
        assert!(e.is_healthy());
    }

    #[test]
    fn test_params_response_roundtrip() {
        let e = start_engine();
        let loaded = e.load_model(&stub_path()).unwrap();
        let (tx, rx) = bounded::<ControlParams>(4);
        let req = Request {
            id: loaded.id,
            input: Arc::from([440.0f32, 440.0, 0.5, 0.5].as_slice()),
            shape: Shape::new(1, 4),
            resp: Response::Params { tx, buffer_size: 2 },
        };
        submit(&e.event_sender(), req);
        let p = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(p.f0, vec![440.0, 440.0]);
        assert_eq!(p.amplitudes, vec![0.5, 0.5]);
    }

    #[test]
    fn test_slow_model_reports_high_latency() {
        // Factory where forward sleeps longer than a buffer period. The probe
        // measures it; the builder elsewhere no longer gates on it — this test
        // just verifies the measurement surfaces.
        let factory: BackendFactory = Box::new(|_cfg| {
            Ok(Backend {
                load: Box::new(|_p| Ok(ModelId::new())),
                register_model: Box::new(|_m| Err(BackendError::UnknownModel)),
                forward: Box::new(|reqs| {
                    std::thread::sleep(Duration::from_millis(50));
                    Ok(reqs.iter().map(|(_, d, _)| d.clone()).collect())
                }),
                unload: Box::new(|_id| Ok(())),
                supported_extensions: &["stub"],
                capabilities: Capabilities {
                    name: "slow",
                    has_gpu: false,
                },
            })
        });
        let e = engine(Config::default(), factory).unwrap();
        let loaded = e.load_model(&stub_path()).unwrap();
        // 50ms >> 512/48000 ≈ 10.6ms; in samples that's > buffer_size.
        assert!(loaded.report.latency >= Duration::from_millis(40));
        assert!(loaded.report.latency_samples(48_000.0) > e.buffer_size());
    }
}
