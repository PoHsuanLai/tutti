//! What a bounce renders past its range for the sampler's nodes, and what it
//! does with a voice pool: a placed clip is the programme and rings out
//! nothing, a free-running voice rings to the end of its wave, and a pool
//! forks only while its voices are the ones it was built with.

use std::sync::Arc;

use tutti_codec::Wave;
use tutti_graph::ForkError;
use tutti_sampler::{Clip, Loop, MemorySource, Playback, SlotId, VoicePool};
use tutti_session::{Bounce, BounceError, BounceTail, Range, Root, Session};
use tutti_types::{Beat, ChannelLayout, PlaybackRate, SampleRate, Samples, Seconds};

const RATE: f64 = 48_000.0;

fn session() -> Session {
    let mut session = Session::builder()
        .inputs(ChannelLayout::EMPTY)
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    session.graph().set_sample_rate(SampleRate::of(RATE));
    session
}

/// A mono wave of `frames` frames of `level`, at the session's rate.
fn constant(frames: usize, level: f32) -> Arc<Wave> {
    let mut wave = Wave::new(1, SampleRate::of(RATE));
    for _ in 0..frames {
        wave.push_frame(&[level]);
    }
    Arc::new(wave)
}

/// The master's frame count of a bounce of the first `seconds`, with the
/// tail the bounce picks without being asked.
fn length_of(session: &mut Session, seconds: f64) -> usize {
    let bounce = Bounce::project()
        .master_buffers()
        .range(Range::Seconds(Seconds(0.0), Seconds(seconds)))
        .sample_rate(SampleRate::of(RATE));
    session
        .bounce(bounce)
        .expect("the bounce renders")
        .into_master()
        .into_planes()[0]
        .len()
}

/// **A placed clip rings out nothing, so a bounce keeps the range and no
/// cap.** A two-second clip at beat 0, bounced over the first second with the
/// default tail: exactly the second's frames. A clip that declared an
/// unbounded tail rendered the tail's cap (ten seconds) after it.
#[test]
fn a_placed_clip_adds_no_tail_to_the_range() {
    let mut session = session();
    let (clip, _) = session
        .graph()
        .add(Clip::new(constant(2 * RATE as usize, 0.25)).expect("a placeable wave"));
    session.graph().connect(clip, Root).unwrap().hard();
    assert_eq!(length_of(&mut session, 1.0), RATE as usize);
}

/// **A free-running voice rings out the whole of its wave at the rate it
/// reads.** A 12 000-frame wave: its length after the range, half of it at
/// double speed; and a looped one never ends, so the bounce's cap is what it
/// keeps.
#[test]
fn a_free_running_voice_rings_out_its_wave() {
    let frames = 12_000;
    let at = |speed: f32, looped: Option<Loop>| {
        let mut session = session();
        let mut source = MemorySource::new(constant(frames, 0.25)).expect("a placeable wave");
        source.set_speed(PlaybackRate::new_clamped(speed));
        source.set_loop(looped);
        let (voice, _) = session.graph().add(source);
        session.graph().connect(voice, Root).unwrap().hard();
        length_of(&mut session, 1.0)
    };
    let second = RATE as usize;
    assert_eq!(at(1.0, None), second + frames);
    assert_eq!(at(2.0, None), second + frames / 2);

    let looped = Loop::new(Samples(0)..Samples(frames)).expect("a loop");
    let mut session = session();
    let mut source = MemorySource::new(constant(frames, 0.25)).expect("a placeable wave");
    source.set_loop(Some(looped));
    let (voice, _) = session.graph().add(source);
    session.graph().connect(voice, Root).unwrap().hard();
    let cap = Seconds(0.5);
    let bounce = Bounce::project()
        .master_buffers()
        .range(Range::Seconds(Seconds(0.0), Seconds(1.0)))
        .sample_rate(SampleRate::of(RATE))
        .tail(BounceTail::Graph { cap });
    let len = session
        .bounce(bounce)
        .expect("renders")
        .into_master()
        .into_planes()[0]
        .len();
    assert_eq!(len, second + (RATE * 0.5) as usize);
}

/// **A pool built with its voices forks them.** A voice placed at beat 0
/// inserted before the pool joins the graph is in the bounce, at its level.
#[test]
fn a_pool_built_with_its_voices_bounces_them() {
    let mut session = session();
    let mut pool = VoicePool::new();
    let source = MemorySource::placed(
        constant(RATE as usize, 0.25),
        tutti_sampler::VoiceWindow::from(Beat(0.0)),
    )
    .expect("a placeable wave");
    pool.insert(SlotId(1), source, Playback::default());
    let (pool, _handle) = session.graph().add(pool);
    session.graph().connect(pool, Root).unwrap().hard();

    let bounce = Bounce::project()
        .master_buffers()
        .range(Range::Seconds(Seconds(0.0), Seconds(0.5)))
        .sample_rate(SampleRate::of(RATE))
        .tail(BounceTail::None);
    let planes = session
        .bounce(bounce)
        .expect("renders")
        .into_master()
        .into_planes();
    assert_eq!(planes[0].len(), RATE as usize / 2);
    assert!(
        planes[0].iter().all(|&s| (s - 0.25).abs() < 1e-6),
        "the pool's voice is in the render at its level"
    );
}

/// **A pool whose handle has queued a command refuses the bounce, naming
/// it.** The voices it holds then arrive on the audio thread, where nothing
/// control-side holds a copy: a fork would render them as silence.
#[test]
fn a_pool_driven_through_its_handle_refuses_the_bounce_by_key() {
    let mut session = session();
    let (pool, handle) = session.graph().add(VoicePool::new());
    session.graph().connect(pool, Root).unwrap().hard();
    let source = MemorySource::placed(
        constant(RATE as usize, 0.25),
        tutti_sampler::VoiceWindow::from(Beat(0.0)),
    )
    .expect("a placeable wave");
    handle
        .add(SlotId(1), source, Playback::default())
        .expect("queued");

    let bounce = Bounce::project()
        .master_buffers()
        .range(Range::Seconds(Seconds(0.0), Seconds(0.5)))
        .sample_rate(SampleRate::of(RATE))
        .tail(BounceTail::None);
    match session.bounce(bounce) {
        Err(BounceError::Fork(ForkError::Source { key, .. })) => assert_eq!(key, pool.erase()),
        other => panic!(
            "expected the pool's refusal by key, got {:?}",
            other.map(|_| ())
        ),
    }
}
