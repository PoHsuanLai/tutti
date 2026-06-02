//! Fluent builder for neural audio effects.
//!
//! Takes a borrow of an [`Engine`](crate::Engine) and a path to a model file;
//! [`NeuralBuilder::build`] loads the model, probes it for real-time
//! compatibility, and returns the effect audio unit paired with its
//! [`ModelId`](crate::ModelId).

use crate::backend::Compatibility;
use crate::effect_node::effect_node;
use crate::engine::Engine;
use crate::model_id::ModelId;
use std::path::{Path, PathBuf};
use tutti_core::AudioUnit;

/// Starts a [`NeuralBuilder`] for a model file on disk. The engine dispatches
/// to the backend whose
/// [`supported_extensions`](crate::backend::Backend::supported_extensions)
/// matches the path (e.g. `.onnx` → `tutti-ort`).
///
/// ```ignore
/// let (effect, id) = tutti_neural::neural(&engine, "my_model.onnx").build()?;
/// engine_bundle.graph.add(effect);
/// ```
pub fn neural(engine: &Engine, path: impl AsRef<Path>) -> NeuralBuilder<'_> {
    NeuralBuilder::new(engine, path.as_ref().to_path_buf())
}

/// Starts a [`NeuralBuilder`] for an ONNX model file. Alias of [`neural`].
pub fn onnx(engine: &Engine, path: impl AsRef<Path>) -> NeuralBuilder<'_> {
    NeuralBuilder::new(engine, path.as_ref().to_path_buf())
}

/// Fluent builder for neural audio effects.
///
/// Construct via [`neural`] or [`onnx`]. Both take a borrow of an
/// [`Engine`]; [`Self::build`] returns `(Box<dyn AudioUnit>, ModelId)` so
/// the caller can both insert the effect into the graph and later refer
/// to the model by id.
pub struct NeuralBuilder<'a> {
    engine: &'a Engine,
    path: PathBuf,
}

impl<'a> NeuralBuilder<'a> {
    fn new(engine: &'a Engine, path: PathBuf) -> Self {
        Self { engine, path }
    }

    /// Load + probe the model, return the effect node paired with its
    /// [`ModelId`].
    ///
    /// The engine dispatches by file extension and runs a probe at its
    /// configured `buffer_size`. The probe's measured latency is fed to the
    /// node as `latency_samples`, which in turn reports it through
    /// [`AudioUnit::latency`] so the graph's PDC aligns the rest of the
    /// graph around the model's constant processing delay.
    ///
    /// Returns `Err` only if the probe reports a shape incompatibility
    /// (output length ≠ input length — the model isn't same-rate audio).
    /// The model is unloaded on failure so no dead ids accumulate.
    pub fn build(self) -> crate::Result<(Box<dyn AudioUnit>, ModelId)> {
        let buffer_size = self.engine.buffer_size();
        let sample_rate = self.engine.sample_rate();
        let loaded = self.engine.load_model(&self.path)?;

        if !matches!(loaded.report.compatibility, Compatibility::Ok) {
            let _ = self.engine.unload(loaded.id);
            return Err(crate::Error::InvalidConfig(format!(
                "Model incompatible with real-time audio: {}",
                loaded.report.compatibility
            )));
        }

        let latency_samples = loaded.report.latency_samples(sample_rate);
        let effect = Box::new(effect_node(
            loaded.id,
            2,
            buffer_size,
            latency_samples,
            self.engine.event_sender(),
        ));
        Ok((effect, loaded.id))
    }
}
