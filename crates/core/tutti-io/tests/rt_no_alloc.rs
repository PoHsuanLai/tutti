//! Regression gate: the monitor node's hot path must not allocate per block.
//!
//! [`MicMonitorNode`] drains a shared capture ring on the audio thread. The pop
//! goes through `AudioThreadCell::borrow_mut` — no lock, no alloc — and an
//! underrun emits silence rather than stalling. A block must be
//! allocation-free, exactly like the playback units'.
//!
//! Driven through `tutti_graph::contract::Direct`: the node's own
//! `Node::process`, called by hand with buffers built once.
//!
//! Lives with the node it covers: a gate in another crate would stop being run
//! against the type it guards.

use assert_no_alloc::AllocDisabler;
use tutti_core::SampleRate;
use tutti_graph::contract::Direct;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Blocks with the ring full, then running dry (both branches of the pop).
///
/// Mutation (run): collect a block's frames into a `Vec` in `process` → the
/// gate aborts → fails.
#[test]
fn mic_monitor_process_is_allocation_free() {
    use ringbuf::{
        traits::{Producer, Split},
        HeapRb,
    };
    use tutti_io::{share_mic_ring, MicMonitorNode};

    let rb = HeapRb::<[f32; 2]>::new(4096);
    let (mut prod, cons) = rb.split();
    let mut node = Direct::new(
        MicMonitorNode::new(share_mic_ring(cons)),
        SampleRate(48_000.0),
        64,
    );

    for _ in 0..2048 {
        let _ = prod.try_push([0.2, 0.2]);
    }
    for _ in 0..8 {
        node.block();
    }

    assert_no_alloc::assert_no_alloc(|| {
        // The loop far outlasts the 2 048 primed frames, so it goes silent.
        for _ in 0..2_000 {
            node.block();
        }
    });
    assert_eq!(node.output(0), [0.0; 64], "ran dry inside the gate");
}
