//! The tap after the fader: `Engine` writes each chunk of its device-width
//! output, as it leaves the fader, to the rings of a `TapReader`. Figures are
//! analytic: the source plays `(t + 1) * (c + 1)` on channel `c` at frame `t`
//! (exact in `f32`), so a frame's lanes name their own instant and a torn,
//! shifted or folded frame is visible. The fade itself is
//! `tutti-session`'s `tests/engine.rs`.

use tutti_engine::{Engine, HeardTapError};
use tutti_graph::NodeKey;
use tutti_graph::{Cx, Editor, Io, Node, Prepare, Shape, Status, TapReader, Unforkable};
use tutti_graph::{OutPort, Source};
use tutti_transport::Transport;
use tutti_types::{ChannelLayout, InterleavedMut, SampleRate, Samples};

const RATE: f64 = 48_000.0;

/// `width` channels; channel `c` at frame `t` plays `(t + 1) * (c + 1)`.
struct Ramp {
    width: u16,
    at: usize,
}

impl Node for Ramp {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::from_count(self.width))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for c in 0..usize::from(self.width) {
            for (k, s) in io.output(c).iter_mut().enumerate() {
                *s = ((self.at + k + 1) * (c + 1)) as f32;
            }
        }
        self.at += io.output(0).len();
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn engine(width: u16) -> Engine {
    let transport = Transport::new(RATE);
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(512)));
    ed.insert(NodeKey(1), "ramp", Unforkable(Ramp { width, at: 0 }));
    ed.spec_mut().topology.outputs = (0..width)
        .map(|port| {
            Source::Node(OutPort {
                node: NodeKey(1),
                port,
            })
        })
        .collect();
    ed.commit().expect("commits");
    let engine = Engine::new(&transport, &mut ed, exec).expect("within limits");
    Box::leak(Box::new((ed, transport)));
    engine
}

fn render(engine: &Engine, frames: usize, width: u16, segment: bool) -> Vec<f32> {
    let mut out = vec![0.0f32; frames * usize::from(width)];
    let mut block = InterleavedMut::new(&mut out, ChannelLayout::from_count(width));
    if segment {
        engine.process_segment(&mut block);
    } else {
        engine.process(&mut block);
    }
    out
}

/// Sets its flag when dropped, so a panic in the rendering thread still lets
/// the reader thread finish and the scope unwind (instead of hanging).
struct Raise<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for Raise<'_> {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

fn drain(tap: &mut TapReader, into: &mut Vec<f32>) {
    let mut buf = vec![0.0f32; tap.width() * 4096];
    loop {
        let n = tap.pop_into(&mut buf).get();
        if n == 0 {
            break;
        }
        into.extend_from_slice(&buf[..n * tap.width()]);
    }
}

/// The tap is the engine's output buffer, every channel, whole frames, bit for bit:
/// a 6-wide device is not folded, and a block longer than the graph's 512-frame
/// blocks (written chunk by chunk) arrives complete and in order. The pure
/// render (`process_segment`) writes too, on purpose: it is a render a host
/// pulls and the tap is "what the engine rendered".
///
/// Mutation (run, recorded here): `push_interleaved` reading every lane from
/// channel 0 (`skip(0)`) -> the lanes are all channel 0 and this fails. Delete
/// the `write` after the executor's render in `process_segment` -> the
/// segment half fails. Delete the one in `process` -> the first half fails.
#[test]
fn the_tap_is_the_device_output_on_every_channel() {
    for segment in [false, true] {
        let engine = engine(6);
        let mut tap = engine
            .heard_taps()
            .open(ChannelLayout::from_count(6), Samples(8192))
            .expect("opens");
        let mut heard = Vec::new();
        let mut tapped = Vec::new();
        for frames in [64, 1500, 1, 700] {
            heard.extend(render(&engine, frames, 6, segment));
            drain(&mut tap, &mut tapped);
        }
        assert_eq!(tap.take_lost(), Samples(0));
        assert_eq!(heard.len(), 2265 * 6);
        let bits = |x: &[f32]| x.iter().map(|s| s.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&tapped), bits(&heard), "segment: {segment}");
        // And the figure itself: frame t, channel c.
        assert_eq!(tapped[(2000 * 6) + 3], (2001 * 4) as f32);
    }
}

/// The first frame is the first of the first call that begins after `open`
/// returns; a close stops the next call's writes. A second open is refused
/// while the reader lives, and allowed once it is dropped.
///
/// Mutation (run, recorded here): delete the `swap` at the top of `process`
/// -> the tap is never installed and the 64-frame assertion fails.
#[test]
fn a_tap_flows_from_the_next_call_and_stops_at_a_close() {
    let engine = engine(2);
    let taps = engine.heard_taps();
    render(&engine, 64, 2, false);
    let mut tap = taps
        .open(ChannelLayout::STEREO, Samples(1024))
        .expect("opens");
    assert_eq!(tap.available(), Samples(0), "nothing before a call renders");
    assert_eq!(
        taps.open(ChannelLayout::STEREO, Samples(1024)).err(),
        Some(HeardTapError::Busy)
    );
    let second = render(&engine, 64, 2, false);
    assert_eq!(
        tap.available(),
        Samples(64),
        "exactly the next call's block"
    );
    let mut got = vec![0.0f32; 128];
    tap.pop_into(&mut got);
    assert_eq!(got, second);
    taps.close();
    render(&engine, 64, 2, false);
    assert_eq!(
        tap.available(),
        Samples(0),
        "a closed tap gets nothing more"
    );
    drop(tap);
    let again = taps.open(ChannelLayout::STEREO, Samples(8));
    assert!(again.is_ok(), "a dropped reader frees the tap");
}

/// A ring that cannot hold the block keeps its first frames, counts the rest
/// once, and a block of another width than the tap's is counted whole.
///
/// Mutation (run, recorded here): drop the `count_lost` in `push_interleaved`
/// -> `take_lost` reads 0. Make `Hearing::write` push a block of the wrong
/// width instead of `skip` -> the width half fails (frames arrive).
#[test]
fn drops_are_counted_and_a_wrong_width_is_lost_whole() {
    let engine = engine(2);
    let mut tap = engine
        .heard_taps()
        .open(ChannelLayout::STEREO, Samples(100))
        .expect("opens");
    render(&engine, 300, 2, false);
    assert_eq!(tap.available(), Samples(100));
    assert_eq!(tap.take_lost(), Samples(200));
    assert_eq!(tap.take_lost(), Samples(0), "reading clears");
    let mut got = vec![0.0f32; 200];
    tap.pop_into(&mut got);
    assert_eq!(got[..4], [1.0, 2.0, 2.0, 4.0], "the first frames were kept");

    // The device now hands a 6-wide block to a 2-wide tap.
    let wide = engine_wide();
    let narrow = wide
        .heard_taps()
        .open(ChannelLayout::STEREO, Samples(1024))
        .expect("opens");
    render(&wide, 50, 6, false);
    assert_eq!(narrow.available(), Samples(0));
    assert_eq!(narrow.take_lost(), Samples(50));
}

fn engine_wide() -> Engine {
    engine(6)
}

/// Lanes cannot tear under a concurrent drain: a reader thread pops in small
/// bursts with pauses against a ring that overflows, and every frame that
/// arrives has all six channels from one instant (`(c + 1) * lane 0`), with
/// frames read plus frames lost equal to frames rendered.
///
/// Mutation (run, recorded here): in `push_interleaved`, push each lane its
/// own `frames` instead of the common `take` -> torn frames and the accounting
/// fails.
#[test]
fn channels_do_not_tear_under_a_concurrent_drain() {
    const CALLS: usize = 200;
    const FRAMES: usize = 64;
    let engine = engine(6);
    let mut tap = engine
        .heard_taps()
        .open(ChannelLayout::from_count(6), Samples(256))
        .expect("opens");
    let done = std::sync::atomic::AtomicBool::new(false);
    let (got, lost) = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let mut got = Vec::new();
            let mut buf = vec![0.0f32; 6 * 5];
            loop {
                let n = tap.pop_into(&mut buf).get();
                got.extend_from_slice(&buf[..n * 6]);
                if n == 0 {
                    if done.load(std::sync::atomic::Ordering::Acquire) && tap.available().get() == 0
                    {
                        break;
                    }
                } else {
                    std::thread::sleep(std::time::Duration::from_micros(100));
                }
            }
            (got, tap.take_lost().get())
        });
        let finished = Raise(&done);
        for _ in 0..CALLS {
            render(&engine, FRAMES, 6, false);
        }
        drop(finished);
        reader.join().expect("the reader finished")
    });
    assert!(lost > 0, "the ring overflowed, so the test saw drops");
    assert_eq!(got.len() / 6 + lost, CALLS * FRAMES);
    for frame in got.as_chunks::<6>().0 {
        for (c, &x) in frame.iter().enumerate() {
            assert_eq!(x, frame[0] * (c + 1) as f32, "a torn frame: {frame:?}");
        }
    }
}

/// Installing, replacing and closing a tap while blocks render never frees on
/// the audio thread (the engine enters `RtScope`, where a `TapWriter`'s drop
/// is a debug panic) and never loses the hand-over: a control thread opens,
/// drops and closes while the main thread renders.
///
/// Mutation (run, recorded here): in `Hearing::swap`, drop the writer it
/// replaces instead of sending it back -> the first replacement panics with
/// "a tap writer dropped on the audio thread".
#[test]
fn opening_and_closing_while_rendering_never_frees_on_the_audio_thread() {
    let engine = engine(2);
    let taps = engine.heard_taps();
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let control = scope.spawn(|| {
            let mut opened = 0;
            while !stop.load(std::sync::atomic::Ordering::Acquire) {
                if let Ok(tap) = taps.open(ChannelLayout::STEREO, Samples(64)) {
                    opened += 1;
                    std::thread::yield_now();
                    drop(tap);
                }
                taps.reclaim();
                taps.close();
            }
            opened
        });
        let finished = Raise(&stop);
        for _ in 0..3000 {
            render(&engine, 16, 2, false);
        }
        drop(finished);
        assert!(control.join().expect("control finished") > 0);
    });
}

/// A close is never lost to a full install queue. With no render in between,
/// opens fill the queue (the readers dropped, bar the last); the close still
/// has its reserved slot, so the next render installs everything in order and
/// ends on the close: the last reader gets nothing.
///
/// Mutation (run, recorded here): no reserved slot (`vacant_len() <= 1` ->
/// `== 0` in `open`, queue one smaller) -> the close fails for want of room,
/// the last writer stays installed and the reader gets the block.
#[test]
fn a_close_is_not_lost_to_a_full_install_queue() {
    let engine = engine(2);
    let taps = engine.heard_taps();
    let layout = ChannelLayout::STEREO;
    // No render between: four opens, each reader dropped before the next.
    let mut last = None;
    for _ in 0..4 {
        drop(last.take());
        last = Some(taps.open(layout, Samples(256)).expect("room for four"));
    }
    let last = last.expect("opened");
    taps.close();
    render(&engine, 64, 2, false);
    assert_eq!(
        last.available(),
        Samples(0),
        "the close came after its open"
    );
    // The queue drained; a new open works again.
    assert!(taps.open(layout, Samples(8)).is_ok());
}
