//! Backend contract — a struct of closures, not a trait.
//!
//! # Why not a trait?
//!
//! The engine only calls three methods on a backend: load a model from a
//! path, run one batched forward pass, unload a model. Expressing those as
//! a [`Backend`] struct of `FnMut` closures eliminates ceremony around a
//! trait object with `as_any_mut` downcasts: each backend captures its own
//! internal state (`Rc<RefCell<OrtState>>`, …) directly inside the closures.
//!
//! # Backend authors
//!
//! A backend crate exports a single entry point:
//!
//! - `fn backend(cfg: Config) -> Result<Backend, BackendError>` — the
//!   [`BackendFactory`] callers pass to [`engine`](crate::engine()). The
//!   backend declares which file extensions it handles via
//!   [`Backend::supported_extensions`] and loads models via
//!   [`Backend::load`].
//!
//! # Example backend skeleton
//!
//! ```ignore
//! pub fn backend(_cfg: Config) -> Result<Backend, BackendError> {
//!     let state = Rc::new(RefCell::new(MyState::new()?));
//!     let s1 = state.clone();
//!     let s2 = state.clone();
//!     let s3 = state;
//!     Ok(Backend {
//!         load:    Box::new(move |path| s1.borrow_mut().load(path)),
//!         forward: Box::new(move |r| s2.borrow_mut().forward(r)),
//!         unload:  Box::new(move |id| s3.borrow_mut().unload(id)),
//!         supported_extensions: &["onnx"],
//!         capabilities: Capabilities { name: "MyBackend", has_gpu: false },
//!     })
//! }
//! ```

use std::any::Any;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::model_id::ModelId;

/// A closure-style model: `(data, [batch, features]) -> output`.
///
/// Kept as a backend-internal type used by backends that need to wrap a
/// pure function into their tensor pipeline (e.g. Burn). Not part of the
/// public registration surface — the app-facing way to register a model
/// is [`Engine::load_model`](crate::Engine::load_model).
pub type Forward = Box<dyn Fn(&[f32], [usize; 2]) -> Vec<f32> + Send>;

/// Opaque backend-specific model value.
///
/// Used by [`Backend::register_model`] for backends that can't express
/// their registration through a path load alone (Burn requires the user's
/// `Module<B>` struct at compile time — see `tutti_burn::model`).
pub struct Model(Box<dyn Any + Send>);

impl Model {
    pub fn new<T: Any + Send>(value: T) -> Self {
        Self(Box::new(value))
    }

    /// Recover the concrete backend-specific value. Backend crates call this
    /// inside their `register_model` closure to decode a [`Model`] they
    /// themselves produced.
    pub fn downcast<T: Any>(self) -> Result<Box<T>, Self> {
        self.0.downcast::<T>().map_err(Self)
    }
}

impl fmt::Debug for Model {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Model").finish_non_exhaustive()
    }
}

/// Engine-wide configuration, forwarded to the backend factory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Health timeout. If the engine thread hasn't sent a heartbeat for
    /// longer than this, [`Engine::is_healthy`](crate::Engine::is_healthy)
    /// returns false. Default 500ms.
    pub health_timeout: Duration,
    /// Audio buffer size in frames. Combined with [`sample_rate`](Self::sample_rate)
    /// gives the buffer period; [`Engine::load_model`](crate::Engine::load_model)
    /// classifies models as realtime-safe iff their forward latency fits
    /// inside one such period. Default 512.
    pub buffer_size: usize,
    /// Audio sample rate in Hz. Combined with [`buffer_size`](Self::buffer_size)
    /// to derive the buffer period. Default 48000.
    pub sample_rate: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            health_timeout: Duration::from_millis(500),
            buffer_size: 512,
            sample_rate: 48_000.0,
        }
    }
}

impl Config {
    /// The audio buffer period — the wall-clock budget one forward pass has
    /// to fit inside to be RT-safe.
    pub fn buffer_period(&self) -> Duration {
        let secs = self.buffer_size as f64 / self.sample_rate;
        Duration::from_secs_f64(secs)
    }
}

/// Backend-side errors.
///
/// These surface to callers as [`Error::Inference`](crate::Error::Inference)
/// carrying the stringified variant.
#[derive(Debug, Clone)]
pub enum BackendError {
    /// Requested model id isn't registered.
    NotFound(ModelId),
    /// Forward pass failed — backend-specific detail in the `String`.
    Forward(String),
    /// Backend initialisation failed (bad config, missing GPU adapter, …).
    Init(String),
    /// Model file format (extension) not handled by this backend.
    UnsupportedFormat(String),
    /// Model file could not be loaded — IO error, parse error, etc.
    LoadFailed(String),
    /// [`Model`] was produced by a different backend. The
    /// [`Backend::register_model`] closure's downcast didn't recognise
    /// the payload type.
    UnknownModel,
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "model not found: {}", id),
            Self::Forward(msg) => write!(f, "forward failed: {}", msg),
            Self::Init(msg) => write!(f, "backend init failed: {}", msg),
            Self::UnsupportedFormat(ext) => {
                write!(f, "backend does not support format: {}", ext)
            }
            Self::LoadFailed(msg) => write!(f, "load failed: {}", msg),
            Self::UnknownModel => {
                write!(f, "model value has unexpected type for this backend")
            }
        }
    }
}

impl std::error::Error for BackendError {}

/// Probe report for a loaded model.
///
/// Produced by [`Engine::load_model`](crate::Engine::load_model). Carries the
/// model's tensor shapes and its measured forward latency. The latency is
/// reported as a [`Duration`]; convert to audio samples via
/// [`Self::latency_samples`] when publishing into the graph's PDC system.
///
/// No realtime-safety flag lives here on purpose. Whether a forward pass
/// fits in a buffer period is machine-dependent — callers who care should
/// compute it from [`Self::latency`] and their own buffer settings, and
/// rely on [`crate::Meter::record_inference`] for live xrun detection.
#[derive(Debug, Clone)]
pub struct ProbeReport {
    /// Input tensor shape. First dimension may be `-1` for dynamic batch.
    pub input_shape: Vec<i64>,
    /// Output tensor shape.
    pub output_shape: Vec<i64>,
    /// Median latency over several zero-filled forwards at the engine's
    /// configured `buffer_size`.
    pub latency: Duration,
    /// Whether output length equals input length (same-rate audio).
    pub compatibility: Compatibility,
}

impl ProbeReport {
    /// Latency expressed in whole audio samples at `sample_rate`.
    ///
    /// This is the value to hand to the graph's PDC so the rest of the
    /// graph aligns around the model's constant processing delay.
    pub fn latency_samples(&self, sample_rate: f64) -> usize {
        (self.latency.as_secs_f64() * sample_rate).round() as usize
    }

    /// Convenience: `true` iff the model is same-rate (produces one output
    /// sample per input sample).
    pub fn is_usable(&self) -> bool {
        matches!(self.compatibility, Compatibility::Ok)
    }
}

/// Same-rate audio models must produce one output sample per input sample.
/// Anything else is incompatible and the concrete mismatch travels in the
/// variant payload rather than a human-readable string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compatibility {
    /// Output length matches input length — usable.
    Ok,
    /// Length mismatch — not a same-rate audio model.
    ShapeMismatch { input_len: usize, output_len: usize },
}

impl fmt::Display for Compatibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => write!(f, "OK"),
            Self::ShapeMismatch {
                input_len,
                output_len,
            } => write!(
                f,
                "output length {} != input length {}",
                output_len, input_len
            ),
        }
    }
}

/// Display-only properties of a backend. Purely informational.
#[derive(Debug, Clone)]
pub struct Capabilities {
    /// Display name (appears in logs, metrics, user-facing UI).
    pub name: &'static str,
    /// Hint that inference runs on a dedicated accelerator. The engine
    /// doesn't use this to route anything — it's passed through to UIs.
    pub has_gpu: bool,
}

type LoadFn = Box<dyn FnMut(&Path) -> Result<ModelId, BackendError>>;
type ForwardFn =
    Box<dyn FnMut(&[(ModelId, Vec<f32>, usize)]) -> Result<Vec<Vec<f32>>, BackendError>>;

/// Backend operations as a struct of closures.
///
/// Lives entirely on the engine thread. The closures are **not** `Send` —
/// only the [`BackendFactory`] that produces `Backend` is. This lets
/// implementations capture `Rc<RefCell<_>>` state without an `Arc<Mutex<_>>`.
pub struct Backend {
    /// Load a model from a file. The path's extension should match one of
    /// [`Self::supported_extensions`]; otherwise return
    /// [`BackendError::UnsupportedFormat`]. On success returns a fresh
    /// [`ModelId`].
    pub load: LoadFn,
    /// Register a backend-specific native [`Model`] value. Used by
    /// backends (like Burn) that can't express registration through a
    /// file path alone — Burn's weights load into a user-supplied
    /// `Module<B>` struct known at compile time, so the registration
    /// carries a closure that constructs the Module on the engine thread.
    ///
    /// The backend's closure is responsible for downcasting `Model` to
    /// its own spec type; mismatches return [`BackendError::UnknownModel`].
    pub register_model: Box<dyn FnMut(Model) -> Result<ModelId, BackendError>>,
    /// Run one batched forward pass. Requests arrive in engine-arrival
    /// order; backends are free to collapse consecutive same-id entries
    /// into one tensor call. Results must come back in the same order.
    pub forward: ForwardFn,
    /// Free resources for one model. Unknown ids should return
    /// [`BackendError::NotFound`].
    pub unload: Box<dyn FnMut(ModelId) -> Result<(), BackendError>>,
    /// File extensions this backend handles (lower-case, without the leading
    /// dot). The engine dispatches `load_model(path)` by matching the path's
    /// extension against this list across every registered backend.
    pub supported_extensions: &'static [&'static str],
    /// Informational capabilities (name, `has_gpu`).
    pub capabilities: Capabilities,
}

impl Backend {
    /// Whether this backend claims to handle `extension` (case-insensitive,
    /// without leading dot).
    pub fn handles_extension(&self, extension: &str) -> bool {
        let lower = extension.to_ascii_lowercase();
        self.supported_extensions.contains(&lower.as_str())
    }
}

/// Factory type handed to [`engine`](crate::engine()). Runs exactly once on the
/// engine thread to produce the per-thread [`Backend`].
pub type BackendFactory = Box<dyn FnOnce(Config) -> Result<Backend, BackendError> + Send>;

/// Group consecutive same-id requests with matching feature dimensions into
/// one batched forward call; fall back to per-request when shapes diverge.
///
/// Backends call this from their [`Backend::forward`] closure to get
/// uniform same-id batching without re-implementing the grouping loop.
/// `forward_one` returns:
///
/// - `Some(output)` — the model's flat output for the requested shape.
/// - `None` — unknown id; the helper passes the input through unchanged so
///   the audio thread stays glitch-free.
pub fn batched_forward<F>(
    requests: &[(ModelId, Vec<f32>, usize)],
    mut forward_one: F,
) -> Vec<Vec<f32>>
where
    F: FnMut(ModelId, &[f32], [usize; 2]) -> Option<Vec<f32>>,
{
    let mut results: Vec<Vec<f32>> = Vec::with_capacity(requests.len());
    let mut i = 0;
    while i < requests.len() {
        let model_id = requests[i].0;
        let run_end = requests[i..]
            .iter()
            .position(|r| r.0 != model_id)
            .map(|p| i + p)
            .unwrap_or(requests.len());
        let run = &requests[i..run_end];

        if run.len() == 1 {
            let (_, ref input, features) = run[0];
            match forward_one(model_id, input, [1, features]) {
                Some(out) => results.push(out),
                None => results.push(input.clone()),
            }
        } else {
            let first_dim = run[0].2;
            let can_batch = run.iter().all(|(_, _, d)| *d == first_dim);
            if can_batch {
                let batch_size = run.len();
                let mut all_input = Vec::with_capacity(batch_size * first_dim);
                for (_, data, _) in run {
                    all_input.extend_from_slice(data);
                }
                match forward_one(model_id, &all_input, [batch_size, first_dim]) {
                    Some(all_output) => {
                        let output_dim = all_output.len() / batch_size;
                        for j in 0..batch_size {
                            let s = j * output_dim;
                            results.push(all_output[s..s + output_dim].to_vec());
                        }
                    }
                    None => {
                        for (_, data, _) in run {
                            results.push(data.clone());
                        }
                    }
                }
            } else {
                for (_, data, feat_dim) in run {
                    match forward_one(model_id, data, [1, *feat_dim]) {
                        Some(out) => results.push(out),
                        None => results.push(data.clone()),
                    }
                }
            }
        }
        i = run_end;
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_buffer_period() {
        let cfg = Config {
            buffer_size: 512,
            sample_rate: 48_000.0,
            ..Config::default()
        };
        // 512 / 48000 ≈ 10.667 ms
        let period = cfg.buffer_period();
        assert!((period.as_secs_f64() - 512.0 / 48_000.0).abs() < 1e-9);
    }

    #[test]
    fn test_handles_extension_case_insensitive() {
        let capabilities = Capabilities {
            name: "stub",
            has_gpu: false,
        };
        let backend = Backend {
            load: Box::new(|_| Err(BackendError::LoadFailed("unused".into()))),
            register_model: Box::new(|_| Err(BackendError::UnknownModel)),
            forward: Box::new(|_| Ok(vec![])),
            unload: Box::new(|_| Ok(())),
            supported_extensions: &["onnx"],
            capabilities,
        };
        assert!(backend.handles_extension("onnx"));
        assert!(backend.handles_extension("ONNX"));
        assert!(backend.handles_extension("OnNx"));
        assert!(!backend.handles_extension("pt"));
    }

    #[test]
    fn test_batched_forward_single_request_one_call() {
        let id = ModelId::new();
        let mut calls: Vec<[usize; 2]> = Vec::new();
        let out = batched_forward(&[(id, vec![1.0, 2.0, 3.0], 3)], |_, input, shape| {
            calls.push(shape);
            Some(input.to_vec())
        });
        assert_eq!(calls, vec![[1, 3]]);
        assert_eq!(out, vec![vec![1.0, 2.0, 3.0]]);
    }

    #[test]
    fn test_batched_forward_same_id_same_features_batches() {
        let id = ModelId::new();
        let mut shapes: Vec<[usize; 2]> = Vec::new();
        let out = batched_forward(
            &[
                (id, vec![1.0, 2.0, 3.0, 4.0], 4),
                (id, vec![5.0, 6.0, 7.0, 8.0], 4),
            ],
            |_, input, shape| {
                shapes.push(shape);
                Some(input.to_vec())
            },
        );
        assert_eq!(shapes, vec![[2, 4]], "should batch into one call");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(out[1], vec![5.0, 6.0, 7.0, 8.0]);
    }

    #[test]
    fn test_batched_forward_same_id_different_features_unbatched() {
        let id = ModelId::new();
        let mut shapes: Vec<[usize; 2]> = Vec::new();
        let out = batched_forward(
            &[
                (id, vec![1.0, 2.0, 3.0], 3),
                (id, vec![4.0, 5.0, 6.0, 7.0], 4),
            ],
            |_, input, shape| {
                shapes.push(shape);
                Some(input.to_vec())
            },
        );
        assert_eq!(shapes, vec![[1, 3], [1, 4]]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn test_batched_forward_different_ids_unbatched() {
        let id_a = ModelId::new();
        let id_b = ModelId::new();
        let mut ids: Vec<ModelId> = Vec::new();
        let out = batched_forward(
            &[(id_a, vec![1.0, 2.0], 2), (id_b, vec![3.0, 4.0], 2)],
            |id, input, _| {
                ids.push(id);
                Some(input.to_vec())
            },
        );
        assert_eq!(ids, vec![id_a, id_b]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn test_batched_forward_unknown_id_passthrough() {
        let id = ModelId::new();
        let out = batched_forward(&[(id, vec![1.0, 2.0, 3.0], 3)], |_, _, _| None);
        assert_eq!(out, vec![vec![1.0, 2.0, 3.0]]);
    }

    #[test]
    fn test_batched_forward_empty_input() {
        let out = batched_forward(&[], |_, _, _| -> Option<Vec<f32>> { Some(vec![]) });
        assert!(out.is_empty());
    }
}
