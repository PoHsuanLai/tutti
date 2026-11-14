//! The block clock: each render call publishes the OS time it began and the
//! graph frame it began at, so a producer thread holding an OS time can place
//! it on a frame. Figures are
//! analytic: the frame of call `k` is the frames rendered before it.

use tutti_buffers::InterleavedMut;
use tutti_engine::Engine;
use tutti_graph::{Editor, Prepare};
use tutti_transport::MotionEvent;
use tutti_transport::Transport;
use tutti_types::MaxBlock;
use tutti_types::{Beat, Seconds};
use tutti_types::{ChannelLayout, Frame, SampleRate, Samples};

const RATE: f64 = 48_000.0;

fn engine() -> Engine {
    let transport = Transport::new(SampleRate::of(RATE));
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate::of(RATE),
        MaxBlock::of(Samples(512)),
    ));
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within limits");
    Box::leak(Box::new(ed));
    TRANSPORTS.with(|t| t.borrow_mut().push(transport));
    engine
}

thread_local! {
    /// The transports of this test's engines: kept alive, and reachable by
    /// `motion`.
    static TRANSPORTS: std::cell::RefCell<Vec<Transport>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Sends `event` over the last engine this test built.
fn motion(event: MotionEvent) {
    TRANSPORTS.with(|t| {
        t.borrow()
            .last()
            .expect("an engine")
            .motion()
            .try_send(event)
            .expect("room");
    });
}

fn render(engine: &Engine, frames: usize, segment: bool) {
    let mut out = vec![0.0f32; frames * 2];
    let mut block = InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap();
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

        let before = tutti_rt::monotonic_ns();
        render(&engine, 256, segment);
        let first = reader.read().expect("published by the first call");
        assert_eq!(first.frame, Frame::new(0));
        assert_eq!(first.rate, SampleRate::of(RATE));
        assert!(first.os_ns >= before && first.os_ns <= tutti_rt::monotonic_ns());

        render(&engine, 512, segment);
        let second = reader.read().expect("published by the second call");
        assert_eq!(
            second.frame,
            Frame::new(256),
            "the second call begins where the first ended"
        );
        assert!(second.os_ns >= first.os_ns);
    }
}

/// **One read is one instant**: through play, a stop and a locate (a few
/// blocks each) the playhead's frame, beat, seconds and playing flag agree
/// with each other at every call. At 120 BPM and 48 kHz a frame is
/// 1 / 24 000 of a beat, and a beat is half a second.
#[test]
fn a_stop_and_a_locate_land_in_one_snapshot() {
    let engine = engine();
    let reader = engine.block_clock();
    let beat_of = |frames: u64| frames as f64 * 2.0 / RATE;

    render(&engine, 256, false);
    let p = reader.playhead();
    assert_eq!(
        (p.playing(), p.beat(), p.frame()),
        (false, Beat(0.0), Frame::new(0))
    );

    motion(MotionEvent::Play);
    render(&engine, 480, false);
    let p = reader.playhead();
    assert!(p.playing(), "the play landed on the call's first frame");
    assert_eq!(
        (p.frame(), p.beat(), p.seconds()),
        (Frame::new(256), Beat(0.0), Seconds(0.0))
    );

    render(&engine, 480, false);
    let p = reader.playhead();
    assert!(p.playing());
    assert_eq!(p.frame(), Frame::new(736));
    assert!((p.beat().get() - beat_of(480)).abs() < 1e-12);
    assert!((p.seconds().get() - p.beat().get() / 2.0).abs() < 1e-12);

    motion(MotionEvent::stop_unfaded());
    render(&engine, 480, false);
    let p = reader.playhead();
    assert!(
        !p.playing(),
        "the stop is in the snapshot of the call it landed on"
    );
    assert_eq!(p.frame(), Frame::new(1_216), "the graph's frame runs on");
    assert!(
        (p.beat().get() - beat_of(960)).abs() < 1e-12,
        "held where it stopped"
    );
    assert!((p.seconds().get() - p.beat().get() / 2.0).abs() < 1e-12);

    motion(MotionEvent::locate(Beat(8.0)));
    render(&engine, 480, false);
    let p = reader.playhead();
    assert_eq!(
        (p.playing(), p.beat(), p.seconds()),
        (false, Beat(8.0), Seconds(4.0))
    );
    assert_eq!(p.frame(), Frame::new(1_696));
}

/// **A reader never sees a torn playhead**: while the audio side renders a
/// rolling transport as fast as it can, every snapshot another thread reads
/// has its beat and seconds on the tempo's line and its beat at the frame's
/// closed form (a snapshot built from two calls would break one of them).
#[test]
fn a_reader_on_another_thread_never_sees_two_calls_mixed() {
    let engine = engine();
    let reader = engine.block_clock();
    motion(MotionEvent::Play);
    render(&engine, 480, false);
    let done = std::sync::atomic::AtomicBool::new(false);
    let reads = std::thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            let mut reads = 0u64;
            while !done.load(std::sync::atomic::Ordering::Acquire) {
                let p = reader.playhead();
                if p.at().is_some() {
                    reads += 1;
                    assert!(p.playing());
                    let beat = (p.frame().get() as f64 - 0.0) * 2.0 / RATE;
                    assert!((p.beat().get() - beat).abs() < 1e-9, "beat {p:?}");
                    assert!(
                        (p.seconds().get() - p.beat().get() / 2.0).abs() < 1e-9,
                        "{p:?}"
                    );
                }
            }
            reads
        });
        for _ in 0..20_000 {
            render(&engine, 480, false);
        }
        done.store(true, std::sync::atomic::Ordering::Release);
        watcher.join().expect("the watcher")
    });
    assert!(reads > 0, "the watcher read");
}
