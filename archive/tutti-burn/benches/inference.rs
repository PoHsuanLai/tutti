//! Benchmark: Burn CPU vs Burn GPU vs ORT CPU inference latency.
//!
//! Uses the same `tiny_effect` model (128→64→128 MLP) through all backends.
//!
//! Run: cargo bench -p tutti-burn --features onnx-models

use burn::backend::ndarray::NdArrayDevice;
use burn::backend::wgpu::{Wgpu, WgpuDevice};
use burn::backend::NdArray;
use criterion::{criterion_group, criterion_main, Criterion};
use std::path::Path;
use tutti_neural::{Backend, Config, ModelId};

// ── Burn setup ──────────────────────────────────────────────────────────────

fn make_burn_cpu_backend() -> Backend {
    tutti_burn::backend::<NdArray>(NdArrayDevice::default())(Config::default()).unwrap()
}

fn make_burn_gpu_backend() -> Backend {
    tutti_burn::backend::<Wgpu>(WgpuDevice::DefaultDevice)(Config::default()).unwrap()
}

fn register_burn_cpu_model(backend: &mut Backend) -> ModelId {
    use tutti_burn::models::tiny_effect::Model;
    use tutti_burn::NeuralModel;
    let m = tutti_burn::model::<NdArray, _>(|device| {
        let model: Model<NdArray<f32>> = Model::from_embedded(device);
        NeuralModel::from_forward(move |input| model.forward(input))
    });
    (backend.register_model)(m).unwrap()
}

fn register_burn_gpu_model(backend: &mut Backend) -> ModelId {
    use tutti_burn::models::tiny_effect::Model;
    use tutti_burn::NeuralModel;
    let m = tutti_burn::model::<Wgpu, _>(|device| {
        let model: Model<Wgpu> = Model::from_embedded(device);
        NeuralModel::from_forward(move |input| model.forward(input))
    });
    (backend.register_model)(m).unwrap()
}

// ── ORT setup ───────────────────────────────────────────────────────────────

fn make_ort_backend() -> Backend {
    tutti_ort::backend(Config::default()).unwrap()
}

fn register_ort_model(backend: &mut Backend) -> ModelId {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/model/tiny_effect.onnx");
    (backend.register_model)(tutti_ort::model(path)).unwrap()
}

// ── Input data ──────────────────────────────────────────────────────────────

fn reference_input() -> Vec<f32> {
    serde_json::from_str(include_str!("../src/model/reference_input.json")).unwrap()
}

// ── CPU benchmarks ──────────────────────────────────────────────────────────

fn bench_cpu_single(c: &mut Criterion) {
    let input = reference_input();

    let mut burn = make_burn_cpu_backend();
    let burn_id = register_burn_cpu_model(&mut burn);
    let _ = (burn.forward)(&[(burn_id, input.clone(), 128)]);

    let mut ort = make_ort_backend();
    let ort_id = register_ort_model(&mut ort);
    let _ = (ort.forward)(&[(ort_id, input.clone(), 128)]);

    c.bench_function("burn_cpu_single", |b| {
        b.iter(|| (burn.forward)(&[(burn_id, input.clone(), 128)]).unwrap())
    });

    c.bench_function("ort_cpu_single", |b| {
        b.iter(|| (ort.forward)(&[(ort_id, input.clone(), 128)]).unwrap())
    });
}

fn bench_cpu_batch8(c: &mut Criterion) {
    let input = reference_input();

    let mut burn = make_burn_cpu_backend();
    let burn_id = register_burn_cpu_model(&mut burn);
    let burn_batch: Vec<_> = (0..8).map(|_| (burn_id, input.clone(), 128usize)).collect();
    let _ = (burn.forward)(&burn_batch);

    let mut ort = make_ort_backend();
    let ort_id = register_ort_model(&mut ort);
    let ort_batch: Vec<_> = (0..8).map(|_| (ort_id, input.clone(), 128usize)).collect();
    let _ = (ort.forward)(&ort_batch);

    c.bench_function("burn_cpu_batch8", |b| {
        b.iter(|| (burn.forward)(&burn_batch).unwrap())
    });

    c.bench_function("ort_cpu_batch8", |b| {
        b.iter(|| (ort.forward)(&ort_batch).unwrap())
    });
}

// ── GPU benchmarks (Burn wgpu) ──────────────────────────────────────────────

fn bench_gpu(c: &mut Criterion) {
    // Probe by trying to construct the backend; if GPU init fails, skip.
    let probe = make_burn_gpu_backend();
    if !probe.capabilities.has_gpu {
        eprintln!("No GPU available, skipping GPU benchmarks");
        return;
    }
    drop(probe);

    let input = reference_input();

    let mut burn_gpu = make_burn_gpu_backend();
    let gpu_id = register_burn_gpu_model(&mut burn_gpu);
    // GPU warmup (first call compiles shaders).
    for _ in 0..10 {
        let _ = (burn_gpu.forward)(&[(gpu_id, input.clone(), 128)]);
    }

    c.bench_function("burn_gpu_single", |b| {
        b.iter(|| (burn_gpu.forward)(&[(gpu_id, input.clone(), 128)]).unwrap())
    });
}

criterion_group!(benches, bench_cpu_single, bench_cpu_batch8, bench_gpu);
criterion_main!(benches);
