//! Regression gate: the inlet's hot path must not allocate per block.
//!
//! `InletNode` drains a ring it owns on the audio thread. A block copies what
//! the ring holds straight out of its storage and advances the read index
//! once (no lock, no alloc), and an underrun emits silence rather than
//! stalling. A block must be allocation-free, exactly like the playback
//! units'.
//!
//! Driven through `tutti_graph::contract::Direct`: the node's own
//! `Node::process`, called by hand with buffers built once.

use assert_no_alloc::AllocDisabler;
use tutti_graph::InletNode;
use tutti_graph::contract::Direct;
use tutti_types::{ChannelLayout, SampleRate, Samples};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Blocks with the ring full, then running dry (both branches of the pop).
#[test]
fn inlet_process_is_allocation_free() {
    let (node, mut prod) = InletNode::pair(ChannelLayout::STEREO, Samples(4096));
    let mut node = Direct::new(node, SampleRate(48_000.0), 64);

    for _ in 0..2048 {
        prod.push_interleaved(&[0.2, 0.2]);
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
