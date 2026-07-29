//! Regression gate: the monitor node's hot path must not allocate per-buffer.
//!
//! [`MicMonitorNode`] drains a shared capture ring on the audio thread. The pop
//! goes through `AudioThreadCell::borrow_mut` — no lock, no alloc — and an
//! underrun emits silence rather than stalling. Both `tick` and `process` must
//! be per-buffer allocation-free, exactly like the playback units.
//!
//! Lives with the node it covers: a gate in another crate would stop being run
//! against the type it guards.

use assert_no_alloc::AllocDisabler;
use tutti_core::{AudioUnit, BufferVec, SampleRate};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

#[test]
fn mic_monitor_tick_is_allocation_free() {
    use ringbuf::{
        traits::{Producer, Split},
        HeapRb,
    };
    use tutti_io::{share_mic_ring, MicMonitorNode};

    let rb = HeapRb::<[f32; 2]>::new(1024);
    let (mut prod, cons) = rb.split();
    let mut node = MicMonitorNode::new(share_mic_ring(cons));

    // Prime the ring, then warm up.
    for _ in 0..512 {
        let _ = prod.try_push([0.1, -0.1]);
    }
    let mut output = [0.0f32; 2];
    for _ in 0..64 {
        node.tick(&[], &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        // Exercise both the ring-has-data and the underrun (empty) branches:
        // the loop far outlasts the 512 primed frames, so it goes silent.
        for _ in 0..100_000 {
            node.tick(&[], &mut output);
        }
    });
}

#[test]
fn mic_monitor_process_is_allocation_free() {
    use ringbuf::{
        traits::{Producer, Split},
        HeapRb,
    };
    use tutti_io::{share_mic_ring, MicMonitorNode};

    let rb = HeapRb::<[f32; 2]>::new(4096);
    let (mut prod, cons) = rb.split();
    let mut node = MicMonitorNode::new(share_mic_ring(cons));
    node.set_sample_rate(SampleRate(48_000.0));

    for _ in 0..2048 {
        let _ = prod.try_push([0.2, 0.2]);
    }

    let input_vec = BufferVec::new(0);
    let mut output_vec = BufferVec::new(2);

    for _ in 0..8 {
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        node.process(64, &input, &mut output);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            node.process(64, &input, &mut output);
        }
    });
}
