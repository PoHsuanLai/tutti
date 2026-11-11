//! The engine an offline session builds, rendered from the builder's own
//! wiring (the click it inserts). Moved from `bevy-tutti`'s
//! `engine::build` tests, which rendered the same engine through the
//! adapter's `assemble`.
//!
//! Each test stands on analytic figures (onset frames, the tone a dry voice
//! reads), on what needs no oracle (a voice's render at two block sizes,
//! which a voice's frames do not depend on), and, for the samples
//! themselves, on golden digests. The click's was checked against an
//! independent reference implementation when it was recorded. The dry voice has
//! no digest: it is pinned to the tone itself. The pitched voice's digest is
//! self-recorded (the vocoder's FFT was replaced and no independent reference
//! renders it), so its cross-check is the fifth-up pitch figure. The click
//! and the pitched voice call `sin` (and the vocoder's FFT), libm
//! quality-of-implementation that differs in the last ulp between C runtimes,
//! so the digests are asserted only where they were recorded
//! (`tutti_fixtures::assert_golden`).

use std::sync::Arc;
use tutti_types::MaxBlock;

use tutti_fixtures::{assert_golden, digest};
use tutti_graph::Op;
use tutti_graph::TapAlign;
use tutti_nodes::MetronomeMode;
use tutti_nodes::testing::Const;
use tutti_session::{GraphSource, MissingSource, Session};
use tutti_transport::MotionEvent;
use tutti_types::{Amplitude, Beat, ChannelLayout, InterleavedMut, SampleRate, Samples};

const RATE: f64 = 48_000.0;

/// An offline session at `RATE`, stereo, graph blocks of up to 1 024 frames
/// (the live engine's `LIVE_MAX_BLOCK`, which the digests were recorded at).
fn offline() -> Session {
    Session::builder()
        .build_offline(
            SampleRate(RATE),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds")
}

/// `frames` stereo frames of `s` in `block`-frame device blocks, the
/// transport rolling from the first block, and (with `seek`) a locate to
/// beat 0.75 between two blocks past the first 60 000 frames.
fn run(s: &Session, frames: usize, block: usize, seek: bool) -> Vec<f32> {
    let transport = s.transport();
    transport
        .motion()
        .try_send(MotionEvent::Play)
        .expect("room");
    let mut out = Vec::with_capacity(frames * 2);
    let mut buf = vec![0.0f32; block * 2];
    let mut located = !seek;
    while out.len() < frames * 2 {
        // A seek between two blocks, past the first second: the beat clock
        // has to follow the transport, not just count frames.
        if !located && out.len() >= 2 * 60_000 {
            located = true;
            transport
                .motion()
                .try_send(MotionEvent::Locate {
                    beat: tutti_types::Beat(0.75),
                    fade: tutti_transport::FadeOut::Immediate,
                    then: tutti_transport::Then::Keep,
                })
                .expect("room");
        }
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        out.extend_from_slice(&buf);
    }
    out.truncate(frames * 2);
    out
}

/// The session's click at full volume on every beat, on both global
/// outputs, rolling from the first block: `frames` stereo frames in
/// 512-frame device blocks, with a seek past the first second.
///
/// The click has no inputs: it reads its block's `Env`.
fn render_click(frames: usize) -> Vec<f32> {
    let mut s = offline();
    let settings = Arc::clone(s.click_settings());
    settings.set_mode(MetronomeMode::Always);
    settings.set_volume(Amplitude::new(1.0));
    let click = s.click_node().expect("the builder inserts the click");
    for port in 0..2 {
        s.graph()
            .set_output_source(port, GraphSource::Node(click, port));
    }
    let _ = s.update();
    run(&s, frames, 512, true)
}

/// Frames on which the left channel starts sounding.
fn onsets(stereo: &[f32]) -> Vec<usize> {
    let left: Vec<f32> = stereo.iter().step_by(2).copied().collect();
    (1..left.len())
        .filter(|&f| left[f] != 0.0 && left[f - 1] == 0.0)
        .chain((left.first().is_some_and(|&x| x != 0.0)).then_some(0))
        .collect()
}

/// **The builder's engine clicks on every beat, across a seek**, and
/// clicks the golden digest's samples. The click reads the
/// beat of every frame from its block's `Env` (the transport the engine's
/// own `TransportClock` reports). The transport starts before the first
/// block, so the play gate opens on the first frame.
///
/// The onsets, analytically: at 120 BPM a beat is 24 000 frames, the seek
/// lands on the 118th 512-frame block (frame 60 416) at beat 0.75, and the
/// next beat is a quarter beat (6 000 frames) later; a click is heard from
/// the frame after its beat (its first sample is `sin(0)`). The locate
/// itself clicks too (60 417): it moves the whole beat from 2 to 0, which
/// the click takes for a new beat.
#[test]
fn the_click_sounds_on_every_beat_across_the_seek() {
    let frames = 3 * RATE as usize;
    let rendered = render_click(frames);
    assert_eq!(
        onsets(&rendered),
        vec![1, 24_001, 48_001, 60_417, 66_417, 90_417, 114_417, 138_417],
        "a click on every beat, across the seek"
    );
    assert_golden("the click", digest(&rendered), 0x9ac0_787e_520a_8085);
}

/// **The graph runs at the rate it was built at.** At 120 BPM and 48 kHz
/// the click lands every 24 000 frames. Every unit inserted is prepared at
/// the graph's rate, so a graph left at its 44.1 kHz default would click
/// every 22 050 frames.
#[test]
fn the_graph_runs_at_the_rate_it_was_built_at() {
    let on = onsets(&render_click(50_000));
    assert_eq!(on, vec![1, 24_001, 48_001]);
}

/// **The transport converts at the session's rate.** Its rate is not the
/// graph's: the engine's clock steps the beat at the graph's rate whatever
/// the transport holds, so the click above cannot see it. What reads it is
/// the conversion a host schedules against (a beat's frame, a time in
/// frames). At 120 BPM and 48 kHz a beat is 24 000 frames; a transport left
/// at its 44.1 kHz default would say 22 050.
#[test]
fn the_transport_converts_at_the_sessions_rate() {
    let s = offline();
    assert_eq!(s.transport().sample_rate(), SampleRate(RATE));
    let beat = (s.transport().settings().tempo_map())
        .frame_at_or_after(Beat(1.0), s.transport().sample_rate());
    assert_eq!(beat.get(), 24_000, "one beat at 120 BPM and 48 kHz");
}

// ---- a clip reader through the builder's engine ---------------------------

/// The tone's frame `i`: 440 Hz at `RATE`.
fn tone_at(i: usize) -> f32 {
    (std::f32::consts::TAU * 440.0 * i as f32 / RATE as f32).sin()
}

/// One sampler voice on a second of the tone, placed at beat 0 and pitched
/// by `cents`. It reads the transport from its blocks' `Env`.
fn voice(cents: f32) -> tutti_sampler::VoicePool {
    use tutti_sampler::{MemorySource, Playback, SlotId, Voice, VoicePool, VoiceSource};
    let mut wave = tutti_codec::Wave::new(1, SampleRate::new(RATE));
    for i in 0..RATE as usize {
        wave.push_frame(&[tone_at(i)]);
    }
    let source = MemorySource::placed(Arc::new(wave), tutti_types::Beat(0.0), None)
        .expect("a placeable wave");
    let mut pool = VoicePool::new();
    pool.insert_voice(
        SlotId(1),
        Voice {
            source: VoiceSource::Memory(source),
            play: Playback {
                pitch: tutti_types::Cents::new(cents),
                ..Default::default()
            },
        },
    );
    pool
}

/// An offline session with one sampler voice ([`voice`]) on both global
/// outputs, the transport rolling from the first block: `frames` stereo
/// frames in `block`-frame device blocks.
fn render_voice(cents: f32, frames: usize, block: usize) -> Vec<f32> {
    let mut s = offline();
    let (voice, _handle) = s.graph().insert(voice(cents));
    for port in 0..2 {
        s.graph()
            .set_output_source(port, GraphSource::Node(voice, port));
    }
    let _ = s.update();
    run(&s, frames, block, false)
}

/// The dominant frequency of `x[from..]` between 500 and 900 Hz: the peak
/// of its Hann-windowed spectrum, scanned in quarter-hertz steps. A
/// tolerance check on it is portable (a last-ulp libm difference moves no
/// peak).
fn dominant_frequency(x: &[f32], from: usize, rate: f64) -> f64 {
    use std::f64::consts::TAU;
    let w = &x[from..];
    let n = w.len() as f64;
    let mut best = (0.0, 0.0);
    let mut f = 500.0;
    while f < 900.0 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &s) in w.iter().enumerate() {
            let hann = 0.5 - 0.5 * (TAU * i as f64 / n).cos();
            let p = TAU * f * i as f64 / rate;
            re += f64::from(s) * hann * p.cos();
            im -= f64::from(s) * hann * p.sin();
        }
        let m = re * re + im * im;
        if m > best.1 {
            best = (f, m);
        }
        f += 0.25;
    }
    best.0
}

/// **A sampler voice plays in time through the builder's engine**, at
/// `block`-frame device blocks with a rolling transport: the dry voice is
/// the tone it plays, to the bit, on both channels; the voice a fifth up
/// renders the same bits as at 64-frame blocks, and the
/// golden digest's.
fn a_voice_plays_in_time(block: usize) {
    let frames = 24_000;
    // The dry voice is pinned to the tone itself; only the pitched one,
    // which has no closed form, carries a digest.
    for (cents, want) in [(0.0f32, None), (700.0, Some(0xacb2_45f3_a464_4a49u64))] {
        let rendered = render_voice(cents, frames, block);
        let peak = rendered.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.5, "{block}-frame blocks, {cents} cents: silent");
        if cents == 0.0 {
            for (i, s) in rendered.as_chunks::<2>().0.iter().enumerate() {
                let want = tone_at(i).to_bits();
                assert!(
                    s[0].to_bits() == want && s[1].to_bits() == want,
                    "{block}-frame blocks: frame {i} read {s:?}, the tone is {}",
                    tone_at(i)
                );
            }
        } else {
            // Portable, where the digest is not: a fifth up from 440 Hz is
            // 440 · 2^(7/12) ≈ 659.26 Hz. Past the vocoder's first few
            // thousand frames, within 1% (a semitone is 6%).
            let left: Vec<f32> = rendered.iter().step_by(2).copied().collect();
            let want = 440.0 * 2f64.powf(7.0 / 12.0);
            let got = dominant_frequency(&left, 4_096, RATE);
            assert!(
                (got - want).abs() < want * 1e-2,
                "{block}-frame blocks: {got} Hz, a fifth up is {want} Hz"
            );
            let by64 = render_voice(cents, frames, 64);
            if let Some(i) = by64
                .iter()
                .zip(&rendered)
                .position(|(a, b)| a.to_bits() != b.to_bits())
            {
                panic!(
                    "{block}-frame blocks, {cents} cents: parts from 64-frame blocks at \
                     frame {} (channel {}): {} vs {}",
                    i / 2,
                    i % 2,
                    rendered[i],
                    by64[i]
                );
            }
        }
        // The pitched digest is self-recorded: the vocoder's scalar FFT
        // rounding over the portable figures above, which are the oracle. It
        // moves only when that rounding does.
        if let Some(want) = want {
            assert_golden(&format!("voice, {cents} cents"), digest(&rendered), want);
        }
    }
}

#[test]
fn a_voice_plays_in_time_at_every_device_block_size() {
    for block in [256, 512] {
        a_voice_plays_in_time(block);
    }
}

/// An offline session's [`Session::render`] feeds its load meter, one block
/// per call; a detached session has no engine and no meter.
#[test]
fn an_offline_render_feeds_the_load_meter() {
    let s = offline();
    let meter = s.load_meter().expect("an offline engine").clone();
    let mut block = vec![0.0f32; 128 * 2];
    for _ in 0..4 {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));
    }
    let r = meter.take();
    assert_eq!(r.blocks, 4);
    assert!(r.mean > 0.0, "a render takes some time");
    let detached = Session::builder()
        .build_detached(SampleRate(RATE))
        .expect("builds");
    assert!(detached.load_meter().is_none());
}

/// A session's master meter is an op in its plan, declared and on from the
/// build: [`Session::render`] feeds it as a device callback would, and so does
/// a detached session's [`Session::render_frame`] (it had no meter while the
/// engine fed it).
#[test]
fn an_offline_render_feeds_the_master_meter() {
    let mut s = offline();
    let mut meter = s.take_master_meter().expect("declared at the build");
    let (dc, _) = s.graph().insert(Const::mono(0.5));
    s.graph().set_output_source(0, GraphSource::Node(dc, 0));
    s.graph().set_output_source(1, GraphSource::Node(dc, 0));
    let _ = s.update();
    let mut block = vec![0.0f32; 128 * 2];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));
    let r = meter.read();
    assert!((r[0].peak.get() - 0.5).abs() < 1e-6, "{r:?}");
    assert!((r[1].rms.get() - 0.5).abs() < 1e-6, "{r:?}");
    assert_eq!(r[0].frames, Samples(128));

    let mut detached = Session::builder()
        .outputs_floor(1)
        .build_detached(SampleRate(RATE))
        .expect("builds");
    let mut meter = detached.take_master_meter().expect("declared at the build");
    let (dc, _) = detached.graph().insert(Const::mono(0.5));
    detached
        .graph()
        .set_output_source(0, GraphSource::Node(dc, 0));
    let mut frame = [0.0f32];
    detached.render_frame(&mut frame);
    assert_eq!(meter.read()[0].peak.get(), 0.5);
    assert!(
        detached.take_master_meter().is_none(),
        "taken once: one consumer"
    );
}

/// Opening a tap or a meter on a source the graph lacks is refused, with the
/// source named, and opens nothing; a key no node has is no zero-width meter.
/// A node inserted since the last update is accepted (the edited graph is
/// what is checked).
#[test]
fn opening_a_tap_or_meter_on_a_missing_source_is_refused() {
    let mut s = offline();
    let (a, _) = s.graph().insert(Const::mono(0.5));
    let ghost = tutti_graph::NodeKey(0xdead);
    assert_eq!(
        s.graph().open_node_meter(ghost).err(),
        Some(MissingSource::Node { node: ghost })
    );
    assert_eq!(
        s.graph()
            .open_tap(&[GraphSource::Node(a, 1)], TapAlign::Raw, Samples(64))
            .err(),
        Some(MissingSource::Port {
            node: a,
            port: 1,
            width: 1
        })
    );
    assert_eq!(
        s.graph()
            .open_meter(&[GraphSource::Input(0)], TapAlign::Raw)
            .err(),
        Some(MissingSource::Global {
            channel: 0,
            width: 0
        })
    );
    s.graph().set_output_source(0, GraphSource::Node(a, 5));
    assert!(
        s.graph().open_output_tap(Samples(64)).is_err(),
        "an output wired to a missing port"
    );
    s.graph().set_output_source(0, GraphSource::Node(a, 0));
    s.graph()
        .open_node_meter(a)
        .expect("it exists, uncommitted");
    s.graph()
        .open_output_tap(Samples(64))
        .expect("the outputs exist");
}

/// A track meter declared on a node's port reads that node, through `Graph`;
/// a second meter on the same port is its own consumer; and removing the node
/// leaves the channel on silence while the graph still commits. A dropped
/// reader's meter is closed by the Reclaim phase and its op leaves the plan.
#[test]
fn a_track_meter_reads_a_nodes_port_and_goes_with_the_node() {
    let mut s = offline();
    let (a, _) = s.graph().insert(Const::mono(0.5));
    let (b, _) = s.graph().insert(Const::mono(0.25));
    s.graph().set_output_source(0, GraphSource::Node(a, 0));
    // The meters are the only edit of the second update.
    let _ = s.update();
    let mut track = s.graph().open_node_meter(a).expect("the sources exist");
    let mut other = s
        .graph()
        .open_meter(
            &[GraphSource::Node(a, 0), GraphSource::Node(b, 0)],
            TapAlign::Raw,
        )
        .expect("the sources exist");
    track.enable();
    other.enable();
    let _ = s.update();
    let mut block = vec![0.0f32; 64 * 2];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));
    assert_eq!(track.channels(), 1);
    assert_eq!(track.read()[0].peak.get(), 0.5);
    let o = other.read();
    assert_eq!((o[0].peak.get(), o[1].peak.get()), (0.5, 0.25));

    s.graph().remove(b);
    let _ = s.update();
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));
    let o = other.read();
    assert_eq!((o[0].peak.get(), o[1].peak.get()), (0.5, 0.0));
}

/// A dropped meter reader is closed by the `Reclaim` phase: the meter's op
/// leaves the plan at the commit after it, and the master meter's (held by
/// the session) stays.
#[test]
fn a_dropped_meter_reader_leaves_the_plan_at_the_reclaim_phase() {
    let meters = |s: &Session| {
        s.graph_ref()
            .sent_plan()
            .expect("a plan was sent")
            .ops()
            .iter()
            .filter(|op| matches!(op, Op::Meter { .. }))
            .count()
    };
    let mut s = offline();
    let (a, _) = s.graph().insert(Const::mono(0.5));
    let track = s.graph().open_node_meter(a).expect("the sources exist");
    let _ = s.update();
    assert_eq!(meters(&s), 2, "the master's and the track's");
    drop(track);
    let _ = s.update();
    assert_eq!(
        meters(&s),
        2,
        "the reclaim of this update closed it; the plan is the last commit's"
    );
    let _ = s.update();
    assert_eq!(meters(&s), 1, "the master's stays");
}

/// A tap on an offline session's outputs is fed by [`Session::render`]: a
/// 64-frame block leaves 64 frames in the tap, each the output's analytic
/// value (a constant 0.5 on the left, silence on the right), in order.
#[test]
fn an_offline_render_feeds_an_output_tap() {
    let mut s = offline();
    let (dc, _) = s.graph().insert(Const::mono(0.5));
    s.graph().set_output_source(0, GraphSource::Node(dc, 0));
    // The tap is the only edit of the second update.
    let _ = s.update();
    let mut tap = s
        .graph()
        .open_output_tap(Samples(1_024))
        .expect("the sources exist");
    assert_eq!(tap.width(), 2);
    let _ = s.update();
    let mut block = vec![0.0f32; 64 * 2];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));

    assert_eq!(tap.available(), Samples(64));
    let mut got = vec![f32::NAN; 64 * 2];
    assert_eq!(tap.pop_into(&mut got), Samples(64));
    assert!(
        got.as_chunks::<2>().0.iter().all(|f| *f == [0.5, 0.0]),
        "{got:?}"
    );
    assert_eq!(tap.take_lost(), Samples(0));
}

/// An output tap follows the outputs when they are re-wired: lane 0 reads
/// the new source from the next commit on.
#[test]
fn an_output_tap_follows_a_rewired_output() {
    let mut s = offline();
    let (a, _) = s.graph().insert(Const::mono(0.5));
    let (b, _) = s.graph().insert(Const::mono(0.25));
    s.graph().set_output_source(0, GraphSource::Node(a, 0));
    let mut tap = s
        .graph()
        .open_output_tap(Samples(1_024))
        .expect("the sources exist");
    let _ = s.update();
    let mut block = vec![0.0f32; 64 * 2];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));
    s.graph().set_output_source(0, GraphSource::Node(b, 0));
    let _ = s.update();
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));

    let mut got = vec![f32::NAN; 128 * 2];
    assert_eq!(tap.pop_into(&mut got), Samples(128));
    let left: Vec<f32> = got.iter().step_by(2).copied().collect();
    assert!(left[..64].iter().all(|&x| x == 0.5), "before: {left:?}");
    assert!(left[64..].iter().all(|&x| x == 0.25), "after: {left:?}");
}

/// A tap whose reader was dropped is closed by the next `Reclaim` phase and
/// leaves the plan at the commit after it.
#[test]
fn a_dropped_reader_leaves_the_plan_at_the_reclaim_phase() {
    let taps = |s: &Session| {
        s.graph_ref()
            .sent_plan()
            .expect("a plan was sent")
            .ops()
            .iter()
            .filter(|op| matches!(op, Op::Tap { .. }))
            .count()
    };
    let mut s = offline();
    let tap = s
        .graph()
        .open_output_tap(Samples(64))
        .expect("the sources exist");
    let _ = s.update();
    assert_eq!(taps(&s), 1, "one op a tap while it is open");
    drop(tap);
    let _ = s.update();
    assert_eq!(
        taps(&s),
        1,
        "the reclaim of this update closed it; the plan is the last commit's"
    );
    let _ = s.update();
    assert_eq!(taps(&s), 0, "closed");
}

/// A detached session renders by hand ([`Session::render_frame`]) and its
/// taps are fed too: the tap is an op in the plan, not the engine's.
#[test]
fn a_detached_render_feeds_a_tap() {
    let mut s = Session::builder()
        .outputs_floor(1)
        .build_detached(SampleRate(RATE))
        .expect("builds");
    let (dc, _) = s.graph().insert(Const::mono(0.5));
    s.graph().set_output_source(0, GraphSource::Node(dc, 0));
    let mut tap = s
        .graph()
        .open_output_tap(Samples(16))
        .expect("the sources exist");
    let mut frame = [0.0f32];
    for _ in 0..3 {
        s.render_frame(&mut frame);
    }
    let mut got = [f32::NAN; 3];
    assert_eq!(tap.pop_into(&mut got), Samples(3));
    assert_eq!(got, [0.5; 3]);
}

/// A source that plays 0.5 only while the transport rolls, as every
/// transport-gated source (a clip, a click, a sampler voice) does.
struct Gated;

impl tutti_graph::Node for Gated {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        cx: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        for k in cx.env.offsets() {
            io.output(0)[k.index()] = if cx.env.transport_at(k).playing {
                0.5
            } else {
                0.0
            };
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// **What a take sees across a transport stop and start.** A tap is the
/// graph's signal before the engine's declick fader. A transport-gated source
/// (a clip, a click, a sampler voice) stops on the stop's frame, so at a
/// declicked stop the tap steps from the source's whole level to silence in
/// one frame while the engine's output buffer has faded to it over the declick (10 ms):
/// a recording through a tap has a click at every stop of such a source, which
/// the old post-fader capture did not. A source that keeps running through the
/// stop (live input, a reverb tail) has none. At a start the output has no fade
/// either (`Play` is not declicked), so the output and the tap step alike.
/// The post-fade write is the fix for the stop (not in this change).
///
/// This is a characterisation, so it fails if
/// the declick stops being a ramp in the output, or the tap stops being
/// pre-fade (a tap fed after the fader would ramp too).
#[test]
fn a_tap_steps_at_a_transport_stop_where_the_output_fades() {
    use tutti_types::{At, Frame};
    let mut s = offline();
    let (gate, _) = s.graph().add(tutti_graph::Unforkable(Gated));
    s.graph().set_output_source(0, GraphSource::Node(gate, 0));
    s.graph().set_output_source(1, GraphSource::Node(gate, 0));
    let mut tap = s
        .graph()
        .open_output_tap(Samples(16_384))
        .expect("the sources exist");
    let _ = s.update();
    let motion = &s.transport().motion();
    motion.try_send(MotionEvent::Play).expect("room");
    motion
        .schedule(At::Frame(Frame(2_000)), MotionEvent::stop())
        .expect("room");
    motion
        .schedule(At::Frame(Frame(4_000)), MotionEvent::Play)
        .expect("room");
    let (mut heard, mut tapped) = (Vec::new(), Vec::new());
    let mut block = vec![0.0f32; 64 * 2];
    for _ in 0..100 {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));
        heard.extend(block.iter().step_by(2));
    }
    let mut got = vec![0.0f32; 6_400 * 2];
    let n = tap.pop_into(&mut got).get();
    tapped.extend(got[..n * 2].iter().step_by(2));
    assert_eq!(tapped.len(), heard.len());
    let step = |x: &[f32], from: usize, to: usize| {
        x[from..to]
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max)
    };
    // Around the stop at frame 2000, and at the start at frame 4000.
    let (out_stop, tap_stop) = (step(&heard, 1_500, 2_500), step(&tapped, 1_500, 2_500));
    let (out_start, tap_start) = (step(&heard, 3_500, 4_500), step(&tapped, 3_500, 4_500));
    println!("stop: output {out_stop}, tap {tap_stop}; start: output {out_start}, tap {tap_start}");
    assert!(
        out_stop < 0.01,
        "the declick ramps the output at a stop: {out_stop}"
    );
    assert!(
        (tap_stop - 0.5).abs() < 1e-6,
        "the tap steps by the whole level: {tap_stop}"
    );
    assert!((out_start - 0.5).abs() < 1e-6 && (tap_start - 0.5).abs() < 1e-6);
}

/// **The heard tap carries the fade at a stop.** The same render as
/// `a_tap_steps_at_a_transport_stop_where_the_output_fades`, read through the
/// tap after the fader: it is the engine's output buffer bit for bit on both channels (the f32 buffer `render` fills, not the device's bytes)
/// (an invariant of this one render, not a second render), so its largest step
/// across the stop is the declick's ramp (about 0.001) where the plan tap's
/// is the source's whole level (0.5). A take recorded through it has no click.
#[test]
fn the_heard_tap_carries_the_fade_at_a_stop() {
    use tutti_types::{At, Frame};
    let mut s = offline();
    let (gate, _) = s.graph().add(tutti_graph::Unforkable(Gated));
    s.graph().set_output_source(0, GraphSource::Node(gate, 0));
    s.graph().set_output_source(1, GraphSource::Node(gate, 0));
    let mut plan = s
        .graph()
        .open_output_tap(Samples(16_384))
        .expect("the sources exist");
    let mut heard = s.open_heard_tap(Samples(16_384)).expect("an engine");
    let _ = s.update();
    let motion = &s.transport().motion();
    motion.try_send(MotionEvent::Play).expect("room");
    motion
        .schedule(At::Frame(Frame(2_000)), MotionEvent::stop())
        .expect("room");
    motion
        .schedule(At::Frame(Frame(4_000)), MotionEvent::Play)
        .expect("room");
    let mut device = Vec::new();
    let mut block = vec![0.0f32; 64 * 2];
    for _ in 0..100 {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));
        device.extend_from_slice(&block);
    }
    let mut tapped = vec![0.0f32; 6_400 * 2];
    assert_eq!(heard.pop_into(&mut tapped), Samples(6_400));
    let bits = |x: &[f32]| x.iter().map(|s| s.to_bits()).collect::<Vec<_>>();
    assert_eq!(
        bits(&tapped),
        bits(&device),
        "the tap is what the engine rendered"
    );
    let mut unfaded = vec![0.0f32; 6_400 * 2];
    plan.pop_into(&mut unfaded);
    let step = |x: &[f32]| {
        x.iter()
            .step_by(2)
            .skip(1_500)
            .take(1_000)
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max)
    };
    let (faded, cliff) = (step(&tapped), step(&unfaded));
    println!("stop: heard tap {faded}, plan tap {cliff}");
    assert!(faded < 0.01, "the heard tap ramps: {faded}");
    assert!((cliff - 0.5).abs() < 1e-6, "the plan tap steps: {cliff}");
}

/// Offline, a block `render` pulls is written whole; a detached session has no
/// engine and says so; a fork (what an export renders) has no write into the
/// live tap.
#[test]
fn render_feeds_the_heard_tap_and_a_fork_does_not() {
    let mut s = offline();
    let (dc, _) = s.graph().insert(Const::mono(0.5));
    s.graph().set_output_source(0, GraphSource::Node(dc, 0));
    s.graph().set_output_source(1, GraphSource::Node(dc, 0));
    let _ = s.update();
    let tap = s.open_heard_tap(Samples(1_024)).expect("an engine");
    assert_eq!(
        s.open_heard_tap(Samples(8)).err(),
        Some(tutti_session::HeardTapError::Busy)
    );
    let mut block = vec![0.0f32; 64 * 2];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));
    assert_eq!(tap.available(), Samples(64));
    // An export renders a fork, without the engine: nothing reaches the tap.
    let mut fork = s.graph_ref().fork_live().expect("forks");
    let mut out = [0.0f32; 2];
    for _ in 0..64 {
        fork.tick(&[], &mut out);
    }
    assert_eq!(tap.available(), Samples(64), "the fork wrote nothing");
    assert_eq!(out, [0.5, 0.5]);
    tap.take_lost();
    let detached = Session::builder()
        .build_detached(SampleRate(RATE))
        .expect("builds");
    assert_eq!(
        detached.open_heard_tap(Samples(8)).err(),
        Some(tutti_session::HeardTapError::Detached)
    );
}

/// A dropped heard reader frees the tap for another open, with the session's
/// update (and its `Reclaim` phase, which sends the close to the engine)
/// between. Opening itself sees a dead reader, so the phase's close is not
/// observable here; it keeps the engine from writing for nobody, which the
/// writer also checks (`read_is_held`).
#[test]
fn a_dropped_heard_reader_frees_the_tap() {
    let mut s = offline();
    let tap = s.open_heard_tap(Samples(64)).expect("an engine");
    drop(tap);
    let _ = s.update();
    let mut block = vec![0.0f32; 64 * 2];
    s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));
    assert!(s.open_heard_tap(Samples(64)).is_ok());
}

/// A project the device has no place for is refused when the session is built,
/// not rendered silent: a six-channel project on a five-channel interface would
/// lose a channel, and neither width has an order to fold it by. Widened, or
/// into a device whose width has one, it builds.
#[test]
fn a_project_the_device_cannot_take_is_refused_at_build() {
    let build = |floor: usize, device: u16| {
        Session::builder().outputs_floor(floor).build_offline(
            SampleRate(RATE),
            ChannelLayout::from(device),
            MaxBlock::of(Samples(256)),
        )
    };
    assert!(matches!(
        build(6, 5),
        Err(tutti_session::Error::Output(
            tutti_types::FoldError::Unplaced { channel: 5, .. }
        ))
    ));
    assert!(matches!(build(8, 7), Err(tutti_session::Error::Output(_))));
    for (floor, device) in [(6, 6), (6, 2), (2, 5), (2, 3), (5, 7)] {
        assert!(build(floor, device).is_ok(), "{floor} on {device}");
    }
}
