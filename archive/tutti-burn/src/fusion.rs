//! Opaque neural model wrapper.

use burn::prelude::*;
use burn::tensor::backend::Backend;
use std::sync::Arc;

/// Wraps a forward function as `Tensor<B, 2> -> Tensor<B, 2>`.
///
/// Lives on the inference thread — no `Send` requirement on the inner closure.
pub struct NeuralModel<B: Backend> {
    forward_fn: Arc<dyn Fn(Tensor<B, 2>) -> Tensor<B, 2>>,
}

impl<B: Backend> NeuralModel<B> {
    pub fn from_forward(f: impl Fn(Tensor<B, 2>) -> Tensor<B, 2> + 'static) -> Self {
        Self {
            forward_fn: Arc::new(f),
        }
    }

    pub fn forward(&self, input: Tensor<B, 2>) -> Tensor<B, 2> {
        (self.forward_fn)(input)
    }
}

impl<B: Backend> Clone for NeuralModel<B> {
    fn clone(&self) -> Self {
        Self {
            forward_fn: Arc::clone(&self.forward_fn),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use burn::backend::ndarray::NdArrayDevice;
    use burn::backend::NdArray;

    type TestBackend = NdArray<f32>;

    #[test]
    fn test_identity() {
        let model = NeuralModel::<TestBackend>::from_forward(|input| input);
        let device = NdArrayDevice::default();
        let input = Tensor::<TestBackend, 2>::ones([2, 128], &device);
        assert_eq!(model.forward(input).shape().dims, [2, 128]);
    }

    #[test]
    fn test_transform() {
        let model = NeuralModel::<TestBackend>::from_forward(|input| {
            burn::tensor::activation::relu(input.mul_scalar(2.0))
        });
        let device = NdArrayDevice::default();
        let output = model.forward(Tensor::zeros([4, 64], &device));
        assert_eq!(output.shape().dims, [4, 64]);
    }
}
