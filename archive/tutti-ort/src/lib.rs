//! ONNX Runtime backend for Tutti neural audio.
//!
//! Loads `.onnx` models at runtime via Microsoft's ONNX Runtime (the
//! [`ort`](https://crates.io/crates/ort) crate). Supports CoreML (macOS GPU/ANE)
//! and CUDA execution providers.
//!
//! ```rust,ignore
//! use std::path::Path;
//! let factory = tutti_ort::backend;
//! let engine = tutti_neural::engine(tutti_neural::Config::default(), Box::new(factory))?;
//! let loaded = engine.load_model(Path::new("path/to/model.onnx"))?;
//! ```

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use ort::session::Session;
use ort::value::Tensor;
use tutti_neural::{Backend, BackendError, Capabilities, Config, ModelId};

struct Entry {
    session: Session,
    input_name: String,
    output_name: String,
}

impl Entry {
    fn forward_flat(&mut self, data: &[f32], shape: [usize; 2]) -> Vec<f32> {
        let tensor = Tensor::from_array((
            vec![shape[0] as i64, shape[1] as i64],
            data.to_vec().into_boxed_slice(),
        ))
        .expect("failed to create ORT tensor");
        let outputs = self
            .session
            .run(ort::inputs![self.input_name.as_str() => tensor])
            .expect("ORT inference failed");
        let (_, flat) = outputs[self.output_name.as_str()]
            .try_extract_tensor::<f32>()
            .expect("failed to extract ORT output");
        flat.to_vec()
    }
}

struct OrtState {
    models: HashMap<ModelId, Entry>,
}

impl OrtState {
    fn new() -> Self {
        Self {
            models: HashMap::new(),
        }
    }

    fn load(&mut self, path: &Path) -> Result<ModelId, BackendError> {
        let session = build_session(path)?;
        let input_name = session.inputs()[0].name().to_string();
        let output_name = session.outputs()[0].name().to_string();
        let id = ModelId::new();
        self.models.insert(
            id,
            Entry {
                session,
                input_name,
                output_name,
            },
        );
        Ok(id)
    }

    fn forward(
        &mut self,
        requests: &[(ModelId, Vec<f32>, usize)],
    ) -> Result<Vec<Vec<f32>>, BackendError> {
        Ok(tutti_neural::batched_forward(
            requests,
            |id, input, shape| {
                self.models
                    .get_mut(&id)
                    .map(|m| m.forward_flat(input, shape))
            },
        ))
    }

    fn unload(&mut self, id: ModelId) -> Result<(), BackendError> {
        self.models
            .remove(&id)
            .map(|_| ())
            .ok_or(BackendError::NotFound(id))
    }
}

fn build_session(path: &Path) -> Result<Session, BackendError> {
    let mut builder =
        Session::builder().map_err(|e| BackendError::Init(format!("ORT session builder: {e}")))?;
    builder = builder
        .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
        .map_err(|e| BackendError::Init(format!("ORT optimization: {e}")))?;
    builder = builder
        .with_intra_threads(1)
        .map_err(|e| BackendError::Init(format!("ORT threads: {e}")))?;

    #[cfg(target_os = "macos")]
    {
        builder = builder
            .with_execution_providers([ort::execution_providers::CoreMLExecutionProvider::default(
            )
            .build()])
            .map_err(|e| BackendError::Init(format!("ORT CoreML EP: {e}")))?;
    }
    #[cfg(feature = "cuda")]
    {
        builder = builder
            .with_execution_providers([
                ort::execution_providers::CUDAExecutionProvider::default().build()
            ])
            .map_err(|e| BackendError::Init(format!("ORT CUDA EP: {e}")))?;
    }

    builder
        .commit_from_file(path)
        .map_err(|e| BackendError::LoadFailed(format!("ORT load model: {e}")))
}

fn backend_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "ONNX Runtime/CoreML"
    } else if cfg!(feature = "cuda") {
        "ONNX Runtime/CUDA"
    } else {
        "ONNX Runtime/CPU"
    }
}

fn backend_has_gpu() -> bool {
    cfg!(target_os = "macos") || cfg!(feature = "cuda")
}

/// Construct an ORT [`Backend`]. Matches [`BackendFactory`](tutti_neural::BackendFactory).
pub fn backend(_cfg: Config) -> Result<Backend, BackendError> {
    let state = Rc::new(RefCell::new(OrtState::new()));
    let s1 = state.clone();
    let s2 = state.clone();
    let s3 = state;
    Ok(Backend {
        load: Box::new(move |path| s1.borrow_mut().load(path)),
        // ORT does not support `register_model` — all ORT models come from
        // file paths via `load`. Reject with UnknownModel so a misrouted
        // Burn `Model` value fails loudly.
        register_model: Box::new(|_m| Err(BackendError::UnknownModel)),
        forward: Box::new(move |reqs| s2.borrow_mut().forward(reqs)),
        unload: Box::new(move |id| s3.borrow_mut().unload(id)),
        supported_extensions: &["onnx"],
        capabilities: Capabilities {
            name: backend_name(),
            has_gpu: backend_has_gpu(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn new_state() -> OrtState {
        OrtState::new()
    }

    fn test_model_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/test_data")
            .join(name)
    }

    #[test]
    fn test_backend_creation() {
        assert!(backend(Config::default()).is_ok());
    }

    #[test]
    fn test_backend_declares_onnx() {
        let b = backend(Config::default()).unwrap();
        assert!(b.handles_extension("onnx"));
        assert!(b.handles_extension("ONNX"));
        assert!(!b.handles_extension("burnpack"));
    }

    #[test]
    fn test_load_nonexistent_path_fails() {
        let mut state = new_state();
        let err = state.load(Path::new("/nonexistent.onnx")).unwrap_err();
        assert!(matches!(err, BackendError::LoadFailed(_)));
    }

    #[test]
    fn test_load_onnx_identity() {
        let mut state = new_state();
        let id = state.load(&test_model_path("identity.onnx")).unwrap();
        let input: Vec<f32> = (0..128).map(|i| i as f32).collect();
        let results = state.forward(&[(id, input.clone(), 128)]).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], input);
    }

    #[test]
    fn test_load_onnx_scale2x() {
        let mut state = new_state();
        let id = state.load(&test_model_path("scale2x.onnx")).unwrap();
        let input: Vec<f32> = (0..64).map(|i| i as f32).collect();
        let expected: Vec<f32> = input.iter().map(|x| x * 2.0).collect();
        let results = state.forward(&[(id, input, 64)]).unwrap();
        assert_eq!(results[0], expected);
    }

    #[test]
    fn test_onnx_batched() {
        let mut state = new_state();
        let id = state.load(&test_model_path("identity.onnx")).unwrap();
        let input1: Vec<f32> = (0..128).map(|i| i as f32).collect();
        let input2: Vec<f32> = (0..128).map(|i| (i as f32) * 0.5).collect();
        let results = state
            .forward(&[(id, input1.clone(), 128), (id, input2.clone(), 128)])
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], input1);
        assert_eq!(results[1], input2);
    }

    #[test]
    fn test_unknown_model_passthrough() {
        let mut state = new_state();
        let fake_id = ModelId::new();
        let results = state.forward(&[(fake_id, vec![1.0, 2.0], 2)]).unwrap();
        assert_eq!(results, vec![vec![1.0, 2.0]]);
    }

    #[test]
    fn test_unload_model() {
        let mut state = new_state();
        let id = state.load(&test_model_path("identity.onnx")).unwrap();
        assert!(state.unload(id).is_ok());
        assert!(state.unload(id).is_err());
    }
}
