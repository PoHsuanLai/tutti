//! The block clock: each render call publishes the OS time it began and the
//! graph frame it began at, so a producer thread holding an OS time can place
//! it on a frame (doc 017 section 5, "Timing at the edge"). Figures are
//! analytic: the frame of call `k` is the frames rendered before it.

use std::time::Instant;

use tutti_engine::Engine;
use tutti_graph::{Editor, Prepare};
use tutti_transport::Transport;
use tutti_types::{ChannelLayout, Frame, InterleavedMut, SampleRate, Samples};

const RATE: f64 = 48_000.0;

fn engine() -> Engine {
    let transport = Transport::new(SampleRate::new(RATE));
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(512)));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within limits");
    Box::leak(Box::new((ed, transport)));
    engine
}

fn render(engine: &Engine, frames: usize, segment: bool) {
    let mut out = vec![0.0f32; frames * 2];
    let mut block = InterleavedMut::new(&mut out, ChannelLayout::STEREO);
    if segment {
        engine.process_segment(&mut block);
    } else {
        engine.process(&mut block);
    }
}

/// **A reading is the call's start**: nothing before the first call; after
/// each call the frame it began at (the frames rendered before it) and an OS
/// time between the moments around it, for both entry points.
#[test]
fn each_call_publishes_where_it_began() {
    for segment in [false, true] {
        let engine = engine();
        let reader = engine.block_clock();
        assert_eq!(reader.read(), None, "nothing before the first call");

        let before = Instant::now();
        render(&engine, 256, segment);
        let (os, at) = reader.read().expect("published by the first call");
        assert_eq!(at.frame(), Frame(0));
        assert_eq!(at.rate(), SampleRate(RATE));
        assert!(os >= before && os <= Instant::now());

        render(&engine, 512, segment);
        let (second, at) = reader.read().expect("published by the second call");
        assert_eq!(
            at.frame(),
            Frame(256),
            "the second call begins where the first ended"
        );
        assert!(second >= os);
    }
}
