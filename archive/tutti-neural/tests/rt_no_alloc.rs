//! Regression gate for RT-safety: `Effect` must not allocate on the
//! audio path.
//!
//! Uses [`assert_no_alloc`] to install a global allocator hook that
//! panics on any heap allocation inside
//! [`assert_no_alloc::assert_no_alloc`] scopes. The hot path is the
//! `AudioUnit::process` / `tick` methods; the factory and construction
//! intentionally run outside the no-alloc scope.

use assert_no_alloc::AllocDisabler;
use crossbeam_channel::unbounded;
use tutti_core::{AudioUnit, BufferVec};
use tutti_neural::ipc::Event;
use tutti_neural::{effect_node, ModelId};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Build a stereo effect node with `buffer_size` accumulation and a
/// disconnected event channel (we don't care whether requests are
/// actually received for allocation testing).
fn build_effect(buffer_size: usize) -> tutti_neural::Effect {
    let (tx, _rx) = unbounded::<Event>();
    effect_node(ModelId::new(), 2, buffer_size, buffer_size, tx)
}

#[test]
fn tick_is_allocation_free() {
    let mut node = build_effect(128);

    // Warm up any lazy initialisation outside the no-alloc scope.
    let mut out = [0.0f32, 0.0];
    node.tick(&[0.1, 0.2], &mut out);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            node.tick(&[0.1, 0.2], &mut out);
        }
    });
}

#[test]
fn process_is_allocation_free() {
    let mut node = build_effect(128);

    // Warm up outside the no-alloc scope.
    let mut out_vec = BufferVec::new(2);
    let input_vec = BufferVec::new(2);
    {
        let input = input_vec.buffer_ref();
        let mut output = out_vec.buffer_mut();
        node.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            let input = input_vec.buffer_ref();
            let mut output = out_vec.buffer_mut();
            node.process(64, &input, &mut output);
        }
    });
}
