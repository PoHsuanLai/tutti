//! Burn ML backend for Tutti neural audio.
//!
//! This crate is **device-agnostic**: it doesn't pick a Burn backend, doesn't
//! construct devices, and doesn't depend on `wgpu`. The application brings
//! the device — pick `NdArray`, `Wgpu`, `LibTorch`, `Cuda`, whatever — and
//! the library wraps it as a [`tutti_neural::Backend`].
//!
//! ```rust,ignore
//! use burn::backend::wgpu::{Wgpu, WgpuDevice};
//!
//! // App constructs the device. Uses Burn's built-in adapter selection.
//! let device = WgpuDevice::DefaultDevice;
//!
//! // Wrap as a Tutti backend factory and hand it to the engine.
//! let engine = tutti_neural::engine(
//!     tutti_neural::Config::default(),
//!     Box::new(tutti_burn::backend::<Wgpu>(device)),
//! )?;
//!
//! // Register a model — same closure shape regardless of which Burn flavor.
//! let id = engine.register_model(tutti_burn::model::<Wgpu>(|device| {
//!     tutti_burn::NeuralModel::from_forward(move |input| input)
//! }))?;
//! ```

mod fusion;
pub mod models;

/// Re-export of the underlying `burn` crate so applications can access
/// concrete backends (`burn::backend::ndarray`, `burn::backend::wgpu`, …)
/// and device types without adding their own `burn` dependency.
pub use burn;
/// Re-export of the Burn `Backend` trait so consumers can write trait
/// bounds without depending on `burn` directly.
pub use burn::tensor::backend::Backend as BurnBackend;
pub use fusion::NeuralModel;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use burn::prelude::*;
use tutti_neural::{Backend, BackendError, Capabilities, Config, Model, ModelId};

type ModelFactory<B> = Box<dyn FnOnce(&<B as BurnBackend>::Device) -> NeuralModel<B> + Send>;

/// Native Burn model factory wrapped for [`Engine::load_model`](tutti_neural::Engine::load_model).
///
/// Construct with [`model`]. The closure runs on the engine thread against
/// the device the [`backend`] factory was built with.
pub struct BurnModelSpec<B: BurnBackend> {
    pub(crate) factory: ModelFactory<B>,
}

/// Build a [`Model`] value from a Burn-backend factory closure.
///
/// The closure receives the device the engine was constructed with and must
/// return a [`NeuralModel<B>`].
pub fn model<B, F>(factory: F) -> Model
where
    B: BurnBackend + 'static,
    B::Device: Send + 'static,
    F: FnOnce(&B::Device) -> NeuralModel<B> + Send + 'static,
{
    Model::new(BurnModelSpec::<B> {
        factory: Box::new(factory),
    })
}

struct BurnState<B: BurnBackend> {
    device: B::Device,
    models: HashMap<ModelId, NeuralModel<B>>,
}

impl<B: BurnBackend> BurnState<B>
where
    B::Device: Clone,
{
    fn new(device: B::Device) -> Self {
        Self {
            device,
            models: HashMap::new(),
        }
    }

    fn register_model(&mut self, model: Model) -> Result<ModelId, BackendError> {
        let spec = model
            .downcast::<BurnModelSpec<B>>()
            .map_err(|_| BackendError::UnknownModel)?;
        let id = ModelId::new();
        let built = (spec.factory)(&self.device);
        self.models.insert(id, built);
        Ok(id)
    }

    fn forward(
        &mut self,
        requests: &[(ModelId, Vec<f32>, usize)],
    ) -> Result<Vec<Vec<f32>>, BackendError> {
        let device = self.device.clone();
        Ok(tutti_neural::batched_forward(
            requests,
            |id, input, shape| {
                self.models.get(&id).map(|m| {
                    let tensor =
                        Tensor::<B, 1>::from_floats(input, &device).reshape([shape[0], shape[1]]);
                    let out = m.forward(tensor);
                    out.into_data().to_vec::<f32>().expect("tensor to vec")
                })
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

/// Build a [`tutti_neural::BackendFactory`] for Burn backend `B` against the
/// supplied device.
///
/// Returns a `FnOnce(Config)` matching [`tutti_neural::BackendFactory`]:
///
/// ```ignore
/// use burn::backend::wgpu::{Wgpu, WgpuDevice};
/// let factory = tutti_burn::backend::<Wgpu>(WgpuDevice::DefaultDevice);
/// let engine = tutti_neural::engine(tutti_neural::Config::default(), Box::new(factory))?;
/// ```
///
/// `name` and `has_gpu` capabilities are inferred from `B::name()` and a
/// simple substring check (`wgpu`/`cuda`/`metal`/`tch` → GPU).
pub fn backend<B>(device: B::Device) -> impl FnOnce(Config) -> Result<Backend, BackendError> + Send
where
    B: BurnBackend + 'static,
    B::Device: Clone + Send + 'static,
{
    move |_cfg| {
        // `B::name(&device)` returns `String`. `Capabilities::name` is
        // `&'static str` for cheap copying through the audio path. Backends
        // are created exactly once per `Engine`, so a one-time leak is the
        // right shape — bounded by the engine count, never the request count.
        let backend_name: &'static str = Box::leak(B::name(&device).into_boxed_str());
        let has_gpu = is_gpu_backend(backend_name);
        let state = Rc::new(RefCell::new(BurnState::<B>::new(device)));
        let s1 = state.clone();
        let s2 = state.clone();
        let s3 = state;
        Ok(Backend {
            // Burn has no runtime file-loader API (weights load *into* a
            // Rust-typed `Module<B>` known at compile time). Phase 2 will
            // introduce `load_burn::<MyModel<B>>(path)` via `burn-store`.
            // Until then this backend supports zero extensions; use
            // `register_model(tutti_burn::model::<B, _>(factory))` for
            // the compile-time escape hatch.
            load: Box::new(|_path| {
                Err(BackendError::UnsupportedFormat(
                    "Burn has no runtime loader — use register_model with tutti_burn::model".into(),
                ))
            }),
            register_model: Box::new(move |m| s1.borrow_mut().register_model(m)),
            forward: Box::new(move |reqs| s2.borrow_mut().forward(reqs)),
            unload: Box::new(move |id| s3.borrow_mut().unload(id)),
            supported_extensions: &[],
            capabilities: Capabilities {
                name: backend_name,
                has_gpu,
            },
        })
    }
}

fn is_gpu_backend(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["wgpu", "cuda", "metal", "rocm", "tch", "vulkan"]
        .iter()
        .any(|tag| lower.contains(tag))
}

#[cfg(test)]
mod tests {
    use super::*;
    use burn::backend::ndarray::NdArrayDevice;
    use burn::backend::NdArray;

    fn cpu_state() -> BurnState<NdArray> {
        BurnState::<NdArray>::new(NdArrayDevice::default())
    }

    #[test]
    fn test_backend_creation() {
        let factory = backend::<NdArray>(NdArrayDevice::default());
        assert!(factory(Config::default()).is_ok());
    }

    fn identity_spec() -> Model {
        model::<NdArray, _>(|_device| NeuralModel::<NdArray>::from_forward(|input| input))
    }

    #[test]
    fn test_register_model_and_forward() {
        let mut s = cpu_state();
        let id = s.register_model(identity_spec()).unwrap();
        let results = s.forward(&[(id, vec![1.0, 2.0, 3.0], 3)]).unwrap();
        assert_eq!(results, vec![vec![1.0, 2.0, 3.0]]);
    }

    #[test]
    fn test_batched_forward() {
        let mut s = cpu_state();
        let id = s.register_model(identity_spec()).unwrap();
        let results = s
            .forward(&[
                (id, vec![1.0, 2.0, 3.0, 4.0], 4),
                (id, vec![5.0, 6.0, 7.0, 8.0], 4),
            ])
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].len(), 4);
    }

    #[test]
    fn test_capabilities_via_backend() {
        let factory = backend::<NdArray>(NdArrayDevice::default());
        let b = factory(Config::default()).unwrap();
        assert!(!b.capabilities.has_gpu);
        assert!(!b.capabilities.name.is_empty());
    }

    #[test]
    fn test_register_model_wrong_type() {
        let mut s = cpu_state();
        let err = s.register_model(Model::new(42u32)).unwrap_err();
        assert!(matches!(err, BackendError::UnknownModel));
    }

    #[test]
    fn test_unload_model() {
        let mut s = cpu_state();
        let id = s.register_model(identity_spec()).unwrap();
        assert!(s.unload(id).is_ok());
        assert!(s.unload(id).is_err());
    }

    #[test]
    fn test_load_returns_unsupported_format() {
        let factory = backend::<NdArray>(NdArrayDevice::default());
        let mut b = factory(Config::default()).unwrap();
        let err = (b.load)(std::path::Path::new("foo.onnx")).unwrap_err();
        assert!(matches!(err, BackendError::UnsupportedFormat(_)));
    }

    #[cfg(feature = "onnx-models")]
    #[test]
    fn test_onnx_model_forward() {
        use crate::models::tiny_effect::Model as TinyModel;
        let device = NdArrayDevice::default();
        let model: TinyModel<NdArray<f32>> = TinyModel::from_embedded(&device);
        let input = Tensor::<NdArray<f32>, 2>::ones([1, 128], &device);
        let output = model.forward(input);
        assert_eq!(output.shape().dims, [1, 128]);
    }

    #[cfg(feature = "onnx-models")]
    #[test]
    fn test_onnx_model_via_register_model() {
        use crate::models::tiny_effect::Model as TinyModel;
        let mut s = cpu_state();
        let m = model::<NdArray, _>(|device| {
            let model: TinyModel<NdArray<f32>> = TinyModel::from_embedded(device);
            NeuralModel::from_forward(move |input| model.forward(input))
        });
        let id = s.register_model(m).unwrap();
        let input: Vec<f32> = (0..128).map(|i| i as f32 / 128.0).collect();
        let results = s.forward(&[(id, input, 128)]).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].len(), 128);
    }

    #[cfg(feature = "onnx-models")]
    #[test]
    fn test_onnx_correctness_vs_pytorch() {
        use crate::models::tiny_effect::Model as TinyModel;
        let ref_input: Vec<f32> =
            serde_json::from_str(include_str!("model/reference_input.json")).unwrap();
        let ref_output: Vec<f32> =
            serde_json::from_str(include_str!("model/reference_output.json")).unwrap();
        let mut s = cpu_state();
        let m = model::<NdArray, _>(|device| {
            let model: TinyModel<NdArray<f32>> = TinyModel::from_embedded(device);
            NeuralModel::from_forward(move |input| model.forward(input))
        });
        let id = s.register_model(m).unwrap();
        let results = s.forward(&[(id, ref_input.clone(), 128)]).unwrap();
        let output = &results[0];
        assert_eq!(output.len(), ref_output.len());
        let max_diff = output
            .iter()
            .zip(ref_output.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max_diff < 1e-4, "max_diff={max_diff:.6}");
    }

    #[test]
    fn test_is_gpu_backend_classification() {
        assert!(is_gpu_backend("wgpu"));
        assert!(is_gpu_backend("Cuda"));
        assert!(is_gpu_backend("metal"));
        assert!(!is_gpu_backend("ndarray"));
    }
}
